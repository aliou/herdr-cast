//! Typed access to Herdr plugin event payloads.
//!
//! Every event hook reads the JSON Herdr injects through
//! `HERDR_PLUGIN_EVENT_JSON`. Parsing lives here once so hooks cannot drift
//! on payload shape: a missing or malformed event fails soft to `None`
//! accessors instead of failing the hook.

use serde_json::Value;

/// The environment variable holding the event payload Herdr injects into
/// event hooks.
const EVENT_ENV: &str = "HERDR_PLUGIN_EVENT_JSON";

/// The pointer paths a workspace id can appear at across Herdr event shapes.
/// Workspace events have carried more than one shape, so every path is tried.
const WORKSPACE_ID_POINTERS: [&str; 3] = [
    "/data/workspace_id",
    "/data/workspace/workspace_id",
    "/workspace_id",
];

pub struct PluginEvent {
    value: Value,
}

impl PluginEvent {
    /// The event payload from the hook environment, or `None` when unset or
    /// malformed.
    pub fn from_environment() -> Option<Self> {
        std::env::var(EVENT_ENV)
            .ok()
            .and_then(|json| Self::from_json(&json))
    }

    /// Parse an event payload, or `None` when it is not valid JSON.
    pub fn from_json(json: &str) -> Option<Self> {
        serde_json::from_str(json).ok().map(|value| Self { value })
    }

    /// The pane the event names.
    pub fn pane_id(&self) -> Option<String> {
        self.string("/data/pane_id")
    }

    /// The workspace the event names, across the shapes Herdr emits.
    pub fn workspace_id(&self) -> Option<String> {
        WORKSPACE_ID_POINTERS
            .iter()
            .find_map(|pointer| self.string(pointer))
    }

    /// The agent status the event reports.
    pub fn agent_status(&self) -> Option<String> {
        self.string("/data/agent_status")
    }

    /// The agent the event names.
    pub fn agent(&self) -> Option<String> {
        self.string("/data/agent")
    }

    fn string(&self, pointer: &str) -> Option<String> {
        self.value
            .pointer(pointer)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .filter(|value| !value.trim().is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_pane_events() {
        let event = PluginEvent::from_json(r#"{"data":{"pane_id":"w1:p2"}}"#).unwrap();
        assert_eq!(event.pane_id().as_deref(), Some("w1:p2"));
        assert_eq!(event.workspace_id(), None);
        assert_eq!(event.agent_status(), None);
    }

    #[test]
    fn reads_agent_status_events() {
        let event = PluginEvent::from_json(
            r#"{"data":{"pane_id":"w1:p2","agent_status":"done","agent":"pi"}}"#,
        )
        .unwrap();
        assert_eq!(event.pane_id().as_deref(), Some("w1:p2"));
        assert_eq!(event.agent_status().as_deref(), Some("done"));
        assert_eq!(event.agent().as_deref(), Some("pi"));
    }

    #[test]
    fn workspace_ids_parse_from_every_shape_herdr_emits() {
        for json in [
            r#"{"data":{"workspace_id":"w:2/a b"}}"#,
            r#"{"data":{"workspace":{"workspace_id":"w:2/a b"}}}"#,
            r#"{"workspace_id":"w:2/a b"}"#,
        ] {
            let event = PluginEvent::from_json(json).unwrap();
            assert_eq!(
                event.workspace_id().as_deref(),
                Some("w:2/a b"),
                "json: {json}"
            );
        }
    }

    #[test]
    fn blank_values_read_as_absent() {
        let event =
            PluginEvent::from_json(r#"{"data":{"pane_id":"  ","workspace_id":""}}"#).unwrap();
        assert_eq!(event.pane_id(), None);
        assert_eq!(event.workspace_id(), None);
    }

    #[test]
    fn non_string_values_read_as_absent() {
        let event = PluginEvent::from_json(r#"{"data":{"pane_id":7}}"#).unwrap();
        assert_eq!(event.pane_id(), None);
    }

    #[test]
    fn malformed_events_fail_soft() {
        assert!(PluginEvent::from_json("not json").is_none());
        let event = PluginEvent::from_json("[]").unwrap();
        assert_eq!(event.pane_id(), None);
    }
}
