//! Local macOS delivery through the bundled `terminal-notifier` bundles:
//! registration with Launch Services, the notifier child runner, and the
//! outstanding-notification state that lets a later focus or pane close
//! remove what was delivered.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use super::compose::{bundle_variant, sound_for_status};
use super::paths::Paths;
use super::{
    hex_key, log, shell_quote, stable_hash, truncate, unix_seconds, DirectoryLock, TRIGGER_STATUSES,
};

const REGISTER_TTL_SECONDS: u64 = 6 * 60 * 60;
const OUTSTANDING_NOTIFICATION_PREFIX: &str = "outstanding-notification";
const LSREGISTER: &str = "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister";

/// A notifier child that has not exited by then is killed. Kept below the
/// bridge relay's ack timeout so a slow notifier reads as a notifier
/// failure rather than a lost host.
const NOTIFIER_TIMEOUT: Duration = Duration::from_secs(3);

/// Run the bundled notifier and wait for it. Stdin is closed because
/// terminal-notifier reads its message from a non-terminal stdin and would
/// otherwise block; a child still running after `NOTIFIER_TIMEOUT` is
/// killed.
pub(crate) fn run_notifier(notifier: &Path, argv: &[String]) -> Result<(), String> {
    let mut child = Command::new(notifier)
        .args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("failed to start notifier: {error}"))?;
    let status = wait_with_timeout(&mut child, NOTIFIER_TIMEOUT)
        .ok_or_else(|| format!("notifier did not exit within {NOTIFIER_TIMEOUT:?}"))?;
    if status.success() {
        return Ok(());
    }
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        use std::io::Read;
        let _ = pipe.read_to_string(&mut stderr);
    }
    Err(format!(
        "notifier failed with {status}: {}",
        truncate(&stderr.replace('\n', " "), 500)
    ))
}

/// Wait for `child` up to `timeout`; kill and reap it past that.
pub(crate) fn wait_with_timeout(
    child: &mut std::process::Child,
    timeout: Duration,
) -> Option<std::process::ExitStatus> {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    None
}

pub(crate) fn ensure_notifier_registered(paths: &Paths, variant: Option<&str>) -> bool {
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

/// Deliver one notification locally on macOS: pick the status bundle, group
/// per pane, run the notifier, and record the outstanding marker so a later
/// focus or close can remove the notification. Returns whether the notifier
/// ran: a `false` lets the caller fall back to `notification.show` instead
/// of silently missing the notification.
pub(crate) fn deliver_local(
    paths: &Paths,
    socket_path: Option<&str>,
    pane_id: &str,
    status: &str,
    title: String,
    subtitle: Option<String>,
) -> Result<bool, String> {
    let variant = bundle_variant(status);
    let notifier = paths.notifier(variant);
    if !notifier.is_file() {
        log(&format!(
            "bundled notifier executable is missing (expected {})",
            notifier.display()
        ));
        return Ok(false);
    }
    if !ensure_notifier_registered(paths, variant) {
        return Ok(false);
    }
    let group = local_notification_group(socket_path, pane_id);
    let Some(_lifecycle_lock) = notification_lifecycle_lock(&paths.state, &group, status) else {
        return Ok(false);
    };
    let mut args = vec!["-title".to_string(), title];
    if let Some(subtitle) = subtitle {
        args.push("-subtitle".to_string());
        args.push(subtitle);
    }
    args.push("-group".to_string());
    args.push(group.clone());
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

    match run_notifier(&notifier, &args) {
        Ok(()) => mark_notification_outstanding(&paths.state, &group, status),
        Err(error) => {
            log(&error);
            return Ok(false);
        }
    }

    Ok(true)
}

/// Remove local macOS notifications previously delivered for `pane_id`.
/// Delivery uses one bundle identity per triggering status, so each
/// outstanding bundle must remove its own pane group.
pub(crate) fn clear_delivered_for_pane(pane_id: &str) {
    if !cfg!(target_os = "macos") {
        return;
    }
    let paths = match Paths::from_environment() {
        Ok(paths) => paths,
        Err(error) => {
            log(&format!(
                "cannot locate notifier while clearing pane {pane_id}: {error}"
            ));
            return;
        }
    };
    let socket_path = std::env::var("HERDR_SOCKET_PATH").ok();
    let group = local_notification_group(socket_path.as_deref(), pane_id);
    clear_delivered_with_paths(&paths, &group);
}

pub(crate) fn clear_delivered_with_paths(paths: &Paths, group: &str) {
    for status in TRIGGER_STATUSES {
        let Some(_lifecycle_lock) = notification_lifecycle_lock(&paths.state, group, status) else {
            continue;
        };
        let marker = outstanding_notification_path(&paths.state, group, status);
        if !marker.is_file() {
            continue;
        }

        let notifier = paths.notifier(bundle_variant(status));
        if !notifier.is_file() {
            log(&format!(
                "cannot clear {status} notification; notifier is missing (expected {})",
                notifier.display()
            ));
            continue;
        }

        match Command::new(&notifier).args(["-remove", group]).output() {
            Ok(output) if output.status.success() => {
                if let Err(error) = fs::remove_file(&marker) {
                    log(&format!(
                        "cleared {status} notification but failed to remove its state marker: {error}"
                    ));
                }
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr).replace('\n', " ");
                log(&format!(
                    "failed to clear {status} notification with {}: {}",
                    output.status,
                    truncate(&stderr, 500)
                ));
            }
            Err(error) => log(&format!(
                "failed to start notifier while clearing {status} notification: {error}"
            )),
        }
    }
}

