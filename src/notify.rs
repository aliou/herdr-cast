use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::api::SocketClient;

const TRIGGER_STATUSES: &[&str] = &["blocked", "done"];
const TERMINAL_APP_IDS: &[&str] = &[
    "com.mitchellh.ghostty",
    "com.apple.Terminal",
    "com.googlecode.iterm2",
    "net.kovidgoyal.kitty",
    "com.github.wez.wezterm",
    "org.alacritty",
];
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

struct Paths {
    root: PathBuf,
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

    if let (Some(client), Some(workspace_id)) = (client.as_ref(), workspace_id.as_deref()) {
        if focused_workspace_id(client).as_deref() == Some(workspace_id) {
            if let Some(frontmost) = frontmost_bundle_id() {
                if TERMINAL_APP_IDS.contains(&frontmost.as_str()) {
                    return Ok(());
                }
            }
        }
    }

    if is_debounced(&paths.state, pane_id, &status)? {
        return Ok(());
    }
    if !paths.notifier.is_file() {
        log("bundled HerdrNotify.app executable is missing");
        return Ok(());
    }
    if !ensure_notifier_registered(&paths) {
        return Ok(());
    }

    let (title, icon_name) = match status.as_str() {
        "blocked" => (format!("⏳ {agent} needs input"), "blocked.png"),
        "done" => (format!("✅ {agent} done"), "done.png"),
        _ => return Ok(()),
    };
    let body = format!("{workspace} · {worktree}");
    let icon = paths.root.join("assets/icons").join(icon_name);

    let mut args = vec![
        "-title".to_string(),
        title,
        "-message".to_string(),
        body,
        "-group".to_string(),
        pane_id.to_string(),
    ];
    if icon.is_file() {
        args.push("-contentImage".to_string());
        args.push(icon.to_string_lossy().into_owned());
    }
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

pub fn focus(socket_path: &str, pane_id: &str) -> Result<(), String> {
    let activation = Command::new("open")
        .args(["-a", ACTIVATE_APP])
        .status()
        .map_err(|error| format!("failed to activate {ACTIVATE_APP}: {error}"))?;
    if !activation.success() {
        return Err(format!("failed to activate {ACTIVATE_APP}: {activation}"));
    }

    SocketClient::new(socket_path).send(
        "cast:notification-focus",
        "agent.focus",
        json!({ "target": pane_id }),
    )?;
    Ok(())
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
            root,
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

fn focused_workspace_id(client: &SocketClient) -> Option<String> {
    let response = client
        .send("cast:workspace-list", "workspace.list", json!({}))
        .ok()?;
    response
        .pointer("/result/workspaces")?
        .as_array()?
        .iter()
        .find(|workspace| workspace.get("focused").and_then(Value::as_bool) == Some(true))
        .and_then(|workspace| workspace.get("workspace_id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn string_at(value: &Value, pointer: &str) -> Option<String> {
    value.pointer(pointer)?.as_str().map(str::to_owned)
}

fn frontmost_bundle_id() -> Option<String> {
    let front = Command::new("lsappinfo").arg("front").output().ok()?;
    if !front.status.success() {
        return None;
    }
    let asn = String::from_utf8(front.stdout).ok()?;
    let info = Command::new("lsappinfo")
        .args(["info", "-only", "bundleid", asn.trim()])
        .output()
        .ok()?;
    if !info.status.success() {
        return None;
    }
    parse_bundle_id(&String::from_utf8(info.stdout).ok()?)
}

fn parse_bundle_id(output: &str) -> Option<String> {
    let (_, value) = output.split_once('=')?;
    let value = value.trim();
    value
        .strip_prefix('"')?
        .strip_suffix('"')
        .map(str::to_owned)
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
    fn parses_frontmost_bundle_id() {
        assert_eq!(
            parse_bundle_id("\"CFBundleIdentifier\"=\"com.mitchellh.ghostty\"\n"),
            Some("com.mitchellh.ghostty".into())
        );
        assert_eq!(parse_bundle_id("unexpected"), None);
    }

    #[test]
    fn encodes_state_file_keys_without_collisions_or_path_characters() {
        assert_eq!(hex_key("w1:p1/../../x"), "77313a70312f2e2e2f2e2e2f78");
        assert_ne!(hex_key("w1:p1"), hex_key("w1_p1"));
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
}
