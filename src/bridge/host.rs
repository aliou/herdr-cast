//! The host Mac's resident bridge daemon. One per user (the state directory
//! is shared by every session), tied to the Herdr server whose hook spawned
//! it. Every few seconds it lists the running `herdr --remote` clients and
//! keeps one ssh link per remote machine, running `herdr-cast bridge-relay`
//! on the far end.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::targets::{Resolver, Wanted};
use super::tunnel::{self, Outcome};
use super::wire::{self, Frame};
use crate::api::SocketClient;

const TICK: Duration = Duration::from_secs(3);
/// A link that has not exchanged hellos by then is killed and retried.
const CONNECT_DEADLINE: Duration = Duration::from_secs(15);
const MIN_BACKOFF: Duration = Duration::from_secs(5);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// A link that lived this long was healthy; its next retry starts over.
const STABLE_UPTIME: Duration = Duration::from_secs(60);
/// A remote without `bridge-relay` is retried this rarely.
const MISSING_RELAY_RETRY: Duration = Duration::from_secs(60);
/// How long ssh gets to exit on its own once its link ended.
const EXIT_GRACE: Duration = Duration::from_secs(2);
const EXIT_AFTER_FAILED_PINGS: u32 = 10;
const SOCKET_TIMEOUT: Duration = Duration::from_millis(500);
const PBCOPY_TIMEOUT: Duration = Duration::from_secs(3);
/// How often the notifier registration is re-checked, well inside its TTL.
const REGISTRATION_INTERVAL: Duration = Duration::from_secs(10 * 60);
const STDERR_TAIL: usize = 5;

pub fn run() -> Result<(), String> {
    if !cfg!(target_os = "macos") {
        return Err("the bridge host runs only on macOS".to_string());
    }
    let state = crate::daemon::state_dir()?;
    let Some(_lock) = crate::daemon::acquire(&state.join(super::LOCK_NAME))? else {
        return Ok(());
    };
    let socket =
        std::env::var("HERDR_SOCKET_PATH").map_err(|_| "HERDR_SOCKET_PATH not set".to_string())?;
    let client = SocketClient::with_timeout(&socket, SOCKET_TIMEOUT);
    log(&format!(
        "started, pid {}, server {socket}",
        std::process::id()
    ));
    let mut resolver = Resolver::default();
    let mut links: BTreeMap<String, Link> = BTreeMap::new();
    let mut failed_pings = 0;
    let mut registered_at: Option<Instant> = None;
    loop {
        if registered_at.is_none_or(|at| at.elapsed() >= REGISTRATION_INTERVAL) {
            crate::notify::prepare_bridged_delivery();
            registered_at = Some(Instant::now());
        }
        match client.send("cast:bridge-ping", "ping", serde_json::json!({})) {
            Ok(_) => failed_pings = 0,
            Err(error) => {
                failed_pings += 1;
                if failed_pings >= EXIT_AFTER_FAILED_PINGS {
                    log(&format!("server unreachable, exiting: {error}"));
                    links.values_mut().for_each(Link::stop);
                    return Ok(());
                }
            }
        }
        match resolver.scan() {
            Ok(wanted) => reconcile(&mut links, wanted),
            Err(error) => log(&format!("target scan failed: {error}")),
        }
        std::thread::sleep(TICK);
    }
}

fn reconcile(links: &mut BTreeMap<String, Link>, wanted: BTreeMap<String, Wanted>) {
    links.retain(|key, link| {
        let keep = wanted.contains_key(key);
        if !keep {
            log(&format!(
                "{}: no remote client left, disconnecting",
                link.target
            ));
            link.stop();
        }
        keep
    });
    let now = Instant::now();
    for (key, found) in wanted {
        let link = links
            .entry(key)
            .or_insert_with(|| Link::new(found.target.clone()));
        link.pids = found.pids;
        link.poll(now);
    }
}

/// One remote machine's link and its retry state.
struct Link {
    target: String,
    pids: BTreeSet<u32>,
    running: Option<Running>,
    retry_at: Instant,
    backoff: Duration,
    /// Set when another link took over the remote socket: stay idle until
    /// the set of clients for this machine changes.
    parked: Option<BTreeSet<u32>>,
}

struct Running {
    handle: JoinHandle<Report>,
    child: Arc<Mutex<Child>>,
    connected: Arc<AtomicBool>,
    started: Instant,
}

