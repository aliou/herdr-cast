//! Space sidebar metadata.
//!
//! Herdr's Space rows only know `state_icon`, `state_text`, `workspace`,
//! `branch`, `git_status`, and custom `$name` tokens. Directories that are not
//! Git repositories therefore render an empty second line, and remote sessions
//! never show at all.
//!
//! This module reports four custom workspace tokens so the second Space row
//! stays useful:
//!
//! - `org`: which organization or client a space belongs to, following the
//!   same taxonomy as the shell prompt.
//! - `repos`: how many repositories a container directory holds, for spaces
//!   that are not repositories themselves.
//! - `host`: the short host name of a remote session running in the root pane.
//! - `hostkind`: `sbx` for lab sandboxes, so the sidebar can color the marker.
//! - `pad`: a blank glyph that holds the row open when a space has nothing to
//!   say, so every entry keeps the same height.
//!
//! A space's own name already tracks its root pane: Herdr labels a space after
//! the repository or directory that pane sits in, and renames it when the pane
//! moves. Nothing here repeats that name. Herdr derives `branch` and
//! `git_status` from the same pane, so this module never reports a branch.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api::SocketClient;

const SOCKET_TIMEOUT: Duration = Duration::from_secs(3);
const METADATA_SOURCE: &str = "plugin:ad.cast";

/// Herdr caps metadata values at 80 characters. Truncate on a character
/// boundary so a long directory name cannot produce a mangled value.
const MAX_TOKEN_LENGTH: usize = 80;

/// A shell hook fires before its remote command exists, so the follow-up sync
/// waits for the process to appear. Nothing else can observe the pane while
/// the remote session owns the terminal.
const REMOTE_ATTEMPTS: usize = 7;
const REMOTE_ATTEMPT_INTERVAL: Duration = Duration::from_millis(500);

const ORG_TOKEN: &str = "org";
const REPOS_TOKEN: &str = "repos";
const HOST_TOKEN: &str = "host";
const HOST_KIND_TOKEN: &str = "hostkind";
const PAD_TOKEN: &str = "pad";

/// Braille pattern blank. Herdr hides a Space row when every token is empty
/// and trims whitespace out of metadata values, so holding a row open takes a
/// character that prints as nothing without being whitespace.
const PAD_VALUE: &str = "\u{2800}";

/// Which organization or client owns a path, keyed by its root below `$HOME`.
/// `None` takes the first path segment below that root as the name, which is
/// how per-client directories work. Keep this aligned with the shell prompt.
const ORGANIZATIONS: &[(&str, Option<&str>)] = &[
    ("code/src/aliou.work", None),
    ("code/src/code.378labs.dev", Some("378")),
    ("code/src/general-dexterity.com", Some("\u{1f4d6}")),
];

/// A container directory earns a count only when it holds more than one
/// repository. Below that the number says less than the space's own name.
const MIN_COUNTED_REPOSITORIES: usize = 2;

/// Commands that hand a terminal to another machine. Only the leading argument
/// is matched, so a local `rsync` over SSH never claims the space.
const REMOTE_COMMANDS: &[&str] = &["ssh", "mosh", "mosh-client", "et", "sshrc"];

/// SSH options that consume the following argument. Anything else starting
/// with `-` is a flag, and the first remaining argument is the destination.
const SSH_VALUE_FLAGS: &[char] = &[
    'B', 'b', 'c', 'D', 'E', 'e', 'F', 'I', 'i', 'J', 'L', 'l', 'm', 'O', 'o', 'p', 'Q', 'R', 'S',
    'W', 'w',
];

/// Hosts under these suffixes are lab sandboxes rather than long-lived boxes.
const SANDBOX_HOST_SUFFIXES: &[&str] = &[".sbx.lab.internal"];

/// Wrappers that spawn a sandbox session. Matched against the command name and
/// its first argument.
const SANDBOX_WRAPPERS: &[(&str, &str)] = &[("sbxctl", "connect")];

#[derive(Serialize)]
struct EmptyParams {}

