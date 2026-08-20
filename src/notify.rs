use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::api::SocketClient;

const TRIGGER_STATUSES: &[&str] = &["blocked", "done"];
const BLOCKED_SOUND: &str = "Glass";
const DONE_SOUND: &str = "Funk";
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
    assets: PathBuf,
}

/// Which identity bundle renders a notification. macOS draws the left-side
/// notification icon from the sender bundle's registered icon and offers no
/// per-notification override, so each status ships as its own bundle with
/// its own composited icon (`HerdrNotify-blocked.app`, ...). Anything
/// without a status-specific bundle uses the neutral HerdrNotify.app.
fn bundle_variant(status: &str) -> Option<&'static str> {
    match status {
        "blocked" => Some("blocked"),
        "done" => Some("done"),
        _ => None,
    }
}

/// The pieces of the shared two-line notification layout. Line 1 (title)
/// names what the agent works on; line 2 (subtitle) states what happened,
/// qualified by the workspace label when it adds anything. `host` is set
/// only for remote (forwarded) notifications.
struct NotificationParts<'a> {
    action: &'a str,
    workspace: &'a str,
    project: &'a str,
    host: Option<&'a str>,
}

/// Render `parts` as (title, subtitle). Title is the project name, falling
/// back to the workspace label then `herdr`, suffixed with `@host` for
/// forwarded notifications. Subtitle is the action, extended with
/// `· workspace` only when the label differs from what the title already
/// shows. No body line exists: status is carried by the icon and sound.
fn compose(parts: &NotificationParts) -> (String, Option<String>) {
    let base = if !parts.project.is_empty() {
        parts.project
    } else if !parts.workspace.is_empty() {
        parts.workspace
    } else {
        "herdr"
    };
    let title = match parts.host {
        Some(host) if !host.is_empty() => format!("{base}@{host}"),
        _ => base.to_string(),
    };
    if parts.action.is_empty() {
        return (title, None);
    }
    let mut subtitle = parts.action.to_string();
    if !parts.workspace.is_empty() && parts.workspace != base {
        subtitle.push_str(" · ");
        subtitle.push_str(parts.workspace);
    }
    (title, Some(subtitle))
}

/// The osascript-independent notification flags Herdr's macOS client hands
/// to whichever `terminal-notifier` it finds on PATH (see
/// `platform::show_desktop_notification`). `forward-notify` is that binary.
#[derive(Default)]
struct ClientNotifyArgs<'a> {
    title: Option<&'a str>,
    body: Option<&'a str>,
    activate: Option<&'a str>,
}

/// The compact payload `forwarded_body` hides inside the notification body
/// so layout parts, grouping, and sound policy survive Herdr's
/// title/body-only protocol. Current senders fill `a`/`w`/`p`/`h`; the
/// legacy `t`/`b` pair stays supported so older remote senders still
/// render during a mixed-version transition.
#[derive(Deserialize)]
struct ForwardedPayload<'a> {
    v: u32,
    #[serde(default)]
    a: Option<&'a str>,
    #[serde(default)]
    w: Option<&'a str>,
    #[serde(default)]
    p: Option<&'a str>,
    #[serde(default)]
    h: Option<&'a str>,
    #[serde(default)]
    g: Option<&'a str>,
    #[serde(default)]
    s: Option<&'a str>,
    #[serde(default)]
    t: Option<&'a str>,
    #[serde(default)]
    b: Option<&'a str>,
}

