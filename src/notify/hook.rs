//! The `notify` hook: one `pane.agent_status_changed` event in, one
//! notification out. Filters to the triggering statuses, enriches through
//! the Herdr socket, debounces per pane and status, then runs the delivery
//! chain: bridge first, then the server's `notification.show`, then the
//! local macOS notifier.

use std::path::Path;
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

use super::compose::{compose, NotificationParts};
use super::forwarder::forwarded_body;
use super::local::{clear_delivered_for_pane, deliver_local};
use super::paths::Paths;
use super::{log, DirectoryLock, TRIGGER_STATUSES};
use crate::api::SocketClient;
use crate::bridge;

const DEBOUNCE_SECONDS: u64 = 2;

/// The osascript-independent notification flags Herdr's macOS client hands
/// to the forwarder (see `forwarder::forward`).
#[derive(Serialize)]
struct NotificationShowParams {
    title: String,
    body: Option<String>,
    position: Option<String>,
    sound: &'static str,
}

/// A triggered notification, enriched and past the debounce, ready for
/// delivery.
struct Pending {
    pane_id: String,
    status: String,
    action: String,
    workspace: String,
    project: String,
}

impl Pending {
    fn parts(&self) -> NotificationParts<'_> {
        NotificationParts {
            action: &self.action,
            workspace: &self.workspace,
            project: &self.project,
            host: None,
        }
    }
}

pub fn run() -> Result<(), String> {
    let paths = Paths::from_environment()?;
    std::fs::create_dir_all(&paths.state)
        .map_err(|error| format!("failed to create plugin state directory: {error}"))?;

    let Some(event) = crate::events::PluginEvent::from_environment() else {
        log("dropped event without a parsable payload");
        return Ok(());
    };
    let socket_path = std::env::var("HERDR_SOCKET_PATH").ok();
    let client = socket_path
        .as_deref()
        .map(|path| SocketClient::with_timeout(path, Duration::from_millis(250)));
    let Some(pending) = prepare(&paths, &event, client.as_ref())? else {
        return Ok(());
    };
    deliver(&paths, socket_path.as_deref(), client.as_ref(), &pending)
}

/// Filter, enrich, and debounce one agent status event. `None` means the
/// event does not notify.
fn prepare(
    paths: &Paths,
    event: &crate::events::PluginEvent,
    client: Option<&SocketClient>,
) -> Result<Option<Pending>, String> {
    let Some(pane_id) = event.pane_id() else {
        log("dropped event without data.pane_id");
        return Ok(None);
    };
    let mut pane = Value::Null;
    let status = event.agent_status().or_else(|| {
        pane = pane_info(client, &pane_id);
        string_at(&pane, "/result/pane/agent_status")
    });
    let Some(status) = status else {
        log("dropped event without an agent status");
        return Ok(None);
    };
    if !TRIGGER_STATUSES.contains(&status.as_str()) {
        return Ok(None);
    }
    if pane.is_null() {
        pane = pane_info(client, &pane_id);
    }

    let workspace_id = event
        .workspace_id()
        .or_else(|| string_at(&pane, "/result/pane/workspace_id"));
    let agent = event
        .agent()
        .or_else(|| string_at(&pane, "/result/pane/agent"))
        .unwrap_or_else(|| "agent".to_string());
    let cwd = string_at(&pane, "/result/pane/cwd");
    let workspace = workspace_id
        .as_deref()
        .and_then(|id| workspace_label(client, id))
        .or_else(|| workspace_id.clone())
        .unwrap_or_default();
    let project = cwd
        .as_deref()
        .and_then(|path| Path::new(path).file_name())
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_string();

    if is_debounced(&paths.state, &pane_id, &status)? {
        return Ok(None);
    }
    // The title/subtitle carry no status glyphs: the per-status bundle icon
    // and sound are the status signal.
    let action = match status.as_str() {
        "blocked" => format!("{agent} needs input"),
        "done" => format!("{agent} done"),
        _ => return Ok(None),
    };
    Ok(Some(Pending {
        pane_id,
        status,
        action,
        workspace,
        project,
    }))
}

