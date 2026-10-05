//! Always-on bridge from remote Herdr machines to the host Mac running the
//! Herdr client. The host dials each attached remote over ssh and runs
//! `herdr-cast bridge-relay` there; the relay owns a Unix socket that local
//! senders (the notify hook, the pasteboard watcher) write to. Without that
//! socket, senders fall back to their pre-bridge paths.
//!
//! ```text
//! host Mac                                remote machine
//! bridge (host daemon) ── ssh stdio ──▶  bridge-relay ◀── bridge.sock ◀── senders
//! ```

use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

mod host;
mod relay;
mod send;
mod targets;
mod tunnel;
pub mod wire;

pub use send::Unavailable;

use wire::Frame;

/// Ordered so an inner timeout never looks like an outer failure: the
/// notifier child (3s, in `notify`) < relay ack wait < sender wait.
const ACK_TIMEOUT: Duration = Duration::from_secs(4);
const SEND_TIMEOUT: Duration = Duration::from_secs(5);
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
const PING_INTERVAL: Duration = Duration::from_secs(15);
const PING_DEADLINE: Duration = Duration::from_secs(45);

const LOCK_NAME: &str = "bridge.lock";
const LOG_NAME: &str = "bridge.log";
/// The host daemon's socket for notification clicks.
const CONTROL_NAME: &str = "bridge-control.sock";

/// Where senders and the relay meet on a remote machine. Fixed so neither
/// side needs configuration; it exists only while a host is connected.
pub fn socket_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".local/state/herdr-cast/bridge.sock"))
}

pub fn send_notification(notification: wire::Notification) -> Result<(), Unavailable> {
    let socket = socket_path().ok_or(Unavailable::NoSocket)?;
    send::send(&socket, &Frame::Notify(notification), None)
}

/// Tell the host to remove what it delivered for `pane`. `host` must match
/// the `Notification.host` the delivery carried so the host resolves the
/// same notification group.
pub fn send_dismiss(host: &str, pane: &str) -> Result<(), Unavailable> {
    let socket = socket_path().ok_or(Unavailable::NoSocket)?;
    send::send(
        &socket,
        &Frame::Dismiss {
            id: 0,
            host: host.to_string(),
            pane: pane.to_string(),
        },
        None,
    )
}

/// Only the macOS pasteboard watcher sends pasteboard copies.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn send_pasteboard(text: &str) -> Result<(), Unavailable> {
    let socket = socket_path().ok_or(Unavailable::NoSocket)?;
    send::send(
        &socket,
        &pasteboard_frame(text.len()),
        Some(text.as_bytes()),
    )
}

fn pasteboard_frame(len: usize) -> Frame {
    Frame::Pasteboard {
        id: 0,
        mime: wire::TEXT_MIME.to_string(),
        len,
    }
}

/// `bridge-start` and the focus hook: make sure the host daemon runs. The
/// host is always a Mac; elsewhere this does nothing.
pub fn ensure() -> Result<(), String> {
    if !cfg!(target_os = "macos") {
        return Ok(());
    }
    let state = crate::daemon::state_dir()?;
    if crate::daemon::is_held(&state.join(LOCK_NAME))? {
        return Ok(());
    }
    crate::daemon::spawn_detached(&["bridge"], Some(&state.join(LOG_NAME)))
}

/// `bridge`: the resident host daemon.
pub fn host() -> Result<(), String> {
    host::run()
}

/// `bridge-relay [--socket PATH]`, started by the host over ssh.
pub fn relay_command(arguments: Vec<String>) -> Result<(), String> {
    const USAGE: &str = "usage: herdr-cast bridge-relay [--socket <path>]";
    let socket = match arguments.as_slice() {
        [] => socket_path().ok_or_else(|| "HOME not set".to_string())?,
        [flag, path] if flag == "--socket" => PathBuf::from(path),
        _ => return Err(USAGE.to_string()),
    };
    relay::run(socket)
}

/// `bridge-send`: hand one request to the relay by hand, for diagnostics.
pub fn send_command(arguments: Vec<String>) -> Result<(), String> {
    let request = parse_send(&arguments)?;
    let socket = match request.socket {
        Some(socket) => socket,
        None => socket_path().ok_or_else(|| "HOME not set".to_string())?,
    };
    let result = match request.frame {
        Some(frame) => send::send(&socket, &frame, None),
        None => {
            let mut text = Vec::new();
            std::io::stdin()
                .take(wire::MAX_BODY as u64 + 1)
                .read_to_end(&mut text)
                .map_err(|error| format!("failed to read stdin: {error}"))?;
            send::send(&socket, &pasteboard_frame(text.len()), Some(&text))
        }
    };
    result.map_err(|error| error.to_string())?;
    println!("ok");
    Ok(())
}