/// Entrypoint for the `terminal-notifier` shim the Nix package installs
/// next to herdr-cast (see AGENTS.md). Renders the notification through the
/// bundled HerdrNotify.app. When the body carries a forwarded payload from a
/// remote herdr-cast, rebuild the invocation with the payload's title,
/// body, grouping, and sound; anything else (Herdr's own local toasts,
/// generic terminal-notifier callers) passes through verbatim. `exec` keeps
/// the process image on the bundle binary, which its NSBundle identity
/// requires for Notification Center delivery.
pub fn forward(arguments: Vec<String>) -> Result<(), String> {
    let paths = Paths::from_executable()?;
    let decision = forward_argv(&arguments);
    let notifier = paths.notifier(decision.variant);
    if !notifier.is_file() {
        return Err(format!(
            "bundled notifier executable is missing (expected {})",
            notifier.display()
        ));
    }
    fs::create_dir_all(&paths.state)
        .map_err(|error| format!("failed to create notifier state directory: {error}"))?;
    // Unlike the plugin notify path, exit non-zero when registration cannot
    // run: this shim is invoked by Herdr's client, which then falls back to
    // its built-in osascript notification instead of showing nothing.
    if cfg!(target_os = "macos") && !ensure_notifier_registered(&paths, decision.variant) {
        return Err("failed to prepare HerdrNotify.app registration".to_string());
    }
    use std::os::unix::process::CommandExt;
    let error = Command::new(&notifier).args(&decision.argv).exec();
    Err(format!(
        "failed to exec notifier {}: {error}",
        notifier.display()
    ))
}

/// What the shim decided for an invocation: the argv handed to the bundled
/// notifier plus which identity bundle renders it. The bundle owns the
/// notification's left icon, so the status carried by a forwarded payload
/// selects `HerdrNotify-<status>.app`; plain pass-through invocations use
/// the neutral bundle.
struct ForwardDecision {
    argv: Vec<String>,
    variant: Option<&'static str>,
}

/// Decide the arguments handed to the bundled notifier: rebuilt when the
/// invocation matches Herdr's exact client grammar (`-title`, `-message`,
/// optional `-activate`) and the body decodes as a forwarded payload. Every
/// other invocation passes through verbatim so unknown flags (today's
/// `-timeout`, future additions) are never silently dropped.
fn forward_argv(arguments: &[String]) -> ForwardDecision {
    let parsed = client_notify_args(arguments);
    let payload = parsed
        .as_ref()
        .and_then(|parsed| parsed.body)
        .and_then(forwarded_payload);
    let (parsed, payload) = match (parsed, payload) {
        (Some(parsed), Some(payload)) => (parsed, payload),
        _ => {
            return ForwardDecision {
                argv: arguments.to_vec(),
                variant: None,
            }
        }
    };
    let variant = payload.s.and_then(bundle_variant);

    // Current payload: recompose the two-line layout from its parts.
    if payload.a.is_some() || payload.w.is_some() || payload.p.is_some() {
        let parts = NotificationParts {
            action: payload.a.unwrap_or(""),
            workspace: payload.w.unwrap_or(""),
            project: payload.p.unwrap_or(""),
            host: payload.h,
        };
        let (title, subtitle) = compose(&parts);
        let mut argv = Vec::with_capacity(12);
        argv.push("-title".to_string());
        argv.push(title);
        if let Some(subtitle) = subtitle {
            argv.push("-subtitle".to_string());
            argv.push(subtitle);
        }
        if let Some(group) = payload.g {
            argv.push("-group".to_string());
            argv.push(group.to_string());
        }
        if let Some(sound) = payload.s.and_then(sound_for_status) {
            argv.push("-sound".to_string());
            argv.push(sound.to_string());
        }
        if let Some(activate) = parsed.activate {
            argv.push("-activate".to_string());
            argv.push(activate.to_string());
        }
        return ForwardDecision { argv, variant };
    }

    // Legacy payload (remote herdr-cast before the parts payload): title and
    // body carried directly, with the origin host as the subtitle.
    let mut argv = Vec::with_capacity(14);
    argv.push("-title".to_string());
    argv.push(payload.t.or(parsed.title).unwrap_or("herdr").to_string());
    if let Some(host) = payload.h {
        argv.push("-subtitle".to_string());
        argv.push(host.to_string());
    }
    argv.push("-body".to_string());
    argv.push(payload.b.unwrap_or_default().to_string());
    if let Some(group) = payload.g {
        argv.push("-group".to_string());
        argv.push(group.to_string());
    }
    if let Some(sound) = payload.s.and_then(sound_for_status) {
        argv.push("-sound".to_string());
        argv.push(sound.to_string());
    }
    if let Some(activate) = parsed.activate {
        argv.push("-activate".to_string());
        argv.push(activate.to_string());
    }
    ForwardDecision { argv, variant }
}

