//! `session` sidebar token for agent panes.
//!
//! pi writes its terminal title as `π - <session name> - <cwd>` (or
//! `π - <cwd>` while the session is unnamed), and Herdr's stripped title
//! keeps the `π`. The Agents sidebar has no way to normalize that, so this
//! module reports a cleaned `session` token per pane through
//! `pane.report_metadata`. The token shows the pi session name, falls back
//! to the title body (e.g. the cwd for an unnamed session) or to the agent
//! kind when nothing named the pane, and is `null` for panes with nothing
//! to say.
//!
//! The resident title daemon is the only reporter: the server tracks the
//! sequence per terminal and source and silently drops stale ones, so a
//! second writer under `plugin:ad.cast` would lose to the daemon's
//! monotonic counter. Tokens attach to the pane's underlying terminal, so
//! the reporter state is keyed by terminal id and re-reports whenever a
//! new pane hangs off a seen terminal.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::api::SocketClient;

/// One token per report; the server accepts up to sixteen keys. Shared
/// with the workspace picker so both read the same token name.
pub const SESSION_TOKEN: &str = "session";
const METADATA_SOURCE: &str = "plugin:ad.cast";

/// How pi joins its mark, session name, and cwd in the terminal title.
const PI_TITLE_PREFIX: &str = "π - ";

/// The full diff state is resent on this cadence. The server drops stale
/// sequences silently and tokens outlive terminal reuse, so a drifted
/// local view would otherwise keep a wrong value for the session.
const RESEND_INTERVAL: Duration = Duration::from_secs(60);

/// Herdr caps pane metadata values this way; truncate on a character
/// boundary so a long session name cannot produce a mangled value.
const MAX_TOKEN_LENGTH: usize = 80;

/// How many words of a first prompt label an unnamed session.
const FIRST_PROMPT_WORDS: usize = 6;

/// pi inlines a skill run as the session's first user message.
const SKILL_TAG_PREFIX: &str = "<skill name=\"";

#[derive(Serialize)]
struct PaneReportMetadataParams {
    pane_id: String,
    source: String,
    tokens: BTreeMap<String, Option<String>>,
    seq: u64,
}

/// A pane's inputs to the token, straight from the daemon's `pane.list`.
#[derive(Debug, Default)]
pub struct PaneSnapshot {
    pub pane_id: String,
    pub terminal_id: String,
    pub terminal_title_stripped: Option<String>,
    pub cwd: Option<String>,
    pub agent: Option<String>,
    pub agent_session_path: Option<String>,
}

/// What the `session` token should say for a pane. `first_prompt` is the
/// label derived from the session's first user message, used when pi never
/// named the session. Empty bodies fall back to the agent kind, so the
/// sidebar row always names the visitor when a pane has no title at all.
pub fn token_for(pane: &PaneSnapshot, first_prompt: Option<&str>) -> Option<String> {
    let title = pane
        .terminal_title_stripped
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty());
    let Some(title) = title else {
        return pane
            .agent
            .as_deref()
            .map(str::trim)
            .filter(|agent| !agent.is_empty())
            .map(str::to_string);
    };
    let body = title.strip_prefix(PI_TITLE_PREFIX).unwrap_or(title);
    // A body derived from a skill run is already prefixed by
    // `first_prompt_token`; a named session that keeps the tag must match.
    if let Some(rest) = body.strip_prefix(SKILL_TAG_PREFIX) {
        if let Some(end) = rest.find(|c| c == '"' || c == ' ') {
            return Some(truncate(&format!("sk:{}", &rest[..end])));
        }
    }
    if body.len() < title.len() {
        if let Some(rest) = cwd_suffix(pane).and_then(|suffix| body.strip_suffix(&suffix)) {
            let named = rest.trim();
            if !named.is_empty() {
                return Some(truncate(named));
            }
        }
        // An unnamed session's title ends at its cwd basename; the first
        // prompt says more than the directory name, and keeping the body
        // beats saying nothing.
        let unnamed = Path::new(pane.cwd.as_deref().unwrap_or_default())
            .file_name()
            .map(|name| name.to_string_lossy() == body)
            .unwrap_or(false);
        if unnamed {
            if let Some(prompt) = first_prompt.filter(|prompt| !prompt.is_empty()) {
                return Some(truncate(prompt));
            }
        }
    }
    Some(truncate(body))
}

