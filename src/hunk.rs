use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;

use crate::api::SocketClient;
use crate::lazygit;
use crate::popup_cli;

/// Hunk extension, embedded and written into the plugin state directory, that
/// dumps the review's user notes to `$HUNK_REVIEW_DUMP` on every note change
/// and once more at shutdown. See `hunk-review-dump.mjs` beside this file.
const DUMP_EXTENSION: &str = include_str!("hunk-review-dump.mjs");

/// State-dir paths for one review session's note dump.
struct DumpPaths {
    extension: PathBuf,
    notes: PathBuf,
}

/// Open hunk's working-tree review against the focused pane's repository, or,
/// when the pane is not inside a repository, fuzzy-pick one found below it,
/// exactly as the `lazygit` entrypoint does.
///
/// `diff` reviews the whole changeset, including untracked files, and
/// `--watch` reloads it while agents keep editing. File navigation comes from
/// the files pane hunk shows on its own at popup width (`,`/`.` step files,
/// `[`/`]` step hunks). Commits are not part of a review stream; they are
/// browsed in `hunk log` through `run_log`.
///
/// Review notes written with `c` are dumped to a temp file while the review
/// runs. When hunk exits, the dump path is copied to the clipboard and Herdr
/// is asked to toast about it, so the notes can be handed to an agent after
/// the popup is gone.
pub fn run() -> Result<(), String> {
    execute(review_command)
}

/// Open hunk's history browser against the focused pane's repository, with
/// the same repository resolution and note dump as `run`. Commit navigation
/// lives here because a review stream has no commits to move between: j/k
/// moves through history, enter reviews the selected commit (or a
/// `v`-selected range), and quitting returns to the log.
pub fn run_log() -> Result<(), String> {
    execute(log_command)
}

fn execute(build: fn(&Path, Option<&DumpPaths>) -> Command) -> Result<(), String> {
    let Some(repository) = resolve()? else {
        return Ok(());
    };
    let dump = prepare_dump();
    let result = popup_cli::run("hunk", build(&repository, dump.as_ref()));
    if let Some(dump) = dump {
        finalize_dump(&dump.notes);
    }
    result
}

fn resolve() -> Result<Option<PathBuf>, String> {
    let cwd = popup_cli::focused_pane_cwd()?;
    lazygit::resolve_repository(&cwd)
}

/// Hunk reads the repository from its working directory and has no repository
/// flag, so the review runs at the repository root.
fn review_command(repository: &Path, dump: Option<&DumpPaths>) -> Command {
    let mut command = Command::new("hunk");
    command.arg("diff").arg("--watch").current_dir(repository);
    attach_dump(&mut command, dump);
    command
}

fn log_command(repository: &Path, dump: Option<&DumpPaths>) -> Command {
    let mut command = Command::new("hunk");
    command.arg("log").current_dir(repository);
    attach_dump(&mut command, dump);
    command
}

/// Load the note-dump extension through `--extension`, which hunk runs
/// immediately without a trust prompt, and point it at the dump file.
fn attach_dump(command: &mut Command, dump: Option<&DumpPaths>) {
    if let Some(dump) = dump {
        command.arg("--extension").arg(&dump.extension);
        command.env("HUNK_REVIEW_DUMP", &dump.notes);
    }
}

/// Write the dump extension into the injected plugin state directory and pick
/// the dump file's name. The extension must live somewhere herdr-cast owns;
/// the dump itself goes to the system temp directory, so reviews do not
/// accumulate in plugin state. Without a state directory there is nowhere to
/// keep the extension, and the review runs without a dump.
fn prepare_dump() -> Option<DumpPaths> {
    let state = std::env::var_os("HERDR_PLUGIN_STATE_DIR").map(PathBuf::from)?;
    prepare_dump_in(&state)
}

fn prepare_dump_in(state: &Path) -> Option<DumpPaths> {
    fs::create_dir_all(state).ok()?;
    let extension = state.join("hunk-review-dump.mjs");
    fs::write(&extension, DUMP_EXTENSION).ok()?;
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis();
    Some(DumpPaths {
        extension,
        notes: std::env::temp_dir().join(format!("hunk-review-notes-{millis}.json")),
    })
}

/// Copy the dump path to the clipboard and ask Herdr to toast about it.
/// Everything here is best-effort: a missing clipboard program or socket must
/// never turn a completed review into an error popup.
fn finalize_dump(notes: &Path) {
    let Ok(contents) = fs::read_to_string(notes) else {
        return;
    };
    if contents.trim().is_empty() {
        return;
    }
    let count = dump_note_count(&contents);
    if count == Some(0) {
        return;
    }
    copy_path_to_clipboard(notes);
    notify_notes_saved(notes, count);
}

/// Number of notes in a `hunk session comment list --json` dump, or None when
/// the contents do not parse. The CLI wraps the list in a `comments` object.
fn dump_note_count(contents: &str) -> Option<usize> {
    let value: serde_json::Value = serde_json::from_str(contents).ok()?;
    Some(match value {
        serde_json::Value::Array(items) => items.len(),
        value => value
            .get("comments")
            .or_else(|| value.get("notes"))
            .and_then(Value::as_array)?
            .len(),
    })
}

