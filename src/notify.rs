use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::api::SocketClient;

const TRIGGER_STATUSES: &[&str] = &["blocked", "done"];
const ACTIVATE_APP: &str = "Ghostty";
const DEBOUNCE_SECONDS: u64 = 2;
const REGISTER_TTL_SECONDS: u64 = 6 * 60 * 60;
const LSREGISTER: &str = "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister";

#[derive(Debug, Default, Deserialize)]
struct EventEnvelope {
    #[serde(default)]
    data: EventData,
}

#[derive(Debug, Default, Deserialize)]
struct EventData {
    pane_id: Option<String>,
    workspace_id: Option<String>,
    agent_status: Option<String>,
    agent: Option<String>,
}

#[derive(Serialize)]
struct NotificationShowParams {
    title: String,
    body: Option<String>,
    position: Option<String>,
    sound: &'static str,
}

struct Paths {
    state: PathBuf,
    app: PathBuf,
    notifier: PathBuf,
}

pub fn run() -> Result<(), String> {
    let paths = Paths::from_environment()?;
    fs::create_dir_all(&paths.state)
        .map_err(|error| format!("failed to create plugin state directory: {error}"))?;

    let event: EventEnvelope = environment_json("HERDR_PLUGIN_EVENT_JSON");
    let Some(pane_id) = event.data.pane_id.as_deref() else {
        log("dropped event without data.pane_id");
        return Ok(());
    };

    let socket_path = std::env::var("HERDR_SOCKET_PATH").ok();
    let client = socket_path
        .as_deref()
        .map(|path| SocketClient::with_timeout(path, Duration::from_millis(250)));
    let mut pane = Value::Null;
    let status = event.data.agent_status.or_else(|| {
        pane = pane_info(client.as_ref(), pane_id);
        string_at(&pane, "/result/pane/agent_status")
    });
    let Some(status) = status else {
        log("dropped event without an agent status");
        return Ok(());
    };
    if !TRIGGER_STATUSES.contains(&status.as_str()) {
        return Ok(());
    }
    if pane.is_null() {
        pane = pane_info(client.as_ref(), pane_id);
    }

    let workspace_id = event
        .data
        .workspace_id
        .or_else(|| string_at(&pane, "/result/pane/workspace_id"));
    let agent = event
        .data
        .agent
        .or_else(|| string_at(&pane, "/result/pane/agent"))
        .unwrap_or_else(|| "agent".to_string());
    let cwd = string_at(&pane, "/result/pane/cwd");
    let workspace = workspace_id
        .as_deref()
        .and_then(|id| workspace_label(client.as_ref(), id))
        .or_else(|| workspace_id.clone())
        .unwrap_or_default();
    let worktree = cwd
        .as_deref()
        .and_then(|path| Path::new(path).file_name())
        .and_then(|name| name.to_str())
        .unwrap_or(&workspace);

    if is_debounced(&paths.state, pane_id, &status)? {
        return Ok(());
    }
    let (title, sound_name) = match status.as_str() {
        "blocked" => (format!("⏳ {agent} needs input"), "Glass"),
        "done" => (format!("✅ {agent} done"), "Funk"),
        _ => return Ok(()),
    };
    let body = format!("{workspace} · {worktree}");

    if cfg!(target_os = "macos") {
        deliver_macos_notification(
            &paths,
            socket_path.as_deref(),
            pane_id,
            title,
            body,
            sound_name,
        )?;
    } else {
        deliver_terminal_notification(client.as_ref(), title, body);
    }

    Ok(())
}

fn deliver_macos_notification(
    paths: &Paths,
    socket_path: Option<&str>,
    pane_id: &str,
    title: String,
    body: String,
    sound_name: &str,
) -> Result<(), String> {
    if !paths.notifier.is_file() {
        log("bundled HerdrNotify.app executable is missing");
        return Ok(());
    }
    if !ensure_notifier_registered(paths) {
        return Ok(());
    }
    let mut args = vec![
        "-title".to_string(),
        title,
        "-message".to_string(),
        body,
        "-group".to_string(),
        pane_id.to_string(),
        "-sound".to_string(),
        sound_name.to_string(),
    ];
    if let Some(socket_path) = socket_path {
        let current_exe = std::env::current_exe()
            .map_err(|error| format!("failed to locate herdr-cast executable: {error}"))?;
        args.push("-execute".to_string());
        args.push(click_command(&current_exe, &socket_path, pane_id));
    }

    match Command::new(&paths.notifier).args(&args).output() {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).replace('\n', " ");
            log(&format!(
                "notifier failed with {}: {}",
                output.status,
                truncate(&stderr, 500)
            ));
        }
        Err(error) => log(&format!("failed to start notifier: {error}")),
    }

    Ok(())
}