struct SendRequest {
    socket: Option<PathBuf>,
    /// `None` means a pasteboard read from stdin.
    frame: Option<Frame>,
}

fn parse_send(arguments: &[String]) -> Result<SendRequest, String> {
    const USAGE: &str = concat!(
        "usage: herdr-cast bridge-send [--socket <path>] notify --status <status> ",
        "--action <text> [--workspace <label>] [--project <name>] [--pane <id>]\n",
        "       [--herdr-socket <path>] [--session <name>]\n",
        "       herdr-cast bridge-send [--socket <path>] pasteboard < text"
    );
    let mut socket = None;
    let mut kind = None;
    let mut notification = wire::Notification {
        id: 0,
        host: this_host(),
        pane: "bridge-send".to_string(),
        status: String::new(),
        action: String::new(),
        workspace: String::new(),
        project: String::new(),
        socket: String::new(),
        session: None,
    };
    let mut iterator = arguments.iter();
    while let Some(argument) = iterator.next() {
        let slot = match argument.as_str() {
            "notify" | "pasteboard" if kind.is_none() => {
                kind = Some(argument.as_str());
                continue;
            }
            "--socket" => {
                let path = iterator.next().ok_or(USAGE)?;
                socket = Some(PathBuf::from(path));
                continue;
            }
            "--status" => &mut notification.status,
            "--action" => &mut notification.action,
            "--workspace" => &mut notification.workspace,
            "--project" => &mut notification.project,
            "--pane" => &mut notification.pane,
            "--herdr-socket" => &mut notification.socket,
            "--session" => {
                notification.session = Some(iterator.next().ok_or(USAGE)?.clone());
                continue;
            }
            _ => return Err(USAGE.to_string()),
        };
        *slot = iterator.next().ok_or(USAGE)?.clone();
    }
    let frame = match kind {
        Some("pasteboard") => None,
        Some(_) if !notification.status.is_empty() && !notification.action.is_empty() => {
            Some(Frame::Notify(notification))
        }
        _ => return Err(USAGE.to_string()),
    };
    Ok(SendRequest { socket, frame })
}

/// `bridge-focus <control> <key> <session> <socket> <pane>`: the click on a
/// bridged notification. Asks the host daemon to focus the remote pane,
/// then raises the Ghostty tab attached to that machine and session. An
/// empty `session` means the remote's default session. Never fails the
/// click: problems go to the bridge log and Ghostty is raised regardless.
pub fn focus_command(arguments: Vec<String>) -> Result<(), String> {
    const USAGE: &str =
        "usage: herdr-cast bridge-focus <control-socket> <key> <session> <herdr-socket> <pane>";
    let [control, key, session, socket, pane] = arguments.as_slice() else {
        return Err(USAGE.to_string());
    };
    let control = PathBuf::from(control);
    let request = Frame::FocusRequest {
        key: key.clone(),
        session: (!session.is_empty()).then(|| session.clone()),
        socket: socket.clone(),
        pane: pane.clone(),
    };
    let pids = match host::request_focus(&control, &request) {
        Ok((pids, None)) => pids,
        Ok((pids, Some(error))) => {
            host::append_log(&control, &format!("click on {key} {pane}: {error}"));
            pids
        }
        Err(error) => {
            host::append_log(&control, &format!("click on {key} {pane}: {error}"));
            Vec::new()
        }
    };
    crate::notify::raise_ghostty_tab(&pids);
    Ok(())
}

fn hello(host: &str, kinds: &[&str]) -> Frame {
    Frame::Hello {
        v: wire::VERSION,
        host: host.to_string(),
        kinds: kinds.iter().map(|kind| kind.to_string()).collect(),
    }
}

fn this_host() -> String {
    crate::daemon::hostname().unwrap_or_else(|| "unknown".to_string())
}

