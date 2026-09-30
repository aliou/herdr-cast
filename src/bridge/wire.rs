//! Bridge frames: one JSON header line, then `len` raw body bytes when the
//! header carries a `len`. The same framing runs on the remote Unix socket
//! and on the ssh stdio link between relay and host.

use std::fmt;
use std::io::{self, BufRead, Read, Write};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const VERSION: u32 = 1;
pub const MAX_HEADER: usize = 16 * 1024;
pub const MAX_BODY: usize = 1024 * 1024;

pub const KIND_NOTIFY: &str = "notify";
pub const KIND_PASTEBOARD: &str = "pasteboard";
pub const TEXT_MIME: &str = "text/plain;charset=utf-8";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Frame {
    /// First frame in both directions on the ssh link.
    Hello {
        v: u32,
        host: String,
        kinds: Vec<String>,
    },
    /// Host to relay keepalive.
    Ping {
        seq: u64,
    },
    /// Relay to host, just before the relay exits.
    Bye {
        reason: String,
    },
    Notify(Notification),
    /// Followed by `len` bytes of `mime` content.
    Pasteboard {
        id: u64,
        mime: String,
        len: usize,
    },
    Ack {
        id: u64,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notification {
    #[serde(default)]
    pub id: u64,
    pub host: String,
    pub pane: String,
    pub status: String,
    pub action: String,
    #[serde(default)]
    pub workspace: String,
    #[serde(default)]
    pub project: String,
}

impl Frame {
    /// The request id of a request frame.
    pub fn request_id(&self) -> Option<u64> {
        match self {
            Frame::Notify(notification) => Some(notification.id),
            Frame::Pasteboard { id, .. } => Some(*id),
            _ => None,
        }
    }

    /// The same request under another id; other frames are unchanged.
    pub fn with_request_id(mut self, new_id: u64) -> Self {
        match &mut self {
            Frame::Notify(notification) => notification.id = new_id,
            Frame::Pasteboard { id, .. } => *id = new_id,
            _ => {}
        }
        self
    }

    /// The capability name a request needs from the host.
    pub fn request_kind(&self) -> Option<&'static str> {
        match self {
            Frame::Notify(_) => Some(KIND_NOTIFY),
            Frame::Pasteboard { .. } => Some(KIND_PASTEBOARD),
            _ => None,
        }
    }

    pub fn ack(id: u64, result: Result<(), String>) -> Self {
        Frame::Ack {
            id,
            ok: result.is_ok(),
            error: result.err(),
        }
    }
}

#[derive(Debug)]
pub enum WireError {
    Io(io::Error),
    HeaderTooLong,
    Truncated,
    Malformed(String),
    BodyTooLarge(u64),
    /// A well-formed frame of a kind this build does not know. The stream
    /// stays in sync: its body, if any, was consumed.
    Unsupported {
        kind: String,
        id: Option<u64>,
    },
}

impl fmt::Display for WireError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WireError::Io(error) => write!(formatter, "{error}"),
            WireError::HeaderTooLong => write!(formatter, "header exceeds {MAX_HEADER} bytes"),
            WireError::Truncated => write!(formatter, "truncated frame"),
            WireError::Malformed(error) => write!(formatter, "malformed frame: {error}"),
            WireError::BodyTooLarge(len) => {
                write!(formatter, "body of {len} bytes exceeds {MAX_BODY}")
            }
            WireError::Unsupported { kind, .. } => write!(formatter, "unsupported kind {kind:?}"),
        }
    }
}

impl From<io::Error> for WireError {
    fn from(error: io::Error) -> Self {
        WireError::Io(error)
    }
}

pub type Received = (Frame, Option<Vec<u8>>);

/// Read one frame. `Ok(None)` is a clean end of stream. Blank lines are
/// skipped. Sizes are checked before anything is allocated for them.
pub fn read_frame(reader: &mut impl BufRead) -> Result<Option<Received>, WireError> {
    let header = loop {
        let mut line = Vec::new();
        let read = reader
            .by_ref()
            .take(MAX_HEADER as u64 + 1)
            .read_until(b'\n', &mut line)?;
        if read == 0 {
            return Ok(None);
        }
        if line.last() != Some(&b'\n') {
            return Err(if line.len() > MAX_HEADER {
                WireError::HeaderTooLong
            } else {
                WireError::Truncated
            });
        }
        let trimmed = line.trim_ascii();
        if !trimmed.is_empty() {
            break serde_json::from_slice::<Value>(trimmed)
                .map_err(|error| WireError::Malformed(error.to_string()))?;
        }
    };
    let body = match header.get("len") {
        None => None,
        Some(len) => {
            let len = len
                .as_u64()
                .ok_or_else(|| WireError::Malformed("len is not a count".to_string()))?;
            if len > MAX_BODY as u64 {
                return Err(WireError::BodyTooLarge(len));
            }
            let mut body = vec![0; len as usize];
            reader
                .read_exact(&mut body)
                .map_err(|error| match error.kind() {
                    io::ErrorKind::UnexpectedEof => WireError::Truncated,
                    _ => WireError::Io(error),
                })?;
            Some(body)
        }
    };
    let kind = header
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let id = header.get("id").and_then(Value::as_u64);
    let frame = serde_json::from_value::<Frame>(header).map_err(|error| {
        if known_kind(&kind) {
            WireError::Malformed(error.to_string())
        } else {
            WireError::Unsupported { kind, id }
        }
    })?;
    if let Frame::Pasteboard { len, .. } = &frame {
        if body.as_ref().map(Vec::len) != Some(*len) {
            return Err(WireError::Malformed("pasteboard body length".to_string()));
        }
    }
    Ok(Some((frame, body)))
}

