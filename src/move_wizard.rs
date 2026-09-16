// The multi-step "move pane" wizard behind the layout palette's
// "Move pane…" action. Every level is a plain picker; the wizard keeps an
// explicit stack of levels so Escape pops back to the parent level and
// Ctrl-C aborts the whole wizard from any depth. Each terminal choice maps
// to exactly one `pane.move` request. The protocol requires a split
// direction when moving into an existing tab, so that always gets its own
// final level.
//
// Levels:
//
//   DestinationKind ──► NewTab (current workspace)          → pane.move
//                   ├──► ChooseTab ──► workspace row        → pane.move new_tab
//                   │              └──► tab row ──► ChooseSplit → pane.move tab
//                   └──► NewWorkspace                       → pane.move
//
// Picking a workspace row in ChooseTab means "new tab in that workspace",
// so "new tab elsewhere" and "existing tab" share one fuzzy tree.
//
// After every move the wizard explicitly focuses the moved pane: the popup
// closes on top of the move, and a `pane.focus` issued after the move is
// what lands focus in the pane's new location instead of restoring the
// pane that spawned the popup.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use std::process::Command;

use serde_json::Value;

use crate::api::SocketClient;
use crate::palette::{
    current_location, layout_export_tab, pane_move, CurrentLocation, Direction, LayoutNode,
    PaneMoveDestination, SplitDirection,
};
use crate::picker::{pick_nav, Choice, PickOutcome, Picker};
use crate::space;

#[derive(Serialize)]
struct EmptyParams {}

