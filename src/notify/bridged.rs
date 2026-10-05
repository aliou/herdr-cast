//! Delivering notifications that arrived through the bridge from a remote
//! machine. The host applies them through the bundled notifier; a click
//! runs `bridge-focus`, which asks the host daemon to send a Focus frame
//! back down the link and raises the matching Ghostty tab.

use std::path::PathBuf;

use super::compose::{bundle_variant, notifier_argv};
use super::local::{
    clear_delivered_with_paths, ensure_notifier_registered, mark_notification_outstanding,
    run_notifier,
};
use super::paths::Paths;
use super::{log, shell_quote};
use crate::bridge;

/// The app a bridged notification raises when clicked.
const ACTIVATE_BUNDLE_ID: &str = "com.mitchellh.ghostty";

/// Where a bridged notification's click goes: the host daemon's control
/// socket and the link that carried the notification.
pub(crate) struct BridgeClick {
    pub(crate) control: PathBuf,
    pub(crate) key: String,
}

/// Deliver a notification that arrived through the bridge from another
/// machine. Grouping is qualified by the origin host because pane ids repeat
/// across machines.
pub(crate) fn deliver_bridged(
    notification: &bridge::wire::Notification,
    click: &BridgeClick,
) -> Result<(), String> {
    let paths = Paths::for_client()?;
    deliver_bridged_with_paths(&paths, notification, click)
}

fn deliver_bridged_with_paths(
    paths: &Paths,
    notification: &bridge::wire::Notification,
    click: &BridgeClick,
) -> Result<(), String> {
    let variant = bundle_variant(&notification.status);
    let notifier = paths.notifier(variant);
    if !notifier.is_file() {
        return Err(format!(
            "bundled notifier executable is missing (expected {})",
            notifier.display()
        ));
    }
    // Registration is kept current by the host daemon's loop
    // (`prepare_bridged_registration`): signing and Launch Services can take
    // longer than the relay waits for an ack.
    let host =
        crate::daemon::short_host(&notification.host).unwrap_or_else(|| notification.host.clone());
    let parts = super::compose::NotificationParts {
        action: &notification.action,
        workspace: &notification.workspace,
        project: &notification.project,
        host: Some(&host),
    };
    let group = bridged_group(&host, &notification.pane);
    // Older senders do not report their Herdr socket; their click can only
    // raise Ghostty.
    let argv = if notification.socket.is_empty() {
        notifier_argv(
            &parts,
            Some(&group),
            Some(&notification.status),
            Some(ACTIVATE_BUNDLE_ID),
        )
    } else {
        let executable = std::env::current_exe()
            .map_err(|error| format!("failed to locate herdr-cast executable: {error}"))?;
        let mut argv = notifier_argv(&parts, Some(&group), Some(&notification.status), None);
        argv.push("-execute".to_string());
        argv.push(bridge_click_command(&executable, click, notification));
        argv
    };
    run_notifier(&notifier, &argv)?;
    // Record the delivery so a Dismiss from the remote — the pane was
    // focused or closed there — removes exactly this group.
    mark_notification_outstanding(&paths.state, &group, &notification.status);
    Ok(())
}

/// The group bridged delivery used for a remote pane's notifications.
fn bridged_group(host: &str, pane: &str) -> String {
    format!("{host}:{pane}")
}

/// Remove what bridged delivery posted for a pane, after the pane handled
/// it on the remote. Markers gate the removal: a pane that never notified
/// costs no notifier spawn. Best-effort like local clearing: a notifier
/// that cannot remove keeps its marker.
pub(crate) fn dismiss_bridged(host: &str, pane: &str) -> Result<(), String> {
    if !cfg!(target_os = "macos") {
        return Ok(());
    }
    let paths = Paths::for_client()?;
    dismiss_bridged_with_paths(&paths, host, pane);
    Ok(())
}

fn dismiss_bridged_with_paths(paths: &Paths, host: &str, pane: &str) {
    let host = crate::daemon::short_host(host).unwrap_or_else(|| host.to_string());
    clear_delivered_with_paths(paths, &bridged_group(&host, pane));
}

