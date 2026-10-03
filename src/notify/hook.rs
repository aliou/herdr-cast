//! The `notify` hook: one `pane.agent_status_changed` event in, one
//! notification out. Filters to the triggering statuses, enriches through
//! the Herdr socket, debounces per pane and status, then runs the delivery
//! chain: bridge first, then the local macOS notifier, then the server's
//! `notification.show`.

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
#[derive(Clone, Debug, PartialEq, Eq)]
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

/// Delivery varies across the real desktop/bridge and a recording test
/// adapter. Keep this seam private: callers submit events, not delivery steps.
trait Delivery {
    fn try_bridge(&mut self, pending: &Pending) -> bool;
    fn try_local(&mut self, pending: &Pending) -> Result<bool, String>;
    fn terminal(&mut self, pending: &Pending);
}

struct ProductionDelivery<'a> {
    paths: &'a Paths,
    socket_path: Option<&'a str>,
    client: Option<&'a SocketClient>,
    host: String,
    session: Option<String>,
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
    let mut delivery = ProductionDelivery {
        paths: &paths,
        socket_path: socket_path.as_deref(),
        client: client.as_ref(),
        host: origin_host(),
        session: std::env::var("HERDR_SESSION")
            .ok()
            .filter(|session| !session.is_empty()),
    };
    handle_event(&paths.state, &event, client.as_ref(), &mut delivery)
}

/// Process one event through the whole notification policy. The state
/// directory must exist. Enrichment is best-effort; delivery stops at the
/// first accepting path. Local adapter errors retain the hook's error mode.
fn handle_event(
    state_dir: &Path,
    event: &crate::events::PluginEvent,
    client: Option<&SocketClient>,
    delivery: &mut impl Delivery,
) -> Result<(), String> {
    let Some(pending) = prepare(state_dir, event, client)? else {
        return Ok(());
    };
    if delivery.try_bridge(&pending) {
        return Ok(());
    }
    if delivery.try_local(&pending)? {
        return Ok(());
    }
    delivery.terminal(&pending);
    Ok(())
}