#[derive(Serialize)]
struct TabListParams {
    workspace_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WizardWorkspace {
    workspace_id: String,
    number: usize,
    label: String,
    pane_count: usize,
    tab_count: usize,
    agent_status: String,
    #[serde(default)]
    tokens: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct WizardTab {
    tab_id: String,
    workspace_id: String,
    number: usize,
    label: String,
    pane_count: usize,
    agent_status: String,
}

/// One fetch of everything the wizard levels render, so re-entering a level
/// after Esc costs no socket round-trips.
struct Snapshot {
    workspaces: Vec<WizardWorkspace>,
    tabs: Vec<WizardTab>,
    current: Option<CurrentLocation>,
    sessions: Vec<WizardSession>,
    source: Option<SourcePane>,
}

impl Snapshot {
    fn fetch(client: &SocketClient, pane_id: &str) -> Result<Self, String> {
        let workspaces = list_workspaces(client)?;
        let tabs = list_tabs(client)?;
        let current = current_location(client, pane_id).ok();
        let sessions = std::env::var("HERDR_SOCKET_PATH")
            .map(|socket| fetch_sessions(&socket))
            .unwrap_or_default();
        let source = fetch_source(client, pane_id).ok();
        Ok(Snapshot {
            workspaces,
            tabs,
            current,
            sessions,
            source,
        })
    }

    fn workspace_label(&self, workspace_id: &str) -> Option<&str> {
        self.workspaces
            .iter()
            .find(|workspace| workspace.workspace_id == workspace_id)
            .map(|workspace| workspace.label.as_str())
    }

    fn current_workspace_id(&self) -> Option<&str> {
        self.current
            .as_ref()
            .map(|location| location.workspace_id.as_str())
    }

    fn current_tab_id(&self) -> Option<&str> {
        self.current
            .as_ref()
            .map(|location| location.tab_id.as_str())
    }
}

/// A running Herdr session other than the current one. Panes cannot move
/// between sessions, so the wizard transplants the focused pane instead: a
/// new workspace with the same label on the other session's socket, a pane
/// with the same name and cwd, the agent resumed via `agent.start` when the
/// source pane reports an official `agent_session`, and finally the source
/// pane closed back in the current session.
#[derive(Debug, Clone)]
struct WizardSession {
    name: String,
    socket_path: String,
}

/// Everything the move needs from the focused pane, read once at wizard
/// entry from the current session's `pane.get`.
#[derive(Debug, Deserialize)]
struct SourcePane {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    foreground_cwd: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    agent_session: Option<AgentSessionRef>,
}

impl SourcePane {
    /// The live foreground directory when Herdr tracks it, otherwise the
    /// pane's launch cwd.
    fn cwd(&self) -> Option<&str> {
        self.foreground_cwd.as_deref().or(self.cwd.as_deref())
    }
}

/// The `agent_session` object Herdr reports for a pane running an agent.
/// `kind` (id vs path) makes no difference to the resume argv, so it is
/// left unparsed.
#[derive(Debug, Clone, Deserialize)]
struct AgentSessionRef {
    source: String,
    agent: String,
    value: String,
}

/// One pane of the workspace being moved, from the source session's
/// `pane.list` (fresh at execution time, since panes change).
#[derive(Debug, Clone, Deserialize)]
struct WizardPane {
    pane_id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    foreground_cwd: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    agent_session: Option<AgentSessionRef>,
}

impl WizardPane {
    fn cwd(&self) -> Option<&str> {
        self.foreground_cwd.as_deref().or(self.cwd.as_deref())
    }
}

/// What "Move to another session…" moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransplantScope {
    Pane,
    Workspace,
}

/// How the wizard ended, so the palette knows whether to close the popup or
/// re-open its root action list.
#[derive(Debug, PartialEq, Eq)]
pub enum WizardExit {
    /// A move executed or the user cancelled (Ctrl-C, or closed the popup):
    /// the palette is done.
    Done,
    /// Esc on the wizard's first level: re-open the root palette.
    Back,
}

#[derive(Serialize)]
struct PaneFocusParams {
    pane_id: String,
}

#[derive(Debug)]
enum Level {
    DestinationKind,
    ChooseTab,
    ChooseSplit { tab_id: String, breadcrumb: String },
    ChooseSession { scope: TransplantScope },
}

/// What selecting a row does: execute a `pane.move` (wizard done) or descend
/// one level.
#[derive(Debug)]
enum Step {
    Execute(PaneMoveDestination),
    Push(Level),
    Transplant {
        scope: TransplantScope,
        session: WizardSession,
    },
}

pub fn run(client: &SocketClient, pane_id: &str) -> Result<WizardExit, String> {
    let snapshot = Snapshot::fetch(client, pane_id)?;
    let mut stack = vec![Level::DestinationKind];
    while let Some(level) = stack.last() {
        let (placeholder, choices) = build_level(&snapshot, level);
        let picker = Picker {
            placeholder: &placeholder,
            empty_message: "No matches",
            order: None,
        };
        match pick_nav(picker, choices)? {
            PickOutcome::Selected(Step::Execute(destination)) => {
                let moved_pane_id = pane_move(client, pane_id, destination, true)?;
                client.send(
                    "cast:pane-focus",
                    "pane.focus",
                    PaneFocusParams {
                        pane_id: moved_pane_id,
                    },
                )?;
                return Ok(WizardExit::Done);
            }
            PickOutcome::Selected(Step::Transplant { scope, session }) => {
                match scope {
                    TransplantScope::Pane => transplant(client, &snapshot, &session, pane_id)?,
                    TransplantScope::Workspace => {
                        transplant_workspace(client, &snapshot, &session)?
                    }
                }
                return Ok(WizardExit::Done);
            }
            PickOutcome::Selected(Step::Push(next)) => stack.push(next),
            PickOutcome::Back => {
                stack.pop();
            }
            PickOutcome::Cancel => return Ok(WizardExit::Done),
        }
    }
    Ok(WizardExit::Back)
}

fn build_level(snapshot: &Snapshot, level: &Level) -> (String, Vec<Choice<Step>>) {
    match level {
        Level::DestinationKind => destination_kind(snapshot),
        Level::ChooseTab => choose_tab(snapshot),
        Level::ChooseSplit { tab_id, breadcrumb } => choose_split(tab_id, breadcrumb),
        Level::ChooseSession { scope } => choose_session(snapshot, *scope),
    }
}

fn destination_kind(snapshot: &Snapshot) -> (String, Vec<Choice<Step>>) {
    let current = snapshot
        .current_workspace_id()
        .and_then(|workspace_id| snapshot.workspace_label(workspace_id))
        .map(str::to_owned);
    let mut search_here = "new tab here current workspace".to_string();
    if let Some(label) = &current {
        search_here.push(' ');
        search_here.push_str(label);
    }
    let mut choices = vec![
        Choice::new(
            Step::Execute(PaneMoveDestination::NewTab {
                label: None,
                workspace_id: None,
            }),
            "New tab here",
            None::<String>,
            search_here,
        ),
        Choice::new(
            Step::Push(Level::ChooseTab),
            "Choose workspace or tab…",
            None::<String>,
            "choose workspace tab existing move into fuzzy tree",
        ),
        Choice::new(
            Step::Execute(PaneMoveDestination::NewWorkspace {
                label: None,
                tab_label: None,
            }),
            "New workspace",
            None::<String>,
            "new workspace detach",
        ),
        // Hidden when the CLI reports no other running sessions.
    ];
    if !snapshot.sessions.is_empty() {
        choices.push(Choice::new(
            Step::Push(Level::ChooseSession {
                scope: TransplantScope::Pane,
            }),
            "Move pane to another session…",
            None::<String>,
            "move pane send another session other herdr running transplant",
        ));
        if snapshot.current_workspace_id().is_some() {
            choices.push(Choice::new(
                Step::Push(Level::ChooseSession {
                    scope: TransplantScope::Workspace,
                }),
                "Move workspace to another session…",
                None::<String>,
                "move workspace send bulk another session other herdr running transplant",
            ));
        }
    }
    ("Search destinations".to_string(), choices)
}

fn choose_session(snapshot: &Snapshot, scope: TransplantScope) -> (String, Vec<Choice<Step>>) {
    let choices = snapshot
        .sessions
        .iter()
        .map(|session| {
            Choice::new(
                Step::Transplant {
                    scope,
                    session: session.clone(),
                },
                session.name.clone(),
                None::<String>,
                format!("move into {} session {}", session.name, session.socket_path),
            )
        })
        .collect();
    let placeholder = match scope {
        TransplantScope::Pane => "Move pane into which session",
        TransplantScope::Workspace => "Move workspace into which session",
    };
    (placeholder.to_string(), choices)
}

fn choose_tab(snapshot: &Snapshot) -> (String, Vec<Choice<Step>>) {
    let mut choices = Vec::new();
    for workspace in &snapshot.workspaces {
        let root_index = choices.len();
        choices.push(workspace_choice(
            workspace,
            snapshot.current_workspace_id() == Some(workspace.workspace_id.as_str()),
        ));
        choices.extend(
            snapshot
                .tabs
                .iter()
                .filter(|tab| tab.workspace_id == workspace.workspace_id)
                .filter(|tab| snapshot.current_tab_id() != Some(tab.tab_id.as_str()))
                .map(|tab| tab_choice(tab, workspace, root_index)),
        );
    }
    ("Search workspaces and tabs".to_string(), choices)
}

fn workspace_choice(workspace: &WizardWorkspace, current: bool) -> Choice<Step> {
    let mut metadata = Vec::new();
    if let Some(location) = space::describe(&workspace.tokens) {
        metadata.push(location);
    }
    metadata.push(format!("#{}", workspace.number));
    if current {
        metadata.push("current".to_string());
    }
    metadata.push(format!(
        "{} {}",
        workspace.pane_count,
        plural(workspace.pane_count, "pane", "panes")
    ));
    metadata.push(format!(
        "{} {}",
        workspace.tab_count,
        plural(workspace.tab_count, "tab", "tabs")
    ));
    if workspace.agent_status != "unknown" {
        metadata.push(workspace.agent_status.clone());
    }
    metadata.push("new tab here".to_string());

    let mut search = format!(
        "{} {} {}",
        workspace.label, workspace.workspace_id, workspace.agent_status
    );
    for value in workspace.tokens.values() {
        search.push(' ');
        search.push_str(value);
    }
    search.push_str(" new tab");

    let title = format!("{} ({} panes)", workspace.label, workspace.pane_count);
    Choice::new(
        Step::Execute(PaneMoveDestination::NewTab {
            label: None,
            workspace_id: Some(workspace.workspace_id.clone()),
        }),
        title,
        Some(metadata.join(" · ")),
        search,
    )
    .tree_root()
    .current(current)
}

fn tab_choice(tab: &WizardTab, workspace: &WizardWorkspace, parent: usize) -> Choice<Step> {
    let title = format!("#{} {}", tab.number, tab.label)
        .trim_end()
        .to_string();
    let mut detail = format!(
        "{} {}",
        tab.pane_count,
        plural(tab.pane_count, "pane", "panes")
    );
    if tab.agent_status != "unknown" {
        detail.push_str(" · ");
        detail.push_str(&tab.agent_status);
    }
    let search = format!(
        "{} {} #{} {} {} existing tab",
        workspace.label, tab.label, tab.number, tab.agent_status, tab.tab_id
    );
    Choice::new(
        Step::Push(Level::ChooseSplit {
            tab_id: tab.tab_id.clone(),
            breadcrumb: format!("{} › {title}", workspace.label),
        }),
        title.clone(),
        Some(detail),
        search,
    )
    .child_of(parent)
}

fn choose_split(tab_id: &str, breadcrumb: &str) -> (String, Vec<Choice<Step>>) {
    let rows = [
        (
            SplitDirection::Right,
            "Split right",
            "split right horizontal side by side column",
        ),
        (
            SplitDirection::Down,
            "Split below",
            "split below down vertical stacked row",
        ),
    ];
    let split = |split: SplitDirection| PaneMoveDestination::Tab {
        tab_id: tab_id.to_owned(),
        target_pane_id: None,
        split,
        ratio: None,
    };
    let choices = rows
        .into_iter()
        .map(|(direction, title, search)| {
            Choice::new(
                Step::Execute(split(direction)),
                title,
                None::<String>,
                search,
            )
        })
        .collect();
    (format!("Split in {breadcrumb}"), choices)
}

#[derive(Serialize)]
struct MoveWorkspaceParams<'a> {
    label: Option<&'a str>,
    cwd: Option<&'a str>,
    focus: bool,
    source_workspace_id: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    env: BTreeMap<String, String>,
}