fn bridge_click_command(
    executable: &std::path::Path,
    click: &BridgeClick,
    notification: &bridge::wire::Notification,
) -> String {
    [
        executable.to_string_lossy().as_ref(),
        "bridge-focus",
        click.control.to_string_lossy().as_ref(),
        &click.key,
        notification.session.as_deref().unwrap_or(""),
        &notification.socket,
        &notification.pane,
    ]
    .into_iter()
    .map(shell_quote)
    .collect::<Vec<_>>()
    .join(" ")
}

/// Keep every notifier identity registered so bridged notifications never
/// pay for signing and Launch Services registration inside the relay's ack
/// window. Cheap while the registration TTL holds. Best-effort.
pub(crate) fn prepare_bridged_delivery() {
    ensure_notifier_registered_all();
}

fn ensure_notifier_registered_all() {
    let paths = match Paths::for_client() {
        Ok(paths) => paths,
        Err(error) => {
            log(&error);
            return;
        }
    };
    if let Err(error) = std::fs::create_dir_all(&paths.state) {
        log(&format!(
            "failed to create notifier state directory: {error}"
        ));
        return;
    }
    for variant in [None, Some("blocked"), Some("done")] {
        if paths.notifier(variant).is_file() {
            ensure_notifier_registered(&paths, variant);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[test]
    #[cfg(unix)]
    fn bridged_delivery_is_marked_and_dismissed_by_group() {
        let root = std::env::temp_dir().join(format!(
            "cast-bridged-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let paths = Paths {
            state: root.join("state"),
            assets: root.join("assets"),
        };
        std::fs::create_dir_all(&paths.state).unwrap();
        let notifier = paths.notifier(Some("blocked"));
        std::fs::create_dir_all(notifier.parent().unwrap()).unwrap();
        let log_path = root.join("args");
        std::fs::write(
            &notifier,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\n",
                log_path.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&notifier, PermissionsExt::from_mode(0o755)).unwrap();

        let notification = bridge::wire::Notification {
            id: 1,
            host: "donut.ts.net".into(),
            pane: "w1:p9".into(),
            status: "blocked".into(),
            action: "pi needs input".into(),
            workspace: "cast".into(),
            project: "herdr-cast".into(),
            socket: String::new(),
            session: None,
        };
        let click = BridgeClick {
            control: root.join("bridge-control.sock"),
            key: "me@donut:22".into(),
        };
        deliver_bridged_with_paths(&paths, &notification, &click).unwrap();
        let delivered = std::fs::read_to_string(&log_path).unwrap();
        assert!(
            delivered.contains("-group\ndonut:w1:p9\n"),
            "delivery groups by short host and pane: {delivered}"
        );

        // A pane that never notified costs no notifier spawn.
        dismiss_bridged_with_paths(&paths, "donut.ts.net", "w1:elsewhere");
        assert!(!std::fs::read_to_string(&log_path)
            .unwrap()
            .contains("-remove"));

        dismiss_bridged_with_paths(&paths, "donut.ts.net", "w1:p9");
        let cleared = std::fs::read_to_string(&log_path).unwrap();
        assert!(
            cleared.contains("-remove\ndonut:w1:p9\n"),
            "dismiss removes the delivered group: {cleared}"
        );

        // The marker is gone: a repeat dismiss does not remove again.
        dismiss_bridged_with_paths(&paths, "donut.ts.net", "w1:p9");
        assert_eq!(
            std::fs::read_to_string(&log_path)
                .unwrap()
                .matches("-remove")
                .count(),
            1
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn quotes_every_bridge_click_argument_for_the_shell() {
        let notification = bridge::wire::Notification {
            id: 1,
            host: "donut".into(),
            pane: "w1:p1;echo bad".into(),
            status: "done".into(),
            action: "pi done".into(),
            workspace: String::new(),
            project: String::new(),
            socket: "/home/me/.config/herdr/it's.sock".into(),
            session: None,
        };
        let click = BridgeClick {
            control: PathBuf::from("/tmp/state dir/bridge-control.sock"),
            key: "me@donut:22".into(),
        };
        assert_eq!(
            bridge_click_command(
                std::path::Path::new("/bin/herdr-cast"),
                &click,
                &notification
            ),
            "'/bin/herdr-cast' 'bridge-focus' '/tmp/state dir/bridge-control.sock' \
             'me@donut:22' '' '/home/me/.config/herdr/it'\\''s.sock' 'w1:p1;echo bad'"
        );
    }
}
