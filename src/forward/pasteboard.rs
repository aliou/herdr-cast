//! Factorial pasteboard watcher. macOS native clients (including pi) write
//! NSPasteboard without emitting a pane OSC 52 sequence. Poll `changeCount`
//! and inject OSC 52 into a pane's PTY. Herdr captures that output and sends
//! the copy to its foreground client without a custom socket method.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use base64::Engine;
use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};

use crate::api::SocketClient;

const POLL_INTERVAL: Duration = Duration::from_millis(300);
const OSC52_GRACE_PERIOD: Duration = Duration::from_millis(200);
const ECHO_WINDOW: Duration = Duration::from_secs(1);
const SOCKET_TIMEOUT: Duration = Duration::from_millis(500);
const MAX_TEXT_BYTES: usize = 192 * 1024;

/// A single watcher per Herdr session. Factorial is the only Mac on which
/// this host-to-client bridge is currently wanted.
pub fn start() -> Result<(), String> {
    if !watch_this_host() {
        return Ok(());
    }
    let socket = socket_path()?;
    let lock_path = lock_path(&socket)?;
    let lock = open_lock(&lock_path)?;
    if try_lock(&lock) {
        drop(lock);
        let executable = std::env::current_exe()
            .map_err(|error| format!("failed to resolve herdr-cast executable: {error}"))?;
        Command::new(executable)
            .arg("forward-daemon")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .map_err(|error| format!("failed to spawn forwarding daemon: {error}"))?;
    }
    Ok(())
}

pub fn daemon() -> Result<(), String> {
    if !watch_this_host() {
        return Ok(());
    }
    let socket = socket_path()?;
    let lock = open_lock(&lock_path(&socket)?)?;
    if !try_lock(&lock) {
        return Ok(());
    }
    let client = SocketClient::with_timeout(socket, SOCKET_TIMEOUT);
    let pasteboard = NSPasteboard::generalPasteboard();
    let mut previous_count = pasteboard.changeCount();
    let mut socket_failures = 0;
    let mut idle_polls = 0;
    let mut last_forwarded: Option<(String, Instant)> = None;
    loop {
        std::thread::sleep(POLL_INTERVAL);
        idle_polls += 1;
        if idle_polls == 10 {
            idle_polls = 0;
            if !socket_reachable(&client, &mut socket_failures) {
                return Ok(());
            }
        }
        let count = pasteboard.changeCount();
        if count == previous_count {
            continue;
        }
        let last_count = previous_count;
        previous_count = count;
        // This symbol is supplied by AppKit. Only read the pasteboard text
        // after a change; checking changeCount itself is cheap.
        let Some(text) = pasteboard
            .stringForType(unsafe { NSPasteboardTypeString })
            .map(|value| value.to_string())
            .filter(|text| !text.is_empty())
        else {
            continue;
        };
        if text.len() > MAX_TEXT_BYTES {
            continue;
        }
        // The pasteboard can change before a tool writes OSC 52. Give the
        // pane parser a moment to forward it before asking for the fallback.
        std::thread::sleep(OSC52_GRACE_PERIOD);
        if pasteboard.changeCount() != count {
            // The new value will be picked up on the next poll. Never send a
            // stale value over a more recent copy.
            continue;
        }
        // A local Herdr client may write the forwarded copy straight back to
        // this Mac's pasteboard. Do not keep re-injecting that echo.
        if last_forwarded
            .as_ref()
            .is_some_and(|(sent, at)| sent == &text && at.elapsed() < ECHO_WINDOW)
        {
            continue;
        }
        match forward_to_focused_pane(&client, &text) {
            Ok(()) => last_forwarded = Some((text, Instant::now())),
            Err(error) => {
                // There may be no runnable pane yet. Keep watching and retry
                // this copy; only sustained socket loss stops the daemon.
                previous_count = last_count;
                eprintln!("[cast] clipboard forwarding failed: {error}");
            }
        }
    }
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
    if !cfg!(target_os = "macos") {
        return false;
    }
    let mut name = [0u8; 256];
    if unsafe { libc::gethostname(name.as_mut_ptr().cast(), name.len()) } != 0 {
        return false;
    }
    let end = name
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(name.len());
    String::from_utf8_lossy(&name[..end]).split('.').next() == Some("factorial-machine")
}

fn socket_path() -> Result<String, String> {
    std::env::var("HERDR_SOCKET_PATH").map_err(|_| "HERDR_SOCKET_PATH not set".to_string())
}

fn lock_path(socket: &str) -> Result<PathBuf, String> {
    let state_dir = std::env::var_os("HERDR_PLUGIN_STATE_DIR")
        .ok_or_else(|| "HERDR_PLUGIN_STATE_DIR not set".to_string())?;
    Ok(PathBuf::from(state_dir).join(format!(
        "forward-{:016x}.lock",
        crate::notify::stable_hash(socket)
    )))
}

fn open_lock(path: &PathBuf) -> Result<File, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create forwarding state directory: {error}"))?;
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(path)
        .map_err(|error| format!("failed to open forwarding lock: {error}"))
}

fn try_lock(file: &File) -> bool {
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
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