#[derive(Serialize)]
struct PaneRenameParams<'a> {
    pane_id: &'a str,
    label: Option<&'a str>,
}

#[derive(Serialize)]
struct AgentStartParams<'a> {
    name: &'a str,
    kind: &'a str,
    pane_id: &'a str,
    args: &'a [String],
    timeout_ms: Option<u64>,
}

#[derive(Serialize)]
struct MoveToastParams {
    title: String,
    body: Option<String>,
    position: Option<String>,
    sound: &'static str,
}

/// Move the snapshot's source pane into another running session: same
/// workspace label, same pane name, same cwd, the agent resumed when the
/// source pane reports a resumable `agent_session`, and the source pane
/// closed at the end. Finishes with a toast in the current session, since
/// focus cannot follow across sessions.
fn transplant(
    client: &SocketClient,
    snapshot: &Snapshot,
    session: &WizardSession,
    pane_id: &str,
) -> Result<(), String> {
    let source = snapshot
        .source
        .as_ref()
        .ok_or_else(|| "could not load the focused pane's details".to_string())?;
    let cwd = source
        .cwd()
        .ok_or_else(|| "the focused pane has no working directory to move".to_string())?;
    let workspace_label = snapshot
        .current_workspace_id()
        .and_then(|workspace_id| snapshot.workspace_label(workspace_id));

    let target = SocketClient::new(&session.socket_path);
    let response = target.send(
        "cast:move-workspace",
        "workspace.create",
        MoveWorkspaceParams {
            label: workspace_label,
            cwd: Some(cwd),
            focus: false,
            source_workspace_id: None,
            env: BTreeMap::new(),
        },
    )?;
    let root_pane_id = response
        .pointer("/result/root_pane/pane_id")
        .and_then(Value::as_str)
        .ok_or_else(|| "workspace.create response missing root pane".to_string())?
        .to_owned();

    if let Some(title) = &source.title {
        target.send(
            "cast:move-pane-rename",
            "pane.rename",
            PaneRenameParams {
                pane_id: &root_pane_id,
                label: Some(title),
            },
        )?;
    }

    let agent_note = match &source.agent_session {
        Some(session_ref) => match resume_args(session_ref) {
            Some(args) => {
                let name = start_resumed_agent(
                    &target,
                    &root_pane_id,
                    source.agent.as_deref(),
                    &session_ref.agent,
                    &args,
                )?;
                Some(match name {
                    Some(name) => format!("resumed {} as \"{}\"", session_ref.agent, name),
                    None => format!("resumed {}", session_ref.agent),
                })
            }
            None => Some(format!(
                "\"{}\" reports no resumable session",
                session_ref.agent
            )),
        },
        None if source.agent.is_some() => {
            Some("agent reports no session; moved shell only".to_string())
        }
        None => None,
    };

    // Only reached after the full move succeeded: a failed agent start
    // returns early and keeps the source pane so nothing is lost.
    client.send(
        "cast:move-close-source",
        "pane.close",
        crate::palette::PaneTargetParams {
            pane_id: pane_id.to_owned(),
        },
    )?;
    let _ = client.send(
        "cast:move-toast",
        "notification.show",
        MoveToastParams {
            title: format!("pane moved to session \"{}\"", session.name),
            body: agent_note,
            position: None,
            sound: "none",
        },
    );
    Ok(())
}

