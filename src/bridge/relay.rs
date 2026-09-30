//! Remote side of the bridge. The host Mac starts `herdr-cast bridge-relay`
//! over ssh; the relay owns this machine's bridge socket for as long as
//! that ssh link lives and forwards each sender's request over its stdio.
//!
//! The socket exists only while a host is connected, so a sender that finds
//! no socket falls back immediately. Requests are handled one at a time.

use std::fs;
use std::io::{self, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::time::{Duration, Instant};

use super::wire::{self, Frame, WireError};
use super::{ACK_TIMEOUT, HELLO_TIMEOUT, PING_DEADLINE, SEND_TIMEOUT};
use crate::api::SocketClient;

/// How long the accept loop waits before re-checking its exit conditions.
const TICK_MS: i32 = 200;
/// A focus request's Herdr call; below the host's wait for the ack.
const FOCUS_TIMEOUT: Duration = Duration::from_secs(2);

static SIGNALED: AtomicBool = AtomicBool::new(false);
static BIND_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn run(socket: PathBuf) -> Result<(), String> {
    for signal in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT] {
        unsafe {
            libc::signal(
                signal,
                on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t,
            );
        }
    }
    serve(io::stdin(), io::stdout(), &socket, &AtomicBool::new(false))
}

extern "C" fn on_signal(_: libc::c_int) {
    SIGNALED.store(true, Ordering::SeqCst);
}

enum Event {
    Frame(Frame),
    Closed(String),
}

/// Run one relay over `input`/`output` (the ssh link) until the host goes
/// away, the socket is taken over, or `stop` is set.
pub fn serve<R: Read + Send + 'static, W: Write>(
    input: R,
    mut output: W,
    socket: &Path,
    stop: &AtomicBool,
) -> Result<(), String> {
    let events = spawn_reader(input);
    let host = super::this_host();
    wire::write_frame(&mut output, &super::hello(&host, &[wire::KIND_FOCUS]), None)
        .map_err(|error| format!("failed to greet the host: {error}"))?;
    let host_kinds = match events.recv_timeout(HELLO_TIMEOUT) {
        Ok(Event::Frame(Frame::Hello { v, kinds, .. })) if v == wire::VERSION => kinds,
        Ok(Event::Frame(Frame::Hello { v, .. })) => {
            let reason = format!("host speaks bridge v{v}, relay v{}", wire::VERSION);
            bye(&mut output, "version");
            return Err(reason);
        }
        Ok(Event::Frame(other)) => return Err(format!("expected hello, got {other:?}")),
        Ok(Event::Closed(reason)) => return Err(reason),
        Err(_) => return Err("no hello from the host".to_string()),
    };
    let (listener, inode) = bind(socket)?;
    log(&format!("listening on {}", socket.display()));
    let mut relay = Relay {
        output,
        events,
        host_kinds,
        last_ping: Instant::now(),
        seq: 0,
        closed: None,
    };
    let reason = relay.accept_loop(&listener, socket, inode, stop);
    // Remove the socket before any other I/O: stdio may already be gone, and
    // senders must stop finding this socket as soon as the relay stops.
    if fs::metadata(socket).is_ok_and(|metadata| metadata.ino() == inode) {
        let _ = fs::remove_file(socket);
    }
    bye(&mut relay.output, &reason);
    log(&format!("exiting: {reason}"));
    Ok(())
}

struct Relay<W> {
    output: W,
    events: Receiver<Event>,
    host_kinds: Vec<String>,
    last_ping: Instant,
    seq: u64,
    closed: Option<String>,
}

impl<W: Write> Relay<W> {
    fn accept_loop(
        &mut self,
        listener: &UnixListener,
        socket: &Path,
        inode: u64,
        stop: &AtomicBool,
    ) -> String {
        loop {
            if let Some(reason) = self.exit_reason(socket, inode, stop) {
                return reason;
            }
            if !readable(listener, TICK_MS) {
                continue;
            }
            match listener.accept() {
                Ok((connection, _)) => self.handle(connection),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => {
                    log(&format!("accept failed: {error}"));
                    std::thread::sleep(Duration::from_millis(TICK_MS as u64));
                }
            }
        }
    }

    fn exit_reason(&mut self, socket: &Path, inode: u64, stop: &AtomicBool) -> Option<String> {
        self.drain_events();
        if let Some(reason) = &self.closed {
            return Some(reason.clone());
        }
        if stop.load(Ordering::SeqCst) || SIGNALED.load(Ordering::SeqCst) {
            return Some("signal".to_string());
        }
        if self.last_ping.elapsed() > PING_DEADLINE {
            return Some("ping timeout".to_string());
        }
        match fs::metadata(socket) {
            Ok(metadata) if metadata.ino() == inode => None,
            Ok(_) => Some("replaced".to_string()),
            Err(_) => Some("socket removed".to_string()),
        }
    }

