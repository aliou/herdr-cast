//! Terminal window title ownership.
//!
//! Herdr re-renders its built-in window-title template only until a plugin
//! sends an explicit `client.window_title.set`; this module takes that
//! explicit ownership for the whole session. The title becomes
//! `HOSTNAME › SESSION_NAME › terminal_title`, where:
//!
//! - the hostname appears only when the server itself was spawned over SSH
//!   (`SSH_CONNECTION`/`SSH_TTY` survive in the daemon's inherited
//!   environment, covering both interactive ssh and `herdr --remote`),
//! - the session name appears only for named sessions (`HERDR_SESSION`),
//! - the tail is whatever the focused pane's program last set.
//!
//! Herdr withholds `pane.updated` from plugin hooks, so nothing announces a
//! title change. A resident `herdr-cast daemon` per session polls `pane.list`
//! and re-sets the composed title when it changes; focus and startup hooks
//! push it immediately and re-ensure the daemon. The lock file is keyed by
//! socket path because the plugin state directory is shared across sessions,
//! and one daemon must serve exactly one server.

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::api::SocketClient;
use crate::daemon;
use crate::session;

/// The fragments that never change over a server's lifetime.
const SEPARATOR: &str = " \u{203a} ";
/// How often the daemon re-reads the focused pane's terminal title. One
/// `pane.list` round trip per tick on the local socket.
const POLL_INTERVAL: Duration = Duration::from_millis(300);
/// The server has been unreachable for this many consecutive polls, so the
/// session is gone: stop rather than spin. Isolated failures must not exit.
const EXIT_AFTER_CONSECUTIVE_FAILURES: u32 = 10;
const SOCKET_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Serialize)]
struct PaneListParams {}

#[derive(Serialize)]
struct ClientWindowTitleSetParams {
    title: String,
}

#[derive(Debug, Default, Deserialize)]
struct PaneEntry {
    #[serde(default)]
    pane_id: String,
    #[serde(default)]
    terminal_id: String,
    #[serde(default)]
    focused: bool,
    terminal_title_stripped: Option<String>,
    cwd: Option<String>,
    agent: Option<String>,
    agent_session: Option<AgentSession>,
}

/// The agent's own session handle. For pi, `value` is the session file.
#[derive(Debug, Deserialize)]
struct AgentSession {
    value: Option<String>,
}

impl PaneEntry {
    /// The focused pane's terminal title, for the window title.
    fn focused_terminal_title(&self) -> Option<&str> {
        self.focused
            .then_some(self.terminal_title_stripped.as_deref())?
    }

    /// The inputs to the `session` sidebar token, expected clean by the
    /// reporter; Herdr already removed spinner glyphs from the title.
    fn session_token_snapshot(&self) -> session::PaneSnapshot {
        session::PaneSnapshot {
            pane_id: self.pane_id.clone(),
            terminal_id: self.terminal_id.clone(),
            terminal_title_stripped: self.terminal_title_stripped.clone(),
            cwd: self.cwd.clone(),
            agent: self.agent.clone(),
            agent_session_path: self
                .agent_session
                .as_ref()
                .and_then(|session| session.value.clone()),
        }
    }
}

/// The session-fixed fragments of the title, resolved once from the
/// server-inherited environment.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TitleContext {
    /// Short host name, present only when the server is remote.
    host: Option<String>,
    /// Named session, absent for the default session.
    session: Option<String>,
}

impl TitleContext {
    pub fn from_environment() -> Self {
        Self {
            host: is_remote()
                .then(daemon::hostname)
                .flatten()
                .and_then(|host| daemon::short_host(&host)),
            session: non_empty(std::env::var("HERDR_SESSION").ok()),
        }
    }
}

/// Join the session fragments with the focused pane's terminal title,
/// dropping absent fragments and their separators. The pane's title is
/// trimmed because programs pad it with spaces.
pub fn compose(context: &TitleContext, terminal_title: Option<&str>) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if let Some(host) = context.host.as_deref() {
        parts.push(host);
    }
    if let Some(session) = context.session.as_deref() {
        parts.push(session);
    }
    if let Some(title) = terminal_title
        .map(str::trim)
        .filter(|title| !title.is_empty())
    {
        parts.push(title);
    }
    parts.join(SEPARATOR)
}

