//! Agent notifications: the `notify` hook, the shared two-line layout,
//! local macOS delivery, bridged delivery on the host Mac, the
//! `terminal-notifier` shim Herdr's client invokes, and click-to-focus.

mod bridged;
mod compose;
mod focus;
mod forwarder;
mod hook;
mod local;
mod paths;

pub(crate) use bridged::{deliver_bridged, prepare_bridged_delivery, BridgeClick};
pub use focus::focus;
pub(crate) use focus::raise_ghostty_tab;
pub use forwarder::forward;
pub use hook::{clear_from_event, run};
pub(crate) use local::{clear_delivered_for_pane, wait_with_timeout};

/// The agent statuses that trigger a notification. Keep personal policy
/// constants here; there is no config file or per-setting environment
/// override.
pub(crate) const TRIGGER_STATUSES: &[&str] = &["blocked", "done"];

pub(crate) fn stable_hash(value: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in value.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Hex-encode a string for use in state file names: no path characters, no
/// collisions between values that differ only in separators.
pub(crate) fn hex_key(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(value.len() * 2);
    for byte in value.bytes() {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn truncate(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn log(message: &str) {
    eprintln!("[cast] {message}");
}

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A lock held for the lifetime of one notification lifecycle step (debounce,
/// delivery, clear, registration): a directory that exists only while its
/// owner works, stolen when older than `stale_after`.
pub(crate) struct DirectoryLock {
    path: std::path::PathBuf,
}

impl DirectoryLock {
    pub(crate) fn acquire(
        path: std::path::PathBuf,
        stale_after: Duration,
        attempts: usize,
    ) -> Result<Option<Self>, String> {
        for _ in 0..attempts {
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Some(Self { path })),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = std::fs::metadata(&path)
                        .and_then(|metadata| metadata.modified())
                        .ok()
                        .and_then(|modified| modified.elapsed().ok())
                        .is_some_and(|age| age > stale_after);
                    if stale {
                        let _ = std::fs::remove_dir(&path);
                        continue;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => {
                    return Err(format!(
                        "failed to acquire lock {}: {error}",
                        path.display()
                    ));
                }
            }
        }
        Ok(None)
    }
}

impl Drop for DirectoryLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_state_file_keys_without_collisions_or_path_characters() {
        assert_eq!(hex_key("w1:p1/../../x"), "77313a70312f2e2e2f2e2e2f78");
        assert_ne!(hex_key("w1:p1"), hex_key("w1_p1"));
    }
}