#[derive(Serialize)]
struct WorkspaceTarget {
    workspace_id: String,
}

#[derive(Serialize)]
struct PaneListParams {
    workspace_id: Option<String>,
}

#[derive(Serialize)]
struct PaneProcessInfoParams {
    pane_id: String,
}

#[derive(Serialize)]
struct WorkspaceReportMetadataParams {
    workspace_id: String,
    source: String,
    tokens: BTreeMap<String, Option<String>>,
    seq: u64,
}

#[derive(Debug, Deserialize)]
struct WorkspaceInfo {
    workspace_id: String,
    label: String,
}

#[derive(Debug, Deserialize)]
struct PaneInfo {
    pane_id: String,
    cwd: Option<String>,
    foreground_cwd: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ProcessInfo {
    #[serde(default)]
    foreground_processes: Vec<ForegroundProcess>,
}

#[derive(Debug, Deserialize)]
struct ForegroundProcess {
    name: String,
    argv0: Option<String>,
    argv: Option<Vec<String>>,
}

/// What the root pane says about a space.
#[derive(Debug, Default, PartialEq, Eq)]
struct SpaceFacts {
    org: Option<String>,
    repos: Option<String>,
    host: Option<String>,
    host_kind: Option<String>,
    /// Whether Herdr renders a branch of its own for this space.
    branch: bool,
}

impl SpaceFacts {
    /// Herdr drops a row whose tokens are all empty, which would leave Spaces
    /// with uneven heights.
    fn pad(&self) -> Option<String> {
        let empty = self.org.is_none()
            && self.repos.is_none()
            && self.host.is_none()
            && self.host_kind.is_none()
            && !self.branch;
        empty.then(|| PAD_VALUE.to_string())
    }
}

/// Refresh the workspace named by the plugin event or the invoking pane.
///
/// `await_remote` waits for a remote session to appear, for callers that ran
/// before the command they announced. Falls back to a full refresh so a stray
/// invocation still does something useful instead of failing.
pub fn sync(await_remote: bool) -> Result<(), String> {
    match current_workspace_id() {
        Some(workspace_id) => {
            let client = socket_client()?;
            let label = workspace_label(&client, &workspace_id)?;
            sync_workspace(&client, &workspace_id, &label, await_remote)
        }
        None => sync_all(),
    }
}

/// Refresh every workspace. Herdr drops metadata tokens when a new server
/// restores a session, so the startup hook rebuilds all of them.
pub fn sync_all() -> Result<(), String> {
    let client = socket_client()?;
    let workspaces = list_workspaces(&client)?;
    let mut failures = Vec::new();
    for workspace in workspaces {
        if let Err(error) =
            sync_workspace(&client, &workspace.workspace_id, &workspace.label, false)
        {
            failures.push(error);
        }
    }
    if failures.is_empty() {
        return Ok(());
    }
    Err(failures.join("; "))
}

/// Print the shell integration. The snippet points at this exact binary, so a
/// moved or rebuilt checkout never leaves a stale path behind.
pub fn shell_init(shell: &str) -> Result<(), String> {
    if shell != "zsh" {
        return Err(format!("unsupported shell {shell}; expected zsh"));
    }
    let executable = std::env::current_exe()
        .map_err(|error| format!("failed to resolve the herdr-cast path: {error}"))?;
    print!("{}", zsh_integration(&executable.to_string_lossy()));
    Ok(())
}

fn sync_workspace(
    client: &SocketClient,
    workspace_id: &str,
    label: &str,
    await_remote: bool,
) -> Result<(), String> {
    let attempts = if await_remote { REMOTE_ATTEMPTS } else { 1 };
    let mut observation = observe(client, workspace_id, label)?;
    for _ in 1..attempts {
        if observation.facts.host.is_some() {
            break;
        }
        std::thread::sleep(REMOTE_ATTEMPT_INTERVAL);
        observation = observe(client, workspace_id, label)?;
    }
    report(client, workspace_id, &observation)
}

/// A snapshot of a space and the sequence number that dates it.
struct Observation {
    seq: u64,
    facts: SpaceFacts,
}

/// The sequence is taken before the reads it describes, so a sync that stalls
/// mid-flight loses to whatever observed the pane afterwards.
fn observe(client: &SocketClient, workspace_id: &str, label: &str) -> Result<Observation, String> {
    let seq = sequence();
    let facts = match root_pane(client, workspace_id)? {
        Some(pane) => space_facts(&pane, &process_info(client, &pane.pane_id), label),
        None => SpaceFacts::default(),
    };
    Ok(Observation { seq, facts })
}

fn report(
    client: &SocketClient,
    workspace_id: &str,
    observation: &Observation,
) -> Result<(), String> {
    client.send(
        "cast:workspace-report-metadata",
        "workspace.report_metadata",
        WorkspaceReportMetadataParams {
            workspace_id: workspace_id.to_string(),
            source: METADATA_SOURCE.to_string(),
            tokens: tokens(&observation.facts),
            seq: observation.seq,
        },
    )?;
    Ok(())
}

fn tokens(facts: &SpaceFacts) -> BTreeMap<String, Option<String>> {
    BTreeMap::from([
        (ORG_TOKEN.to_string(), truncate(facts.org.clone())),
        (REPOS_TOKEN.to_string(), truncate(facts.repos.clone())),
        (HOST_TOKEN.to_string(), truncate(facts.host.clone())),
        (
            HOST_KIND_TOKEN.to_string(),
            truncate(facts.host_kind.clone()),
        ),
        (PAD_TOKEN.to_string(), facts.pad()),
    ])
}

fn truncate(value: Option<String>) -> Option<String> {
    value.map(|value| match value.char_indices().nth(MAX_TOKEN_LENGTH) {
        Some((index, _)) => value[..index].to_string(),
        None => value,
    })
}

/// Milliseconds since the epoch. Herdr keeps the highest sequence it has seen
/// per source, so a slow background sync never overwrites a newer one.
fn sequence() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default()
}

