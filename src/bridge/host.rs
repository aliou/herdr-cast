//! The host Mac's resident bridge daemon. One per user (the state directory
//! is shared by every session), tied to the Herdr server whose hook spawned
//! it. Every few seconds it lists the running `herdr --remote` clients and
//! keeps one ssh link per remote machine, running `herdr-cast bridge-relay`
//! on the far end. A control socket in the state directory lets a
//! notification click ask for a remote pane to be focused over the link.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::targets::{Resolver, Wanted};
use super::tunnel::{self, Outcome, Port};
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
/// How long a click waits for the daemon: the relay's focus ack plus slack.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(6);
/// How often the notifier registration is re-checked, well inside its TTL.
const REGISTRATION_INTERVAL: Duration = Duration::from_secs(10 * 60);
const STDERR_TAIL: usize = 5;

type Links = Arc<Mutex<BTreeMap<String, Link>>>;

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
    let control = state.join(super::CONTROL_NAME);
    let links: Links = Arc::default();
    serve_control(&control, Arc::clone(&links))?;

    let mut resolver = Resolver::default();
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
                    if let Ok(mut links) = links.lock() {
                        links.values_mut().for_each(Link::stop);
                    }
                    let _ = std::fs::remove_file(&control);
                    return Ok(());
                }
            }
        }
        match resolver.scan() {
            Ok(wanted) => {
                if let Ok(mut links) = links.lock() {
                    reconcile(&mut links, wanted, &control);
                }
            }
            Err(error) => log(&format!("target scan failed: {error}")),
        }
        std::thread::sleep(TICK);
    }
}

fn reconcile(links: &mut BTreeMap<String, Link>, wanted: BTreeMap<String, Wanted>, control: &Path) {
    links.retain(|key, link| {
        let keep = wanted.contains_key(key);
        if !keep {
            log(&format!(
                "{}: no remote client left, disconnecting",
                link.wanted.target
            ));
            link.stop();
        }
        keep
    });
    let now = Instant::now();
    for (key, found) in wanted {
        let link = links
            .entry(key.clone())
            .or_insert_with(|| Link::new(key, found.clone()));
        link.wanted = found;
        link.poll(now, control);
    }
}

/// One remote machine's link and its retry state.
struct Link {
    key: String,
    wanted: Wanted,
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
    port: Arc<Port>,
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
    fn new(key: String, wanted: Wanted) -> Self {
        Self {
            key,
            wanted,
            running: None,
            retry_at: Instant::now(),
            backoff: Duration::ZERO,
            parked: None,
        }
    }

    fn target(&self) -> &str {
        &self.wanted.target
    }

    fn stop(&mut self) {
        if let Some(running) = &self.running {
            kill(&running.child);
        }
    }