/// Parse Herdr's client notifier grammar strictly: every argument must be
/// one of the known `-title`/`-body`/`-activate` flag/value pairs, and a
/// body must be present. Anything else returns `None` so the caller passes
/// the invocation through untouched.
fn client_notify_args(arguments: &[String]) -> Option<ClientNotifyArgs<'_>> {
    let mut parsed = ClientNotifyArgs::default();
    let mut index = 0;
    while index + 1 < arguments.len() {
        let slot = match arguments[index].as_str() {
            "-title" => &mut parsed.title,
            // Herdr's client sends `-message`; the bundled notifier documents
            // `-body` (which the rewrite emits). Accept both.
            "-message" | "-body" => &mut parsed.body,
            "-activate" => &mut parsed.activate,
            _ => return None,
        };
        *slot = Some(arguments[index + 1].as_str());
        index += 2;
    }
    (index == arguments.len() && parsed.body.is_some()).then_some(parsed)
}

fn forwarded_payload(body: &str) -> Option<ForwardedPayload<'_>> {
    let body = body.trim_start();
    if !body.starts_with('{') {
        return None;
    }
    let payload: ForwardedPayload = serde_json::from_str(body).ok()?;
    (payload.v == 1).then_some(payload)
}

fn sound_for_status(status: &str) -> Option<&'static str> {
    match status {
        "blocked" => Some(BLOCKED_SOUND),
        "done" => Some(DONE_SOUND),
        _ => None,
    }
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
    let project = cwd
        .as_deref()
        .and_then(|path| Path::new(path).file_name())
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_string();

    if is_debounced(&paths.state, pane_id, &status)? {
        return Ok(());
    }
    // The title/subtitle carry no status glyphs: the per-status bundle icon
    // and sound are the status signal.
    let action = match status.as_str() {
        "blocked" => format!("{agent} needs input"),
        "done" => format!("{agent} done"),
        _ => return Ok(()),
    };
    let parts = NotificationParts {
        action: &action,
        workspace: &workspace,
        project: &project,
        host: None,
    };

    if cfg!(target_os = "macos") {
        let (title, subtitle) = compose(&parts);
        deliver_macos_notification(
            &paths,
            socket_path.as_deref(),
            pane_id,
            &status,
            title,
            subtitle,
        )?;
    } else {
        deliver_terminal_notification(client.as_ref(), pane_id, &status, &parts);
    }

    Ok(())
}