fn space_facts(pane: &PaneInfo, process_info: &ProcessInfo, label: &str) -> SpaceFacts {
    if let Some(remote) = remote_session(process_info) {
        return SpaceFacts {
            host: Some(remote.host),
            host_kind: remote.kind.map(str::to_string),
            ..SpaceFacts::default()
        };
    }
    let Some(cwd) = pane.working_directory() else {
        return SpaceFacts::default();
    };
    SpaceFacts {
        org: organization(cwd, &home()).filter(|org| org != label),
        repos: repository_count(cwd),
        host: None,
        host_kind: None,
        branch: repository_root(cwd).is_some(),
    }
}

/// The organization or client owning a path, dropped when it only repeats the
/// workspace label the sidebar already prints above it.
fn organization(cwd: &Path, home: &Path) -> Option<String> {
    let relative = cwd.strip_prefix(home).ok()?;
    ORGANIZATIONS.iter().find_map(|(root, name)| {
        let below = relative.strip_prefix(root).ok()?;
        match name {
            Some(name) => Some((*name).to_string()),
            None => below
                .components()
                .next()
                .map(|client| client.as_os_str().to_string_lossy().into_owned()),
        }
    })
}

/// How many repositories a container directory holds. A space that is itself
/// a repository has a branch to show instead, and Herdr already shows it.
fn repository_count(cwd: &Path) -> Option<String> {
    if repository_root(cwd).is_some() {
        return None;
    }
    let count = std::fs::read_dir(cwd)
        .ok()?
        .flatten()
        .filter(|entry| entry.path().join(".git").exists())
        .count();
    (count >= MIN_COUNTED_REPOSITORIES).then(|| format!("{count} repos"))
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// Nearest ancestor holding a `.git` entry. Worktrees store a file there
/// rather than a directory, so both count.
fn repository_root(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors()
        .find(|directory| directory.join(".git").exists())
        .map(Path::to_path_buf)
}

#[derive(Debug, PartialEq, Eq)]
struct RemoteSession {
    host: String,
    kind: Option<&'static str>,
}

/// Identify a remote session from the pane's foreground processes.
///
/// The process list is the only reliable signal: a shell hook cannot run while
/// SSH holds the terminal, and wrappers such as `sbxctl connect` hide the real
/// destination behind their own arguments.
fn remote_session(process_info: &ProcessInfo) -> Option<RemoteSession> {
    let host = process_info
        .foreground_processes
        .iter()
        .find_map(remote_host)?;
    let sandbox = SANDBOX_HOST_SUFFIXES
        .iter()
        .any(|suffix| host.ends_with(suffix))
        || process_info
            .foreground_processes
            .iter()
            .any(is_sandbox_wrapper);
    Some(RemoteSession {
        host: short_host(&host),
        kind: sandbox.then_some("sbx"),
    })
}

fn remote_host(process: &ForegroundProcess) -> Option<String> {
    let argv = process.argv.as_ref()?;
    if !REMOTE_COMMANDS.contains(&command_name(process)) {
        return None;
    }
    destination(argv).map(|destination| {
        destination
            .rsplit_once('@')
            .map(|(_, host)| host)
            .unwrap_or(destination)
            .to_string()
    })
}

/// First non-option argument after the command name.
fn destination(argv: &[String]) -> Option<&str> {
    let mut arguments = argv.iter().skip(1);
    while let Some(argument) = arguments.next() {
        let Some(cluster) = argument.strip_prefix('-') else {
            return Some(argument.as_str());
        };
        // Options cluster, and a value option takes the rest of its cluster
        // when there is one: `-vp 2222` and `-p2222` both carry a port.
        let mut characters = cluster.chars();
        while let Some(flag) = characters.next() {
            if !SSH_VALUE_FLAGS.contains(&flag) {
                continue;
            }
            if characters.as_str().is_empty() {
                arguments.next();
            }
            break;
        }
    }
    None
}

/// A wrapper only counts when it is the command itself, either directly or
/// through an interpreter, and its subcommand follows it. Otherwise an SSH
/// command that merely mentions the wrapper would mislabel the space.
fn is_sandbox_wrapper(process: &ForegroundProcess) -> bool {
    let Some(argv) = process.argv.as_ref() else {
        return false;
    };
    if REMOTE_COMMANDS.contains(&command_name(process)) {
        return false;
    }
    SANDBOX_WRAPPERS.iter().any(|(wrapper, subcommand)| {
        argv.iter().take(2).enumerate().any(|(index, argument)| {
            is_named(argument, wrapper) && follows(argv, index, subcommand)
        })
    })
}

fn is_named(argument: &str, name: &str) -> bool {
    Path::new(argument)
        .file_stem()
        .is_some_and(|stem| stem == name)
}

fn follows(argv: &[String], index: usize, expected: &str) -> bool {
    argv.get(index + 1)
        .is_some_and(|argument| argument == expected)
}

/// Herdr's sidebar is narrow, so only the first label survives.
fn short_host(host: &str) -> String {
    host.split('.').next().unwrap_or(host).to_string()
}

fn command_name(process: &ForegroundProcess) -> &str {
    let name = process
        .argv0
        .as_deref()
        .filter(|argv0| !argv0.is_empty())
        .unwrap_or(&process.name);
    Path::new(name)
        .file_name()
        .map(|name| name.to_str().unwrap_or_default())
        .unwrap_or(name)
}

impl PaneInfo {
    /// Prefer the shell's own directory. `foreground_cwd` follows whatever
    /// command is running and drifts once an agent changes directories.
    fn working_directory(&self) -> Option<&Path> {
        self.cwd
            .as_deref()
            .or(self.foreground_cwd.as_deref())
            .map(Path::new)
    }
}

/// Herdr resolves a space's Git identity from the first tab's root pane, and
/// `pane.list` returns tabs in order with each tab's panes in layout order.
/// The first entry is therefore that same pane.
fn root_pane(client: &SocketClient, workspace_id: &str) -> Result<Option<PaneInfo>, String> {
    let response = client.send(
        "cast:pane-list",
        "pane.list",
        PaneListParams {
            workspace_id: Some(workspace_id.to_string()),
        },
    )?;
    let panes: Vec<PaneInfo> = serde_json::from_value(
        response
            .pointer("/result/panes")
            .cloned()
            .ok_or_else(|| "pane.list missing panes".to_string())?,
    )
    .map_err(|error| format!("failed to parse pane.list response: {error}"))?;
    Ok(panes.into_iter().next())
}

/// Best effort: a pane whose process list cannot be read is treated as local
/// rather than failing the whole sync.
fn process_info(client: &SocketClient, pane_id: &str) -> ProcessInfo {
    client
        .send(
            "cast:pane-process-info",
            "pane.process_info",
            PaneProcessInfoParams {
                pane_id: pane_id.to_string(),
            },
        )
        .ok()
        .and_then(|response| response.pointer("/result/process_info").cloned())
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default()
}

fn list_workspaces(client: &SocketClient) -> Result<Vec<WorkspaceInfo>, String> {
    let response = client.send("cast:workspace-list", "workspace.list", EmptyParams {})?;
    serde_json::from_value(
        response
            .pointer("/result/workspaces")
            .cloned()
            .ok_or_else(|| "workspace.list missing workspaces".to_string())?,
    )
    .map_err(|error| format!("failed to parse workspace.list response: {error}"))
}

fn workspace_label(client: &SocketClient, workspace_id: &str) -> Result<String, String> {
    let response = client.send(
        "cast:workspace-get",
        "workspace.get",
        WorkspaceTarget {
            workspace_id: workspace_id.to_string(),
        },
    )?;
    response
        .pointer("/result/workspace/label")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "workspace.get missing label".to_string())
}