/// `sync-title` hook entrypoint: push the composed title now and make sure
/// the session's daemon keeps it current.
pub fn sync_title() -> Result<(), String> {
    let socket_path = socket_path()?;
    let client = SocketClient::with_timeout(socket_path.clone(), SOCKET_TIMEOUT);
    let context = TitleContext::from_environment();
    let mut failures = Vec::new();
    if let Err(error) = refresh_once(&context, &client) {
        failures.push(error);
    }
    if let Err(error) = ensure_daemon(&socket_path) {
        failures.push(error);
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

/// Compose from the current focused pane and set the title unconditionally.
fn refresh_once(context: &TitleContext, client: &SocketClient) -> Result<(), String> {
    let terminal_title = focused_terminal_title(client)?;
    let title = compose(context, terminal_title.as_deref());
    set_title(client, &title)
}

/// Resident entrypoint: hold the session's daemon lock and keep the title
/// current until the session's server goes away.
pub fn daemon() -> Result<(), String> {
    let socket_path = socket_path()?;
    let lock_path = daemon_lock_path(&socket_path)
        .ok_or_else(|| "HERDR_PLUGIN_STATE_DIR not set".to_string())?;
    // A daemon already owns this session's lock; the loser exits quietly so
    // a spawn race between two hooks cannot leave two pollers running.
    let Some(_lock) = daemon::acquire(&lock_path)? else {
        return Ok(());
    };

    let client = SocketClient::with_timeout(&socket_path, SOCKET_TIMEOUT);
    let context = TitleContext::from_environment();
    let mut sessions = session::SessionReporter::new();
    let mut last: Option<String> = None;
    let mut failures: u32 = 0;
    loop {
        std::thread::sleep(POLL_INTERVAL);
        match list_panes(&client) {
            Ok(panes) => {
                failures = 0;
                let terminal_title = panes.iter().find_map(|pane| pane.focused_terminal_title());
                let title = compose(&context, terminal_title);
                if last.as_deref() != Some(title.as_str()) {
                    match set_title(&client, &title) {
                        Ok(()) => last = Some(title),
                        Err(error) => log(&format!("failed to set window title: {error}")),
                    }
                }
                let snapshots: Vec<session::PaneSnapshot> = panes
                    .iter()
                    .map(|pane| pane.session_token_snapshot())
                    .collect();
                sessions.sync(&client, &snapshots);
            }
            Err(error) => {
                failures += 1;
                if failures >= EXIT_AFTER_CONSECUTIVE_FAILURES {
                    log(&format!(
                        "session socket unreachable after {failures} polls; exiting: {error}"
                    ));
                    return Ok(());
                }
            }
        }
    }
}

/// Spawn the session's daemon unless one already holds its lock.
pub fn ensure_daemon(socket_path: &str) -> Result<(), String> {
    let Some(lock_path) = daemon_lock_path(socket_path) else {
        return Ok(());
    };
    if daemon::is_held(&lock_path)? {
        return Ok(());
    }
    // A concurrent spawner may win the lock race; the losing daemon exits.
    daemon::spawn_detached(&["daemon"], None)
}

/// Every pane in the session. The daemon reads the window title's inputs
/// and the `session` token's inputs from this single snapshot.
fn list_panes(client: &SocketClient) -> Result<Vec<PaneEntry>, String> {
    let response = client.send("cast:pane-list", "pane.list", PaneListParams {})?;
    serde_json::from_value(
        response
            .pointer("/result/panes")
            .cloned()
            .ok_or_else(|| "pane.list missing panes".to_string())?,
    )
    .map_err(|error| format!("failed to parse pane.list response: {error}"))
}

/// The session's terminal title, from the focused pane's last program-set
/// title. Herdr strips escape sequences itself; absent means the pane's
/// program never set one.
fn focused_terminal_title(client: &SocketClient) -> Result<Option<String>, String> {
    Ok(list_panes(client)?
        .into_iter()
        .find_map(|pane| pane.focused_terminal_title().map(str::to_string)))
}

fn set_title(client: &SocketClient, title: &str) -> Result<(), String> {
    client
        .send(
            "cast:client-window-title-set",
            "client.window_title.set",
            ClientWindowTitleSetParams {
                title: title.to_string(),
            },
        )
        .map(|_| ())
}

fn socket_path() -> Result<String, String> {
    std::env::var("HERDR_SOCKET_PATH").map_err(|_| "HERDR_SOCKET_PATH not set".to_string())
}

/// The server is remote when the daemon inherited an SSH-spawned
/// environment. A server first started outside ssh and later attached to
/// cannot be told apart; it keeps no hostname fragment.
fn is_remote() -> bool {
    std::env::var_os("SSH_CONNECTION")
        .or_else(|| std::env::var_os("SSH_TTY"))
        .is_some()
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.trim().is_empty())
}

/// One lock file per session: the state directory is shared across named
/// sessions, but a daemon serves exactly one server socket.
fn daemon_lock_path(socket_path: &str) -> Option<PathBuf> {
    std::env::var_os("HERDR_PLUGIN_STATE_DIR").map(|directory| {
        PathBuf::from(directory).join(format!("daemon-{:016x}.lock", stable_hash(socket_path)))
    })
}

fn stable_hash(value: &str) -> u64 {
    crate::notify::stable_hash(value)
}

fn log(message: &str) {
    eprintln!("[cast] {message}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(host: Option<&str>, session: Option<&str>) -> TitleContext {
        TitleContext {
            host: host.map(str::to_string),
            session: session.map(str::to_string),
        }
    }

    #[test]
    fn compose_drops_absent_fragments_and_their_separators() {
        assert_eq!(
            compose(&context(None, None), Some("vim - file")),
            "vim - file"
        );
        assert_eq!(
            compose(&context(Some("buildbox"), Some("work")), Some("vim - file")),
            "buildbox \u{203a} work \u{203a} vim - file"
        );
        assert_eq!(
            compose(&context(None, Some("work")), Some("vim - file")),
            "work \u{203a} vim - file"
        );
        assert_eq!(
            compose(&context(Some("buildbox"), None), Some("vim - file")),
            "buildbox \u{203a} vim - file"
        );
    }

    #[test]
    fn compose_without_a_terminal_title_shows_only_the_session_fragments() {
        assert_eq!(compose(&context(None, None), None), "");
        assert_eq!(
            compose(&context(Some("buildbox"), Some("work")), None),
            "buildbox \u{203a} work"
        );
    }

    #[test]
    fn compose_drops_blank_terminal_titles() {
        assert_eq!(
            compose(&context(Some("buildbox"), None), Some("   ")),
            "buildbox"
        );
        assert_eq!(compose(&context(None, None), Some("  vim  ")), "vim");
    }

    #[test]
    fn title_request_matches_the_installed_protocol() {
        let request = ClientWindowTitleSetParams {
            title: "buildbox \u{203a} vim".into(),
        };
        assert_eq!(
            serde_json::to_value(request).unwrap(),
            serde_json::json!({ "title": "buildbox \u{203a} vim" })
        );
    }

    #[test]
    fn pane_list_reads_the_focused_pane_terminal_title() {
        let response = serde_json::json!({
            "result": {
                "panes": [
                    { "pane_id": "w1:p1", "focused": false, "terminal_title_stripped": "zsh" },
                    { "pane_id": "w1:p2", "focused": true }
                ]
            }
        });
        let panes: Vec<PaneEntry> =
            serde_json::from_value(response.pointer("/result/panes").cloned().unwrap()).unwrap();
        assert_eq!(
            panes
                .into_iter()
                .find(|pane| pane.focused)
                .and_then(|pane| pane.terminal_title_stripped),
            None,
            "an unset terminal title stays absent"
        );
    }

    #[test]
    fn daemon_locks_key_by_socket_path() {
        let _guard = crate::test_support::ENV_MUTEX.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("cast-title-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let previous = std::env::var_os("HERDR_PLUGIN_STATE_DIR");
        std::env::set_var("HERDR_PLUGIN_STATE_DIR", &dir);
        let first = daemon_lock_path("/tmp/herdr-a.sock").unwrap();
        let second = daemon_lock_path("/tmp/herdr-b.sock").unwrap();
        let repeat = daemon_lock_path("/tmp/herdr-a.sock").unwrap();
        if let Some(previous) = previous {
            std::env::set_var("HERDR_PLUGIN_STATE_DIR", previous);
        } else {
            std::env::remove_var("HERDR_PLUGIN_STATE_DIR");
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(first, repeat, "the same socket keeps one lock");
        assert_ne!(first, second, "sessions never share a lock");
    }
}
