mod api;
mod events;
mod lazygit;
mod notify;
mod palette;
mod picker;
mod popup;
mod popup_cli;
mod recency;
mod space;
mod title;
mod workspace;
mod zoxide;

#[cfg(test)]
mod test_support {
    use std::sync::Mutex;

    /// Serializes tests that mutate process-global environment variables
    /// (`HERDR_PLUGIN_STATE_DIR`, `HERDR_PLUGIN_EVENT_JSON`). Without this,
    /// parallel cargo tests race on the shared env and intermittently fail.
    pub static ENV_MUTEX: Mutex<()> = Mutex::new(());
}

fn main() {
    let mut arguments = std::env::args().skip(1);
    let command = arguments.next();
    let result = match command.as_deref() {
        Some("pane-focused") if arguments.next().is_none() => pane_focused_hook(),
        Some("daemon") if arguments.next().is_none() => title::daemon(),
        Some("clear-notification") if arguments.next().is_none() => notify::clear_from_event(),
        Some("notify") if arguments.next().is_none() => notify::run(),
        Some("forward-notify") => notify::forward(arguments.collect()),
        Some("palette") if arguments.next().is_none() => palette::run(),
        Some("directory-workspace") if arguments.next().is_none() => {
            workspace::create_from_directory()
        }
        Some("workspace-picker") if arguments.next().is_none() => workspace::focus_existing(),
        Some("lazygit") if arguments.next().is_none() => lazygit::run(),
        Some("sync-space") => match (arguments.next().as_deref(), arguments.next()) {
            (None, _) => space::sync(false),
            (Some("--await-remote"), None) => space::sync(true),
            _ => Err("usage: herdr-cast sync-space [--await-remote]".to_string()),
        },
        Some("sync-title") if arguments.next().is_none() => title::sync_title(),
        Some("sync-spaces") if arguments.next().is_none() => space::sync_all(),
        Some("shell-init") => match (arguments.next(), arguments.next()) {
            (Some(shell), None) => space::shell_init(&shell),
            _ => Err("usage: herdr-cast shell-init zsh".to_string()),
        },
        Some("open-popup") => {
            let arguments: Vec<String> = arguments.collect();
            parse_open_popup_args(&arguments)
                .and_then(|(entrypoint, spec)| popup::run(&entrypoint, spec))
        }
        Some("focus") => {
            let socket_path = arguments.next();
            let pane_id = arguments.next();
            match (socket_path, pane_id, arguments.next()) {
                (Some(socket_path), Some(pane_id), None) => notify::focus(&socket_path, &pane_id),
                _ => Err("usage: herdr-cast focus <socket-path> <pane-id>".to_string()),
            }
        }
        _ => Err(concat!(
            "usage: herdr-cast <pane-focused|clear-notification|notify|forward-notify|daemon|palette|directory-workspace",
            "|workspace-picker|lazygit|sync-space|sync-title|sync-spaces|shell-init|open-popup|focus>"
        )
        .to_string()),
    };

    if let Err(error) = result {
        use std::io::{IsTerminal, Write};
        let colorize = std::io::stderr().is_terminal();
        let line = format_error_line(&error, colorize);
        let stderr = std::io::stderr();
        let mut stderr = stderr.lock();
        let _ = stderr.write_all(line.as_bytes());
        let _ = stderr.flush();
        if holds_popup_error(command.as_deref().unwrap_or("")) {
            wait_for_keypress();
        }
        std::process::exit(1);
    }
}

