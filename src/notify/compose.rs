//! The shared two-line notification layout and the per-status identity
//! selection. Both local delivery and the forwarder render through
//! `compose`, so the two can never drift apart.

pub(crate) const BLOCKED_SOUND: &str = "Glass";
pub(crate) const DONE_SOUND: &str = "Funk";

/// Which identity bundle renders a notification. macOS draws the left-side
/// notification icon from the sender bundle's registered icon and offers no
/// per-notification override, so each status ships as its own bundle with
/// its own composited icon (`HerdrNotify-blocked.app`, ...). Anything
/// without a status-specific bundle uses the neutral HerdrNotify.app.
pub(crate) fn bundle_variant(status: &str) -> Option<&'static str> {
    match status {
        "blocked" => Some("blocked"),
        "done" => Some("done"),
        _ => None,
    }
}

pub(crate) fn sound_for_status(status: &str) -> Option<&'static str> {
    match status {
        "blocked" => Some(BLOCKED_SOUND),
        "done" => Some(DONE_SOUND),
        _ => None,
    }
}

/// The pieces of the shared two-line notification layout. Line 1 (title)
/// names what the agent works on; line 2 (subtitle) states what happened,
/// qualified by the workspace label when it adds anything. `host` is set
/// only for remote (forwarded) notifications.
pub(crate) struct NotificationParts<'a> {
    pub(crate) action: &'a str,
    pub(crate) workspace: &'a str,
    pub(crate) project: &'a str,
    pub(crate) host: Option<&'a str>,
}

/// Render `parts` as (title, subtitle). Title is the project name, falling
/// back to the workspace label then `herdr`, suffixed with `@host` for
/// forwarded notifications. Subtitle is the action, extended with
/// `· workspace` only when the label differs from what the title already
/// shows. No body line exists: status is carried by the icon and sound.
pub(crate) fn compose(parts: &NotificationParts) -> (String, Option<String>) {
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

/// The bundled notifier's arguments for the shared two-line layout, plus
/// optional grouping, the status sound, and the app a click activates.
pub(crate) fn notifier_argv(
    parts: &NotificationParts,
    group: Option<&str>,
    status: Option<&str>,
    activate: Option<&str>,
) -> Vec<String> {
    let (title, subtitle) = compose(parts);
    let mut argv = vec!["-title".to_string(), title];
    if let Some(subtitle) = subtitle {
        argv.extend(["-subtitle".to_string(), subtitle]);
    }
    if let Some(group) = group {
        argv.extend(["-group".to_string(), group.to_string()]);
    }
    if let Some(sound) = status.and_then(sound_for_status) {
        argv.extend(["-sound".to_string(), sound.to_string()]);
    }
    if let Some(activate) = activate {
        argv.extend(["-activate".to_string(), activate.to_string()]);
    }
    argv
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