/// Local wall-clock time for daemon log lines.
fn timestamp() -> String {
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    let mut local: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&now, &mut local) }.is_null() {
        return now.to_string();
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        local.tm_year + 1900,
        local.tm_mon + 1,
        local.tm_mday,
        local.tm_hour,
        local.tm_min,
        local.tm_sec
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;
    use std::time::Instant;

    use tunnel::Outcome;

    type Applied = Arc<Mutex<Vec<(Frame, Option<Vec<u8>>)>>>;

    struct Harness {
        stop: Arc<AtomicBool>,
        relay: JoinHandle<Result<(), String>>,
        host: JoinHandle<Outcome>,
        applied: Applied,
        port: Arc<tunnel::Port>,
    }

    /// A relay and a host session joined by a socket pair in place of ssh.
    /// Pasteboard bodies equal to `reject` are refused by the host.
    fn start(socket: &Path, reject: &'static [u8]) -> Harness {
        let (relay_end, host_end) = UnixStream::pair().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let relay = {
            let stop = Arc::clone(&stop);
            let socket = socket.to_path_buf();
            let output = relay_end.try_clone().unwrap();
            std::thread::spawn(move || relay::serve(relay_end, output, &socket, &stop))
        };
        let applied: Applied = Arc::default();
        let port = Arc::new(tunnel::Port::default());
        let host = {
            let applied = Arc::clone(&applied);
            let port = Arc::clone(&port);
            std::thread::spawn(move || {
                let apply = move |frame: &Frame, body: Option<Vec<u8>>| {
                    if body.as_deref() == Some(reject) {
                        return Err("boom".to_string());
                    }
                    applied.lock().unwrap().push((frame.clone(), body));
                    Ok(())
                };
                let reader = host_end.try_clone().unwrap();
                tunnel::run(reader, host_end, "test", &apply, &port, &|_| {})
            })
        };
        wait_for(|| socket.exists());
        Harness {
            stop,
            relay,
            host,
            applied,
            port,
        }
    }

    fn wait_for(condition: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < deadline, "timed out waiting");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Unix socket paths are capped near 104 bytes, so stay short and out
    /// of deep temporary directories.
    fn test_socket(label: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        PathBuf::from(format!(
            "/tmp/cb-{label}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ))
        .join("b.sock")
    }

    fn notification() -> wire::Notification {
        wire::Notification {
            id: 99,
            host: "donut".into(),
            pane: "w1:p1".into(),
            status: "done".into(),
            action: "pi done".into(),
            workspace: "cast".into(),
            project: "herdr-cast".into(),
            socket: "/tmp/herdr.sock".into(),
            session: None,
        }
    }

    #[test]
    fn requests_reach_the_host_and_the_socket_leaves_with_the_relay() {
        let socket = test_socket("roundtrip");
        let harness = start(&socket, b"reject me");

        send::send(&socket, &Frame::Notify(notification()), None).unwrap();
        send::send(
            &socket,
            &Frame::Dismiss {
                id: 0,
                host: "donut".into(),
                pane: "w1:p1".into(),
            },
            None,
        )
        .unwrap();
        let text: Vec<u8> = "é漢\n".repeat(100_000).into_bytes();
        assert!(text.len() > 192 * 1024);
        send::send(&socket, &pasteboard_frame(text.len()), Some(&text)).unwrap();
        let rejected = send::send(&socket, &pasteboard_frame(9), Some(b"reject me"));
        assert!(
            matches!(&rejected, Err(Unavailable::Rejected(error)) if error == "boom"),
            "{rejected:?}"
        );

        {
            let applied = harness.applied.lock().unwrap();
            assert_eq!(applied.len(), 3);
            let Frame::Notify(received) = &applied[0].0 else {
                panic!("expected a notification");
            };
            assert_eq!(received.pane, "w1:p1");
            assert_eq!(received.host, "donut");
            assert_eq!(
                applied[1].0,
                Frame::Dismiss {
                    id: 2,
                    host: "donut".into(),
                    pane: "w1:p1".into(),
                },
                "the relay rewrites request ids in order"
            );
            assert_eq!(applied[2].1.as_deref(), Some(text.as_slice()));
        }

        harness.stop.store(true, Ordering::SeqCst);
        harness.relay.join().unwrap().unwrap();
        assert!(!socket.exists(), "the relay removes its socket");
        assert_eq!(
            harness.host.join().unwrap(),
            Outcome::Bye("signal".to_string())
        );
        assert!(matches!(
            send::send(&socket, &Frame::Notify(notification()), None),
            Err(Unavailable::NoSocket)
        ));
        let _ = std::fs::remove_dir_all(socket.parent().unwrap());
    }

    #[test]
    fn a_second_relay_takes_over_the_socket() {
        let socket = test_socket("takeover");
        let first = start(&socket, b"");
        let inode = |path: &Path| {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(path).unwrap().ino()
        };
        let before = inode(&socket);
        let second = start(&socket, b"");
        wait_for(|| inode(&socket) != before);

        assert_eq!(
            first.host.join().unwrap(),
            Outcome::Bye("replaced".to_string())
        );
        first.relay.join().unwrap().unwrap();
        assert!(socket.exists(), "the replaced relay leaves the new socket");
        send::send(&socket, &Frame::Notify(notification()), None).unwrap();
        assert_eq!(second.applied.lock().unwrap().len(), 1);

        second.stop.store(true, Ordering::SeqCst);
        second.relay.join().unwrap().unwrap();
        assert!(!socket.exists());
        let _ = std::fs::remove_dir_all(socket.parent().unwrap());
    }

    #[test]
    fn the_relay_leaves_when_the_host_closes_the_link() {
        let socket = test_socket("eof");
        let (relay_end, host_end) = UnixStream::pair().unwrap();
        let relay = {
            let socket = socket.clone();
            let output = relay_end.try_clone().unwrap();
            std::thread::spawn(move || {
                relay::serve(relay_end, output, &socket, &AtomicBool::new(false))
            })
        };
        let mut reader = std::io::BufReader::new(host_end.try_clone().unwrap());
        let (greeting, _) = wire::read_frame(&mut reader).unwrap().unwrap();
        assert!(matches!(greeting, Frame::Hello { .. }));
        wire::write_frame(&mut &host_end, &hello("host", &[wire::KIND_NOTIFY]), None).unwrap();
        wait_for(|| socket.exists());
        host_end.shutdown(std::net::Shutdown::Write).unwrap();
        relay.join().unwrap().unwrap();
        assert!(!socket.exists());
        let _ = std::fs::remove_dir_all(socket.parent().unwrap());
    }

    /// A one-shot stand-in for a Herdr server socket that records the
    /// request and answers with an empty result.
    fn fake_herdr(socket: &Path) -> JoinHandle<serde_json::Value> {
        let listener = std::os::unix::net::UnixListener::bind(socket).unwrap();
        std::thread::spawn(move || {
            use std::io::{BufRead, Write};
            let (mut connection, _) = listener.accept().unwrap();
            let mut line = String::new();
            std::io::BufReader::new(&connection)
                .read_line(&mut line)
                .unwrap();
            connection
                .write_all(b"{\"id\":\"cast:bridge-focus\",\"result\":{}}\n")
                .unwrap();
            serde_json::from_str(&line).unwrap()
        })
    }

    #[test]
    fn the_host_focuses_remote_panes_through_the_link() {
        let socket = test_socket("focus");
        let harness = start(&socket, b"");
        let herdr = socket.with_file_name("h.sock");
        let server = fake_herdr(&herdr);

        let focus = |herdr: &Path| {
            harness.port.request(Frame::Focus {
                id: 0,
                socket: herdr.to_string_lossy().into_owned(),
                pane: "w2:p3".into(),
            })
        };
        focus(&herdr).unwrap();
        let request = server.join().unwrap();
        assert_eq!(request["method"], "agent.focus");
        assert_eq!(request["params"]["target"], "w2:p3");

        let missing = focus(&socket.with_file_name("gone.sock"));
        assert!(
            missing
                .as_ref()
                .is_err_and(|error| error.contains("failed to connect")),
            "{missing:?}"
        );

        harness.stop.store(true, Ordering::SeqCst);
        harness.relay.join().unwrap().unwrap();
        harness.host.join().unwrap();
        assert!(
            focus(&herdr).is_err_and(|error| error.contains("not connected")),
            "a closed link refuses requests"
        );
        let _ = std::fs::remove_dir_all(socket.parent().unwrap());
    }

    #[test]
    fn parses_bridge_send_arguments() {
        let arguments = |line: &str| line.split(' ').map(str::to_string).collect::<Vec<_>>();
        let request = parse_send(&arguments(
            "--socket /tmp/b.sock notify --status blocked --action pi --pane p9",
        ))
        .unwrap();
        assert_eq!(request.socket.as_deref(), Some(Path::new("/tmp/b.sock")));
        let Some(Frame::Notify(notification)) = request.frame else {
            panic!("expected a notification");
        };
        assert_eq!(notification.status, "blocked");
        assert_eq!(notification.pane, "p9");
        let request = parse_send(&arguments(
            "notify --status done --action a --herdr-socket /tmp/h.sock --session work",
        ))
        .unwrap();
        let Some(Frame::Notify(notification)) = request.frame else {
            panic!("expected a notification");
        };
        assert_eq!(notification.socket, "/tmp/h.sock");
        assert_eq!(notification.session.as_deref(), Some("work"));
        assert!(parse_send(&arguments("pasteboard"))
            .unwrap()
            .frame
            .is_none());
        assert!(parse_send(&arguments("notify --status done")).is_err());
        assert!(parse_send(&arguments("open --status done")).is_err());
        assert!(parse_send(&arguments("pasteboard --socket")).is_err());
    }
}