#[cfg(target_os = "macos")]
fn clipboard_invocations() -> &'static [&'static [&'static str]] {
    &[&["pbcopy"]]
}

#[cfg(not(target_os = "macos"))]
fn clipboard_invocations() -> &'static [&'static [&'static str]] {
    &[&["wl-copy"], &["xclip", "-selection", "clipboard"]]
}

/// Copy the path through the first available clipboard program. The path
/// travels over stdin, so spaces and shell metacharacters need no quoting.
fn copy_path_to_clipboard(path: &Path) -> bool {
    for invocation in clipboard_invocations() {
        let Ok(mut child) = Command::new(invocation[0])
            .args(&invocation[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        // Drop stdin after writing so the clipboard program sees EOF and can
        // finish.
        let written = child
            .stdin
            .take()
            .and_then(|mut stdin| stdin.write_all(path.to_string_lossy().as_bytes()).ok());
        let succeeded = match written {
            Some(()) => child.wait().map(|status| status.success()).unwrap_or(false),
            None => {
                let _ = child.kill();
                false
            }
        };
        if succeeded {
            return true;
        }
        let _ = child.kill();
    }
    false
}

#[derive(Serialize)]
struct NotesSavedParams {
    title: String,
    body: Option<String>,
    position: Option<String>,
    sound: &'static str,
}

/// Ask Herdr to toast that the review notes were dumped. Sound stays "none":
/// the plugin keeps every sound out of Herdr socket requests.
fn notify_notes_saved(notes: &Path, count: Option<usize>) {
    let Some(socket_path) =
        std::env::var_os("HERDR_SOCKET_PATH").map(|value| value.to_string_lossy().into_owned())
    else {
        return;
    };
    let body = match count {
        Some(1) => format!(
            "1 note saved; path copied to clipboard: {}",
            notes.display()
        ),
        Some(count) => format!(
            "{count} notes saved; path copied to clipboard: {}",
            notes.display()
        ),
        None => format!("notes saved; path copied to clipboard: {}", notes.display()),
    };
    let client = SocketClient::new(socket_path);
    let _ = client.send(
        "cast:hunk-notes-saved",
        "notification.show",
        NotesSavedParams {
            title: "review notes saved".to_string(),
            body: Some(body),
            position: None,
            sound: "none",
        },
    );
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;

    #[test]
    fn review_reviews_the_working_tree_with_watch() {
        let command = review_command(Path::new("/repo"), None);
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            vec!["diff", "--watch"]
        );
        assert_eq!(command.get_current_dir(), Some(Path::new("/repo")));
        assert_eq!(command.get_envs().count(), 0);
    }

    #[test]
    fn review_loads_the_dump_extension_when_a_dump_is_prepared() {
        let dump = DumpPaths {
            extension: PathBuf::from("/state/hunk-review-dump.mjs"),
            notes: PathBuf::from("/state/hunk-review-notes-1.json"),
        };
        let command = review_command(Path::new("/repo"), Some(&dump));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            vec![
                "diff",
                "--watch",
                "--extension",
                "/state/hunk-review-dump.mjs"
            ]
        );
        assert_eq!(
            command.get_envs().collect::<Vec<_>>(),
            vec![(
                OsString::from("HUNK_REVIEW_DUMP").as_os_str(),
                Some(OsString::from("/state/hunk-review-notes-1.json")).as_deref()
            )]
        );
        assert_eq!(command.get_current_dir(), Some(Path::new("/repo")));
    }

    #[test]
    fn log_opens_the_history_browser() {
        let command = log_command(Path::new("/repo"), None);
        assert_eq!(command.get_args().collect::<Vec<_>>(), vec!["log"]);
        assert_eq!(command.get_current_dir(), Some(Path::new("/repo")));
    }

    #[test]
    fn dump_count_counts_array_and_object_shapes() {
        assert_eq!(dump_note_count("[]"), Some(0));
        assert_eq!(dump_note_count(r#"[{"noteId":"user:1"}]"#), Some(1));
        assert_eq!(dump_note_count(r#"{"notes":[{},{}]}"#), Some(2));
        assert_eq!(dump_note_count(r#"{"comments":[]}"#), Some(0));
        assert_eq!(
            dump_note_count(r#"{"comments":[{"noteId":"user:1"}]}"#),
            Some(1)
        );
        assert_eq!(dump_note_count(""), None);
        assert_eq!(dump_note_count("not json"), None);
    }

    #[test]
    fn dump_preparation_writes_the_extension_into_the_state_directory() {
        let state =
            std::env::temp_dir().join(format!("herdr-cast-hunk-test-{}", std::process::id()));
        let dump = prepare_dump_in(&state).expect("dump paths");
        assert_eq!(dump.extension.parent(), Some(state.as_path()));
        let written = fs::read_to_string(&dump.extension).expect("extension contents");
        assert!(written.contains("note_created"));
        assert!(written.contains("shutdown"));
        assert_eq!(dump.notes.parent(), Some(std::env::temp_dir().as_path()));
        assert!(dump
            .notes
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("hunk-review-notes-")));
        fs::remove_dir_all(&state).ok();
    }
}
