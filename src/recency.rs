//! Pane focus-recency log. The `pane.focused` event hook appends the focused
//! pane id to a move-to-front, bounded log in the plugin state directory; the
//! workspace picker reads it to order its "panes" view most-recent-first.
//!
//! The log only ever orders panes the current `pane.list` already returns, so
//! stale ids from closed panes or other sessions are filtered out at read time
//! and never name a pane that no longer exists.

use std::fs;
use std::path::PathBuf;

use serde::Deserialize;

const PANE_RECENCY_FILE: &str = "pane-recency";
/// Bound the log so a long-running session cannot grow it without limit. Far
/// more than any realistic number of panes, small enough to read and rewrite
/// atomically on every focus.
const MAX_ENTRIES: usize = 256;

#[derive(Debug, Default, Deserialize)]
struct EventEnvelope {
    #[serde(default)]
    data: EventData,
}

#[derive(Debug, Default, Deserialize)]
struct EventData {
    pane_id: Option<String>,
}

/// Hook entrypoint: read the focused pane id from the event payload and record
/// it at the front of the log.
pub fn record_focus() -> Result<(), String> {
    let Some(path) = recency_file() else {
        return Ok(());
    };
    let Some(pane_id) = event_pane_id() else {
        log("dropped pane.focused event without data.pane_id");
        return Ok(());
    };
    let mut entries = read_log(&path);
    move_to_front(&mut entries, &pane_id);
    write_log(&path, &entries)
}

/// Read the recency log as an ordered list of pane ids, most recent first.
/// Unknown ids are harmless here; the caller filters them against `pane.list`.
pub fn load() -> Vec<String> {
    let Some(path) = recency_file() else {
        return Vec::new();
    };
    read_log(&path)
}

fn recency_file() -> Option<PathBuf> {
    std::env::var_os("HERDR_PLUGIN_STATE_DIR")
        .map(PathBuf::from)
        .map(|directory| directory.join(PANE_RECENCY_FILE))
}

fn event_pane_id() -> Option<String> {
    let event = std::env::var("HERDR_PLUGIN_EVENT_JSON").ok()?;
    let envelope: EventEnvelope = serde_json::from_str(&event).ok()?;
    envelope
        .data
        .pane_id
        .filter(|pane_id| !pane_id.trim().is_empty())
}

fn read_log(path: &PathBuf) -> Vec<String> {
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

/// Move `pane_id` to the front, dropping any earlier copy so each id appears at
/// most once. Bound the log to [`MAX_ENTRIES`].
fn move_to_front(entries: &mut Vec<String>, pane_id: &str) {
    entries.retain(|entry| entry != pane_id);
    entries.insert(0, pane_id.to_string());
    if entries.len() > MAX_ENTRIES {
        entries.truncate(MAX_ENTRIES);
    }
}

fn write_log(path: &PathBuf, entries: &[String]) -> Result<(), String> {
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

fn log(message: &str) {
    eprintln!("[cast] {message}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn move_to_front_promotes_an_existing_id_and_drops_the_duplicate() {
        let mut entries = vec!["a".into(), "b".into(), "c".into()];
        move_to_front(&mut entries, "b");
        assert_eq!(entries, vec!["b", "a", "c"]);
    }

    #[test]
    fn move_to_front_inserts_a_new_id_at_the_front() {
        let mut entries = vec!["a".into(), "b".into()];
        move_to_front(&mut entries, "c");
        assert_eq!(entries, vec!["c", "a", "b"]);
    }

    #[test]
    fn move_to_front_is_idempotent_for_the_same_id() {
        let mut entries = vec!["a".into(), "b".into()];
        move_to_front(&mut entries, "a");
        assert_eq!(entries, vec!["a", "b"]);
        move_to_front(&mut entries, "a");
        assert_eq!(entries, vec!["a", "b"]);
    }

    #[test]
    fn move_to_front_bounds_the_log_to_max_entries() {
        let mut entries = (0..MAX_ENTRIES).map(|index| index.to_string()).collect();
        move_to_front(&mut entries, "new");
        assert_eq!(entries.len(), MAX_ENTRIES);
        assert_eq!(entries[0], "new");
        // The newest entry was promoted to the front; the oldest (the last one
        // in the original sequence) was evicted to make room.
        assert!(!entries.contains(&(MAX_ENTRIES - 1).to_string()));
    }

    #[test]
    fn read_log_ignores_blank_lines_and_whitespace() {
        let temp = tempfile_state();
        fs::write(&temp, "  a  \n\nb\n\n").unwrap();
        assert_eq!(read_log(&temp), vec!["a", "b"]);
    }

    #[test]
    fn write_then_read_round_trips() {
        let temp = tempfile_state();
        write_log(&temp, &["a".into(), "b".into()]).unwrap();
        assert_eq!(read_log(&temp), vec!["a", "b"]);
    }

    #[test]
    fn record_focus_promotes_the_event_pane_id() {
        let _guard = crate::test_support::ENV_MUTEX.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("cast-recency-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let previous = std::env::var_os("HERDR_PLUGIN_STATE_DIR");
        std::env::set_var("HERDR_PLUGIN_STATE_DIR", &dir);
        std::env::set_var("HERDR_PLUGIN_EVENT_JSON", r#"{"data":{"pane_id":"p:new"}}"#);
        // Seed an existing log.
        write_log(&dir.join(PANE_RECENCY_FILE), &["p:old".into()]).unwrap();
        record_focus().unwrap();
        let loaded = load();
        std::env::remove_var("HERDR_PLUGIN_EVENT_JSON");
        if let Some(previous) = previous {
            std::env::set_var("HERDR_PLUGIN_STATE_DIR", previous);
        } else {
            std::env::remove_var("HERDR_PLUGIN_STATE_DIR");
        }
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(loaded, vec!["p:new", "p:old"]);
    }

    fn tempfile_state() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "cast-recency-unit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        // Cleanup from any prior run; the test will write its own contents.
        let _ = fs::remove_file(&path);
        path
    }
}