fn deliver_terminal_notification(client: Option<&SocketClient>, title: String, body: String) {
    let Some(client) = client else {
        log("HERDR_SOCKET_PATH is not set; cannot request terminal notification");
        return;
    };

    let response = client.send(
        "cast:notification-show",
        "notification.show",
        NotificationShowParams {
            title,
            body: Some(body),
            position: None,
            sound: "none",
        },
    );
    match response {
        Ok(response) => {
            let shown = response
                .pointer("/result/shown")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !shown {
                let reason = response
                    .pointer("/result/reason")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                log(&format!("terminal notification was not shown: {reason}"));
            }
        }
        Err(error) => log(&format!("failed to request terminal notification: {error}")),
    }
}

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
    run_applescript(&focus_terminal_by_pid_script(pid))
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
fn focus_terminal_by_pid_script(pid: u32) -> String {
    format!(
        r#"tell application "Ghostty"
    repeat with candidate in terminals
        if pid of candidate is {pid} then
            focus candidate
            return true
        end if
    end repeat
    return false
end tell"#
    )
}

#[cfg(target_os = "macos")]
fn run_applescript(script: &str) -> bool {
    match Command::new("osascript").arg("-e").arg(script).output() {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim() == "true"
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).replace('\n', " ");
            log(&format!("osascript failed: {}", truncate(&stderr, 500)));
            false
        }
        Err(error) => {
            log(&format!("failed to run osascript: {error}"));
            false
        }
    }
}

impl Paths {
    fn from_environment() -> Result<Self, String> {
        let root = std::env::var_os("HERDR_PLUGIN_ROOT")
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .ok_or_else(|| "plugin root is unavailable".to_string())?;
        let state = std::env::var_os("HERDR_PLUGIN_STATE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("herdr-cast"));
        let app = root.join("assets/HerdrNotify.app");
        let notifier = app.join("Contents/MacOS/terminal-notifier");
        Ok(Self {
            state,
            app,
            notifier,
        })
    }
}

fn environment_json<T: for<'de> Deserialize<'de> + Default>(name: &str) -> T {
    std::env::var(name)
        .ok()
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default()
}

fn pane_info(client: Option<&SocketClient>, pane_id: &str) -> Value {
    client
        .and_then(|client| {
            client
                .send("cast:pane-get", "pane.get", json!({ "pane_id": pane_id }))
                .ok()
        })
        .unwrap_or_default()
}

fn workspace_label(client: Option<&SocketClient>, workspace_id: &str) -> Option<String> {
    let response = client?
        .send(
            "cast:workspace-get",
            "workspace.get",
            json!({ "workspace_id": workspace_id }),
        )
        .ok()?;
    string_at(&response, "/result/workspace/label")
}

fn string_at(value: &Value, pointer: &str) -> Option<String> {
    value.pointer(pointer)?.as_str().map(str::to_owned)
}

fn is_debounced(state_dir: &Path, pane_id: &str, status: &str) -> Result<bool, String> {
    let key = format!("{}-{}", hex_key(pane_id), hex_key(status));
    let path = state_dir.join(format!("debounce-{key}"));
    let lock_path = state_dir.join(format!("debounce-{key}.lock"));
    let Some(_lock) = DirectoryLock::acquire(lock_path, Duration::from_secs(DEBOUNCE_SECONDS), 20)?
    else {
        return Ok(false);
    };
    let now = unix_seconds();
    let previous = fs::read_to_string(&path)
        .ok()
        .and_then(|contents| contents.trim().parse::<u64>().ok());
    if previous.is_some_and(|timestamp| now.saturating_sub(timestamp) < DEBOUNCE_SECONDS) {
        return Ok(true);
    }
    fs::write(&path, format!("{now}\n"))
        .map_err(|error| format!("failed to write debounce state: {error}"))?;
    Ok(false)
}

struct DirectoryLock {
    path: PathBuf,
}

impl DirectoryLock {
    fn acquire(
        path: PathBuf,
        stale_after: Duration,
        attempts: usize,
    ) -> Result<Option<Self>, String> {
        for _ in 0..attempts {
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Some(Self { path })),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = fs::metadata(&path)
                        .and_then(|metadata| metadata.modified())
                        .ok()
                        .and_then(|modified| modified.elapsed().ok())
                        .is_some_and(|age| age > stale_after);
                    if stale {
                        let _ = fs::remove_dir(&path);
                        continue;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => {
                    return Err(format!("failed to acquire debounce lock: {error}"));
                }
            }
        }
        Ok(None)
    }
}

