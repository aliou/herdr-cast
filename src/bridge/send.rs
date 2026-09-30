//! Sender side: hand one request to the local relay socket and wait for the
//! host's ack. Every failure maps to `Unavailable` so callers fall back to
//! the pre-bridge delivery path.

use std::fmt;
use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::Path;

use super::wire::{self, Frame};
use super::SEND_TIMEOUT;

#[derive(Debug)]
pub enum Unavailable {
    /// No relay socket: no host is connected to this machine.
    NoSocket,
    TooLarge(usize),
    /// The socket exists but nothing accepts on it (a stale socket).
    Refused(String),
    /// The relay or host answered with an error.
    Rejected(String),
    Failed(String),
}

impl fmt::Display for Unavailable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unavailable::NoSocket => write!(formatter, "no bridge socket"),
            Unavailable::TooLarge(len) => write!(
                formatter,
                "body of {len} bytes exceeds the bridge limit of {}",
                wire::MAX_BODY
            ),
            Unavailable::Refused(error) => write!(formatter, "bridge socket refused: {error}"),
            Unavailable::Rejected(error) => write!(formatter, "host rejected: {error}"),
            Unavailable::Failed(error) => write!(formatter, "bridge failed: {error}"),
        }
    }
}

pub fn send(socket: &Path, frame: &Frame, body: Option<&[u8]>) -> Result<(), Unavailable> {
    let len = body.map_or(0, <[u8]>::len);
    if len > wire::MAX_BODY {
        return Err(Unavailable::TooLarge(len));
    }
    if !socket.exists() {
        return Err(Unavailable::NoSocket);
    }
    let stream =
        UnixStream::connect(socket).map_err(|error| Unavailable::Refused(error.to_string()))?;
    let failed = |error: std::io::Error| Unavailable::Failed(error.to_string());
    stream
        .set_read_timeout(Some(SEND_TIMEOUT))
        .map_err(failed)?;
    stream
        .set_write_timeout(Some(SEND_TIMEOUT))
        .map_err(failed)?;
    wire::write_frame(&mut &stream, frame, body).map_err(failed)?;
    let ack = wire::read_frame(&mut BufReader::new(&stream))
        .map_err(|error| Unavailable::Failed(error.to_string()))?;
    match ack {
        Some((Frame::Ack { ok: true, .. }, _)) => Ok(()),
        Some((Frame::Ack { error, .. }, _)) => Err(Unavailable::Rejected(
            error.unwrap_or_else(|| "unknown error".to_string()),
        )),
        Some((other, _)) => Err(Unavailable::Failed(format!("unexpected reply {other:?}"))),
        None => Err(Unavailable::Failed(
            "relay closed without an ack".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn frame() -> Frame {
        Frame::Pasteboard {
            id: 1,
            mime: wire::TEXT_MIME.into(),
            len: 0,
        }
    }

    #[test]
    fn a_missing_socket_is_no_bridge() {
        let path = std::env::temp_dir().join("cast-bridge-send-missing.sock");
        assert!(matches!(
            send(&path, &frame(), None),
            Err(Unavailable::NoSocket)
        ));
    }

    #[test]
    fn oversized_bodies_are_refused_before_connecting() {
        let path = std::env::temp_dir().join("cast-bridge-send-missing.sock");
        let body = vec![b'a'; wire::MAX_BODY + 1];
        assert!(matches!(
            send(&path, &frame(), Some(&body)),
            Err(Unavailable::TooLarge(_))
        ));
    }

    #[test]
    fn a_stale_socket_is_refused() {
        let path = PathBuf::from(format!("/tmp/cb-stale-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        assert!(path.exists(), "the listener leaves its socket file behind");
        assert!(matches!(
            send(&path, &frame(), None),
            Err(Unavailable::Refused(_))
        ));
        let _ = std::fs::remove_file(&path);
    }
}
