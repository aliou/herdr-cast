use std::process::Command;

use crate::popup_cli;

/// Open yazi in the focused pane's working directory inside a Herdr popup.
///
/// Herdr runs plugin commands with the plugin root as cwd, so yazi must be
/// launched with the focused pane's cwd explicitly: a bare `yazi` popup would
/// open in the plugin root instead of the directory the user is working in.
pub fn run() -> Result<(), String> {
    let cwd = popup_cli::focused_pane_cwd()?;
    let mut command = Command::new("yazi");
    command.current_dir(&cwd);
    popup_cli::run("yazi", command)
}
