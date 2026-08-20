mod api;
mod lazygit;
mod notify;
mod palette;
mod picker;
mod popup;
mod recency;
mod space;
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
    let result = match arguments.next().as_deref() {
        Some("record-focus") if arguments.next().is_none() => recency::record_focus(),
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
        Some("sync-title") if arguments.next().is_none() => space::sync_title(),
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
            "usage: herdr-cast <record-focus|notify|forward-notify|palette|directory-workspace",
            "|workspace-picker|lazygit|sync-space|sync-title|sync-spaces|shell-init|open-popup|focus>"
        )
        .to_string()),
    };

    if let Err(error) = result {
        eprintln!("[cast] {error}");
        std::process::exit(1);
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