/// Write one frame in a single buffer, then flush.
pub fn write_frame(writer: &mut impl Write, frame: &Frame, body: Option<&[u8]>) -> io::Result<()> {
    let mut buffer = serde_json::to_vec(frame).map_err(io::Error::other)?;
    buffer.push(b'\n');
    if let Some(body) = body {
        buffer.extend_from_slice(body);
    }
    writer.write_all(&buffer)?;
    writer.flush()
}

fn known_kind(kind: &str) -> bool {
    matches!(
        kind,
        "hello" | "ping" | "bye" | "notify" | "pasteboard" | "ack"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn notification() -> Frame {
        Frame::Notify(Notification {
            id: 7,
            host: "factorial-machine.local".into(),
            pane: "w1:p2".into(),
            status: "done".into(),
            action: "pi done".into(),
            workspace: "cast".into(),
            project: "herdr-cast".into(),
        })
    }

    fn encode(frames: &[(Frame, Option<&[u8]>)]) -> Vec<u8> {
        let mut buffer = Vec::new();
        for (frame, body) in frames {
            write_frame(&mut buffer, frame, *body).unwrap();
        }
        buffer
    }

    #[test]
    fn frames_round_trip_with_and_without_bodies() {
        let text = "é漢\nline2".as_bytes();
        let frames = vec![
            (
                Frame::Hello {
                    v: VERSION,
                    host: "cleo".into(),
                    kinds: vec![KIND_NOTIFY.into(), KIND_PASTEBOARD.into()],
                },
                None,
            ),
            (Frame::Ping { seq: 3 }, None),
            (notification(), None),
            (
                Frame::Pasteboard {
                    id: 8,
                    mime: TEXT_MIME.into(),
                    len: text.len(),
                },
                Some(text),
            ),
            (Frame::ack(8, Err("boom".into())), None),
            (
                Frame::Bye {
                    reason: "replaced".into(),
                },
                None,
            ),
        ];
        let mut reader = Cursor::new(encode(&frames));
        for (frame, body) in frames {
            let (read, read_body) = read_frame(&mut reader).unwrap().unwrap();
            assert_eq!(read, frame);
            assert_eq!(read_body.as_deref(), body);
        }
        assert!(read_frame(&mut reader).unwrap().is_none());
    }

    #[test]
    fn the_wire_shape_is_flat_json() {
        let bytes = encode(&[(Frame::ack(3, Ok(())), None)]);
        assert_eq!(bytes, b"{\"kind\":\"ack\",\"id\":3,\"ok\":true}\n");
        let bytes = encode(&[(notification(), None)]);
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["kind"], "notify");
        assert_eq!(value["pane"], "w1:p2");
    }

    #[test]
    fn tolerates_crlf_and_blank_lines() {
        let mut reader = Cursor::new(b"\r\n{\"kind\":\"ping\",\"seq\":1}\r\n".to_vec());
        let (frame, _) = read_frame(&mut reader).unwrap().unwrap();
        assert_eq!(frame, Frame::Ping { seq: 1 });
    }

    #[test]
    fn rejects_oversized_headers_and_bodies_before_allocating() {
        let long = vec![b' '; MAX_HEADER + 10];
        assert!(matches!(
            read_frame(&mut Cursor::new(long)),
            Err(WireError::HeaderTooLong)
        ));
        let huge = format!(
            "{{\"kind\":\"pasteboard\",\"id\":1,\"mime\":\"x\",\"len\":{}}}\n",
            u64::MAX
        );
        assert!(matches!(
            read_frame(&mut Cursor::new(huge.into_bytes())),
            Err(WireError::BodyTooLarge(_))
        ));
    }

    #[test]
    fn reports_truncated_headers_and_bodies() {
        assert!(matches!(
            read_frame(&mut Cursor::new(b"{\"kind\":\"ping\"".to_vec())),
            Err(WireError::Truncated)
        ));
        let short = b"{\"kind\":\"pasteboard\",\"id\":1,\"mime\":\"x\",\"len\":5}\nab".to_vec();
        assert!(matches!(
            read_frame(&mut Cursor::new(short)),
            Err(WireError::Truncated)
        ));
    }

    #[test]
    fn unknown_kinds_consume_their_body_and_keep_the_stream_in_sync() {
        let mut bytes = b"{\"kind\":\"open\",\"id\":4,\"len\":3}\nabc".to_vec();
        bytes.extend(encode(&[(Frame::Ping { seq: 2 }, None)]));
        let mut reader = Cursor::new(bytes);
        match read_frame(&mut reader) {
            Err(WireError::Unsupported { kind, id }) => {
                assert_eq!(kind, "open");
                assert_eq!(id, Some(4));
            }
            other => panic!("expected unsupported, got {other:?}"),
        }
        let (frame, _) = read_frame(&mut reader).unwrap().unwrap();
        assert_eq!(frame, Frame::Ping { seq: 2 });
    }

    #[test]
    fn known_kinds_with_bad_fields_are_malformed() {
        assert!(matches!(
            read_frame(&mut Cursor::new(
                b"{\"kind\":\"notify\",\"id\":1}\n".to_vec()
            )),
            Err(WireError::Malformed(_))
        ));
    }

    #[test]
    fn request_ids_can_be_rewritten() {
        let frame = notification().with_request_id(42);
        assert_eq!(frame.request_id(), Some(42));
        assert_eq!(frame.request_kind(), Some(KIND_NOTIFY));
        assert_eq!(Frame::Ping { seq: 1 }.request_id(), None);
    }
}