impl Drop for DirectoryLock {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.path);
    }
}

fn hex_key(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(value.len() * 2);
    for byte in value.bytes() {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn ensure_notifier_registered(paths: &Paths) -> bool {
    let lock_path = paths.state.join(".notifier-registration.lock");
    let _lock = match DirectoryLock::acquire(lock_path, Duration::from_secs(120), 500) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            log("timed out waiting for notifier registration lock");
            return false;
        }
        Err(error) => {
            log(&error);
            return false;
        }
    };
    let sentinel = paths.state.join(".notifier-registered");
    if !registration_expired(&sentinel, &paths.notifier) {
        return true;
    }

    if !quiet_status(
        Command::new("codesign")
            .args(["--verify", "--deep"])
            .arg(&paths.app),
    ) && !quiet_status(
        Command::new("codesign")
            .args(["--force", "--deep", "-s", "-"])
            .arg(&paths.app),
    ) {
        log("failed to ad-hoc sign HerdrNotify.app");
    }

    if quiet_status(Command::new(LSREGISTER).arg("-f").arg(&paths.app)) {
        if let Err(error) = fs::write(&sentinel, unix_seconds().to_string()) {
            log(&format!(
                "failed to update notifier registration state: {error}"
            ));
        }
    } else {
        log("failed to register HerdrNotify.app with Launch Services");
    }
    true
}

fn registration_expired(sentinel: &Path, notifier: &Path) -> bool {
    let Some(timestamp) = fs::read_to_string(sentinel)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    else {
        return true;
    };
    if unix_seconds().saturating_sub(timestamp) >= REGISTER_TTL_SECONDS {
        return true;
    }

    let sentinel_modified = fs::metadata(sentinel).and_then(|metadata| metadata.modified());
    let notifier_modified = fs::metadata(notifier).and_then(|metadata| metadata.modified());
    match (sentinel_modified, notifier_modified) {
        (Ok(sentinel), Ok(notifier)) => notifier > sentinel,
        _ => true,
    }
}

fn quiet_status(command: &mut Command) -> bool {
    command
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn click_command(executable: &Path, socket_path: &str, pane_id: &str) -> String {
    [
        executable.to_string_lossy().as_ref(),
        "focus",
        socket_path,
        pane_id,
    ]
    .into_iter()
    .map(shell_quote)
    .collect::<Vec<_>>()
    .join(" ")
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

fn truncate(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn log(message: &str) {
    eprintln!("[cast] {message}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_state_file_keys_without_collisions_or_path_characters() {
        assert_eq!(hex_key("w1:p1/../../x"), "77313a70312f2e2e2f2e2e2f78");
        assert_ne!(hex_key("w1:p1"), hex_key("w1_p1"));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn builds_a_script_that_matches_the_terminal_by_pid_and_focuses_it() {
        let script = focus_terminal_by_pid_script(1760);
        assert!(script.contains(r#"tell application "Ghostty""#));
        assert!(script.contains("if pid of candidate is 1760 then"));
        assert!(script.contains("focus candidate"));
        assert!(script.contains("return false"));
    }

    #[test]
    fn quotes_every_click_command_argument_for_the_shell() {
        assert_eq!(
            click_command(
                Path::new("/tmp/cast app"),
                "/tmp/a'b.sock",
                "w1:p1;echo bad"
            ),
            "'/tmp/cast app' 'focus' '/tmp/a'\\''b.sock' 'w1:p1;echo bad'"
        );
    }

    #[test]
    fn terminal_notification_request_disables_sound() {
        let params = NotificationShowParams {
            title: "✅ Pi done".into(),
            body: Some("cast · herdr-cast".into()),
            position: None,
            sound: "none",
        };

        assert_eq!(
            serde_json::to_value(params).unwrap(),
            serde_json::json!({
                "title": "✅ Pi done",
                "body": "cast · herdr-cast",
                "position": null,
                "sound": "none"
            })
        );
    }
}
