use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use serde_json::Value;

use crate::api::SocketClient;
use crate::palette::focused_pane_id;
use crate::picker::{pick_streaming, Choice, Picker};
use crate::space;

/// How many directory levels below the current directory to search for git
/// repositories when the current directory is not one itself.
const MAX_DEPTH: u32 = 3;

const SOCKET_TIMEOUT: Duration = Duration::from_secs(3);

/// Open lazygit against the current directory's repository, or, when the
/// current directory is not inside a repository, fuzzy-pick one from the
/// repositories found up to `MAX_DEPTH` levels below it.
pub fn run() -> Result<(), String> {
    let pane_id = focused_pane_id().ok_or_else(|| "focused pane not available".to_string())?;
    let socket =
        std::env::var("HERDR_SOCKET_PATH").map_err(|_| "HERDR_SOCKET_PATH not set".to_string())?;
    let client = SocketClient::with_timeout(socket, SOCKET_TIMEOUT);
    let cwd = pane_cwd(&client, &pane_id)?;

    let repository = match space::repository_root(&cwd) {
        Some(root) => root,
        None => {
            // Open the picker immediately and stream repositories into it as
            // the scan below finds them, rather than blocking the popup on a
            // full scan first. A directory tree large or slow enough to
            // matter (a network mount, a huge monorepo) would otherwise leave
            // the popup looking hung.
            let (sender, receiver) = mpsc::channel();
            let scan_root = cwd.clone();
            // Cancelled once the picker returns, so an unfinished scan of a
            // large or slow tree does not keep walking the filesystem for
            // the whole interactive lazygit session that follows.
            let cancelled = Arc::new(AtomicBool::new(false));
            let scan_cancelled = Arc::clone(&cancelled);
            std::thread::spawn(move || {
                scan_repositories(&scan_root, MAX_DEPTH, &scan_cancelled, &mut |path| {
                    let _ = sender.send(repository_choice(&scan_root, path));
                });
            });
            let selection = pick_streaming(
                Picker {
                    placeholder: "Search repositories",
                    empty_message: "No git repository found nearby",
                    order: None,
                },
                receiver,
            );
            cancelled.store(true, Ordering::Relaxed);
            match selection? {
                Some(path) => path,
                None => return Ok(()),
            }
        }
    };

    let status = Command::new("lazygit")
        .arg("-p")
        .arg(&repository)
        .status()
        .map_err(|error| format!("failed to launch lazygit: {error}"))?;
    if !status.success() {
        return Err(format!("lazygit exited with {status}"));
    }
    Ok(())
}

/// The focused pane's own directory, preferring `cwd` (the shell's own
/// directory) over `foreground_cwd`, matching `PaneInfo::working_directory`.
fn pane_cwd(client: &SocketClient, pane_id: &str) -> Result<PathBuf, String> {
    let response = client.send(
        "cast:pane-get",
        "pane.get",
        serde_json::json!({ "pane_id": pane_id }),
    )?;
    extract_cwd(&response).ok_or_else(|| "pane.get missing cwd".to_string())
}

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

/// Walks 1 to `max_depth` directory levels below `directory`, calling
/// `on_found` with each directory holding a `.git` entry as soon as it is
/// found. Stops descending into a directory once it is identified as a
/// repository, so nested/vendored repositories do not surface separately.
/// Checks `cancelled` between entries so a caller that has moved on can stop
/// an unfinished scan instead of leaving it to walk the rest of the tree.
fn scan_repositories(
    directory: &Path,
    max_depth: u32,
    cancelled: &AtomicBool,
    on_found: &mut impl FnMut(PathBuf),
) {
    if cancelled.load(Ordering::Relaxed) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        if cancelled.load(Ordering::Relaxed) {
            return;
        }
        let path = entry.path();
        let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
        if !is_dir {
            continue;
        }
        let hidden = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| name.starts_with('.'))
            .unwrap_or(true);
        if hidden {
            continue;
        }
        if path.join(".git").exists() {
            on_found(path);
            continue;
        }
        if max_depth > 1 {
            scan_repositories(&path, max_depth - 1, cancelled, on_found);
        }
    }
}

#[cfg(test)]
fn find_repositories(root: &Path, max_depth: u32) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let cancelled = AtomicBool::new(false);
    scan_repositories(root, max_depth, &cancelled, &mut |path| found.push(path));
    found.sort();
    found
}

fn repository_choice(cwd: &Path, path: PathBuf) -> Choice<PathBuf> {
    let relative = path
        .strip_prefix(cwd)
        .unwrap_or(&path)
        .to_string_lossy()
        .into_owned();
    let search = path.to_string_lossy().into_owned();
    Choice::new(path.clone(), relative, None::<String>, search)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn init_repo(path: &Path) {
        fs::create_dir_all(path).unwrap();
        fs::create_dir_all(path.join(".git")).unwrap();
    }

    #[test]
    fn finds_repositories_up_to_three_levels_below() {
        let root = std::env::temp_dir().join(format!("cast-lazygit-scan-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        init_repo(&root.join("one"));
        init_repo(&root.join("a/two"));
        init_repo(&root.join("a/b/three"));
        // Four levels below root: must not surface.
        init_repo(&root.join("a/b/c/four"));
        // Hidden directories are skipped entirely.
        init_repo(&root.join(".hidden/repo"));

        let mut found = find_repositories(&root, 3);
        found.sort();
        let mut expected = vec![root.join("one"), root.join("a/two"), root.join("a/b/three")];
        expected.sort();
        assert_eq!(found, expected);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn does_not_descend_into_a_found_repository() {
        let root = std::env::temp_dir().join(format!("cast-lazygit-nested-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        init_repo(&root.join("outer"));
        init_repo(&root.join("outer/vendor/nested"));

        let found = find_repositories(&root, 3);
        assert_eq!(found, vec![root.join("outer")]);

        let _ = fs::remove_dir_all(&root);
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