/// The `pane.focused` coordinator. Each feature that reacts to a focus
/// change runs independently; one failing must not suppress the others.
fn pane_focused_hook() -> Result<(), String> {
    let mut failures = Vec::new();
    if let Some(pane_id) = events::PluginEvent::from_environment()
        .as_ref()
        .and_then(events::PluginEvent::pane_id)
    {
        if let Err(error) = recency::record(&pane_id) {
            failures.push(error);
        }
        notify::clear_delivered_for_pane(&pane_id);
    } else {
        eprintln!("[cast] dropped pane.focused event without data.pane_id");
    }
    if let Err(error) = title::sync_title() {
        failures.push(error);
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

/// Renders the `[cast] <error>` line, colored bold-red when `colorize` is true.
/// Color is applied only when stderr is an interactive terminal so plugin logs
/// and other captured output never receive escape sequences.
fn format_error_line(error: &str, colorize: bool) -> String {
    if colorize {
        format!("\n\x1b[1;31m[cast] {error}\x1b[0m\n")
    } else {
        format!("\n[cast] {error}\n")
    }
}

/// Entry points that run inside a Herdr plugin popup and are interactive from
/// the user's point of view. Herdr closes a popup pane as soon as its command
/// exits, so without a pause a failure message would vanish before the user
/// could read it. Background hooks and non-interactive commands must never
/// wait, so this is an allowlist rather than a denylist.
fn holds_popup_error(command: &str) -> bool {
    matches!(
        command,
        "palette" | "directory-workspace" | "workspace-picker" | "lazygit"
    )
}

/// The whole prompt block shown before waiting for a keypress. Kept as a pure
/// helper so formatting is unit-testable without touching a terminal.
fn popup_wait_prompt() -> String {
    "\nPress any key to close this popup.\n".to_string()
}

/// Prints the wait prompt and blocks for a single keypress so the user can
/// read an error before the popup closes. Only blocks when stdin is a
/// terminal, so hooks, tests, and other non-interactive invocations can never
/// hang. Reads a single key via crossterm in raw mode (cooked mode would
/// require Enter); if raw mode cannot be enabled it falls back to reading a
/// line so the pause still happens. Every step is best-effort: a failure to
/// read must not swallow the error that triggered the wait.
fn wait_for_keypress() {
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        return;
    }
    let stderr = std::io::stderr();
    let mut stderr = stderr.lock();
    let _ = stderr.write_all(popup_wait_prompt().as_bytes());
    let _ = stderr.flush();

    if crossterm::terminal::enable_raw_mode().is_ok() {
        // A key the user held while the popup closed can surface as a pending
        // event; drain those, then wait for the first fresh key press. The
        // poll timeout bounds the wait so a wedged terminal cannot hang the
        // popup indefinitely.
        let timeout = std::time::Duration::from_secs(60);
        while crossterm::event::poll(timeout).unwrap_or(false) {
            if let Ok(crossterm::event::Event::Key(_)) = crossterm::event::read() {
                break;
            }
        }
        let _ = crossterm::terminal::disable_raw_mode();
    } else {
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holds_popup_error_for_interactive_entrypoints() {
        assert!(holds_popup_error("palette"));
        assert!(holds_popup_error("directory-workspace"));
        assert!(holds_popup_error("workspace-picker"));
        assert!(holds_popup_error("lazygit"));
    }

    #[test]
    fn does_not_hold_for_background_or_noninteractive_commands() {
        assert!(!holds_popup_error("notify"));
        assert!(!holds_popup_error("clear-notification"));
        assert!(!holds_popup_error("pane-focused"));
        assert!(!holds_popup_error("daemon"));
        assert!(!holds_popup_error("sync-space"));
        assert!(!holds_popup_error("sync-title"));
        assert!(!holds_popup_error("sync-spaces"));
        assert!(!holds_popup_error("shell-init"));
        assert!(!holds_popup_error("open-popup"));
        assert!(!holds_popup_error("focus"));
        assert!(!holds_popup_error("forward-notify"));
        assert!(!holds_popup_error(""));
    }

    #[test]
    fn format_error_line_plain_without_color() {
        assert_eq!(
            format_error_line("lazygit exited with exit status: 1", false),
            "\n[cast] lazygit exited with exit status: 1\n"
        );
    }

    #[test]
    fn format_error_line_bold_red_when_colorized() {
        let line = format_error_line("lazygit exited with exit status: 1", true);
        assert!(line.starts_with("\n\x1b[1;31m[cast] lazygit exited with exit status: 1"));
        assert!(line.ends_with("\x1b[0m\n"));
    }

    #[test]
    fn popup_wait_prompt_leads_with_blank_line() {
        assert_eq!(
            popup_wait_prompt(),
            "\nPress any key to close this popup.\n"
        );
    }
}

fn parse_open_popup_args(arguments: &[String]) -> Result<(String, popup::PopupSizeSpec), String> {
    const USAGE: &str = "usage: herdr-cast open-popup --entrypoint <id> --min-width <cells> --min-height <cells> --pct-width <0-100> --pct-height <0-100>";

    let mut entrypoint: Option<String> = None;
    let mut min_width: Option<u32> = None;
    let mut min_height: Option<u32> = None;
    let mut pct_width: Option<u32> = None;
    let mut pct_height: Option<u32> = None;

    let mut iterator = arguments.iter();
    while let Some(flag) = iterator.next() {
        let mut value = || {
            iterator
                .next()
                .ok_or_else(|| format!("missing value for {flag}\n{USAGE}"))
        };
        match flag.as_str() {
            "--entrypoint" => entrypoint = Some(value()?.clone()),
            "--min-width" => {
                min_width = Some(
                    value()?
                        .parse()
                        .map_err(|_| format!("invalid --min-width\n{USAGE}"))?,
                )
            }
            "--min-height" => {
                min_height = Some(
                    value()?
                        .parse()
                        .map_err(|_| format!("invalid --min-height\n{USAGE}"))?,
                )
            }
            "--pct-width" => {
                pct_width = Some(
                    value()?
                        .parse()
                        .map_err(|_| format!("invalid --pct-width\n{USAGE}"))?,
                )
            }
            "--pct-height" => {
                pct_height = Some(
                    value()?
                        .parse()
                        .map_err(|_| format!("invalid --pct-height\n{USAGE}"))?,
                )
            }
            _ => return Err(format!("unknown flag {flag}\n{USAGE}")),
        }
    }

    let entrypoint = entrypoint.ok_or_else(|| format!("missing --entrypoint\n{USAGE}"))?;
    let min_width = min_width.ok_or_else(|| format!("missing --min-width\n{USAGE}"))?;
    let min_height = min_height.ok_or_else(|| format!("missing --min-height\n{USAGE}"))?;
    let pct_width = pct_width.ok_or_else(|| format!("missing --pct-width\n{USAGE}"))?;
    let pct_height = pct_height.ok_or_else(|| format!("missing --pct-height\n{USAGE}"))?;

    Ok((
        entrypoint,
        popup::PopupSizeSpec {
            min_width,
            min_height,
            pct_width,
            pct_height,
        },
    ))
}