/// Plugin hooks carry the workspace in their event payload; shell hooks only
/// have the injected pane environment.
fn current_workspace_id() -> Option<String> {
    event_workspace_id().or_else(|| non_empty(std::env::var("HERDR_WORKSPACE_ID").ok()))
}

fn event_workspace_id() -> Option<String> {
    let event = std::env::var("HERDR_PLUGIN_EVENT_JSON").ok()?;
    let value: Value = serde_json::from_str(&event).ok()?;
    for pointer in [
        "/data/workspace_id",
        "/data/workspace/workspace_id",
        "/workspace_id",
    ] {
        if let Some(workspace_id) = value.pointer(pointer).and_then(Value::as_str) {
            return non_empty(Some(workspace_id.to_string()));
        }
    }
    None
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.trim().is_empty())
}

fn socket_client() -> Result<SocketClient, String> {
    let socket =
        std::env::var("HERDR_SOCKET_PATH").map_err(|_| "HERDR_SOCKET_PATH not set".to_string())?;
    Ok(SocketClient::with_timeout(socket, SOCKET_TIMEOUT))
}

/// Zsh hooks. `precmd` covers directory changes, `preexec` starts a second
/// pass for commands that hand the terminal to another machine, because the
/// remote process does not exist yet when `preexec` runs.
fn zsh_integration(executable: &str) -> String {
    let quoted = zsh_quote(executable);
    format!(
        r#"_cast_executable={quoted}
_cast_space_cwd=
_cast_space_pending=

function _cast_space_sync() {{
  ( "$_cast_executable" sync-space "$@" >/dev/null 2>&1 ) &!
}}

function _cast_space_precmd() {{
  [[ -n ${{HERDR_WORKSPACE_ID-}} ]] || return 0
  # Another integration can rebuild preexec_functions wholesale.
  if (( ${{preexec_functions[(Ie)_cast_space_preexec]}} == 0 )); then
    preexec_functions+=(_cast_space_preexec)
  fi
  if [[ -n $_cast_space_pending ]]; then
    _cast_space_pending=
    _cast_space_sync
    return 0
  fi
  [[ "$PWD" != "$_cast_space_cwd" ]] || return 0
  _cast_space_cwd=$PWD
  _cast_space_sync
}}

function _cast_space_preexec() {{
  [[ -n ${{HERDR_WORKSPACE_ID-}} ]] || return 0
  emulate -L zsh
  local -a _cast_command
  _cast_command=("${{(z)3}}")
  case ${{_cast_command[1]:t}} in
    ssh|mosh|sshrc|et|sbxctl)
      _cast_space_pending=1
      _cast_space_sync --await-remote ;;
  esac
}}

autoload -Uz add-zsh-hook
add-zsh-hook precmd _cast_space_precmd
add-zsh-hook preexec _cast_space_preexec
"#
    )
}

