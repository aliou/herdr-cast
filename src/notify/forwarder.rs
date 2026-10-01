//! The `forward-notify` shim: the `terminal-notifier` binary the Nix package
//! installs so Herdr's macOS client renders forwarded remote payloads
//! through the status's HerdrNotify bundle while passing every other
//! notification through the neutral bundle verbatim.

use std::fs;
use std::process::Command;

use serde::Deserialize;

use super::compose::{bundle_variant, notifier_argv, sound_for_status, NotificationParts};
use super::local::ensure_notifier_registered;
use super::paths::Paths;
use super::truncate;

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
/// next to herdr-cast. Renders the notification through the bundled
/// HerdrNotify.app. When the body carries a forwarded payload from a
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
        let argv = notifier_argv(&parts, payload.g, payload.s, parsed.activate);
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

/// Encode the remote-delivery payload consumed by the local HerdrNotify
/// forwarder. Herdr's client protocol carries only title and body strings,
/// so the layout parts, grouping key, and sound policy ride inside a
/// compact JSON body. Field caps keep the worst case under the server's
/// 240-character body cap (checked by tests) so the JSON cannot be sliced:
/// a client without the forwarder would otherwise show a truncated payload.
pub(crate) fn forwarded_body(pane_id: &str, status: &str, parts: &NotificationParts) -> String {
    let body = serde_json::to_string(&serde_json::json!({
        "v": 1,
        "a": truncate(parts.action, 40),
        "w": truncate(parts.workspace, 40),
        "p": truncate(parts.project, 40),
        "h": truncate(&super::hook::origin_host(), 32),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parts<'a>(action: &'a str, workspace: &'a str, project: &'a str) -> NotificationParts<'a> {
        NotificationParts {
            action,
            workspace,
            project,
            host: None,
        }
    }

    #[test]
    fn forwarded_body_encodes_forwarder_payload() {
        let body = forwarded_body(
            "pane-1",
            "blocked",
            &parts("pi needs input", "herdr-cast", "herdr-cast"),
        );
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
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
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
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
            &parts("pi needs input", "herdr-cast", "herdr-cast"),
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