/// Try each delivery path in order until one takes the notification:
///
/// 1. the bridge to the host Mac, when a relay socket exists on this
///    machine;
/// 2. the server's `notification.show`, which an attached client renders
///    (through the forwarder on macOS clients);
/// 3. on macOS, the local notifier, for the desktop app itself or a
///    headless server nobody is attached to.
fn deliver(
    paths: &Paths,
    socket_path: Option<&str>,
    client: Option<&SocketClient>,
    pending: &Pending,
) -> Result<(), String> {
    match bridge::send_notification(bridge::wire::Notification {
        id: 0,
        host: origin_host(),
        pane: pending.pane_id.clone(),
        status: pending.status.clone(),
        action: pending.action.clone(),
        workspace: pending.workspace.clone(),
        project: pending.project.clone(),
        socket: socket_path.unwrap_or_default().to_string(),
        session: std::env::var("HERDR_SESSION")
            .ok()
            .filter(|session| !session.is_empty()),
    }) {
        Ok(()) => return Ok(()),
        // The usual case on the host Mac itself; not worth a log line.
        Err(bridge::Unavailable::NoSocket) => {}
        Err(error) => log(&format!("bridge: {error}; using notification.show")),
    }
    let parts = pending.parts();
    if request_terminal_notification(client, &pending.pane_id, &pending.status, &parts) {
        return Ok(());
    }
    if !cfg!(target_os = "macos") {
        return Ok(());
    }
    let (title, subtitle) = compose(&parts);
    deliver_local(
        paths,
        socket_path,
        &pending.pane_id,
        &pending.status,
        title,
        subtitle,
    )
}

/// Hook entrypoint for lifecycle events such as `pane.closed` that should
/// discard a pane's no-longer-actionable notification.
pub fn clear_from_event() -> Result<(), String> {
    let Some(event) = crate::events::PluginEvent::from_environment() else {
        log("dropped notification-clear event without a parsable payload");
        return Ok(());
    };
    let Some(pane_id) = event.pane_id() else {
        log("dropped notification-clear event without data.pane_id");
        return Ok(());
    };
    clear_delivered_for_pane(&pane_id);
    Ok(())
}

/// Request a terminal notification through the server's `notification.show`.
/// Returns whether the server reported it shown to an attached client shell;
/// callers on platforms with a local notifier use that to skip a duplicate.
fn request_terminal_notification(
    client: Option<&SocketClient>,
    pane_id: &str,
    status: &str,
    parts: &NotificationParts,
) -> bool {
    let Some(client) = client else {
        log("HERDR_SOCKET_PATH is not set; cannot request terminal notification");
        return false;
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
            shown
        }
        Err(error) => {
            log(&format!("failed to request terminal notification: {error}"));
            false
        }
    }
}

/// Origin hostname for the payload's subtitle marker. Sandbox VMs expose
/// their sandbox name as the hostname, which is the identifier that names
/// the session the notification came from; on other hosts this is simply
/// the machine name. Falls back to `remote` when the hostname cannot be
/// read so forwarded notifications never silently lose the origin marker.
pub(crate) fn origin_host() -> String {
    crate::daemon::hostname().unwrap_or_else(|| "remote".to_string())
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
    let key = format!("{}-{}", super::hex_key(pane_id), super::hex_key(status));
    let path = state_dir.join(format!("debounce-{key}"));
    let lock_path = state_dir.join(format!("debounce-{key}.lock"));
    let Some(_lock) = DirectoryLock::acquire(lock_path, Duration::from_secs(DEBOUNCE_SECONDS), 20)?
    else {
        return Ok(false);
    };
    let now = super::unix_seconds();
    let previous = std::fs::read_to_string(&path)
        .ok()
        .and_then(|contents| contents.trim().parse::<u64>().ok());
    if previous.is_some_and(|timestamp| now.saturating_sub(timestamp) < DEBOUNCE_SECONDS) {
        return Ok(true);
    }
    std::fs::write(&path, format!("{now}\n"))
        .map_err(|error| format!("failed to write debounce state: {error}"))?;
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