fn zsh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_request_matches_the_installed_protocol() {
        let request = WorkspaceReportMetadataParams {
            workspace_id: "w:2/a b".into(),
            source: METADATA_SOURCE.into(),
            tokens: tokens(&SpaceFacts {
                org: Some("378".into()),
                repos: Some("11 repos".into()),
                ..SpaceFacts::default()
            }),
            seq: 17,
        };
        assert_eq!(
            serde_json::to_value(request).unwrap(),
            serde_json::json!({
                "workspace_id": "w:2/a b",
                "source": "plugin:ad.cast",
                "tokens": {
                    "org": "378",
                    "repos": "11 repos",
                    "host": null,
                    "hostkind": null,
                    "pad": null
                },
                "seq": 17
            })
        );
    }

    #[test]
    fn absent_values_clear_their_tokens() {
        let cleared = tokens(&SpaceFacts {
            branch: true,
            ..SpaceFacts::default()
        });
        assert_eq!(cleared.get(ORG_TOKEN), Some(&None));
        assert_eq!(cleared.get(REPOS_TOKEN), Some(&None));
        assert_eq!(cleared.get(HOST_TOKEN), Some(&None));
        assert_eq!(cleared.get(HOST_KIND_TOKEN), Some(&None));
        assert_eq!(cleared.get(PAD_TOKEN), Some(&None));
    }

    #[test]
    fn a_space_with_nothing_to_say_still_holds_its_row() {
        let padded = tokens(&SpaceFacts::default());
        assert_eq!(
            padded.get(PAD_TOKEN),
            Some(&Some("\u{2800}".to_string())),
            "a space with no tokens and no branch needs a blank glyph"
        );
        assert!(
            !PAD_VALUE.trim().is_empty(),
            "Herdr trims whitespace values"
        );
    }

    #[test]
    fn any_visible_token_replaces_the_blank_glyph() {
        for facts in [
            SpaceFacts {
                org: Some("378".into()),
                ..SpaceFacts::default()
            },
            SpaceFacts {
                repos: Some("11 repos".into()),
                ..SpaceFacts::default()
            },
            SpaceFacts {
                host: Some("donut".into()),
                ..SpaceFacts::default()
            },
            SpaceFacts {
                branch: true,
                ..SpaceFacts::default()
            },
        ] {
            assert_eq!(facts.pad(), None, "facts: {facts:?}");
        }
    }

    #[test]
    fn ssh_panes_report_the_short_host() {
        let info = process_list(vec![process(
            "ssh",
            &[
                "ssh",
                "-o",
                "StrictHostKeyChecking=accept-new",
                "donut.tetra-albacore.ts.net",
            ],
        )]);
        assert_eq!(
            remote_session(&info),
            Some(RemoteSession {
                host: "donut".into(),
                kind: None
            })
        );
    }

    #[test]
    fn sandbox_hosts_carry_a_kind_marker() {
        let info = process_list(vec![
            process(
                "ssh",
                &[
                    "ssh",
                    "-o",
                    "UserKnownHostsFile=/dev/null",
                    "sandbox@copper-eva-stratt.sbx.lab.internal",
                ],
            ),
            process(
                "node",
                &[
                    "/nix/store/hash-nodejs/bin/node",
                    "/nix/store/hash-sbxctl/lib/@378labs/sbxctl/sbxctl.cjs",
                    "connect",
                    "copper-eva-stratt",
                ],
            ),
        ]);
        assert_eq!(
            remote_session(&info),
            Some(RemoteSession {
                host: "copper-eva-stratt".into(),
                kind: Some("sbx")
            })
        );
    }

    #[test]
    fn sandbox_wrappers_mark_hosts_without_a_known_suffix() {
        let info = process_list(vec![
            process("ssh", &["ssh", "sandbox@10.0.0.4"]),
            process("sbxctl", &["sbxctl", "connect", "copper-eva-stratt"]),
        ]);
        assert_eq!(remote_session(&info).unwrap().kind, Some("sbx"));
    }

    #[test]
    fn ssh_value_flags_never_swallow_the_destination() {
        for argv in [
            vec!["ssh", "-p", "2222", "donut"],
            vec!["ssh", "-p2222", "donut"],
            vec!["ssh", "-i", "/tmp/key", "-4", "donut"],
            vec!["ssh", "-tt", "donut"],
            vec!["ssh", "-J", "jump", "donut"],
            vec!["ssh", "-vp", "2222", "donut"],
            vec!["ssh", "-vF", "/tmp/config", "donut"],
            vec!["ssh", "-4tv", "donut"],
            vec!["ssh", "-oPort=2222", "donut"],
            vec!["ssh", "-", "donut"],
        ] {
            let owned = argv
                .iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>();
            assert_eq!(destination(&owned), Some("donut"), "argv: {argv:?}");
        }
    }

    #[test]
    fn a_remote_command_mentioning_the_wrapper_is_not_a_sandbox() {
        let info = process_list(vec![process(
            "ssh",
            &["ssh", "ordinary-host", "sbxctl", "connect"],
        )]);
        assert_eq!(
            remote_session(&info),
            Some(RemoteSession {
                host: "ordinary-host".into(),
                kind: None
            })
        );
    }

    #[test]
    fn token_values_stay_within_the_protocol_limit() {
        let tokens = tokens(&SpaceFacts {
            org: Some("d".repeat(200)),
            ..SpaceFacts::default()
        });
        assert_eq!(
            tokens.get(ORG_TOKEN).unwrap().as_deref().map(str::len),
            Some(MAX_TOKEN_LENGTH)
        );
    }

    #[test]
    fn truncation_keeps_character_boundaries() {
        let truncated = truncate(Some("\u{e9}".repeat(200))).unwrap();
        assert_eq!(truncated.chars().count(), MAX_TOKEN_LENGTH);
    }

    #[test]
    fn local_commands_never_claim_the_space() {
        let info = process_list(vec![
            process("rsync", &["rsync", "-a", "src", "donut:/tmp"]),
            process("pi", &["pi", "--resume"]),
        ]);
        assert_eq!(remote_session(&info), None);
    }

    #[test]
    fn panes_without_argv_are_treated_as_local() {
        let info = ProcessInfo {
            foreground_processes: vec![ForegroundProcess {
                name: "ssh".into(),
                argv0: None,
                argv: None,
            }],
        };
        assert_eq!(remote_session(&info), None);
    }

    #[test]
    fn a_remote_session_replaces_the_local_context() {
        let facts = space_facts(
            &pane("/Users/a/code/src/code.378labs.dev/homelab"),
            &process_list(vec![process("ssh", &["ssh", "donut.ts.net"])]),
            "homelab",
        );
        assert_eq!(
            facts,
            SpaceFacts {
                org: None,
                repos: None,
                host: Some("donut".into()),
                host_kind: None,
                branch: false,
            }
        );
    }

    #[test]
    fn organizations_name_their_client_or_their_tag() {
        let home = Path::new("/Users/a");
        for (path, expected) in [
            (
                "/Users/a/code/src/code.378labs.dev/homelab/infra",
                Some("378"),
            ),
            ("/Users/a/code/src/code.378labs.dev", Some("378")),
            (
                "/Users/a/code/src/aliou.work/factorial.co/f0",
                Some("factorial.co"),
            ),
            (
                "/Users/a/code/src/general-dexterity.com/book",
                Some("\u{1f4d6}"),
            ),
            ("/Users/a/code/src/github.com/aliou/herdr-cast", None),
            ("/Users/a/tmp", None),
            ("/elsewhere/code/src/code.378labs.dev/homelab", None),
        ] {
            assert_eq!(
                organization(Path::new(path), home).as_deref(),
                expected,
                "path: {path}"
            );
        }
    }

    #[test]
    fn an_organization_repeating_the_space_name_is_dropped() {
        let facts = space_facts(
            &pane("/Users/a/code/src/aliou.work/factorial.co"),
            &ProcessInfo::default(),
            "factorial.co",
        );
        assert_eq!(facts.org, None);
    }

    #[test]
    fn container_directories_count_their_repositories() {
        let root = temporary_directory("homelab");
        for name in ["infra", "modules", "secrets"] {
            std::fs::create_dir_all(root.join(name).join(".git")).unwrap();
        }
        std::fs::create_dir_all(root.join("notes")).unwrap();
        assert_eq!(repository_count(&root), Some("3 repos".into()));
    }

    #[test]
    fn a_lone_repository_says_less_than_the_space_name() {
        let root = temporary_directory("factorial.co");
        std::fs::create_dir_all(root.join("code").join(".git")).unwrap();
        std::fs::create_dir_all(root.join("work")).unwrap();
        assert_eq!(repository_count(&root), None);
    }

    #[test]
    fn repositories_have_a_branch_instead_of_a_count() {
        let root = temporary_repository("herdr-cast");
        std::fs::create_dir_all(root.join("vendor").join(".git")).unwrap();
        std::fs::create_dir_all(root.join("assets").join(".git")).unwrap();
        assert_eq!(repository_count(&root), None);
    }

    #[test]
    fn worktree_checkouts_count_as_repositories() {
        let root = temporary_directory("feature-branch");
        std::fs::write(root.join(".git"), "gitdir: /tmp/elsewhere\n").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        assert_eq!(repository_root(&root.join("src")), Some(root));
    }

    #[test]
    fn the_shell_integration_quotes_its_own_path() {
        let script = zsh_integration("/tmp/a dir/herdr-cast");
        assert!(script.contains("_cast_executable='/tmp/a dir/herdr-cast'"));
        assert!(!script.contains(" $_cast_executable "));
        assert!(script.contains("add-zsh-hook precmd _cast_space_precmd"));
        assert!(script.contains("add-zsh-hook preexec _cast_space_preexec"));
    }

    #[test]
    fn the_shell_integration_waits_for_announced_remote_commands() {
        let script = zsh_integration("/tmp/herdr-cast");
        assert!(script.contains("_cast_space_sync --await-remote"));
        assert!(script.contains("preexec_functions+=(_cast_space_preexec)"));
    }

    #[test]
    fn the_shell_integration_survives_quotes_in_its_path() {
        let script = zsh_integration("/tmp/it's here/herdr-cast");
        assert!(script.contains(r"_cast_executable='/tmp/it'\''s here/herdr-cast'"));
    }

    fn pane(cwd: &str) -> PaneInfo {
        PaneInfo {
            pane_id: "w1:p1".into(),
            cwd: Some(cwd.into()),
            foreground_cwd: None,
        }
    }

    fn process(name: &str, argv: &[&str]) -> ForegroundProcess {
        ForegroundProcess {
            name: name.into(),
            argv0: argv.first().map(|value| value.to_string()),
            argv: Some(argv.iter().map(|value| value.to_string()).collect()),
        }
    }

    fn process_list(processes: Vec<ForegroundProcess>) -> ProcessInfo {
        ProcessInfo {
            foreground_processes: processes,
        }
    }

    fn temporary_directory(name: &str) -> PathBuf {
        let root = std::env::temp_dir()
            .join(format!(
                "herdr-cast-space-{}-{}",
                std::process::id(),
                sequence()
            ))
            .join(name);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn temporary_repository(name: &str) -> PathBuf {
        let root = temporary_directory(name);
        std::fs::create_dir_all(root.join(".git")).unwrap();
        root
    }
}
