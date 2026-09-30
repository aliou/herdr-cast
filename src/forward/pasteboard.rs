//! Factorial pasteboard watcher. macOS native clients (including pi) write
//! NSPasteboard without emitting a pane OSC 52 sequence. Poll `changeCount`
//! and send each new text copy to the host Mac through the bridge. Without
//! a bridge, inject OSC 52 into the focused pane's PTY instead; Herdr
//! captures that output and sends the copy to its foreground client.

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use base64::Engine;
use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};

use crate::api::SocketClient;
use crate::bridge;
use crate::daemon;

const POLL_INTERVAL: Duration = Duration::from_millis(300);
const OSC52_GRACE_PERIOD: Duration = Duration::from_millis(200);
const ECHO_WINDOW: Duration = Duration::from_secs(1);
const SOCKET_TIMEOUT: Duration = Duration::from_millis(500);
/// Herdr forwards OSC 52 through its client protocol; keep injected copies
/// well under what that path carries. The bridge accepts larger bodies.
const OSC52_MAX_BYTES: usize = 192 * 1024;

/// A single watcher per Herdr session. Factorial is the only Mac on which
/// this host-to-client bridge is currently wanted.
pub fn start() -> Result<(), String> {
    if !watch_this_host() {
        return Ok(());
    }
    if daemon::is_held(&lock_path(&socket_path()?)?)? {
        return Ok(());
    }
    daemon::spawn_detached(&["forward-daemon"], None)
}

pub fn daemon() -> Result<(), String> {
    if !watch_this_host() {
        return Ok(());
    }
    let socket = socket_path()?;
    let Some(_lock) = daemon::acquire(&lock_path(&socket)?)? else {
        return Ok(());
    };
    let client = SocketClient::with_timeout(socket, SOCKET_TIMEOUT);
    let pasteboard = NSPasteboard::generalPasteboard();
    let mut watcher = Watcher::new(&pasteboard);
    let mut socket_failures = 0;
    let mut idle_polls = 0;
    loop {
        std::thread::sleep(POLL_INTERVAL);
        idle_polls += 1;
        if idle_polls == 10 {
            idle_polls = 0;
            if !socket_reachable(&client, &mut socket_failures) {
                return Ok(());
            }
        }
        let Some(text) = watcher.next_change(&pasteboard) else {
            continue;
        };
        match deliver(&client, &text) {
            Ok(()) => watcher.delivered(text),
            Err(error) => {
                // There may be no runnable pane yet. Keep watching and retry
                // this copy; only sustained socket loss stops the daemon.
                watcher.retry();
                eprintln!("[cast] clipboard forwarding failed: {error}");
            }
        }
    }
}

/// Pasteboard change detection, separate from delivery.
struct Watcher {
    previous_count: isize,
    last_count: isize,
    last_forwarded: Option<(String, Instant)>,
}

impl Watcher {
    fn new(pasteboard: &NSPasteboard) -> Self {
        let previous_count = pasteboard.changeCount();
        Self {
            previous_count,
            last_count: previous_count,
            last_forwarded: None,
        }
    }

    /// The pasteboard text to forward, if it changed since the last poll
    /// and survives the size, grace-period, and echo filters.
    fn next_change(&mut self, pasteboard: &NSPasteboard) -> Option<String> {
        let count = pasteboard.changeCount();
        if count == self.previous_count {
            return None;
        }
        self.last_count = self.previous_count;
        self.previous_count = count;
        // This symbol is supplied by AppKit. Only read the pasteboard text
        // after a change; checking changeCount itself is cheap.
        let text = pasteboard
            .stringForType(unsafe { NSPasteboardTypeString })
            .map(|value| value.to_string())
            .filter(|text| !text.is_empty())?;
        if text.len() > bridge::wire::MAX_BODY {
            return None;
        }
        // The pasteboard can change before a tool writes OSC 52. Give the
        // pane parser a moment to forward it before asking for the fallback.
        std::thread::sleep(OSC52_GRACE_PERIOD);
        if pasteboard.changeCount() != count {
            // The new value will be picked up on the next poll. Never send a
            // stale value over a more recent copy.
            return None;
        }
        // A local Herdr client may write the forwarded copy straight back to
        // this Mac's pasteboard. Do not keep re-injecting that echo.
        if self
            .last_forwarded
            .as_ref()
            .is_some_and(|(sent, at)| sent == &text && at.elapsed() < ECHO_WINDOW)
        {
            return None;
        }
        Some(text)
    }

