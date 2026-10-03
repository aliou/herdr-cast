//! Pane focus-recency log. The `pane.focused` coordinator appends the focused
//! pane id to a move-to-front, bounded log in the plugin state directory; the
//! workspace picker reads the log to order its "panes" view
//! most-recent-first.
//!
//! The log only ever orders panes the current `pane.list` already returns, so
//! stale ids from closed panes or other sessions are filtered out at read time
//! and never name a pane that no longer exists.

use std::fs;
use std::path::{Path, PathBuf};

const PANE_RECENCY_FILE: &str = "pane-recency";
/// Bound the log so a long-running session cannot grow it without limit. Far
/// more than any realistic number of panes, small enough to read and rewrite
/// atomically on every focus.
const MAX_ENTRIES: usize = 256;

/// A bounded, persistent focus history. With no state directory, recording
/// is a no-op and loading returns an empty history.
pub struct RecencyLog {
    path: Option<PathBuf>,
}

impl RecencyLog {
    pub fn new(state_dir: Option<&Path>) -> Self {
        Self {
            path: state_dir.map(|directory| directory.join(PANE_RECENCY_FILE)),
        }
    }

    /// Record a pane at the front, removing duplicates and evicting the oldest
    /// entries. Persist atomically so readers never see a partial log.
    pub fn record(&self, pane_id: &str) -> Result<(), String> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let mut entries = read_log(path);
        entries.retain(|entry| entry != pane_id);
        entries.insert(0, pane_id.to_string());
        entries.truncate(MAX_ENTRIES);
        write_log(path, &entries)
    }

    /// Most recent first. The caller filters unknown ids against `pane.list`.
    /// An absent or unreadable file supplies an empty history.
    pub fn load(&self) -> Vec<String> {
        self.path.as_deref().map(read_log).unwrap_or_default()
    }
}

fn read_log(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .ok()
        .map(|contents| {
            contents
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn write_log(path: &Path, entries: &[String]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create pane recency state: {error}"))?;
    }
    let mut contents = String::new();
    for entry in entries {
        contents.push_str(entry);
        contents.push('\n');
    }
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&temporary, &contents)
        .map_err(|error| format!("failed to save pane recency: {error}"))?;
    fs::rename(&temporary, path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        format!("failed to activate pane recency: {error}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestDir;

    #[test]
    fn recording_persists_focus_order_without_duplicates() {
        let dir = TestDir::new();
        let log = RecencyLog::new(Some(dir.path()));
        for pane in ["a", "b", "c", "b", "b"] {
            log.record(pane).unwrap();
        }
        // A fresh reader sees the persisted history, not in-memory state.
        assert_eq!(
            RecencyLog::new(Some(dir.path())).load(),
            vec!["b", "c", "a"]
        );
    }

    #[test]
    fn recording_bounds_the_persisted_history() {
        let dir = TestDir::new();
        let log = RecencyLog::new(Some(dir.path()));
        for index in 0..=MAX_ENTRIES {
            log.record(&index.to_string()).unwrap();
        }
        let entries = log.load();
        assert_eq!(entries.len(), MAX_ENTRIES);
        assert_eq!(entries.first(), Some(&MAX_ENTRIES.to_string()));
        assert_eq!(entries.last(), Some(&"1".to_string()));
        assert!(!entries.contains(&"0".to_string()));
    }

    #[test]
    fn loading_tolerates_absent_files_blank_lines_and_whitespace() {
        let dir = TestDir::new();
        let log = RecencyLog::new(Some(dir.path()));
        assert!(log.load().is_empty());
        fs::write(dir.path().join(PANE_RECENCY_FILE), "  a  \n\nb\n\n").unwrap();
        assert_eq!(log.load(), vec!["a", "b"]);
    }

    #[test]
    fn recording_creates_the_state_directory() {
        let dir = TestDir::new();
        let state = dir.path().join("nested");
        let log = RecencyLog::new(Some(&state));
        log.record("p:new").unwrap();
        assert_eq!(log.load(), vec!["p:new"]);
    }

    #[test]
    fn without_a_state_directory_recording_is_a_noop() {
        let log = RecencyLog::new(None);
        log.record("p:new").unwrap();
        assert!(log.load().is_empty());
    }

    #[test]
    fn recording_reports_an_unwritable_state_directory() {
        let dir = TestDir::new();
        let state = dir.path().join("not-a-directory");
        fs::write(&state, "file").unwrap();
        let log = RecencyLog::new(Some(&state));
        assert!(log
            .record("p:new")
            .unwrap_err()
            .contains("failed to create pane recency state"));
    }
}