    fn drain_events(&mut self) {
        loop {
            match self.events.try_recv() {
                Ok(event) => self.note(event),
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.closed
                        .get_or_insert_with(|| "host link reader stopped".to_string());
                    return;
                }
            }
        }
    }

    /// Handle a host event: pings refresh the deadline, focus requests run
    /// and are acked, a closed link ends the relay, stray acks are dropped.
    fn note(&mut self, event: Event) {
        match event {
            Event::Frame(Frame::Ping { .. }) => self.last_ping = Instant::now(),
            Event::Frame(Frame::Focus { id, socket, pane }) => self.focus(id, &socket, &pane),
            Event::Frame(_) => {}
            Event::Closed(reason) => {
                self.closed.get_or_insert(reason);
            }
        }
    }

    fn focus(&mut self, id: u64, socket: &str, pane: &str) {
        let result = SocketClient::with_timeout(socket, FOCUS_TIMEOUT)
            .send(
                "cast:bridge-focus",
                "agent.focus",
                serde_json::json!({ "target": pane }),
            )
            .map(|_| ());
        match &result {
            Ok(()) => log(&format!("focused {pane}")),
            Err(error) => log(&format!("failed to focus {pane}: {error}")),
        }
        if let Err(error) = wire::write_frame(&mut self.output, &Frame::ack(id, result), None) {
            self.closed
                .get_or_insert_with(|| format!("host link write failed: {error}"));
        }
    }

    fn handle(&mut self, connection: UnixStream) {
        let _ = connection.set_nonblocking(false);
        let _ = connection.set_read_timeout(Some(SEND_TIMEOUT));
        let _ = connection.set_write_timeout(Some(SEND_TIMEOUT));
        let (id, result) = match wire::read_frame(&mut BufReader::new(&connection)) {
            Ok(None) => return,
            Ok(Some((frame, body))) => {
                let id = frame.request_id().unwrap_or(0);
                (id, self.forward(frame, body))
            }
            Err(WireError::Unsupported { kind, id }) => {
                (id.unwrap_or(0), Err(format!("unsupported kind {kind:?}")))
            }
            Err(error) => (0, Err(error.to_string())),
        };
        let _ = wire::write_frame(&mut &connection, &Frame::ack(id, result), None);
    }

    fn forward(&mut self, frame: Frame, body: Option<Vec<u8>>) -> Result<(), String> {
        let kind = frame
            .request_kind()
            .ok_or_else(|| "not a request".to_string())?;
        if !self.host_kinds.iter().any(|accepted| accepted == kind) {
            return Err(format!("host does not accept {kind}"));
        }
        if let Some(reason) = &self.closed {
            return Err(reason.clone());
        }
        self.seq += 1;
        let seq = self.seq;
        if let Err(error) = wire::write_frame(
            &mut self.output,
            &frame.with_request_id(seq),
            body.as_deref(),
        ) {
            let reason = format!("host link write failed: {error}");
            self.closed = Some(reason.clone());
            return Err(reason);
        }
        self.await_ack(seq)
    }

    fn await_ack(&mut self, seq: u64) -> Result<(), String> {
        let deadline = Instant::now() + ACK_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.events.recv_timeout(remaining) {
                Ok(Event::Frame(Frame::Ack { id, ok, error })) if id == seq => {
                    return if ok {
                        Ok(())
                    } else {
                        Err(error.unwrap_or_else(|| "host error".to_string()))
                    };
                }
                Ok(Event::Closed(reason)) => {
                    self.closed = Some(reason.clone());
                    return Err(reason);
                }
                Ok(event) => self.note(event),
                Err(RecvTimeoutError::Timeout) => return Err("host timeout".to_string()),
                Err(RecvTimeoutError::Disconnected) => {
                    let reason = "host link reader stopped".to_string();
                    self.closed = Some(reason.clone());
                    return Err(reason);
                }
            }
        }
    }
}

/// Read host frames on a thread so the accept loop can poll for them.
fn spawn_reader<R: Read + Send + 'static>(input: R) -> Receiver<Event> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(input);
        loop {
            let event = match wire::read_frame(&mut reader) {
                Ok(Some((frame, _))) => Event::Frame(frame),
                Ok(None) => Event::Closed("host closed the link".to_string()),
                Err(WireError::Unsupported { .. }) => continue,
                Err(error) => Event::Closed(format!("bad frame from host: {error}")),
            };
            let last = matches!(event, Event::Closed(_));
            if sender.send(event).is_err() || last {
                return;
            }
        }
    });
    receiver
}

/// Bind a fresh socket beside `socket`, then rename it into place. The
/// rename atomically replaces a stale socket or another relay's socket; the
/// returned inode tells this relay whether it still owns the path.
fn bind(socket: &Path) -> Result<(UnixListener, u64), String> {
    let parent = socket
        .parent()
        .ok_or_else(|| format!("socket path has no parent: {}", socket.display()))?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)
        .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
    let name = socket
        .file_name()
        .ok_or_else(|| format!("socket path has no file name: {}", socket.display()))?
        .to_string_lossy();
    let temporary = parent.join(format!(
        "{name}.{}.{}",
        std::process::id(),
        BIND_COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = fs::remove_file(&temporary);
    let listener = UnixListener::bind(&temporary)
        .map_err(|error| format!("failed to bind {}: {error}", temporary.display()))?;
    let placed = fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))
        .and_then(|()| fs::rename(&temporary, socket))
        .and_then(|()| fs::metadata(socket))
        .and_then(|metadata| listener.set_nonblocking(true).map(|()| metadata.ino()));
    match placed {
        Ok(inode) => Ok((listener, inode)),
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            Err(format!("failed to place {}: {error}", socket.display()))
        }
    }
}

fn readable(listener: &UnixListener, timeout_ms: i32) -> bool {
    let mut descriptor = libc::pollfd {
        fd: listener.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    unsafe { libc::poll(&mut descriptor, 1, timeout_ms) > 0 }
}

/// Best-effort goodbye; the link may already be closed.
fn bye(output: &mut impl Write, reason: &str) {
    let _ = wire::write_frame(
        output,
        &Frame::Bye {
            reason: reason.to_string(),
        },
        None,
    );
}

/// Never panics: stderr is an ssh pipe that may already be closed.
fn log(message: &str) {
    let _ = writeln!(io::stderr(), "[cast bridge-relay] {message}");
}