fn local_notification_group(socket_path: Option<&str>, pane_id: &str) -> String {
    let session = socket_path
        .map(str::to_owned)
        .or_else(|| std::env::var("HERDR_SESSION").ok())
        .unwrap_or_else(|| "default".to_string());
    format!("cast-local-{:016x}-{pane_id}", stable_hash(&session))
}

pub(crate) fn mark_notification_outstanding(state_dir: &Path, group: &str, status: &str) {
    let marker = outstanding_notification_path(state_dir, group, status);
    if let Err(error) = fs::write(&marker, []) {
        log(&format!(
            "notification was delivered but its clear state could not be saved: {error}"
        ));
    }
}

fn outstanding_notification_path(state_dir: &Path, group: &str, status: &str) -> PathBuf {
    state_dir.join(format!(
        "{OUTSTANDING_NOTIFICATION_PREFIX}-{:016x}-{}",
        stable_hash(group),
        hex_key(status)
    ))
}

fn notification_lifecycle_lock(
    state_dir: &Path,
    group: &str,
    status: &str,
) -> Option<DirectoryLock> {
    let path = state_dir.join(format!(
        ".notification-lifecycle-{:016x}-{}.lock",
        stable_hash(group),
        hex_key(status)
    ));
    match DirectoryLock::acquire(path, Duration::from_secs(30), 500) {
        Ok(Some(lock)) => Some(lock),
        Ok(None) => {
            log("timed out waiting for notification lifecycle lock");
            None
        }
        Err(error) => {
            log(&error);
            None
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[cfg(unix)]
    fn notification_test_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "cast-notification-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    #[cfg(unix)]
    fn deliver_local_reports_not_delivered_when_the_notifier_is_missing() {
        let root = notification_test_root("missing-notifier");
        let paths = Paths {
            state: root.join("state"),
            assets: root.join("assets"),
        };

        let delivered = deliver_local(
            &paths,
            Some("/tmp/herdr-session.sock"),
            "w1:p1",
            "blocked",
            "title".to_string(),
            None,
        )
        .unwrap();

        assert!(!delivered);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    #[cfg(unix)]
    fn clears_each_outstanding_status_from_its_own_bundle() {
        let root = notification_test_root("clear");
        let state = root.join("state");
        let assets = root.join("assets");
        fs::create_dir_all(&state).unwrap();
        let group = local_notification_group(Some("/tmp/herdr-session.sock"), "w1:p1");
        for status in TRIGGER_STATUSES {
            let notifier = Paths {
                state: state.clone(),
                assets: assets.clone(),
            }
            .notifier(bundle_variant(status));
            fs::create_dir_all(notifier.parent().unwrap()).unwrap();
            let log_path = root.join(format!("{status}.args"));
            fs::write(
                &notifier,
                format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n",
                    log_path.display()
                ),
            )
            .unwrap();
            fs::set_permissions(&notifier, fs::Permissions::from_mode(0o755)).unwrap();
            mark_notification_outstanding(&state, &group, status);
        }

        let paths = Paths { state, assets };
        clear_delivered_with_paths(&paths, &group);

        for status in TRIGGER_STATUSES {
            assert_eq!(
                fs::read_to_string(root.join(format!("{status}.args"))).unwrap(),
                format!("-remove\n{group}\n")
            );
            assert!(!outstanding_notification_path(&paths.state, &group, status).exists());
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    #[cfg(unix)]
    fn keeps_outstanding_state_when_the_notifier_cannot_clear() {
        let root = notification_test_root("clear-failure");
        let state = root.join("state");
        let assets = root.join("assets");
        fs::create_dir_all(&state).unwrap();
        let paths = Paths { state, assets };
        let notifier = paths.notifier(Some("blocked"));
        fs::create_dir_all(notifier.parent().unwrap()).unwrap();
        fs::write(&notifier, "#!/bin/sh\nexit 1\n").unwrap();
        fs::set_permissions(&notifier, fs::Permissions::from_mode(0o755)).unwrap();
        let group = local_notification_group(Some("/tmp/herdr-session.sock"), "pane-1");
        mark_notification_outstanding(&paths.state, &group, "blocked");

        clear_delivered_with_paths(&paths, &group);

        assert!(outstanding_notification_path(&paths.state, &group, "blocked").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn local_notification_groups_are_stable_and_session_scoped() {
        assert_eq!(
            local_notification_group(Some("/tmp/a.sock"), "w1:p1"),
            local_notification_group(Some("/tmp/a.sock"), "w1:p1")
        );
        assert_ne!(
            local_notification_group(Some("/tmp/a.sock"), "w1:p1"),
            local_notification_group(Some("/tmp/b.sock"), "w1:p1")
        );
        assert!(local_notification_group(Some("/tmp/a.sock"), "w1:p1").starts_with("cast-local-"));
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