fn deliver_macos_notification(
    paths: &Paths,
    socket_path: Option<&str>,
    pane_id: &str,
    status: &str,
    title: String,
    subtitle: Option<String>,
) -> Result<(), String> {
    let variant = bundle_variant(status);
    let notifier = paths.notifier(variant);
    if !notifier.is_file() {
        log(&format!(
            "bundled notifier executable is missing (expected {})",
            notifier.display()
        ));
        return Ok(());
    }
    if !ensure_notifier_registered(paths, variant) {
        return Ok(());
    }
    let mut args = vec!["-title".to_string(), title];
    if let Some(subtitle) = subtitle {
        args.push("-subtitle".to_string());
        args.push(subtitle);
    }
    args.push("-group".to_string());
    args.push(pane_id.to_string());
    if let Some(sound) = sound_for_status(status) {
        args.push("-sound".to_string());
        args.push(sound.to_string());
    }
    if let Some(socket_path) = socket_path {
        let current_exe = std::env::current_exe()
            .map_err(|error| format!("failed to locate herdr-cast executable: {error}"))?;
        args.push("-execute".to_string());
        args.push(click_command(&current_exe, &socket_path, pane_id));
    }

    match Command::new(&notifier).args(&args).output() {
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

fn deliver_terminal_notification(
    client: Option<&SocketClient>,
    pane_id: &str,
    status: &str,
    parts: &NotificationParts,
) {
    let Some(client) = client else {
        log("HERDR_SOCKET_PATH is not set; cannot request terminal notification");
        return;
    };

    // The socket-level title reads sensibly for a client that does not
    // run the forwarder; the macOS forwarder rebuilds its own title and
    // subtitle from the payload parts and ignores this string.
    let response = client.send(
        "cast:notification-show",
        "notification.show",
        NotificationShowParams {
            body: Some(forwarded_body(pane_id, status, parts)),
            title: parts.action.to_string(),
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

/// Encode the remote-delivery payload consumed by the local HerdrNotify
/// forwarder. Herdr's client protocol carries only title and body strings,
/// so the layout parts, grouping key, and sound policy ride inside a
/// compact JSON body. Field caps keep the worst case under the server's
/// 240-character body cap (checked by tests) so the JSON cannot be sliced:
/// a client without the forwarder would otherwise show a truncated payload.
fn forwarded_body(pane_id: &str, status: &str, parts: &NotificationParts) -> String {
    let body = serde_json::to_string(&json!({
        "v": 1,
        "a": truncate(parts.action, 40),
        "w": truncate(parts.workspace, 40),
        "p": truncate(parts.project, 40),
        "h": truncate(&origin_host(), 32),
        "g": truncate(pane_id, 24),
        "s": status,
    }))
    .unwrap_or_else(|_| parts.action.to_string());
    // Defensive: with the caps above this never triggers, but a sliced
    // payload is much worse than a plain one.
    if body.chars().count() > 240 {
        return parts.action.to_string();
    }
    body
}

/// Origin hostname for the payload's subtitle marker. Sandbox VMs expose
/// their sandbox name as the hostname, which is the identifier that names
/// the session the notification came from; on other hosts this is simply
/// the machine name. Falls back to `remote` when the hostname cannot be
/// read so forwarded notifications never silently lose the origin marker.
fn origin_host() -> String {
    let mut buffer = [0u8; 256];
    let result =
        unsafe { libc::gethostname(buffer.as_mut_ptr() as *mut libc::c_char, buffer.len()) };
    if result != 0 {
        return "remote".to_string();
    }
    let end = buffer
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(buffer.len());
    let host = String::from_utf8_lossy(&buffer[..end]);
    let host = host.trim();
    if host.is_empty() {
        "remote".to_string()
    } else {
        host.to_string()
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
        let state = Self::state_dir();
        let assets = root.join("assets");
        Ok(Self { state, assets })
    }

    /// The app bundle rendering notifications for a given status variant
    /// (`None` = the neutral bundle for pass-through and Herdr's own
    /// toasts).
    fn app(&self, variant: Option<&str>) -> PathBuf {
        match variant {
            None => self.assets.join("HerdrNotify.app"),
            Some(variant) => self.assets.join(format!("HerdrNotify-{variant}.app")),
        }
    }

    fn notifier(&self, variant: Option<&str>) -> PathBuf {
        self.app(variant).join("Contents/MacOS/terminal-notifier")
    }

    /// Resolve the bundled app for `forward-notify`, which runs in client
    /// context (invoked via PATH) and cannot rely on `HERDR_PLUGIN_ROOT`.
    /// The Nix package installs `bin/herdr-cast` and
    /// `libexec/HerdrNotify.app` side by side, so the app lives two
    /// directories up from the resolved executable. Canonicalizing matters:
    /// the binary is usually reached through the `~/.local/bin/herdr-cast`
    /// symlink, and `current_exe` may report the link path.
    fn from_executable() -> Result<Self, String> {
        let exe = std::env::current_exe()
            .map_err(|error| format!("failed to locate herdr-cast executable: {error}"))?;
        let exe = fs::canonicalize(&exe).unwrap_or(exe);
        let prefix = exe
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| format!("unexpected executable location: {}", exe.display()))?;
        let assets = prefix.join("libexec");
        Ok(Self {
            state: Self::state_dir(),
            assets,
        })
    }

    fn state_dir() -> PathBuf {
        std::env::var_os("HERDR_PLUGIN_STATE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("herdr-cast"))
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

fn ensure_notifier_registered(paths: &Paths, variant: Option<&str>) -> bool {
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
    let sentinel = match variant {
        None => paths.state.join(".notifier-registered"),
        Some(variant) => paths.state.join(format!(".notifier-registered-{variant}")),
    };
    let notifier = paths.notifier(variant);
    if !registration_expired(&sentinel, &notifier) {
        return true;
    }

    let app = paths.app(variant);
    let name = app
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("HerdrNotify.app");
    if !quiet_status(
        Command::new("codesign")
            .args(["--verify", "--deep"])
            .arg(&app),
    ) && !quiet_status(
        Command::new("codesign")
            .args(["--force", "--deep", "-s", "-"])
            .arg(&app),
    ) {
        log(&format!("failed to ad-hoc sign {name}"));
    }

    if quiet_status(Command::new(LSREGISTER).arg("-f").arg(&app)) {
        if let Err(error) = fs::write(&sentinel, unix_seconds().to_string()) {
            log(&format!(
                "failed to update notifier registration state: {error}"
            ));
        }
    } else {
        log(&format!("failed to register {name} with Launch Services"));
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
            title: "Pi done".into(),
            body: Some("cast · herdr-cast".into()),
            position: None,
            sound: "none",
        };

        assert_eq!(
            serde_json::to_value(params).unwrap(),
            serde_json::json!({
                "title": "Pi done",
                "body": "cast · herdr-cast",
                "position": null,
                "sound": "none"
            })
        );
    }

    #[test]
    fn variants_map_trigger_statuses_to_identity_bundles() {
        assert_eq!(bundle_variant("blocked"), Some("blocked"));
        assert_eq!(bundle_variant("done"), Some("done"));
        assert_eq!(bundle_variant("working"), None);
    }

    #[test]
    fn compose_local_title_dedupes_workspace_and_project() {
        let (title, subtitle) = compose(&NotificationParts {
            action: "pi needs input",
            workspace: "herdr-cast",
            project: "herdr-cast",
            host: None,
        });
        assert_eq!(title, "herdr-cast");
        assert_eq!(subtitle, Some("pi needs input".to_string()));
    }

    #[test]
    fn compose_subtitle_keeps_a_differing_workspace_label() {
        let (title, subtitle) = compose(&NotificationParts {
            action: "pi done",
            workspace: "frank",
            project: "x",
            host: None,
        });
        assert_eq!(title, "x");
        assert_eq!(subtitle, Some("pi done · frank".to_string()));
    }

    #[test]
    fn compose_remote_title_carries_the_origin_host() {
        let (title, subtitle) = compose(&NotificationParts {
            action: "pi needs input",
            workspace: "herdr-cast",
            project: "herdr-cast",
            host: Some("cast-notify-fwd-0492f3"),
        });
        assert_eq!(title, "herdr-cast@cast-notify-fwd-0492f3");
        assert_eq!(subtitle, Some("pi needs input".to_string()));
    }

    #[test]
    fn compose_falls_back_to_workspace_then_herdr_for_the_title() {
        let parts = NotificationParts {
            action: "pi done",
            workspace: "frank",
            project: "",
            host: Some("donut"),
        };
        // Project unknown: the workspace label names the title instead, so
        // it must not repeat in the subtitle.
        assert_eq!(
            compose(&parts),
            ("frank@donut".to_string(), Some("pi done".to_string()))
        );
        let empty = NotificationParts {
            action: "pi done",
            workspace: "",
            project: "",
            host: None,
        };
        assert_eq!(compose(&empty).0, "herdr");
    }

    #[test]
    fn forwarded_body_encodes_forwarder_payload() {
        let body = forwarded_body(
            "pane-1",
            "blocked",
            &NotificationParts {
                action: "pi needs input",
                workspace: "herdr-cast",
                project: "herdr-cast",
                host: None,
            },
        );
        let value: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["v"], 1);
        assert_eq!(value["a"], "pi needs input");
        assert_eq!(value["w"], "herdr-cast");
        assert_eq!(value["p"], "herdr-cast");
        assert_eq!(value["g"], "pane-1");
        assert_eq!(value["s"], "blocked");
        assert!(value["h"].as_str().is_some_and(|host| !host.is_empty()));
    }

    #[test]
    fn forwarded_body_fits_the_server_body_cap() {
        let body = forwarded_body(
            &"p".repeat(64),
            "blocked",
            &NotificationParts {
                action: &"a".repeat(200),
                workspace: &"w".repeat(200),
                project: &"p".repeat(200),
                host: None,
            },
        );
        let value: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["a"], "a".repeat(40));
        assert_eq!(value["w"], "w".repeat(40));
        assert_eq!(value["p"], "p".repeat(40));
        assert_eq!(value["g"], "p".repeat(24));
        // The only un-capped field is the machine hostname; even the
        // synthetic maximum-length variant must stay under the server cap.
        assert!(body.chars().count() <= 240, "payload too long: {body}");
    }

    #[test]
    fn forward_argv_rewrites_a_forwarded_payload() {
        let body = r#"{"v":1,"a":"pi needs input","w":"herdr-cast","p":"herdr-cast","h":"cast-notify-fwd-0492f3","g":"w1:p1","s":"blocked"}"#.to_string();
        let arguments = vec![
            "-title".to_string(),
            "pi needs input".to_string(),
            "-message".to_string(),
            body,
            "-activate".to_string(),
            "com.mitchellh.ghostty".to_string(),
        ];

        let decision = forward_argv(&arguments);
        assert_eq!(
            decision.argv,
            vec![
                "-title",
                "herdr-cast@cast-notify-fwd-0492f3",
                "-subtitle",
                "pi needs input",
                "-group",
                "w1:p1",
                "-sound",
                "Glass",
                "-activate",
                "com.mitchellh.ghostty",
            ]
        );
        assert_eq!(decision.variant, Some("blocked"));
    }

    #[test]
    fn forward_argv_keeps_a_differing_workspace_in_the_subtitle() {
        let body =
            r#"{"v":1,"a":"pi done","w":"frank","p":"x","h":"donut","s":"done"}"#.to_string();
        let arguments = vec!["-message".to_string(), body];

        let decision = forward_argv(&arguments);
        assert_eq!(
            decision.argv,
            vec![
                "-title",
                "x@donut",
                "-subtitle",
                "pi done · frank",
                "-sound",
                "Funk",
            ]
        );
        assert_eq!(decision.variant, Some("done"));
    }

    #[test]
    fn forward_argv_supports_legacy_title_body_payloads() {
        let body = r#"{"v":1,"t":"pi needs input","b":"sbx · repo","g":"w1:p1","s":"blocked","h":"donut"}"#.to_string();
        let arguments = vec![
            "-title".to_string(),
            "pi needs input".to_string(),
            "-message".to_string(),
            body,
            "-activate".to_string(),
            "com.mitchellh.ghostty".to_string(),
        ];

        let decision = forward_argv(&arguments);
        assert_eq!(
            decision.argv,
            vec![
                "-title",
                "pi needs input",
                "-subtitle",
                "donut",
                "-body",
                "sbx · repo",
                "-group",
                "w1:p1",
                "-sound",
                "Glass",
                "-activate",
                "com.mitchellh.ghostty",
            ]
        );
        assert_eq!(decision.variant, Some("blocked"));
    }

    #[test]
    fn forward_argv_ignores_sounds_for_unknown_statuses() {
        let arguments = vec![
            "-message".to_string(),
            r#"{"v":1,"a":"still going","s":"working"}"#.to_string(),
        ];

        let decision = forward_argv(&arguments);
        assert_eq!(
            decision.argv,
            vec!["-title", "herdr", "-subtitle", "still going"]
        );
        assert_eq!(decision.variant, None);
    }

    #[test]
    fn forward_argv_passes_payloads_with_unknown_flags_through_verbatim() {
        let body = forwarded_body(
            "w1:p1",
            "blocked",
            &NotificationParts {
                action: "pi needs input",
                workspace: "herdr-cast",
                project: "herdr-cast",
                host: None,
            },
        );
        let arguments = vec![
            "-title".to_string(),
            "pi needs input".to_string(),
            "-message".to_string(),
            body,
            "-timeout".to_string(),
            "5".to_string(),
        ];
        let decision = forward_argv(&arguments);
        assert_eq!(decision.argv, arguments);
        assert_eq!(decision.variant, None);
    }

    #[test]
    fn forward_argv_passes_plain_notifications_through_verbatim() {
        for body in ["cast · herdr-cast", "\"{not json}", "{\"v\":2}"] {
            let arguments = vec![
                "-title".to_string(),
                "herdr".to_string(),
                "-message".to_string(),
                body.to_string(),
                "-activate".to_string(),
                "com.mitchellh.ghostty".to_string(),
                "-timeout".to_string(),
                "5".to_string(),
            ];
            let decision = forward_argv(&arguments);
            assert_eq!(decision.argv, arguments);
            assert_eq!(decision.variant, None);
        }
    }
}
