//! Host side of one bridge link: greet the relay, keep it alive with pings,
//! and apply each forwarded request before acking it. Transport-agnostic so
//! tests can run it over a socket pair instead of ssh.

use std::io::{BufReader, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};

use super::wire::{self, Frame, WireError};
use super::PING_INTERVAL;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The relay said goodbye with this reason.
    Bye(String),
    /// The link closed without a goodbye.
    Closed,
    Failed(String),
}

pub type Apply = dyn Fn(&Frame, Option<Vec<u8>>) -> Result<(), String>;

/// Run one link until it ends. `connected` flips once both hellos passed.
pub fn run<R: Read, W: Write + Send + 'static>(
    reader: R,
    writer: W,
    label: &str,
    apply: &Apply,
    connected: &AtomicBool,
    log: &dyn Fn(&str),
) -> Outcome {
    let mut reader = BufReader::new(reader);
    let relay_host = match wire::read_frame(&mut reader) {
        Ok(Some((Frame::Hello { v, host, .. }, _))) if v == wire::VERSION => host,
        Ok(Some((Frame::Hello { v, .. }, _))) => {
            return Outcome::Failed(format!("relay speaks bridge v{v}"))
        }
        Ok(Some((Frame::Bye { reason }, _))) => return Outcome::Bye(reason),
        Ok(Some((other, _))) => return Outcome::Failed(format!("expected hello, got {other:?}")),
        Ok(None) => return Outcome::Closed,
        Err(error) => return Outcome::Failed(error.to_string()),
    };
    let writer = Arc::new(Mutex::new(writer));
    if let Err(error) = send(&writer, &super::hello(&super::this_host())) {
        return Outcome::Failed(format!("failed to greet the relay: {error}"));
    }
    connected.store(true, Ordering::SeqCst);
    log(&format!("{label}: connected to {relay_host}"));
    // Dropping `_stop_pings` when this function returns ends the pinger.
    let (_stop_pings, stop) = mpsc::channel::<()>();
    let pinger = Arc::clone(&writer);
    std::thread::spawn(move || {
        let mut seq = 0;
        while let Err(RecvTimeoutError::Timeout) = stop.recv_timeout(PING_INTERVAL) {
            seq += 1;
            if send(&pinger, &Frame::Ping { seq }).is_err() {
                return;
            }
        }
    });
    loop {
        let (frame, body) = match wire::read_frame(&mut reader) {
            Ok(Some(received)) => received,
            Ok(None) => return Outcome::Closed,
            Err(WireError::Unsupported { kind, id }) => {
                if let Some(id) = id {
                    let _ = send(
                        &writer,
                        &Frame::ack(id, Err(format!("unsupported {kind:?}"))),
                    );
                }
                continue;
            }
            Err(error) => return Outcome::Failed(error.to_string()),
        };
        if let Frame::Bye { reason } = frame {
            return Outcome::Bye(reason);
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
        if let Err(error) = send(&writer, &Frame::ack(id, result)) {
            return Outcome::Failed(format!("failed to ack: {error}"));
        }
    }
}

fn send<W: Write>(writer: &Mutex<W>, frame: &Frame) -> std::io::Result<()> {
    let mut writer = writer
        .lock()
        .map_err(|_| std::io::Error::other("writer lock poisoned"))?;
    wire::write_frame(&mut *writer, frame, None)
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