struct Report {
    outcome: Outcome,
    status: Option<ExitStatus>,
    stderr: Vec<String>,
    uptime: Duration,
    connected: bool,
}

impl Link {
    fn new(target: String) -> Self {
        Self {
            target,
            pids: BTreeSet::new(),
            running: None,
            retry_at: Instant::now(),
            backoff: Duration::ZERO,
            parked: None,
        }
    }

    fn stop(&mut self) {
        if let Some(running) = &self.running {
            kill(&running.child);
        }
    }

    fn poll(&mut self, now: Instant) {
        if let Some(running) = &self.running {
            if !running.handle.is_finished() {
                let stuck = !running.connected.load(Ordering::SeqCst)
                    && running.started.elapsed() > CONNECT_DEADLINE;
                if stuck {
                    log(&format!(
                        "{}: no hello within {CONNECT_DEADLINE:?}",
                        self.target
                    ));
                    kill(&running.child);
                }
                return;
            }
        }
        if let Some(running) = self.running.take() {
            match running.handle.join() {
                Ok(report) => self.schedule(report, now),
                Err(_) => self.retry_later(now, false),
            }
        }
        if self.parked.as_ref() == Some(&self.pids) {
            return;
        }
        self.parked = None;
        if now < self.retry_at {
            return;
        }
        match spawn(&self.target) {
            Ok(running) => self.running = Some(running),
            Err(error) => {
                log(&format!("{}: {error}", self.target));
                self.retry_later(now, false);
            }
        }
    }

    fn schedule(&mut self, report: Report, now: Instant) {
        log(&format!(
            "{}: link ended after {:?}: {:?}, ssh {}",
            self.target,
            report.uptime,
            report.outcome,
            report
                .status
                .map_or_else(|| "still running".to_string(), |status| status.to_string())
        ));
        if report.outcome == Outcome::Bye("replaced".to_string()) {
            log(&format!(
                "{}: another link owns the remote socket; parked",
                self.target
            ));
            self.parked = Some(self.pids.clone());
            return;
        }
        if !report.connected && missing_relay(&report) {
            log(&format!(
                "{}: remote herdr-cast lacks bridge-relay; retrying in {MISSING_RELAY_RETRY:?}",
                self.target
            ));
            self.retry_at = now + MISSING_RELAY_RETRY;
            return;
        }
        self.retry_later(now, report.uptime >= STABLE_UPTIME);
    }

    fn retry_later(&mut self, now: Instant, was_stable: bool) {
        self.backoff = if was_stable {
            MIN_BACKOFF
        } else {
            (self.backoff * 2).clamp(MIN_BACKOFF, MAX_BACKOFF)
        };
        self.retry_at = now + self.backoff;
    }
}

/// An old remote build prints its usage for the unknown command; a remote
/// without herdr-cast on PATH exits 127 from its shell.
fn missing_relay(report: &Report) -> bool {
    report.status.and_then(|status| status.code()) == Some(127)
        || report
            .stderr
            .iter()
            .any(|line| line.contains("usage: herdr-cast"))
}

fn spawn(target: &str) -> Result<Running, String> {
    let mut child = Command::new("ssh")
        .args(ssh_arguments(target))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("failed to start ssh: {error}"))?;
    let (Some(stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        let _ = child.kill();
        let _ = child.wait();
        return Err("ssh started without piped stdio".to_string());
    };
    log(&format!("{target}: connecting"));
    let child = Arc::new(Mutex::new(child));
    let connected = Arc::new(AtomicBool::new(false));
    let tail = Arc::new(Mutex::new(VecDeque::new()));
    let stderr_reader = {
        let tail = Arc::clone(&tail);
        let target = target.to_string();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                log(&format!("{target}: remote: {line}"));
                if let Ok(mut tail) = tail.lock() {
                    if tail.len() == STDERR_TAIL {
                        tail.pop_front();
                    }
                    tail.push_back(line);
                }
            }
        })
    };
    let started = Instant::now();
    let handle = {
        let child = Arc::clone(&child);
        let connected = Arc::clone(&connected);
        let target = target.to_string();
        std::thread::spawn(move || {
            let outcome = tunnel::run(stdout, stdin, &target, &apply, &connected, &log);
            let status = reap(&child);
            let _ = stderr_reader.join();
            Report {
                outcome,
                status,
                stderr: tail
                    .lock()
                    .map(|tail| tail.iter().cloned().collect())
                    .unwrap_or_default(),
                uptime: started.elapsed(),
                connected: connected.load(Ordering::SeqCst),
            }
        })
    };
    Ok(Running {
        handle,
        child,
        connected,
        started,
    })
}

