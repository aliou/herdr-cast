use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use serde_json::Value;

use crate::api::SocketClient;
use crate::palette::focused_pane_id;

/// Socket round-trips inside a popup entrypoint only resolve the focused
/// pane's directory, so a short timeout is enough and keeps the popup from
/// hanging when the server is slow to respond.
const SOCKET_TIMEOUT: Duration = Duration::from_secs(3);

/// Runs a user-facing CLI inside a Herdr popup pane.
///
/// Stdin, stdout, and stderr are inherited, so a TUI such as lazygit or yazi
/// renders directly in the popup and keeps its keyboard input. The caller is
/// an allowlisted popup entrypoint, so a non-zero exit becomes a uniform
/// `<program> exited with <status>` error that the top-level dispatch renders
/// as the bold-red `[cast] ...` line and then waits for a keypress before the
/// popup closes.
///
/// Use this only for CLIs whose output belongs in the popup. For CLIs whose
/// stdout must be parsed or whose output should never reach the pane (zoxide,
/// codesign, Launch Services), keep using `Command::output()` directly.
pub fn run(program: &str, mut command: Command) -> Result<(), String> {
    let status = command
        .status()
        .map_err(|error| format!("failed to launch {program}: {error}"))?;
    if !status.success() {
        return Err(format!("{program} exited with {status}"));
    }
    Ok(())
}

/// The focused pane's working directory, resolved through the Herdr socket.
///
/// Herdr runs plugin commands with the plugin root as cwd, so a popup CLI such
/// as lazygit or yazi must set its own cwd to this path to open where the user
/// expects. Used only inside popup entrypoints where the focused pane id and
/// socket are injected.
pub fn focused_pane_cwd() -> Result<PathBuf, String> {
    let pane_id = focused_pane_id().ok_or_else(|| "focused pane not available".to_string())?;
    let socket =
        std::env::var("HERDR_SOCKET_PATH").map_err(|_| "HERDR_SOCKET_PATH not set".to_string())?;
    let client = SocketClient::with_timeout(socket, SOCKET_TIMEOUT);
    let response = client.send(
        "cast:pane-get",
        "pane.get",
        serde_json::json!({ "pane_id": pane_id }),
    )?;
    extract_cwd(&response).ok_or_else(|| "pane.get missing cwd".to_string())
}

/// The focused pane's own directory, preferring `cwd` (the shell's own
/// directory) over `foreground_cwd`, matching `PaneInfo::working_directory`.
fn extract_cwd(response: &Value) -> Option<PathBuf> {
    response
        .pointer("/result/pane/cwd")
        .and_then(Value::as_str)
        .or_else(|| {
            response
                .pointer("/result/pane/foreground_cwd")
                .and_then(Value::as_str)
        })
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ok_on_zero_exit() {
        run("true", Command::new("true")).unwrap();
    }

    #[test]
    fn reports_nonzero_exit() {
        let error = run("false", Command::new("false")).unwrap_err();
        assert!(
            error.starts_with("false exited with "),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn reports_launch_failure_for_missing_program() {
        let error = run(
            "herdr-cast-no-such-binary",
            Command::new("herdr-cast-no-such-binary"),
        )
        .unwrap_err();
        assert!(
            error.starts_with("failed to launch herdr-cast-no-such-binary: "),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn extract_cwd_prefers_cwd_over_foreground_cwd() {
        let response = serde_json::json!({
            "result": {
                "pane": {
                    "cwd": "/repo",
                    "foreground_cwd": "/repo/subdir"
                }
            }
        });
        assert_eq!(extract_cwd(&response), Some(PathBuf::from("/repo")));
    }

    #[test]
    fn extract_cwd_falls_back_to_foreground_cwd() {
        let response = serde_json::json!({
            "result": {
                "pane": {
                    "foreground_cwd": "/repo/subdir"
                }
            }
        });
        assert_eq!(extract_cwd(&response), Some(PathBuf::from("/repo/subdir")));
    }

    #[test]
    fn extract_cwd_missing_is_none() {
        let response = serde_json::json!({ "result": { "pane": {} } });
        assert_eq!(extract_cwd(&response), None);
    }
}