#[derive(Serialize)]
struct TabCreateParams<'a> {
    workspace_id: &'a str,
    label: Option<&'a str>,
    cwd: Option<&'a str>,
    focus: bool,
}

#[derive(Serialize)]
struct PaneSplitParams<'a> {
    target_pane_id: &'a str,
    direction: &'a str,
    cwd: Option<&'a str>,
    ratio: Option<f32>,
    focus: bool,
}

#[derive(Serialize)]
struct WorkspaceCloseParams<'a> {
    workspace_id: &'a str,
    close_group: bool,
}

#[derive(Serialize)]
struct PaneListParams {
    workspace_id: Option<String>,
}

/// One step of a tab's layout replay. Pane indices refer to the replay's
/// created-pane list; index 0 is the tab's freshly created root pane.
#[derive(Debug, Clone, PartialEq)]
enum ReplayStep {
    /// Split a previously created pane to make room for the `second`
    /// subtree, using the exported direction and ratio verbatim (Herdr's
    /// `split_at` keeps the anchor as `first`, so the round trip is exact).
    Split {
        anchor: usize,
        direction: &'static str,
        ratio: f32,
        cwd_source_pane_id: String,
    },
    /// Associate a source pane with its target counterpart.
    Pair { source_pane_id: String, pane: usize },
}

fn plan_replay(node: &LayoutNode) -> Result<Vec<ReplayStep>, String> {
    let mut steps = Vec::new();
    let mut created = 0usize;
    plan_replay_node(node, 0, &mut steps, &mut created)?;
    Ok(steps)
}

fn plan_replay_node(
    node: &LayoutNode,
    pane: usize,
    steps: &mut Vec<ReplayStep>,
    created: &mut usize,
) -> Result<(), String> {
    match node {
        LayoutNode::Pane { pane_id } => steps.push(ReplayStep::Pair {
            source_pane_id: pane_id
                .clone()
                .ok_or_else(|| "layout export pane missing id".to_string())?,
            pane,
        }),
        LayoutNode::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            steps.push(ReplayStep::Split {
                anchor: pane,
                direction: split_direction_name(*direction),
                ratio: *ratio,
                cwd_source_pane_id: first_leaf_pane_id(second)
                    .ok_or_else(|| "layout export pane missing id".to_string())?,
            });
            *created += 1;
            let new_pane = *created;
            plan_replay_node(first, pane, steps, created)?;
            plan_replay_node(second, new_pane, steps, created)?;
        }
    }
    Ok(())
}

fn first_leaf_pane_id(node: &LayoutNode) -> Option<String> {
    match node {
        LayoutNode::Pane { pane_id } => pane_id.clone(),
        LayoutNode::Split { first, .. } => first_leaf_pane_id(first),
    }
}

fn split_direction_name(direction: Direction) -> &'static str {
    match direction {
        Direction::Right => "right",
        Direction::Down => "down",
    }
}

