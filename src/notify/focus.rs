//! Click-to-focus: the `focus` command a macOS notification click runs.
//! Focuses the pane inside Herdr first, then raises the Ghostty window or
//! tab serving that session. Best-effort throughout on the window part: a
//! failed raise must never lose the pane focus.

use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::json;

use super::{log, truncate, wait_with_timeout};
use crate::api::SocketClient;

/// The app a local notification click raises when no matching Ghostty
/// terminal is found.
const ACTIVATE_APP: &str = "Ghostty";

pub fn focus(socket_path: &str, pane_id: &str) -> Result<(), String> {
    let client = SocketClient::new(socket_path);

    // Focus the pane inside Herdr first. Raising the right macOS window is
    // best-effort and can involve a slow or hung `osascript`/socket call;
    // it must never delay or block the actual pane focus.
    client.send(
        "cast:notification-focus",
        "agent.focus",
        json!({ "target": pane_id }),
    )?;

    #[cfg(target_os = "macos")]
    {
        // `open -a Ghostty` only activates the app, i.e. whatever Ghostty
        // window macOS last treated as key. With more than one Ghostty
        // window/tab open (one per Herdr session), that can raise the wrong
        // one entirely. Prefer raising the exact terminal surface hosting
        // this session, matched by pid through Ghostty's own AppleScript
        // `focus` command, which brings its window and tab forward
        // directly. `agent.focus` above already picked the right pane
        // inside Herdr; this only has to reveal the OS window/tab Herdr is
        // rendering into.
        if !focus_ghostty_terminal_for_session(socket_path) {
            activate_ghostty_app();
        }
    }

    Ok(())
}

/// Raise the Ghostty tab whose foreground process is the first of `pids`
/// found, or just activate Ghostty when none is. Best-effort.
pub(crate) fn raise_ghostty_tab(pids: &[u32]) {
    #[cfg(target_os = "macos")]
    if pids.is_empty() || !run_applescript(&focus_terminal_by_pid_script(pids)) {
        activate_ghostty_app();
    }
    #[cfg(not(target_os = "macos"))]
    let _ = pids;
}

/// Best-effort fallback when no matching Ghostty terminal was found, such as
/// a remote session or a socket this host isn't serving. Logs rather than
/// fails: raising the wrong (or no) window should never stop the pane from
/// being focused inside Herdr.
#[cfg(target_os = "macos")]
fn activate_ghostty_app() {
    match Command::new("open").args(["-a", ACTIVATE_APP]).status() {
        Ok(status) if status.success() => {}
        Ok(status) => log(&format!("failed to activate {ACTIVATE_APP}: {status}")),
        Err(error) => log(&format!("failed to activate {ACTIVATE_APP}: {error}")),
    }
}

/// Raise the specific Ghostty window/tab serving `socket_path`. Returns false
/// when the session's own OS process can't be identified (a remote session,
/// or the socket call failed) or no Ghostty terminal reports that pid, so the
/// caller can fall back to activating the app.
///
/// A Herdr session multiplexes every internal pane and tab inside a single
/// outer PTY, so Ghostty only ever sees one `terminal` surface per Herdr
/// session, not one per pane. That surface's process is the `herdr` client
/// Ghostty itself launched, which is the parent of the `herdr server`
/// process holding the session's socket open.
#[cfg(target_os = "macos")]
fn focus_ghostty_terminal_for_session(socket_path: &str) -> bool {
    let Some(pid) = herdr_client_pid(socket_path) else {
        return false;
    };
    run_applescript(&focus_terminal_by_pid_script(&[pid]))
}

/// The pid of the `herdr` client process Ghostty launched for `socket_path`.
///
/// Connects to the session's own socket and reads the peer's pid directly
/// via the `LOCAL_PEERPID` socket option (the `herdr server` process
/// accepting the connection), then reads that process's parent pid via
/// `proc_pidinfo`, which is the `herdr` client Ghostty launched. Both steps
/// use the same libproc/sysctl primitives `lsof`/`ps` are built on, without
/// spawning either.
#[cfg(target_os = "macos")]
fn herdr_client_pid(socket_path: &str) -> Option<u32> {
    use std::os::unix::io::AsRawFd;
    use std::os::unix::net::UnixStream;

    let stream = UnixStream::connect(socket_path).ok()?;
    let server_pid = peer_pid(stream.as_raw_fd())?;
    parent_pid(server_pid)
}

/// The pid on the other end of a connected Unix domain socket.
#[cfg(target_os = "macos")]
fn peer_pid(fd: std::os::unix::io::RawFd) -> Option<u32> {
    let mut pid: libc::pid_t = 0;
    let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            &mut pid as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    (result == 0 && pid > 0).then_some(pid as u32)
}

/// The parent pid of `pid`, read via `proc_pidinfo`.
#[cfg(target_os = "macos")]
fn parent_pid(pid: u32) -> Option<u32> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let result = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    (result == size && info.pbi_ppid > 0).then_some(info.pbi_ppid)
}

/// AppleScript that walks every Ghostty terminal surface across every window
/// and tab (the `terminals` element on `application` is flattened), and
/// focuses the one whose foreground pid matches. Ghostty's `focus` command
/// brings that terminal's window and tab to the front directly, so no
/// separate app activation step is needed on success.
#[cfg(target_os = "macos")]
fn focus_terminal_by_pid_script(pids: &[u32]) -> String {
    let pids = pids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r#"tell application "Ghostty"
    repeat with wanted in {{{pids}}}
        repeat with candidate in terminals
            if pid of candidate is (contents of wanted) then
                focus candidate
                return true
            end if
        end repeat
    end repeat
    return false
end tell"#
    )
}

#[cfg(target_os = "macos")]
fn run_applescript(script: &str) -> bool {
    use std::io::Read;
    // A hung Ghostty must not hang a notification click.
    const TIMEOUT: Duration = Duration::from_secs(5);
    let mut child = match Command::new("osascript")
        .arg("-e")
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            log(&format!("failed to run osascript: {error}"));
            return false;
        }
    };
    let Some(status) = wait_with_timeout(&mut child, TIMEOUT) else {
        log(&format!("osascript did not exit within {TIMEOUT:?}"));
        return false;
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_string(&mut stdout);
    }
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    if !status.success() {
        let stderr = stderr.replace('\n', " ");
        log(&format!("osascript failed: {}", truncate(&stderr, 500)));
        return false;
    }
    stdout.trim() == "true"
}

#[cfg(target_os = "macos")]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_a_script_that_matches_the_terminal_by_pid_and_focuses_it() {
        let script = focus_terminal_by_pid_script(&[1760, 1759]);
        assert!(script.contains(r#"tell application "Ghostty""#));
        assert!(script.contains("repeat with wanted in {1760, 1759}"));
        assert!(script.contains("if pid of candidate is (contents of wanted) then"));
        assert!(script.contains("focus candidate"));
        assert!(script.contains("return false"));
    }
}