    fn poll(&mut self, now: Instant, control: &Path) {
        if let Some(running) = &self.running {
            if !running.handle.is_finished() {
                let stuck =
                    !running.port.has_connected() && running.started.elapsed() > CONNECT_DEADLINE;
                if stuck {
                    log(&format!(
                        "{}: no hello within {CONNECT_DEADLINE:?}",
                        self.target()
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
        if self.parked.as_ref() == Some(&self.wanted.pids()) {
            return;
        }
        self.parked = None;
        if now < self.retry_at {
            return;
        }
        match spawn(&self.key, self.target(), control) {
            Ok(running) => self.running = Some(running),
            Err(error) => {
                log(&format!("{}: {error}", self.target()));
                self.retry_later(now, false);
            }
        }
    }

    fn schedule(&mut self, report: Report, now: Instant) {
        log(&format!(
            "{}: link ended after {:?}: {:?}, ssh {}",
            self.target(),
            report.uptime,
            report.outcome,
            report
                .status
                .map_or_else(|| "still running".to_string(), |status| status.to_string())
        ));
        if report.outcome == Outcome::Bye("replaced".to_string()) {
            log(&format!(
                "{}: another link owns the remote socket; parked",
                self.target()
            ));
            self.parked = Some(self.wanted.pids());
            return;
        }
        if !report.connected && missing_relay(&report) {
            log(&format!(
                "{}: remote herdr-cast lacks bridge-relay; retrying in {MISSING_RELAY_RETRY:?}",
                self.target()
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

fn spawn(key: &str, target: &str, control: &Path) -> Result<Running, String> {
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
    let port = Arc::new(Port::default());
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
        let port = Arc::clone(&port);
        let target = target.to_string();
        let click = crate::notify::BridgeClick {
            control: control.to_path_buf(),
            key: key.to_string(),
        };
        std::thread::spawn(move || {
            let apply = |frame: &Frame, body: Option<Vec<u8>>| apply(frame, body, &click);
            let outcome = tunnel::run(stdout, stdin, &target, &apply, &port, &log);
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
                connected: port.has_connected(),
            }
        })
    };
    Ok(Running {
        handle,
        child,
        port,
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

fn apply(
    frame: &Frame,
    body: Option<Vec<u8>>,
    click: &crate::notify::BridgeClick,
) -> Result<(), String> {
    match frame {
        Frame::Notify(notification) => crate::notify::deliver_bridged(notification, click),
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

/// Accept notification clicks on `path`. The daemon holds the singleton
/// lock, so a leftover socket from a previous daemon is safe to replace.
fn serve_control(path: &Path, links: Links) -> Result<(), String> {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)
        .map_err(|error| format!("failed to bind {}: {error}", path.display()))?;
    std::thread::spawn(move || {
        for connection in listener.incoming() {
            match connection {
                Ok(connection) => {
                    let links = Arc::clone(&links);
                    std::thread::spawn(move || answer_control(connection, &links));
                }
                Err(error) => log(&format!("control accept failed: {error}")),
            }
        }
    });
    Ok(())
}

fn answer_control(connection: UnixStream, links: &Links) {
    let _ = connection.set_read_timeout(Some(CONTROL_TIMEOUT));
    let _ = connection.set_write_timeout(Some(CONTROL_TIMEOUT));
    let reply = match wire::read_frame(&mut BufReader::new(&connection)) {
        Ok(Some((
            Frame::FocusRequest {
                key,
                session,
                socket,
                pane,
            },
            _,
        ))) => focus(links, &key, session.as_deref(), socket, pane),
        Ok(Some((other, _))) => Frame::FocusReply {
            pids: Vec::new(),
            error: Some(format!("unexpected control request {other:?}")),
        },
        Ok(None) => return,
        Err(error) => Frame::FocusReply {
            pids: Vec::new(),
            error: Some(error.to_string()),
        },
    };
    let _ = wire::write_frame(&mut &connection, &reply, None);
}

/// Focus `pane` on the machine behind link `key` and name the Ghostty tabs
/// that show it, best first.
fn focus(links: &Links, key: &str, session: Option<&str>, socket: String, pane: String) -> Frame {
    let found = links.lock().ok().and_then(|links| {
        let link = links.get(key)?;
        Some((
            link.wanted.tab_pids(session),
            link.running
                .as_ref()
                .map(|running| Arc::clone(&running.port)),
        ))
    });
    let Some((pids, port)) = found else {
        return Frame::FocusReply {
            pids: Vec::new(),
            error: Some(format!("no remote client for {key}")),
        };
    };
    let result = match port {
        Some(port) => port.request(Frame::Focus {
            id: 0,
            socket,
            pane: pane.clone(),
        }),
        None => Err("link not running".to_string()),
    };
    match &result {
        Ok(()) => log(&format!("{key}: focused {pane}")),
        Err(error) => log(&format!("{key}: failed to focus {pane}: {error}")),
    }
    Frame::FocusReply {
        pids,
        error: result.err(),
    }
}

/// Ask the daemon behind `control` to focus a remote pane. Returns the
/// candidate Ghostty tab pids and the remote focus error, if any.
pub fn request_focus(
    control: &Path,
    request: &Frame,
) -> Result<(Vec<u32>, Option<String>), String> {
    let connection = UnixStream::connect(control)
        .map_err(|error| format!("bridge daemon unreachable: {error}"))?;
    let _ = connection.set_read_timeout(Some(CONTROL_TIMEOUT));
    let _ = connection.set_write_timeout(Some(CONTROL_TIMEOUT));
    wire::write_frame(&mut &connection, request, None).map_err(|error| error.to_string())?;
    match wire::read_frame(&mut BufReader::new(&connection)) {
        Ok(Some((Frame::FocusReply { pids, error }, _))) => Ok((pids, error)),
        Ok(other) => Err(format!("unexpected control reply {other:?}")),
        Err(error) => Err(error.to_string()),
    }
}

/// Append a line to the bridge log beside `control`, for processes (such as
/// a notification click) whose stderr goes nowhere.
pub fn append_log(control: &Path, message: &str) {
    let path: PathBuf = control.with_file_name(super::LOG_NAME);
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{} [cast bridge] {message}", super::timestamp());
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
        let mut link = Link::new("donut".into(), Wanted::default());
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

    #[test]
    fn an_unknown_machine_gets_no_tabs_and_an_error() {
        let links: Links = Arc::default();
        let Frame::FocusReply { pids, error } = focus(
            &links,
            "me@nowhere:22",
            None,
            "/tmp/h.sock".into(),
            "p".into(),
        ) else {
            panic!("expected a reply");
        };
        assert!(pids.is_empty());
        assert!(error.is_some_and(|error| error.contains("me@nowhere:22")));
    }
}