    fn delivered(&mut self, text: String) {
        self.last_forwarded = Some((text, Instant::now()));
    }

    /// Forget the last change so the next poll offers the same copy again.
    fn retry(&mut self) {
        self.previous_count = self.last_count;
    }
}

/// Bridge first; OSC 52 into the focused pane when no bridge takes it. A
/// copy too large for OSC 52 is dropped rather than retried.
fn deliver(client: &SocketClient, text: &str) -> Result<(), String> {
    match bridge::send_pasteboard(text) {
        Ok(()) => return Ok(()),
        Err(bridge::Unavailable::NoSocket) => {}
        Err(error) => eprintln!("[cast] bridge: {error}; using OSC 52"),
    }
    if text.len() > OSC52_MAX_BYTES {
        eprintln!(
            "[cast] dropped a {} byte copy: too large for OSC 52",
            text.len()
        );
        return Ok(());
    }
    forward_to_focused_pane(client, text)
}

/// `pane.process_info` exposes the focused pane's shell PID. macOS `ps`
/// supplies that process's controlling tty; writing to its slave device
/// becomes pane output, which Herdr already parses for OSC 52.
fn forward_to_focused_pane(client: &SocketClient, text: &str) -> Result<(), String> {
    let response = client.send(
        "cast:forward-pane",
        "pane.process_info",
        serde_json::json!({}),
    )?;
    let pid = response
        .pointer("/result/process_info/shell_pid")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| "focused pane has no shell PID".to_string())?;
    let output = Command::new("/bin/ps")
        .args(["-o", "tty=", "-p", &pid.to_string()])
        .output()
        .map_err(|error| format!("failed to read pane tty: {error}"))?;
    if !output.status.success() {
        return Err(format!("ps failed for pane shell {pid}"));
    }
    let tty = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !valid_pane_tty(&tty) {
        return Err(format!("pane shell {pid} has no macOS PTY: {tty}"));
    }
    let path = format!("/dev/{tty}");
    let mut device = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK)
        .open(&path)
        .map_err(|error| format!("failed to open pane PTY {path}: {error}"))?;
    let data = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    device
        .write_all(format!("\x1b]52;c;{data}\x07").as_bytes())
        .map_err(|error| format!("failed to write OSC 52 to {path}: {error}"))
}

fn valid_pane_tty(tty: &str) -> bool {
    tty.strip_prefix("ttys").is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
    })
}

fn socket_reachable(client: &SocketClient, failures: &mut u32) -> bool {
    match client.send("cast:forward-ping", "ping", serde_json::json!({})) {
        Ok(_) => *failures = 0,
        Err(error) => {
            *failures += 1;
            eprintln!("[cast] forwarding socket unavailable: {error}");
        }
    }
    *failures < 10
}

fn watch_this_host() -> bool {
    daemon::hostname()
        .and_then(|host| daemon::short_host(&host))
        .is_some_and(|host| host == "factorial-machine")
}

fn socket_path() -> Result<String, String> {
    std::env::var("HERDR_SOCKET_PATH").map_err(|_| "HERDR_SOCKET_PATH not set".to_string())
}

fn lock_path(socket: &str) -> Result<PathBuf, String> {
    Ok(daemon::state_dir()?.join(format!(
        "forward-{:016x}.lock",
        crate::notify::stable_hash(socket)
    )))
}

#[cfg(test)]
mod tests {
    use super::valid_pane_tty;

    #[test]
    fn accepts_only_macos_pty_names() {
        assert!(valid_pane_tty("ttys003"));
        for name in ["", "ttys", "ttys3/../../x", "console", "??", "ttys3\n"] {
            assert!(!valid_pane_tty(name), "accepted {name:?}");
        }
    }
}