fn ssh_arguments(target: &str) -> Vec<String> {
    let mut arguments = vec!["-T".to_string()];
    for option in [
        "BatchMode=yes",
        "ConnectTimeout=5",
        "ServerAliveInterval=15",
        "ServerAliveCountMax=3",
        "ControlMaster=no",
        "ControlPath=none",
    ] {
        arguments.push("-o".to_string());
        arguments.push(option.to_string());
    }
    arguments.push(target.to_string());
    arguments.push("exec herdr-cast bridge-relay".to_string());
    arguments
}

fn kill(child: &Mutex<Child>) {
    if let Ok(mut child) = child.lock() {
        let _ = child.kill();
    }
}

/// Give ssh a moment to exit on its own so its real status is kept, then
/// kill it.
fn reap(child: &Mutex<Child>) -> Option<ExitStatus> {
    let deadline = Instant::now() + EXIT_GRACE;
    while Instant::now() < deadline {
        match child.lock().ok()?.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => return None,
        }
    }
    let mut child = child.lock().ok()?;
    let _ = child.kill();
    child.wait().ok()
}

fn apply(frame: &Frame, body: Option<Vec<u8>>) -> Result<(), String> {
    match frame {
        Frame::Notify(notification) => crate::notify::deliver_bridged(notification),
        Frame::Pasteboard { mime, .. } if mime == wire::TEXT_MIME => {
            set_pasteboard(body.unwrap_or_default())
        }
        Frame::Pasteboard { mime, .. } => Err(format!("unsupported pasteboard type {mime}")),
        _ => Err("not a request".to_string()),
    }
}

fn set_pasteboard(body: Vec<u8>) -> Result<(), String> {
    let text = String::from_utf8(body).map_err(|_| "pasteboard text is not UTF-8".to_string())?;
    let mut child = Command::new("/usr/bin/pbcopy")
        .env("LANG", "en_US.UTF-8")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("failed to start pbcopy: {error}"))?;
    let written = child
        .stdin
        .take()
        .ok_or_else(|| "pbcopy has no stdin".to_string())
        .and_then(|mut stdin| {
            stdin
                .write_all(text.as_bytes())
                .map_err(|error| format!("failed to write to pbcopy: {error}"))
        });
    let status = crate::notify::wait_with_timeout(&mut child, PBCOPY_TIMEOUT)
        .ok_or_else(|| format!("pbcopy did not exit within {PBCOPY_TIMEOUT:?}"))?;
    written?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("pbcopy failed with {status}"))
    }
}

fn log(message: &str) {
    let _ = writeln!(
        std::io::stderr(),
        "{} [cast bridge] {message}",
        super::timestamp()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_never_reuses_a_control_master_or_prompts() {
        let arguments = ssh_arguments("donut");
        let joined = arguments.join(" ");
        assert!(joined.starts_with("-T "));
        assert!(joined.contains("-o BatchMode=yes"));
        assert!(joined.contains("-o ControlPath=none"));
        assert_eq!(
            &arguments[arguments.len() - 2..],
            ["donut", "exec herdr-cast bridge-relay"]
        );
    }

    #[test]
    fn backoff_doubles_to_a_cap_and_resets_after_a_stable_link() {
        let mut link = Link::new("donut".into());
        let now = Instant::now();
        let mut delays = Vec::new();
        for _ in 0..6 {
            link.retry_later(now, false);
            delays.push(link.backoff.as_secs());
        }
        assert_eq!(delays, [5, 10, 20, 40, 60, 60]);
        link.retry_later(now, true);
        assert_eq!(link.backoff, MIN_BACKOFF);
    }

    #[test]
    fn detects_a_remote_without_the_relay_command() {
        let report = |stderr: &[&str]| Report {
            outcome: Outcome::Closed,
            status: None,
            stderr: stderr.iter().map(|line| line.to_string()).collect(),
            uptime: Duration::ZERO,
            connected: false,
        };
        assert!(missing_relay(&report(&[
            "",
            "[cast] usage: herdr-cast <pane-focused|...>"
        ])));
        assert!(!missing_relay(&report(&["Connection refused"])));
    }
}
