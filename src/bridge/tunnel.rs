//! Host side of one bridge link: greet the relay, keep it alive with pings,
//! apply each forwarded request before acking it, and carry the host's own
//! requests (focus) to the relay. Transport-agnostic so tests can run it
//! over a socket pair instead of ssh.

use std::collections::HashMap;
use std::io::{BufReader, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::Mutex;

use super::wire::{self, Frame, WireError};
use super::{ACK_TIMEOUT, PING_INTERVAL};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The relay said goodbye with this reason.
    Bye(String),
    /// The link closed without a goodbye.
    Closed,
    Failed(String),
}

pub type Apply<'a> = dyn Fn(&Frame, Option<Vec<u8>>) -> Result<(), String> + 'a;

type Writer = Box<dyn Write + Send>;

/// The host's handle for sending its own requests down a link. Usable only
/// while `run` has the link connected.
#[derive(Default)]
pub struct Port {
    writer: Mutex<Option<Writer>>,
    relay_kinds: Mutex<Vec<String>>,
    pending: Mutex<HashMap<u64, Sender<Result<(), String>>>>,
    next_id: AtomicU64,
    connected: AtomicBool,
}

impl Port {
    /// Whether the link ever completed its hellos.
    pub fn has_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    /// Send a host request and wait for the relay's ack.
    pub fn request(&self, frame: Frame) -> Result<(), String> {
        let kind = frame
            .request_kind()
            .ok_or_else(|| "not a request".to_string())?;
        let accepted = self
            .relay_kinds
            .lock()
            .map(|kinds| kinds.iter().any(|accepted| accepted == kind))
            .unwrap_or(false);
        if !accepted {
            return Err(format!("the remote relay does not accept {kind}"));
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let (sender, receiver) = mpsc::channel();
        if let Ok(mut pending) = self.pending.lock() {
            pending.insert(id, sender);
        }
        let sent = self.send(&frame.with_request_id(id));
        let result = match sent {
            Ok(()) => receiver
                .recv_timeout(ACK_TIMEOUT)
                .unwrap_or_else(|_| Err("relay timeout".to_string())),
            Err(error) => Err(error),
        };
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&id);
        }
        result
    }

    fn send(&self, frame: &Frame) -> Result<(), String> {
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| "link writer lock poisoned".to_string())?;
        let writer = writer
            .as_mut()
            .ok_or_else(|| "link not connected".to_string())?;
        wire::write_frame(writer, frame, None).map_err(|error| error.to_string())
    }

    fn settle(&self, id: u64, result: Result<(), String>) {
        let sender = self
            .pending
            .lock()
            .ok()
            .and_then(|mut pending| pending.remove(&id));
        if let Some(sender) = sender {
            let _ = sender.send(result);
        }
    }

    fn open(&self, writer: Writer, relay_kinds: Vec<String>) {
        if let Ok(mut slot) = self.writer.lock() {
            *slot = Some(writer);
        }
        if let Ok(mut kinds) = self.relay_kinds.lock() {
            *kinds = relay_kinds;
        }
        self.connected.store(true, Ordering::SeqCst);
    }

    /// Drop the writer and fail every waiting request.
    fn close(&self) {
        if let Ok(mut slot) = self.writer.lock() {
            *slot = None;
        }
        let waiting: Vec<_> = self
            .pending
            .lock()
            .map(|mut pending| pending.drain().collect())
            .unwrap_or_default();
        for (_, sender) in waiting {
            let _ = sender.send(Err("link closed".to_string()));
        }
    }
}

/// Run one link until it ends. `port.has_connected()` turns true once both
/// hellos passed and stays true afterwards.
pub fn run<R: Read, W: Write + Send + 'static>(
    reader: R,
    writer: W,
    label: &str,
    apply: &Apply<'_>,
    port: &Port,
    log: &dyn Fn(&str),
) -> Outcome {
    let outcome = serve(reader, writer, label, apply, port, log);
    port.close();
    outcome
}

fn serve<R: Read, W: Write + Send + 'static>(
    reader: R,
    mut writer: W,
    label: &str,
    apply: &Apply<'_>,
    port: &Port,
    log: &dyn Fn(&str),
) -> Outcome {
    let mut reader = BufReader::new(reader);
    let (relay_host, relay_kinds) = match wire::read_frame(&mut reader) {
        Ok(Some((Frame::Hello { v, host, kinds }, _))) if v == wire::VERSION => (host, kinds),
        Ok(Some((Frame::Hello { v, .. }, _))) => {
            return Outcome::Failed(format!("relay speaks bridge v{v}"))
        }
        Ok(Some((Frame::Bye { reason }, _))) => return Outcome::Bye(reason),
        Ok(Some((other, _))) => return Outcome::Failed(format!("expected hello, got {other:?}")),
        Ok(None) => return Outcome::Closed,
        Err(error) => return Outcome::Failed(error.to_string()),
    };
    let hello = super::hello(
        &super::this_host(),
        &[wire::KIND_NOTIFY, wire::KIND_PASTEBOARD],
    );
    if let Err(error) = wire::write_frame(&mut writer, &hello, None) {
        return Outcome::Failed(format!("failed to greet the relay: {error}"));
    }
    port.open(Box::new(writer), relay_kinds);
    log(&format!("{label}: connected to {relay_host}"));
    let (_stop_pings, stop) = mpsc::channel::<()>();
    std::thread::scope(|scope| {
        // Dropping `_stop_pings` when this function returns ends the pinger.
        scope.spawn(move || {
            let mut seq = 0;
            while let Err(RecvTimeoutError::Timeout) = stop.recv_timeout(PING_INTERVAL) {
                seq += 1;
                if port.send(&Frame::Ping { seq }).is_err() {
                    return;
                }
            }
        });
        let outcome = read_loop(&mut reader, label, apply, port, log);
        port.close();
        drop(_stop_pings);
        outcome
    })
}

fn read_loop(
    reader: &mut impl std::io::BufRead,
    label: &str,
    apply: &Apply<'_>,
    port: &Port,
    log: &dyn Fn(&str),
) -> Outcome {
    loop {
        let (frame, body) = match wire::read_frame(reader) {
            Ok(Some(received)) => received,
            Ok(None) => return Outcome::Closed,
            Err(WireError::Unsupported { kind, id }) => {
                if let Some(id) = id {
                    let _ = port.send(&Frame::ack(id, Err(format!("unsupported {kind:?}"))));
                }
                continue;
            }
            Err(error) => return Outcome::Failed(error.to_string()),
        };
        match frame {
            Frame::Bye { reason } => return Outcome::Bye(reason),
            Frame::Ack { id, ok, error } => {
                port.settle(
                    id,
                    ok.then_some(()).ok_or_else(|| error.unwrap_or_default()),
                );
                continue;
            }
            _ => {}
        }
        let Some(id) = frame.request_id() else {
            continue;
        };
        let result = apply(&frame, body);
        match &result {
            Ok(()) => log(&format!("{label}: applied {}", describe(&frame))),
            Err(error) => log(&format!(
                "{label}: failed to apply {}: {error}",
                describe(&frame)
            )),
        }
        if let Err(error) = port.send(&Frame::ack(id, result)) {
            return Outcome::Failed(format!("failed to ack: {error}"));
        }
    }
}

fn describe(frame: &Frame) -> String {
    match frame {
        Frame::Notify(notification) => format!(
            "{} notification from {}",
            notification.status, notification.host
        ),
        Frame::Pasteboard { len, .. } => format!("{len} byte pasteboard"),
        other => format!("{other:?}"),
    }
}