/// Execute one tab's replay plan against the target session, appending
/// (source pane id, target pane id) pairs.
fn execute_replay(
    steps: &[ReplayStep],
    root_pane_id: &str,
    sources: &BTreeMap<String, WizardPane>,
    target: &SocketClient,
    pairs: &mut Vec<(String, String)>,
) -> Result<(), String> {
    let mut created = vec![root_pane_id.to_owned()];
    for step in steps {
        match step {
            ReplayStep::Split {
                anchor,
                direction,
                ratio,
                cwd_source_pane_id,
            } => {
                let cwd = sources.get(cwd_source_pane_id).and_then(WizardPane::cwd);
                let response = target.send(
                    "cast:move-ws-split",
                    "pane.split",
                    PaneSplitParams {
                        target_pane_id: &created[*anchor],
                        direction,
                        cwd,
                        ratio: Some(*ratio),
                        focus: false,
                    },
                )?;
                created.push(
                    response
                        .pointer("/result/pane/pane_id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| "pane.split missing resulting pane id".to_string())?
                        .to_owned(),
                );
            }
            ReplayStep::Pair {
                source_pane_id,
                pane,
            } => pairs.push((source_pane_id.clone(), created[*pane].clone())),
        }
    }
    Ok(())
}

/// Move every tab and pane of the current workspace into another running
/// session. Structure is re-read from the source session at execution time
/// (panes change while the wizard sits open), each tab's exported layout is
/// replayed with `workspace.create` / `tab.create` / `pane.split`, every
/// recreated pane gets the source pane's name and a resumed agent when
/// possible, and the source workspace only closes after every pane was
/// recreated successfully.
fn transplant_workspace(
    client: &SocketClient,
    snapshot: &Snapshot,
    session: &WizardSession,
) -> Result<(), String> {
    let workspace_id = snapshot
        .current_workspace_id()
        .ok_or_else(|| "current workspace unavailable".to_string())?
        .to_owned();
    let workspace_label = snapshot.workspace_label(&workspace_id).map(str::to_owned);

    let mut tabs = list_tabs_in(client, &workspace_id)?;
    tabs.sort_by_key(|tab| tab.number);
    if tabs.is_empty() {
        return Err("current workspace has no tabs".to_string());
    }
    let panes = list_workspace_panes(client, &workspace_id)?;
    let sources: BTreeMap<String, WizardPane> = panes
        .into_iter()
        .map(|pane| (pane.pane_id.clone(), pane))
        .collect();

    let target = SocketClient::new(&session.socket_path);
    let mut target_workspace_id: Option<String> = None;
    let mut pairs: Vec<(String, String)> = Vec::new();

    for tab in &tabs {
        let root_tree = layout_export_tab(client, &tab.tab_id)?;
        let steps = plan_replay(&root_tree)?;
        let cwd = steps
            .iter()
            .find_map(|step| match step {
                ReplayStep::Pair {
                    source_pane_id,
                    pane: 0,
                } => sources.get(source_pane_id).and_then(WizardPane::cwd),
                _ => None,
            })
            .or_else(|| {
                first_leaf_pane_id(&root_tree)
                    .and_then(|pane_id| sources.get(&pane_id))
                    .and_then(WizardPane::cwd)
            });

        let root_pane_id = match &target_workspace_id {
            None => {
                let response = target.send(
                    "cast:move-ws-root",
                    "workspace.create",
                    MoveWorkspaceParams {
                        label: workspace_label.as_deref(),
                        cwd,
                        focus: false,
                        source_workspace_id: None,
                        env: BTreeMap::new(),
                    },
                )?;
                target_workspace_id = Some(
                    response
                        .pointer("/result/workspace/workspace_id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            "workspace.create response missing workspace id".to_string()
                        })?
                        .to_owned(),
                );
                response
                    .pointer("/result/root_pane/pane_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "workspace.create response missing root pane".to_string())?
                    .to_owned()
            }
            Some(workspace_id) => {
                let response = target.send(
                    "cast:move-ws-tab",
                    "tab.create",
                    TabCreateParams {
                        workspace_id,
                        label: Some(&tab.label),
                        cwd,
                        focus: false,
                    },
                )?;
                response
                    .pointer("/result/root_pane/pane_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "tab.create response missing root pane".to_string())?
                    .to_owned()
            }
        };

        execute_replay(&steps, &root_pane_id, &sources, &target, &mut pairs)?;
    }

    // Nothing may be left behind: every pane the source session listed
    // must have been recreated, or we abort before closing the workspace.
    for source_pane_id in sources.keys() {
        if !pairs.iter().any(|(id, _)| id == source_pane_id) {
            return Err(format!(
                "pane {source_pane_id} missing from the layout export; workspace left untouched"
            ));
        }
    }

    let mut resumed = 0usize;
    let mut unresumable = 0usize;
    for (source_pane_id, target_pane_id) in &pairs {
        let source = sources
            .get(source_pane_id)
            .ok_or_else(|| format!("pane {source_pane_id} vanished from the source workspace"))?;
        if let Some(title) = &source.title {
            target.send(
                "cast:move-ws-rename",
                "pane.rename",
                PaneRenameParams {
                    pane_id: target_pane_id,
                    label: Some(title),
                },
            )?;
        }
        if let Some(session_ref) = &source.agent_session {
            match resume_args(session_ref) {
                Some(args) => {
                    start_resumed_agent(
                        &target,
                        target_pane_id,
                        source.agent.as_deref(),
                        &session_ref.agent,
                        &args,
                    )?;
                    resumed += 1;
                }
                None => unresumable += 1,
            }
        }
    }

    // Reached only after every pane was recreated: the source workspace is
    // safe to close. (Herdr rejects closing a workspace with linked
    // worktree workspaces without close_group=true; that error surfaces and
    // leaves the source intact.)
    client.send(
        "cast:move-ws-close",
        "workspace.close",
        WorkspaceCloseParams {
            workspace_id: &workspace_id,
            close_group: false,
        },
    )?;

    let mut bits = vec![
        format!("{} {}", tabs.len(), plural(tabs.len(), "tab", "tabs")),
        format!("{} {}", pairs.len(), plural(pairs.len(), "pane", "panes")),
    ];
    if resumed > 0 {
        bits.push(format!("{} {}", resumed, plural_agents(resumed)));
    }
    if unresumable > 0 {
        bits.push(format!(
            "{} without resumable sessions",
            plural_agents(unresumable)
        ));
    }
    let _ = client.send(
        "cast:move-ws-toast",
        "notification.show",
        MoveToastParams {
            title: format!("workspace moved to session \"{}\"", session.name),
            body: Some(bits.join(" · ")),
            position: None,
            sound: "none",
        },
    );
    Ok(())
}

fn plural_agents(count: usize) -> String {
    format!("{} agent{}", count, if count == 1 { "" } else { "s" })
}

fn list_tabs_in(client: &SocketClient, workspace_id: &str) -> Result<Vec<WizardTab>, String> {
    let response = client.send(
        "cast:ws-tab-list",
        "tab.list",
        TabListParams {
            workspace_id: Some(workspace_id.to_owned()),
        },
    )?;
    serde_json::from_value(
        response
            .pointer("/result/tabs")
            .cloned()
            .ok_or_else(|| "tab.list missing tabs".to_string())?,
    )
    .map_err(|error| format!("failed to parse tab.list response: {error}"))
}

fn list_workspace_panes(
    client: &SocketClient,
    workspace_id: &str,
) -> Result<Vec<WizardPane>, String> {
    let response = client.send(
        "cast:ws-pane-list",
        "pane.list",
        PaneListParams {
            workspace_id: Some(workspace_id.to_owned()),
        },
    )?;
    serde_json::from_value(
        response
            .pointer("/result/panes")
            .cloned()
            .ok_or_else(|| "pane.list missing panes".to_string())?,
    )
    .map_err(|error| format!("failed to parse pane.list response: {error}"))
}

/// Start the source pane's agent in the freshly created pane, resuming its
/// session. A custom pane name is preserved when free (`agent_name_taken`
/// retries with `-2`, `-3`, …). An unnamed source pane must stay unnamed:
/// shell-launched agents in Herdr have no name and display their kind
/// ("pi"), so instead of inventing one we satisfy `agent.start`'s required
/// name with a scratch value and clear it right after — anything else
/// would show fake suffixes for agents that never had a name.
/// Returns the kept custom name, or None when the agent runs unnamed.
fn start_resumed_agent(
    target: &SocketClient,
    root_pane_id: &str,
    display_name: Option<&str>,
    kind: &str,
    args: &[String],
) -> Result<Option<String>, String> {
    let Some(base) = display_name else {
        start_agent_with_free_name(target, root_pane_id, kind, args, "cast-move")?;
        target.send(
            "cast:agent-clear-name",
            "agent.rename",
            AgentRenameParams {
                target: root_pane_id,
                name: None,
            },
        )?;
        return Ok(None);
    };
    start_agent_with_free_name(target, root_pane_id, kind, args, base).map(Some)
}

#[derive(Serialize)]
struct AgentRenameParams<'a> {
    target: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
}

fn start_agent_with_free_name(
    target: &SocketClient,
    root_pane_id: &str,
    kind: &str,
    args: &[String],
    base: &str,
) -> Result<String, String> {
    for attempt in 1..=5 {
        let name = candidate_name(base, attempt);
        match target.send(
            "cast:agent-start",
            "agent.start",
            AgentStartParams {
                name: &name,
                kind,
                pane_id: root_pane_id,
                args,
                timeout_ms: None,
            },
        ) {
            Ok(_) => return Ok(name),
            // Herdr reports a name conflict as agent_name_taken (the
            // DuplicateName variant's wire code).
            Err(error) if error.contains("agent_name_taken") => continue,
            Err(error) => return Err(format!("failed to start {kind}: {error}")),
        }
    }
    Err(format!("could not pick a unique agent name for \"{base}\""))
}

/// Herdr agent names match `^[a-z][a-z0-9_-]*$` with a 32-character limit,
/// so dedupe suffixes are `-2`, `-3`, … with the base trimmed to fit.
fn candidate_name(base: &str, attempt: usize) -> String {
    let suffix = if attempt == 1 {
        String::new()
    } else {
        format!("-{attempt}")
    };
    let keep = 32usize.saturating_sub(suffix.len());
    let mut name: String = base.chars().take(keep).collect();
    name.push_str(&suffix);
    name
}

/// Resume argv for the agents Herdr itself knows how to resume. Mirrors
/// `agent_resume::plan` in Herdr 0.9.0 (only official `herdr:<agent>`
/// sources qualify); `agent.start` itself prepends the agent executable, so
/// this returns only the flags.
fn resume_args(session_ref: &AgentSessionRef) -> Option<Vec<String>> {
    let value = session_ref.value.clone();
    let args = match (session_ref.source.as_str(), session_ref.agent.as_str()) {
        ("herdr:claude", "claude") => vec!["--resume".into(), value],
        ("herdr:codex", "codex") => vec!["resume".into(), value],
        ("herdr:copilot", "copilot") => vec![format!("--resume={value}")],
        ("herdr:devin", "devin") => vec!["--resume".into(), value],
        ("herdr:droid", "droid") => vec!["--resume".into(), value],
        ("herdr:kimi", "kimi") => vec!["--session".into(), value],
        ("herdr:omp", "omp") => vec![format!("--resume={value}")],
        ("herdr:mastracode", "mastracode") => vec!["--thread".into(), value],
        ("herdr:pi", "pi") => vec!["--session".into(), value],
        ("herdr:hermes", "hermes") => vec!["--resume".into(), value],
        ("herdr:opencode", "opencode") => vec!["--session".into(), value],
        ("herdr:qodercli", "qodercli") => vec!["--resume".into(), value],
        ("herdr:qwen", "qwen") => vec!["--resume".into(), value],
        ("herdr:kilo", "kilo") => vec!["--session".into(), value],
        ("herdr:cursor", "cursor") => vec!["--resume".into(), value],
        ("herdr:antigravity_cli", "agy") => vec!["--conversation".into(), value],
        ("herdr:grok", "grok") => vec!["--resume".into(), value],
        _ => return None,
    };
    Some(args)
}

#[derive(Debug, Deserialize)]
struct SessionListResponse {
    sessions: Vec<SessionRecord>,
}

#[derive(Debug, Deserialize)]
struct SessionRecord {
    name: String,
    running: bool,
    socket_path: String,
}

/// Sessions come from the `herdr session list --json` CLI — the socket
/// protocol has no session listing method. Best-effort: any failure yields
/// an empty list, which hides the move row entirely.
fn fetch_sessions(current_socket: &str) -> Vec<WizardSession> {
    let herdr = std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".to_string());
    let output = Command::new(herdr)
        .args(["session", "list", "--json"])
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let Ok(parsed) = serde_json::from_slice::<SessionListResponse>(&output.stdout) else {
        return Vec::new();
    };
    filter_sessions(parsed.sessions, current_socket)
}

fn filter_sessions(records: Vec<SessionRecord>, current_socket: &str) -> Vec<WizardSession> {
    records
        .into_iter()
        .filter(|record| record.running && record.socket_path != current_socket)
        .map(|record| WizardSession {
            name: record.name,
            socket_path: record.socket_path,
        })
        .collect()
}

fn fetch_source(client: &SocketClient, pane_id: &str) -> Result<SourcePane, String> {
    let response = client.send(
        "cast:clone-source",
        "pane.get",
        crate::palette::PaneTargetParams {
            pane_id: pane_id.to_owned(),
        },
    )?;
    serde_json::from_value(
        response
            .pointer("/result/pane")
            .cloned()
            .ok_or_else(|| "pane.get missing pane".to_string())?,
    )
    .map_err(|error| format!("failed to parse pane.get response: {error}"))
}

fn plural<'a>(count: usize, singular: &'a str, plural: &'a str) -> &'a str {
    if count == 1 {
        singular
    } else {
        plural
    }
}

fn list_workspaces(client: &SocketClient) -> Result<Vec<WizardWorkspace>, String> {
    let response = client.send("cast:workspace-list", "workspace.list", EmptyParams {})?;
    serde_json::from_value(
        response
            .pointer("/result/workspaces")
            .cloned()
            .ok_or_else(|| "workspace.list missing workspaces".to_string())?,
    )
    .map_err(|error| format!("failed to parse workspace.list response: {error}"))
}

fn list_tabs(client: &SocketClient) -> Result<Vec<WizardTab>, String> {
    let response = client.send(
        "cast:tab-list",
        "tab.list",
        TabListParams { workspace_id: None },
    )?;
    serde_json::from_value(
        response
            .pointer("/result/tabs")
            .cloned()
            .ok_or_else(|| "tab.list missing tabs".to_string())?,
    )
    .map_err(|error| format!("failed to parse tab.list response: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(id: &str, number: usize) -> WizardWorkspace {
        WizardWorkspace {
            workspace_id: id.to_string(),
            number,
            label: format!("ws-{id}"),
            pane_count: 2,
            tab_count: 2,
            agent_status: "unknown".to_string(),
            tokens: BTreeMap::new(),
        }
    }

    fn tab(tab_id: &str, workspace_id: &str, number: usize) -> WizardTab {
        WizardTab {
            tab_id: tab_id.to_string(),
            workspace_id: workspace_id.to_string(),
            number,
            label: format!("tab-{tab_id}"),
            pane_count: 1,
            agent_status: "unknown".to_string(),
        }
    }

    fn snapshot() -> Snapshot {
        Snapshot {
            workspaces: vec![workspace("w1", 1), workspace("w2", 2)],
            tabs: vec![tab("t1", "w1", 1), tab("t2", "w1", 2), tab("t3", "w2", 1)],
            current: Some(CurrentLocation {
                workspace_id: "w1".to_string(),
                tab_id: "t1".to_string(),
            }),
            sessions: Vec::new(),
            source: None,
        }
    }

    fn matches_json(destination: &PaneMoveDestination, expected: serde_json::Value) {
        assert_eq!(serde_json::to_value(destination).unwrap(), expected);
    }

    #[test]
    fn destination_kind_leads_with_a_new_tab_in_the_current_workspace() {
        let (placeholder, choices) = destination_kind(&snapshot());
        assert_eq!(placeholder, "Search destinations");
        assert_eq!(choices.len(), 3);

        let Step::Execute(destination) = &choices[0].value else {
            panic!("first destination must execute directly");
        };
        matches_json(
            destination,
            serde_json::json!({"type": "new_tab", "label": null, "workspace_id": null}),
        );
        assert!(choices[0].search_text.contains("ws-w1"));

        let Step::Push(Level::ChooseTab) = &choices[1].value else {
            panic!("second destination must descend into the tab tree");
        };
        let Step::Execute(destination) = &choices[2].value else {
            panic!("third destination must execute directly");
        };
        matches_json(
            destination,
            serde_json::json!({"type": "new_workspace", "label": null, "tab_label": null}),
        );
    }

    #[test]
    fn choose_tab_builds_a_workspace_tree_without_the_current_tab() {
        let (_, choices) = choose_tab(&snapshot());
        // Two workspace roots plus tabs t2 and t3 (current tab t1 excluded).
        assert_eq!(choices.len(), 4);

        let Step::Execute(destination) = &choices[0].value else {
            panic!("workspace rows create a new tab in that workspace");
        };
        matches_json(
            destination,
            serde_json::json!({"type": "new_tab", "label": null, "workspace_id": "w1"}),
        );

        let Step::Push(Level::ChooseSplit { tab_id, breadcrumb }) = &choices[1].value else {
            panic!("tab rows descend into the split level");
        };
        assert_eq!(tab_id, "t2");
        assert!(breadcrumb.contains("ws-w1"));

        let Step::Push(Level::ChooseSplit { tab_id, .. }) = &choices[3].value else {
            panic!("tab rows descend into the split level");
        };
        assert_eq!(tab_id, "t3");
    }

    #[test]
    fn choose_tab_survives_a_missing_current_location() {
        let mut snapshot = snapshot();
        snapshot.current = None;
        let (_, choices) = choose_tab(&snapshot);
        // Nothing excluded: both workspaces plus all three tabs.
        assert_eq!(choices.len(), 5);
    }

    #[test]
    fn destination_kind_hides_session_moves_without_other_sessions() {
        let (_, choices) = destination_kind(&snapshot());
        assert_eq!(choices.len(), 3);

        let mut snapshot = snapshot();
        snapshot.sessions.push(WizardSession {
            name: "factorial".to_string(),
            socket_path: "/tmp/herdr/sessions/factorial/herdr.sock".to_string(),
        });
        let (_, choices) = destination_kind(&snapshot);
        assert_eq!(choices.len(), 5);
        assert_eq!(choices[3].title, "Move pane to another session…");
        assert_eq!(choices[4].title, "Move workspace to another session…");
    }

    #[test]
    fn destination_kind_hides_workspace_move_without_a_current_workspace() {
        let mut snapshot = snapshot();
        snapshot.current = None;
        snapshot.sessions.push(WizardSession {
            name: "factorial".to_string(),
            socket_path: "/tmp/herdr/sessions/factorial/herdr.sock".to_string(),
        });
        let (_, choices) = destination_kind(&snapshot);
        assert_eq!(choices.len(), 4);
        assert_eq!(choices[3].title, "Move pane to another session…");
    }

    #[test]
    fn choose_session_offers_one_row_per_session() {
        let mut snapshot = snapshot();
        for name in ["sandboxes", "factorial"] {
            snapshot.sessions.push(WizardSession {
                name: name.to_string(),
                socket_path: format!("/tmp/herdr/sessions/{name}/herdr.sock"),
            });
        }
        let (placeholder, choices) = choose_session(&snapshot, TransplantScope::Pane);
        assert_eq!(placeholder, "Move pane into which session");
        assert_eq!(choices.len(), 2);
        let Step::Transplant { scope, session } = &choices[1].value else {
            panic!("session rows transplant into the selected session");
        };
        assert_eq!(*scope, TransplantScope::Pane);
        assert_eq!(session.name, "factorial");
        assert!(choices[1].search_text.contains("factorial"));

        let (placeholder, _) = choose_session(&snapshot, TransplantScope::Workspace);
        assert_eq!(placeholder, "Move workspace into which session");
    }

    fn leaf(pane_id: &str) -> LayoutNode {
        LayoutNode::Pane {
            pane_id: Some(pane_id.to_string()),
        }
    }

    fn split(
        direction: Direction,
        ratio: f32,
        first: LayoutNode,
        second: LayoutNode,
    ) -> LayoutNode {
        LayoutNode::Split {
            direction,
            ratio,
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    #[test]
    fn replay_plan_pairs_a_single_pane_tab_with_the_root() {
        let steps = plan_replay(&leaf("p1")).unwrap();
        assert_eq!(
            steps,
            vec![ReplayStep::Pair {
                source_pane_id: "p1".to_string(),
                pane: 0
            }]
        );
    }

    #[test]
    fn replay_plan_reproduces_nested_split_geometry() {
        // right
        // ├── down(0.25): a / b
        // └── c
        let tree = split(
            Direction::Right,
            0.5,
            split(Direction::Down, 0.25, leaf("a"), leaf("b")),
            leaf("c"),
        );
        let steps = plan_replay(&tree).unwrap();
        assert_eq!(
            steps,
            vec![
                ReplayStep::Split {
                    anchor: 0,
                    direction: "right",
                    ratio: 0.5,
                    cwd_source_pane_id: "c".to_string()
                },
                ReplayStep::Split {
                    anchor: 0,
                    direction: "down",
                    ratio: 0.25,
                    cwd_source_pane_id: "b".to_string()
                },
                ReplayStep::Pair {
                    source_pane_id: "a".to_string(),
                    pane: 0
                },
                ReplayStep::Pair {
                    source_pane_id: "b".to_string(),
                    pane: 2
                },
                ReplayStep::Pair {
                    source_pane_id: "c".to_string(),
                    pane: 1
                },
            ]
        );
    }

    #[test]
    fn replay_plan_rejects_leafless_layouts() {
        let tree = LayoutNode::Pane { pane_id: None };
        assert!(plan_replay(&tree).is_err());
    }

    #[test]
    fn session_filter_keeps_only_other_running_sessions() {
        let record = |name: &str, running: bool, socket: &str| SessionRecord {
            name: name.to_string(),
            running,
            socket_path: socket.to_string(),
        };
        let sessions = filter_sessions(
            vec![
                record("default", true, "/tmp/herdr/herdr.sock"),
                record(
                    "factorial",
                    true,
                    "/tmp/herdr/sessions/factorial/herdr.sock",
                ),
                record("stale", false, "/tmp/herdr/sessions/stale/herdr.sock"),
            ],
            "/tmp/herdr/herdr.sock",
        );
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].name, "factorial");
    }

    fn session_ref(source: &str, agent: &str) -> AgentSessionRef {
        AgentSessionRef {
            source: source.to_string(),
            agent: agent.to_string(),
            value: "session-42".to_string(),
        }
    }

    #[test]
    fn resume_args_mirror_herdr_agent_resume_plans() {
        assert_eq!(
            resume_args(&session_ref("herdr:pi", "pi")),
            Some(vec!["--session".to_string(), "session-42".to_string()])
        );
        assert_eq!(
            resume_args(&session_ref("herdr:claude", "claude")),
            Some(vec!["--resume".to_string(), "session-42".to_string()])
        );
        assert_eq!(
            resume_args(&session_ref("herdr:codex", "codex")),
            Some(vec!["resume".to_string(), "session-42".to_string()])
        );
        assert_eq!(
            resume_args(&session_ref("herdr:omp", "omp")),
            Some(vec!["--resume=session-42".to_string()])
        );
    }

    #[test]
    fn candidate_names_follow_herdr_agent_name_rules() {
        assert_eq!(candidate_name("pi", 1), "pi");
        assert_eq!(candidate_name("pi", 2), "pi-2");
        let long = "an-agent-name-that-is-already-very-long";
        let name = candidate_name(long, 13);
        assert_eq!(name, format!("{}-13", &long[..29]));
        assert_eq!(name.len(), 32);
    }

    #[test]
    fn resume_args_reject_unofficial_sources() {
        assert_eq!(resume_args(&session_ref("hook:custom", "pi")), None);
        assert_eq!(resume_args(&session_ref("herdr:pi", "ninja")), None);
    }

    #[test]
    fn split_level_always_sends_the_required_split_field() {
        let (placeholder, choices) = choose_split("t2", "ws-w1 › #2 x");
        assert_eq!(placeholder, "Split in ws-w1 › #2 x");
        assert_eq!(choices.len(), 2);
        let Step::Execute(right) = &choices[0].value else {
            panic!("split rows must execute directly");
        };
        let Step::Execute(down) = &choices[1].value else {
            panic!("split rows must execute directly");
        };
        matches_json(
            right,
            serde_json::json!({
                "type": "tab",
                "tab_id": "t2",
                "target_pane_id": null,
                "split": "right",
                "ratio": null
            }),
        );
        matches_json(
            down,
            serde_json::json!({
                "type": "tab",
                "tab_id": "t2",
                "target_pane_id": null,
                "split": "down",
                "ratio": null
            }),
        );
    }
}