/// Filter, enrich, and debounce one agent status event. `None` means the
/// event does not notify.
fn prepare(
    state_dir: &Path,
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

    if is_debounced(state_dir, &pane_id, &status)? {
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

impl Delivery for ProductionDelivery<'_> {
    fn try_bridge(&mut self, pending: &Pending) -> bool {
        match bridge::send_notification(bridge::wire::Notification {
            id: 0,
            host: self.host.clone(),
            pane: pending.pane_id.clone(),
            status: pending.status.clone(),
            action: pending.action.clone(),
            workspace: pending.workspace.clone(),
            project: pending.project.clone(),
            socket: self.socket_path.unwrap_or_default().to_string(),
            session: self.session.clone(),
        }) {
            Ok(()) => true,
            // The usual case on the host Mac itself; not worth a log line.
            Err(bridge::Unavailable::NoSocket) => false,
            Err(error) => {
                log(&format!("bridge: {error}; using notification.show"));
                false
            }
        }
    }

    fn try_local(&mut self, pending: &Pending) -> Result<bool, String> {
        if !cfg!(target_os = "macos") {
            return Ok(false);
        }
        // Only this path carries the pane-focus command on a click. The
        // terminal forwarder can only activate Ghostty.
        let parts = pending.parts();
        let (title, subtitle) = compose(&parts);
        deliver_local(
            self.paths,
            self.socket_path,
            &pending.pane_id,
            &pending.status,
            title,
            subtitle,
        )
    }

    fn terminal(&mut self, pending: &Pending) {
        request_terminal_notification(
            self.client,
            &pending.pane_id,
            &pending.status,
            &pending.parts(),
        );
    }
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
/// Returns whether the server reported it shown to an attached client shell.
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
    use crate::events::PluginEvent;
    use crate::test_support::{SocketFixture, TestDir};

    #[derive(Default)]
    struct RecordingDelivery {
        bridge_accepts: bool,
        local_accepts: bool,
        local_error: Option<String>,
        attempts: Vec<(&'static str, Pending)>,
    }

    impl Delivery for RecordingDelivery {
        fn try_bridge(&mut self, pending: &Pending) -> bool {
            self.attempts.push(("bridge", pending.clone()));
            self.bridge_accepts
        }

        fn try_local(&mut self, pending: &Pending) -> Result<bool, String> {
            self.attempts.push(("local", pending.clone()));
            match &self.local_error {
                Some(error) => Err(error.clone()),
                None => Ok(self.local_accepts),
            }
        }

        fn terminal(&mut self, pending: &Pending) {
            self.attempts.push(("terminal", pending.clone()));
        }
    }

    fn event(pane: &str, status: &str) -> PluginEvent {
        PluginEvent::from_json(
            &json!({
                "type": "pane.agent_status_changed",
                "data": {
                    "type": "pane_agent_status_changed",
                    "pane_id": pane,
                    "workspace_id": "w:event",
                    "agent_status": status,
                    "agent": "pi"
                }
            })
            .to_string(),
        )
        .unwrap()
    }

    #[test]
    fn triggering_events_keep_their_identity_when_enrichment_is_unavailable() {
        let dir = TestDir::new();
        let mut delivery = RecordingDelivery {
            bridge_accepts: true,
            ..Default::default()
        };
        handle_event(
            dir.path(),
            &event("p:background", "done"),
            None,
            &mut delivery,
        )
        .unwrap();
        assert_eq!(
            delivery.attempts,
            vec![(
                "bridge",
                Pending {
                    pane_id: "p:background".into(),
                    status: "done".into(),
                    action: "pi done".into(),
                    workspace: "w:event".into(),
                    project: String::new(),
                }
            )]
        );
    }

    #[test]
    fn irrelevant_or_incomplete_events_do_not_attempt_delivery() {
        let dir = TestDir::new();
        let mut delivery = RecordingDelivery::default();
        for payload in [
            json!({ "data": { "pane_id": "p:1", "agent_status": "working" } }),
            json!({ "data": { "agent_status": "done" } }),
            json!({ "data": { "pane_id": "p:1" } }),
        ] {
            let event = PluginEvent::from_json(&payload.to_string()).unwrap();
            handle_event(dir.path(), &event, None, &mut delivery).unwrap();
        }
        assert!(delivery.attempts.is_empty());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn debounce_is_scoped_to_both_pane_and_status() {
        let dir = TestDir::new();
        let mut delivery = RecordingDelivery {
            bridge_accepts: true,
            ..Default::default()
        };
        for (pane, status) in [
            ("p:1", "done"),
            ("p:1", "done"),
            ("p:1", "blocked"),
            ("p:2", "done"),
        ] {
            handle_event(dir.path(), &event(pane, status), None, &mut delivery).unwrap();
        }
        let delivered: Vec<_> = delivery
            .attempts
            .iter()
            .map(|(_, pending)| (pending.pane_id.as_str(), pending.status.as_str()))
            .collect();
        assert_eq!(
            delivered,
            vec![("p:1", "done"), ("p:1", "blocked"), ("p:2", "done")]
        );
    }

    #[test]
    fn delivery_stops_at_the_first_accepting_path() {
        for (bridge_accepts, local_accepts, expected) in [
            (true, false, vec!["bridge"]),
            (false, true, vec!["bridge", "local"]),
            (false, false, vec!["bridge", "local", "terminal"]),
        ] {
            let dir = TestDir::new();
            let mut delivery = RecordingDelivery {
                bridge_accepts,
                local_accepts,
                ..Default::default()
            };
            handle_event(dir.path(), &event("p:1", "blocked"), None, &mut delivery).unwrap();
            let attempts: Vec<_> = delivery.attempts.iter().map(|(path, _)| *path).collect();
            assert_eq!(attempts, expected);
            assert!(delivery
                .attempts
                .iter()
                .all(|(_, pending)| pending.action == "pi needs input"));
        }
    }

    #[test]
    fn local_adapter_errors_keep_the_hooks_error_mode() {
        let dir = TestDir::new();
        let mut delivery = RecordingDelivery {
            local_error: Some("cannot locate executable".into()),
            ..Default::default()
        };
        let result = handle_event(dir.path(), &event("p:1", "done"), None, &mut delivery);
        assert_eq!(result, Err("cannot locate executable".into()));
        let attempts: Vec<_> = delivery.attempts.iter().map(|(path, _)| *path).collect();
        assert_eq!(attempts, vec!["bridge", "local"]);
    }

    #[test]
    fn enrichment_uses_the_event_pane_and_workspace_not_the_focused_pane() {
        let dir = TestDir::new();
        let server = SocketFixture::new(vec![
            json!({ "result": { "pane": {
                "pane_id": "p:background", "workspace_id": "w:other",
                "agent_status": "working", "agent": "other", "focused": false,
                "cwd": "/code/herdr-cast"
            } } }),
            json!({ "result": { "workspace": { "label": "cast" } } }),
        ]);
        let mut delivery = RecordingDelivery {
            bridge_accepts: true,
            ..Default::default()
        };
        handle_event(
            dir.path(),
            &event("p:background", "done"),
            Some(&server.client()),
            &mut delivery,
        )
        .unwrap();
        let pending = &delivery.attempts[0].1;
        assert_eq!(pending.pane_id, "p:background");
        assert_eq!(pending.status, "done");
        assert_eq!(pending.action, "pi done");
        assert_eq!(pending.workspace, "cast");
        assert_eq!(pending.project, "herdr-cast");
        let requests = server.finish();
        assert_eq!(requests[0]["method"], "pane.get");
        assert_eq!(requests[0]["params"], json!({ "pane_id": "p:background" }));
        assert_eq!(requests[1]["method"], "workspace.get");
        assert_eq!(requests[1]["params"], json!({ "workspace_id": "w:event" }));
    }

    #[test]
    fn missing_event_fields_are_enriched_without_repeating_the_pane_query() {
        let dir = TestDir::new();
        let server = SocketFixture::new(vec![
            json!({ "result": { "pane": {
                "workspace_id": "w:from-pane", "agent_status": "blocked", "agent": "codex"
            } } }),
            json!({ "error": { "code": "not_found", "message": "workspace gone" } }),
        ]);
        let event = PluginEvent::from_json(r#"{"data":{"pane_id":"p:1"}}"#).unwrap();
        let mut delivery = RecordingDelivery {
            bridge_accepts: true,
            ..Default::default()
        };
        handle_event(dir.path(), &event, Some(&server.client()), &mut delivery).unwrap();
        let pending = &delivery.attempts[0].1;
        assert_eq!(pending.action, "codex needs input");
        assert_eq!(pending.workspace, "w:from-pane");
        assert_eq!(server.finish().len(), 2);
    }

    #[test]
    fn failed_enrichment_does_not_drop_a_trigger() {
        let dir = TestDir::new();
        let server = SocketFixture::new(vec![
            json!({ "error": { "code": "not_found", "message": "pane gone" } }),
            json!({ "error": { "code": "not_found", "message": "workspace gone" } }),
        ]);
        let mut delivery = RecordingDelivery {
            bridge_accepts: true,
            ..Default::default()
        };
        handle_event(
            dir.path(),
            &event("p:1", "done"),
            Some(&server.client()),
            &mut delivery,
        )
        .unwrap();
        assert_eq!(delivery.attempts[0].1.action, "pi done");
        assert_eq!(delivery.attempts[0].1.workspace, "w:event");
        assert_eq!(server.finish().len(), 2);
    }

    #[test]
    fn terminal_adapter_sends_a_silent_forwarded_payload() {
        let dir = TestDir::new();
        let paths = Paths {
            state: dir.path().into(),
            assets: dir.path().join("unused"),
        };
        let server = SocketFixture::new(vec![json!({ "result": {
            "type": "notification_show", "shown": true, "reason": "shown"
        } })]);
        let client = server.client();
        let mut delivery = ProductionDelivery {
            paths: &paths,
            socket_path: None,
            client: Some(&client),
            host: "test-host".into(),
            session: None,
        };
        // Never call the production bridge or local methods in a test.
        delivery.terminal(&Pending {
            pane_id: "p:1".into(),
            status: "done".into(),
            action: "pi done".into(),
            workspace: "cast".into(),
            project: "herdr-cast".into(),
        });
        let requests = server.finish();
        let request = &requests[0];
        assert_eq!(request["method"], "notification.show");
        assert_eq!(request["params"]["sound"], "none");
        assert_eq!(request["params"]["title"], "pi done");
        assert_eq!(request["params"]["position"], Value::Null);
        let body: Value =
            serde_json::from_str(request["params"]["body"].as_str().unwrap()).unwrap();
        assert_eq!(body["a"], "pi done");
        assert_eq!(body["w"], "cast");
        assert_eq!(body["p"], "herdr-cast");
        assert_eq!(body["s"], "done");
    }
}