/// The label hidden in a pi session file's first user message: the skill
/// name when the session opened with a skill run, else the first words of
/// the prompt. The first message never changes, so callers cache per path.
pub fn first_prompt_token(session_path: &str) -> Option<String> {
    let reader = BufReader::new(std::fs::File::open(session_path).ok()?);
    for line in reader.lines() {
        let Ok(line) = line else { continue };
        let entry: serde_json::Value = match serde_json::from_str(&line) {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        if entry.pointer("/type").and_then(|t| t.as_str()) != Some("message") {
            continue;
        }
        if entry.pointer("/message/role").and_then(|r| r.as_str()) != Some("user") {
            continue;
        }
        return message_text(&entry).as_deref().and_then(label_from_prompt);
    }
    None
}

/// A pi message's text, whether the content is a plain string or a list of
/// parts.
fn message_text(entry: &serde_json::Value) -> Option<String> {
    let content = entry.pointer("/message/content")?;
    if let Some(text) = content.as_str() {
        return Some(text.to_string());
    }
    let parts = content.as_array()?;
    let text: String = parts
        .iter()
        .filter_map(|part| part.pointer("/text").and_then(|t| t.as_str()))
        .collect::<Vec<_>>()
        .join(" ");
    Some(text)
}

/// The token for one first user message.
fn label_from_prompt(text: &str) -> Option<String> {
    let text = text.trim_start();
    if let Some(rest) = text.strip_prefix(SKILL_TAG_PREFIX) {
        let end = rest.find('"')?;
        return Some(truncate(&format!("sk:{}", &rest[..end])));
    }
    let words: Vec<&str> = text.split_whitespace().take(FIRST_PROMPT_WORDS).collect();
    if words.is_empty() {
        return None;
    }
    Some(truncate(&words.join(" ")))
}

/// The ` - <cwd basename>` ending of a pi title. pi runs at the pane's own
/// directory, so `cwd` names the suffix.
fn cwd_suffix(pane: &PaneSnapshot) -> Option<String> {
    Path::new(pane.cwd.as_deref()?)
        .file_name()
        .map(|name| format!(" - {}", name.to_string_lossy()))
}

fn truncate(value: &str) -> String {
    match value.char_indices().nth(MAX_TOKEN_LENGTH) {
        Some((index, _)) => value[..index].to_string(),
        None => value.to_string(),
    }
}

/// Diff state for the daemon: what each terminal was last told to show.
/// Terminal ids survive pane recreation; pane ids do not. First-prompt
/// labels are cached per session file because the first message of a
/// session never changes.
pub struct SessionReporter {
    reported: HashMap<String, Option<String>>,
    first_prompts: HashMap<String, Option<String>>,
    last_seq: u64,
    last_resend: Instant,
}

impl Default for SessionReporter {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionReporter {
    pub fn new() -> Self {
        Self {
            reported: HashMap::new(),
            first_prompts: HashMap::new(),
            last_seq: 0,
            last_resend: Instant::now(),
        }
    }

    /// The first-prompt label for a pi pane's session, read once from the
    /// session file and cached.
    fn first_prompt(&mut self, pane: &PaneSnapshot) -> Option<String> {
        if pane.agent.as_deref() != Some("pi") {
            return None;
        }
        let path = pane.agent_session_path.as_deref()?;
        self.first_prompts
            .entry(path.to_string())
            .or_insert_with(|| first_prompt_token(path))
            .clone()
    }

    /// Report every pane whose token changed since the last tick.
    /// Reporting is best effort: failures are logged and retried on the
    /// next tick, never credited to the daemon's socket-failure counter.
    pub fn sync(&mut self, client: &SocketClient, panes: &[PaneSnapshot]) {
        let resend = self.last_resend.elapsed() >= RESEND_INTERVAL;
        if resend {
            self.reported.clear();
            self.last_resend = Instant::now();
        }
        let mut live: Vec<String> = Vec::with_capacity(panes.len());
        for pane in panes {
            if pane.pane_id.is_empty() || pane.terminal_id.is_empty() {
                continue;
            }
            live.push(pane.terminal_id.clone());
            let first_prompt = self.first_prompt(pane);
            let desired = token_for(pane, first_prompt.as_deref());
            if !resend && self.reported.get(&pane.terminal_id) == Some(&desired) {
                continue;
            }
            match report(client, pane, desired.clone(), self.next_seq()) {
                Ok(()) => {
                    self.reported.insert(pane.terminal_id.clone(), desired);
                }
                Err(error) => log(&format!(
                    "failed to report session token for {}: {error}",
                    pane.pane_id
                )),
            }
        }
        self.reported
            .retain(|terminal, _| live.iter().any(|id| id == terminal));
    }

    /// The server keeps the highest sequence per source, so the counter
    /// must climb even when the wall clock steps back.
    fn next_seq(&mut self) -> u64 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or_default();
        self.last_seq = self.last_seq.max(now) + 1;
        self.last_seq
    }
}

fn report(
    client: &SocketClient,
    pane: &PaneSnapshot,
    value: Option<String>,
    seq: u64,
) -> Result<(), String> {
    client.send(
        "cast:pane-report-metadata",
        "pane.report_metadata",
        PaneReportMetadataParams {
            pane_id: pane.pane_id.clone(),
            source: METADATA_SOURCE.to_string(),
            tokens: BTreeMap::from([(SESSION_TOKEN.to_string(), value)]),
            seq,
        },
    )?;
    Ok(())
}

fn log(message: &str) {
    eprintln!("[cast] {message}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(
        terminal_title_stripped: Option<&str>,
        cwd: Option<&str>,
        agent: Option<&str>,
    ) -> PaneSnapshot {
        PaneSnapshot {
            pane_id: "w1:p1".into(),
            terminal_id: "term_1".into(),
            terminal_title_stripped: terminal_title_stripped.map(str::to_string),
            cwd: cwd.map(str::to_string),
            agent: agent.map(str::to_string),
            agent_session_path: None,
        }
    }

    #[test]
    fn a_named_pi_session_shows_its_name_only() {
        let pane = snapshot(
            Some("π - herdr-cast sidebar config inventory - herdr-cast"),
            Some("/Users/a/code/src/github.com/aliou/herdr-cast"),
            Some("pi"),
        );
        assert_eq!(
            token_for(&pane, None).as_deref(),
            Some("herdr-cast sidebar config inventory")
        );
    }

    #[test]
    fn an_unnamed_pi_session_keeps_its_directory_without_a_prompt() {
        let pane = snapshot(
            Some("π - demo-sidebar-ui"),
            Some("/Users/a/.herdr/worktrees/herdr-cast/demo-sidebar-ui"),
            Some("pi"),
        );
        assert_eq!(token_for(&pane, None).as_deref(), Some("demo-sidebar-ui"));
    }

    #[test]
    fn an_unnamed_pi_session_prefers_its_first_prompt() {
        let pane = snapshot(
            Some("π - aliou-dot-me"),
            Some("/Users/a/code/src/github.com/aliou/aliou-dot-me"),
            Some("pi"),
        );
        assert_eq!(
            token_for(&pane, Some("i keep getting conflicts because of")).as_deref(),
            Some("i keep getting conflicts because of")
        );
        // A named session never shows the prompt.
        let named = snapshot(
            Some("π - release prep - aliou-dot-me"),
            Some("/Users/a/code/src/github.com/aliou/aliou-dot-me"),
            Some("pi"),
        );
        assert_eq!(
            token_for(&named, Some("i keep getting conflicts because")).as_deref(),
            Some("release prep")
        );
    }

    #[test]
    fn a_skill_run_labels_the_session_with_the_prefixed_skill_name() {
        assert_eq!(
            label_from_prompt("<skill name=\"skill-creator\" location=\"/x\">\n# Skill"),
            Some("sk:skill-creator".to_string())
        );
        assert_eq!(
            label_from_prompt("  <skill name=\"grilling\" location=\"/x\">"),
            Some("sk:grilling".to_string())
        );
    }

    #[test]
    fn a_skill_tag_in_the_title_gets_the_prefix() {
        let pane = snapshot(
            Some("π - <skill name=\"skill-creator\"> - herdr-cast"),
            Some("/work/herdr-cast"),
            Some("pi"),
        );
        assert_eq!(token_for(&pane, None).as_deref(), Some("sk:skill-creator"));
    }

    #[test]
    fn a_plain_prompt_labels_the_session_with_its_first_words() {
        assert_eq!(
            label_from_prompt("i keep getting conflicts because of the generated file, always"),
            Some("i keep getting conflicts because of".to_string())
        );
        assert_eq!(label_from_prompt("   "), None);
    }

    #[test]
    fn the_first_prompt_is_read_from_the_session_file() {
        let dir = std::env::temp_dir().join(format!("cast-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"type\":\"session\",\"version\":3}\n",
                "{\"type\":\"message\",\"message\":{\"role\":\"system\",\"content\":\"\"}}\n",
                "not json at all\n",
                "{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"<skill name=\\\"skill-creator\\\" location=\\\"/x\\\">body\"}}\n"
            ),
        )
        .unwrap();
        assert_eq!(
            first_prompt_token(path.to_str().unwrap()).as_deref(),
            Some("sk:skill-creator")
        );
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(first_prompt_token("/nonexistent/session.jsonl"), None);
    }

    #[test]
    fn a_pi_session_name_can_contain_separators() {
        let pane = snapshot(
            Some("π - a - b - herdr-cast"),
            Some("/Users/a/code/src/github.com/aliou/herdr-cast"),
            Some("pi"),
        );
        assert_eq!(token_for(&pane, None).as_deref(), Some("a - b"));
    }

    #[test]
    fn only_an_exact_cwd_suffix_is_stripped() {
        let pane = snapshot(
            Some("π - herdr-cast-extra"),
            Some("/work/herdr-cast"),
            Some("pi"),
        );
        assert_eq!(token_for(&pane, None).as_deref(), Some("herdr-cast-extra"));
        let missing_cwd = snapshot(Some("π - topic - herdr-cast"), None, Some("pi"));
        assert_eq!(
            token_for(&missing_cwd, None).as_deref(),
            Some("topic - herdr-cast")
        );
    }

    #[test]
    fn a_pane_without_a_title_falls_back_to_the_agent_kind() {
        let pane = snapshot(None, Some("/work/herdr-cast"), Some("pi"));
        assert_eq!(token_for(&pane, None).as_deref(), Some("pi"));
        let blank_blank = snapshot(Some("   "), Some("/work/herdr-cast"), Some("pi"));
        assert_eq!(token_for(&blank_blank, None).as_deref(), Some("pi"));
    }

    #[test]
    fn a_pane_without_title_or_agent_has_no_token() {
        assert_eq!(token_for(&snapshot(None, Some("/work"), None), None), None);
    }

    #[test]
    fn a_non_pi_title_survives_whole() {
        let pane = snapshot(Some("zsh"), Some("/work"), None);
        assert_eq!(token_for(&pane, None).as_deref(), Some("zsh"));
        let claude = snapshot(Some("✳ refactor parser"), Some("/work"), Some("claude"));
        assert_eq!(
            token_for(&claude, None).as_deref(),
            Some("✳ refactor parser")
        );
    }

    #[test]
    fn values_stay_within_the_protocol_limit() {
        let pane = snapshot(
            Some(&format!("π - {} - repo", "x".repeat(200))),
            Some("/work/repo"),
            Some("pi"),
        );
        let token = token_for(&pane, None).unwrap();
        assert_eq!(token.chars().count(), MAX_TOKEN_LENGTH);
    }

    #[test]
    fn the_report_request_matches_the_installed_protocol() {
        let request = PaneReportMetadataParams {
            pane_id: "w1:p1".into(),
            source: METADATA_SOURCE.into(),
            tokens: BTreeMap::from([(SESSION_TOKEN.into(), Some("topic".into()))]),
            seq: 42,
        };
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            serde_json::json!({
                "pane_id": "w1:p1",
                "source": "plugin:ad.cast",
                "tokens": { "session": "topic" },
                "seq": 42,
            })
        );
        assert_eq!(
            serde_json::to_value(&request)
                .unwrap()
                .as_object()
                .unwrap()
                .len(),
            4,
            "0.8.0-compatible requests carry no agent, title, or ttl fields"
        );
        let cleared = PaneReportMetadataParams {
            tokens: BTreeMap::from([(SESSION_TOKEN.into(), None)]),
            ..request
        };
        assert_eq!(
            serde_json::to_value(&cleared)
                .unwrap()
                .pointer("/tokens/session"),
            Some(&serde_json::Value::Null),
            "clearing a token reports an explicit JSON null"
        );
    }

    #[test]
    fn sequences_climb_even_when_the_clock_does_not() {
        let mut reporter = SessionReporter::new();
        let mut previous = reporter.next_seq();
        for _ in 0..100 {
            let next = reporter.next_seq();
            assert!(next > previous, "{next} must exceed {previous}");
            previous = next;
        }
    }
}
