use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::api::SocketClient;
use crate::picker::{pick_with_detail, Choice, ChoiceStatus, OrderToggle, Picker, ToggleKind};
use crate::space;
use crate::zoxide::{self, RankedDirectory};

const SOCKET_TIMEOUT: Duration = Duration::from_secs(3);
const WORKSPACE_PICKER_VIEW_FILE: &str = "workspace-picker-view";
/// View labels in display order. The persisted view file stores one of these
/// labels; the index here matches the view bitmask indices used by
/// [`picker_choices`].
const WORKSPACE_PICKER_VIEWS: &[&str] = &["spaces", "agents", "panes"];

#[derive(Serialize)]
struct EmptyParams {}

#[derive(Serialize)]
struct WorkspaceCreateParams {
    cwd: String,
    env: BTreeMap<String, String>,
    focus: bool,
    label: Option<String>,
}

#[derive(Serialize)]
struct WorkspaceTarget {
    workspace_id: String,
}

#[derive(Serialize)]
struct PaneListParams {
    workspace_id: Option<String>,
}

#[derive(Serialize)]
struct PaneTarget {
    pane_id: String,
}

#[derive(Debug, PartialEq, Eq)]
enum FocusTarget {
    Workspace(String),
    Pane(String),
}

impl FocusTarget {
    #[cfg(test)]
    fn is_pane(&self) -> bool {
        matches!(self, FocusTarget::Pane(_))
    }
}

#[derive(Debug, PartialEq, Eq)]
enum DirectoryTarget {
    Create(PathBuf),
    Focus(String),
}

struct ExistingWorkspace {
    workspace_id: String,
}

#[derive(Debug, Deserialize)]
struct WorkspaceInfo {
    workspace_id: String,
    number: usize,
    label: String,
    focused: bool,
    pane_count: usize,
    tab_count: usize,
    agent_status: String,
    #[serde(default)]
    tokens: BTreeMap<String, String>,
    worktree: Option<WorkspaceWorktreeInfo>,
}

#[derive(Debug, Deserialize)]
struct WorkspaceWorktreeInfo {
    checkout_path: String,
}

#[derive(Debug, Deserialize)]
struct PaneInfo {
    pane_id: String,
    workspace_id: String,
    focused: bool,
    cwd: Option<String>,
    foreground_cwd: Option<String>,
    label: Option<String>,
    agent: Option<String>,
    title: Option<String>,
    terminal_title: Option<String>,
    terminal_title_stripped: Option<String>,
    display_agent: Option<String>,
    agent_status: String,
    #[serde(default)]
    tokens: BTreeMap<String, String>,
}

pub fn create_from_directory() -> Result<(), String> {
    let client = socket_client()?;
    let workspaces = list_workspaces(&client)?;
    let panes = list_panes(&client)?;
    let existing = workspace_directories(&workspaces, &panes);
    let directories = zoxide::ranked_directories()?;
    if directories.is_empty() {
        return Err("zoxide has no ranked directories".to_string());
    }
    let choices = directories
        .into_iter()
        .map(|directory| {
            let normalized = normalize_path(&directory.path);
            directory_choice(directory, existing.get(&normalized))
        })
        .collect();
    let (target, _alphabetical) = pick_with_detail(
        Picker {
            placeholder: "Search ranked directories",
            empty_message: "No matching directories",
            order: Some(OrderToggle {
                labels: &["zoxide", "alpha"],
                initial: 0,
                kind: ToggleKind::Sort,
            }),
        },
        choices,
        |_| None,
    )?;
    let Some(target) = target else {
        return Ok(());
    };

    match target {
        DirectoryTarget::Create(path) => {
            client.send(
                "cast:workspace-create",
                "workspace.create",
                WorkspaceCreateParams {
                    cwd: path.to_string_lossy().into_owned(),
                    env: BTreeMap::new(),
                    focus: true,
                    label: None,
                },
            )?;
        }
        DirectoryTarget::Focus(workspace_id) => {
            client.send(
                "cast:workspace-focus",
                "workspace.focus",
                WorkspaceTarget { workspace_id },
            )?;
        }
    }
    Ok(())
}

pub fn focus_existing() -> Result<(), String> {
    let client = socket_client()?;
    let workspaces = list_workspaces(&client)?;
    if workspaces.is_empty() {
        return Err("Herdr has no workspaces".to_string());
    }
    let panes = list_panes(&client)?;
    let recency = crate::recency::load();
    let choices = picker_choices(workspaces, panes, &recency);
    let (target, view) = pick_with_detail(
        Picker {
            placeholder: "Search workspaces and panes",
            empty_message: "No matching workspaces or panes",
            order: Some(OrderToggle {
                labels: &["spaces", "agents", "panes"],
                initial: load_workspace_picker_view(),
                kind: ToggleKind::View,
            }),
        },
        choices,
        |_| None,
    )?;
    save_workspace_picker_view(view)?;
    let Some(target) = target else {
        return Ok(());
    };

    match target {
        FocusTarget::Workspace(workspace_id) => {
            client.send(
                "cast:workspace-focus",
                "workspace.focus",
                WorkspaceTarget { workspace_id },
            )?;
        }
        FocusTarget::Pane(pane_id) => {
            client.send("cast:pane-focus", "pane.focus", PaneTarget { pane_id })?;
        }
    }
    Ok(())
}

fn socket_client() -> Result<SocketClient, String> {
    let socket =
        std::env::var("HERDR_SOCKET_PATH").map_err(|_| "HERDR_SOCKET_PATH not set".to_string())?;
    Ok(SocketClient::with_timeout(socket, SOCKET_TIMEOUT))
}

fn load_workspace_picker_view() -> usize {
    let raw = workspace_picker_view_file()
        .and_then(|path| fs::read_to_string(path).ok())
        .unwrap_or_default();
    let label = raw.trim();
    WORKSPACE_PICKER_VIEWS
        .iter()
        .position(|view| *view == label)
        .unwrap_or(0)
}

fn save_workspace_picker_view(view: usize) -> Result<(), String> {
    let Some(path) = workspace_picker_view_file() else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create workspace picker state: {error}"))?;
    }
    let label = WORKSPACE_PICKER_VIEWS
        .get(view)
        .or_else(|| WORKSPACE_PICKER_VIEWS.first())
        .expect("view labels are non-empty");
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&temporary, format!("{label}\n"))
        .map_err(|error| format!("failed to save workspace picker view: {error}"))?;
    fs::rename(&temporary, &path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        format!("failed to activate workspace picker view: {error}")
    })
}

fn workspace_picker_view_file() -> Option<PathBuf> {
    std::env::var_os("HERDR_PLUGIN_STATE_DIR")
        .map(PathBuf::from)
        .map(|directory| directory.join(WORKSPACE_PICKER_VIEW_FILE))
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

fn list_panes(client: &SocketClient) -> Result<Vec<PaneInfo>, String> {
    let response = client.send(
        "cast:pane-list",
        "pane.list",
        PaneListParams { workspace_id: None },
    )?;
    serde_json::from_value(
        response
            .pointer("/result/panes")
            .cloned()
            .ok_or_else(|| "pane.list missing panes".to_string())?,
    )
    .map_err(|error| format!("failed to parse pane.list response: {error}"))
}

fn workspace_directories(
    workspaces: &[WorkspaceInfo],
    panes: &[PaneInfo],
) -> BTreeMap<PathBuf, ExistingWorkspace> {
    let mut directories = BTreeMap::new();
    for workspace in workspaces {
        if let Some(worktree) = &workspace.worktree {
            insert_workspace_directory(&mut directories, workspace, &worktree.checkout_path);
        }
        for pane in panes
            .iter()
            .filter(|pane| pane.workspace_id == workspace.workspace_id)
        {
            if let Some(path) = pane.cwd.as_deref() {
                insert_workspace_directory(&mut directories, workspace, path);
            }
            if let Some(path) = pane.foreground_cwd.as_deref() {
                insert_workspace_directory(&mut directories, workspace, path);
            }
        }
    }
    directories
}

fn insert_workspace_directory(
    directories: &mut BTreeMap<PathBuf, ExistingWorkspace>,
    workspace: &WorkspaceInfo,
    path: &str,
) {
    let path = normalize_path(Path::new(path));
    if workspace.focused || !directories.contains_key(&path) {
        directories.insert(
            path,
            ExistingWorkspace {
                workspace_id: workspace.workspace_id.clone(),
            },
        );
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn directory_choice(
    directory: RankedDirectory,
    existing: Option<&ExistingWorkspace>,
) -> Choice<DirectoryTarget> {
    let search = format!("{} {}", directory.label, directory.display_path);
    let target = existing
        .map(|workspace| DirectoryTarget::Focus(workspace.workspace_id.clone()))
        .unwrap_or_else(|| DirectoryTarget::Create(directory.path.clone()));
    Choice::new(
        target,
        directory.label,
        Some(directory.display_path),
        search,
    )
    .alternate_order(directory.alpha_order)
    .inline_detail(false)
    .with_primary_suffix(format!("{:.0}", directory.score))
    .highlighted(existing.is_some())
    .preserve_primary_order_in_search()
    .match_kind(crate::picker::MatchKind::Zoxide)
    .with_match_text(directory.path.to_string_lossy())
}

#[cfg(test)]
fn workspace_tree_choices(
    workspaces: Vec<WorkspaceInfo>,
    panes: Vec<PaneInfo>,
) -> Vec<Choice<FocusTarget>> {
    picker_choices(workspaces, panes, &[])
}

/// Build the choices for every view of the workspace picker.
///
/// - View 0 ("spaces"): workspace -> pane tree, plus agent-only rows hidden
///   from this view.
/// - View 1 ("agents"): flat list of agent panes, ranked by agent status.
/// - View 2 ("panes"): flat list of every pane (shells and agents), ranked by
///   most-recent-focus first via `recency`, falling back to workspace order.
///
/// `recency` is most-recent-first; stale ids not in `panes` are ignored.
fn picker_choices(
    workspaces: Vec<WorkspaceInfo>,
    panes: Vec<PaneInfo>,
    recency: &[String],
) -> Vec<Choice<FocusTarget>> {
    let workspace_labels: BTreeMap<String, String> = workspaces
        .iter()
        .map(|workspace| (workspace.workspace_id.clone(), workspace.label.clone()))
        .collect();
    let mut choices = Vec::new();
    // Build the spaces tree and the agents flat list first, exactly as before.
    for workspace in workspaces {
        let root_index = choices.len();
        let workspace_id = workspace.workspace_id.clone();
        let workspace_label = workspace.label.clone();
        choices.push(workspace_choice(workspace).primary_only());
        let workspace_panes = panes
            .iter()
            .filter(|pane| pane.workspace_id == workspace_id)
            .collect::<Vec<_>>();
        choices.extend(
            workspace_panes
                .iter()
                .map(|pane| pane_choice(pane, root_index).primary_only()),
        );
        choices.extend(
            workspace_panes
                .into_iter()
                .filter(|pane| pane.agent_name().is_some())
                .map(|pane| agent_choice(pane, &workspace_label).alternate_only()),
        );
    }
    // View 2: every pane flat, most-recent-first.
    choices.extend(pane_recency_choices(&panes, recency, &workspace_labels));
    choices
}

/// Flat pane choices for the "panes" view, ordered most-recent-focus first.
/// `recency` is most-recent-first; panes absent from the log keep their
/// workspace/pane order and sort after every pane that was ever focused.
fn pane_recency_choices(
    panes: &[PaneInfo],
    recency: &[String],
    workspace_labels: &BTreeMap<String, String>,
) -> Vec<Choice<FocusTarget>> {
    let mut fallback = usize::MAX;
    panes
        .iter()
        .map(|pane| {
            let workspace_label = workspace_labels
                .get(&pane.workspace_id)
                .cloned()
                .unwrap_or_else(|| pane.workspace_id.clone());
            let recency_rank = match recency.iter().position(|id| id == &pane.pane_id) {
                Some(position) => position,
                None => {
                    // Preserve stable workspace/pane order among unseen panes
                    // while keeping them after every focused pane.
                    fallback = fallback.saturating_add(1);
                    fallback
                }
            };
            flat_pane_choice(pane, &workspace_label, recency_rank).only_in_view(2)
        })
        .collect()
}

/// A flat pane row shown in the "panes" view. Like [`agent_choice`] but for
/// every pane (shells included) and ordered by `recency_rank` (ascending), so
/// the most recently focused pane surfaces first on an empty query.
fn flat_pane_choice(
    pane: &PaneInfo,
    workspace_label: &str,
    recency_rank: usize,
) -> Choice<FocusTarget> {
    let title = pane_title(pane);
    let agent_name = pane.agent_name();
    let detail = agent_name.unwrap_or("shell").to_string();
    let search = format!(
        "{} {}",
        workspace_label,
        pane_search_text(pane, &title, &detail)
    );
    Choice::new(
        FocusTarget::Pane(pane.pane_id.clone()),
        title,
        Some(detail),
        search,
    )
    .with_context(workspace_label)
    .inline_detail(false)
    .current(pane.focused)
    .with_optional_status(agent_name.map(|_| parse_status(&pane.agent_status)))
    .alternate_order(recency_rank)
}

fn workspace_choice(workspace: WorkspaceInfo) -> Choice<FocusTarget> {
    // Where a space lives leads, as it does in the sidebar, where the same
    // tokens sit directly under the name.
    let mut metadata = Vec::new();
    if let Some(location) = space::describe(&workspace.tokens) {
        metadata.push(location);
    }
    metadata.push(format!("#{}", workspace.number));
    if workspace.focused {
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
    if let Some(worktree) = &workspace.worktree {
        metadata.push(compact_home(Path::new(&worktree.checkout_path)));
    }

    let mut search = format!(
        "{} {} {}",
        workspace.label, workspace.workspace_id, workspace.agent_status
    );
    for value in workspace.tokens.values() {
        search.push(' ');
        search.push_str(value);
    }
    if let Some(worktree) = &workspace.worktree {
        search.push(' ');
        search.push_str(&worktree.checkout_path);
    }

    let order = status_order(parse_status(&workspace.agent_status));
    let title = format!("{} ({})", workspace.label, workspace.pane_count);
    Choice::new(
        FocusTarget::Workspace(workspace.workspace_id),
        title,
        Some(metadata.join(" · ")),
        search,
    )
    .tree_root()
    .current(workspace.focused)
    .alternate_order(order)
}

fn pane_choice(pane: &PaneInfo, parent: usize) -> Choice<FocusTarget> {
    let title = pane_title(pane);
    let agent_name = pane.agent_name();
    let detail = agent_name.unwrap_or("shell").to_string();
    let search = pane_search_text(pane, &title, &detail);

    Choice::new(
        FocusTarget::Pane(pane.pane_id.clone()),
        title,
        Some(detail),
        search,
    )
    .child_of(parent)
    .current(pane.focused)
    .with_optional_status(agent_name.map(|_| parse_status(&pane.agent_status)))
    .alternate_order(status_order(parse_status(&pane.agent_status)))
}

fn agent_choice(pane: &PaneInfo, workspace_label: &str) -> Choice<FocusTarget> {
    let title = pane_title(pane);
    let agent_name = pane.agent_name().unwrap_or("agent").to_string();
    let search = format!(
        "{} {}",
        workspace_label,
        pane_search_text(pane, &title, &agent_name)
    );
    Choice::new(
        FocusTarget::Pane(pane.pane_id.clone()),
        title,
        Some(agent_name),
        search,
    )
    .with_context(workspace_label)
    .inline_detail(false)
    .current(pane.focused)
    .with_optional_status(Some(parse_status(&pane.agent_status)))
    .alternate_order(status_order(parse_status(&pane.agent_status)))
    .prioritize_alternate_order()
}

fn pane_title(pane: &PaneInfo) -> String {
    pane.title
        .as_deref()
        .or(pane.terminal_title_stripped.as_deref())
        .or(pane.label.as_deref())
        .or(pane.terminal_title.as_deref())
        .or(pane.display_agent.as_deref())
        .or(pane.agent.as_deref())
        .unwrap_or("shell")
        .to_string()
}

fn pane_search_text(pane: &PaneInfo, title: &str, detail: &str) -> String {
    let mut search = format!(
        "{} {} {} {} {}",
        title, pane.pane_id, pane.agent_status, detail, pane.workspace_id
    );
    for value in pane.tokens.values() {
        search.push(' ');
        search.push_str(value);
    }
    for path in [pane.foreground_cwd.as_deref(), pane.cwd.as_deref()]
        .into_iter()
        .flatten()
    {
        search.push(' ');
        search.push_str(path);
    }
    search
}

impl PaneInfo {
    fn agent_name(&self) -> Option<&str> {
        self.display_agent.as_deref().or(self.agent.as_deref())
    }
}

fn parse_status(status: &str) -> ChoiceStatus {
    match status {
        "idle" => ChoiceStatus::Idle,
        "working" => ChoiceStatus::Working,
        "blocked" => ChoiceStatus::Blocked,
        "done" => ChoiceStatus::Done,
        _ => ChoiceStatus::Unknown,
    }
}

fn status_order(status: ChoiceStatus) -> usize {
    match status {
        ChoiceStatus::Blocked => 0,
        ChoiceStatus::Done => 1,
        ChoiceStatus::Working => 2,
        ChoiceStatus::Idle => 3,
        ChoiceStatus::Unknown => 4,
    }
}

fn compact_home(path: &Path) -> String {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return path.to_string_lossy().into_owned();
    };
    path.strip_prefix(home)
        .map(|relative| format!("~/{}", relative.display()))
        .unwrap_or_else(|_| path.to_string_lossy().into_owned())
}

fn plural<'a>(count: usize, singular: &'a str, plural: &'a str) -> &'a str {
    if count == 1 {
        singular
    } else {
        plural
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_request_matches_the_installed_protocol() {
        let request = WorkspaceCreateParams {
            cwd: "/tmp/a project".into(),
            env: BTreeMap::new(),
            focus: true,
            label: None,
        };
        assert_eq!(
            serde_json::to_value(request).unwrap(),
            serde_json::json!({
                "cwd": "/tmp/a project",
                "env": {},
                "focus": true,
                "label": null
            })
        );
    }

    #[test]
    fn workspace_choice_searches_labels_tokens_and_worktree_paths() {
        let choice = workspace_choice(WorkspaceInfo {
            workspace_id: "w:opaque".into(),
            number: 4,
            label: "cast".into(),
            focused: true,
            pane_count: 2,
            tab_count: 1,
            agent_status: "working".into(),
            tokens: BTreeMap::from([("org".into(), "378".into())]),
            worktree: Some(WorkspaceWorktreeInfo {
                checkout_path: "/tmp/herdr-cast".into(),
            }),
        });
        assert_eq!(choice.value, FocusTarget::Workspace("w:opaque".into()));
        assert!(choice.search_text.contains("cast"));
        assert!(choice.search_text.contains("378"));
        assert!(choice.search_text.contains("/tmp/herdr-cast"));
        assert!(choice.detail.unwrap().contains("current"));
    }

    #[test]
    fn a_workspace_leads_with_where_it_lives() {
        let mut remote = workspace("w:sbx", "tmp");
        remote.tokens = BTreeMap::from([
            ("host".into(), "copper-eva-stratt".into()),
            ("hostkind".into(), "sbx".into()),
            ("pad".into(), "\u{2800}".into()),
        ]);
        let detail = workspace_choice(remote).detail.unwrap();
        assert!(
            detail.starts_with("sbx \u{b7} copper-eva-stratt \u{b7} #1"),
            "detail: {detail}"
        );
        assert!(!detail.contains('\u{2800}'), "detail: {detail}");

        let plain = workspace_choice(workspace("w:plain", "herdr-cast"))
            .detail
            .unwrap();
        assert!(plain.starts_with("#1"), "detail: {plain}");
    }

    #[test]
    fn focus_target_preserves_opaque_workspace_ids() {
        assert_eq!(
            serde_json::to_value(WorkspaceTarget {
                workspace_id: "w:2/a b".into()
            })
            .unwrap(),
            serde_json::json!({"workspace_id": "w:2/a b"})
        );
        assert_eq!(
            serde_json::to_value(PaneTarget {
                pane_id: "p:2/a b".into()
            })
            .unwrap(),
            serde_json::json!({"pane_id": "p:2/a b"})
        );
    }

    #[test]
    fn workspace_tree_keeps_panes_under_their_workspace() {
        let workspaces = vec![workspace("w:one", "one"), workspace("w:two", "two")];
        let panes = vec![pane("p:two", "w:two", "agent two")];
        let choices = workspace_tree_choices(workspaces, panes);
        // Spaces tree (view 0): two workspaces, one pane child, one agent row.
        // Plus one flat pane row for the "panes" view (view 2).
        assert_eq!(choices.len(), 5);
        assert_eq!(choices[0].value, FocusTarget::Workspace("w:one".into()));
        assert_eq!(choices[1].value, FocusTarget::Workspace("w:two".into()));
        assert_eq!(choices[2].value, FocusTarget::Pane("p:two".into()));
        assert_eq!(choices[3].value, FocusTarget::Pane("p:two".into()));
        assert_eq!(choices[4].value, FocusTarget::Pane("p:two".into()));
    }

    #[test]
    fn status_order_matches_herdr_agent_priority() {
        assert!(status_order(ChoiceStatus::Blocked) < status_order(ChoiceStatus::Done));
        assert!(status_order(ChoiceStatus::Done) < status_order(ChoiceStatus::Working));
        assert!(status_order(ChoiceStatus::Working) < status_order(ChoiceStatus::Idle));
        assert!(status_order(ChoiceStatus::Idle) < status_order(ChoiceStatus::Unknown));
    }

    #[test]
    fn directory_choice_focuses_an_existing_workspace() {
        let mut workspace = workspace("w:existing", "existing");
        workspace.worktree = Some(WorkspaceWorktreeInfo {
            checkout_path: "/tmp/existing".into(),
        });
        let directories = workspace_directories(&[workspace], &[]);
        let existing = directories.get(Path::new("/tmp/existing")).unwrap();
        let choice = directory_choice(
            RankedDirectory {
                path: PathBuf::from("/tmp/existing"),
                label: "tmp/existing".into(),
                display_path: "~/tmp/existing".into(),
                score: 12.0,
                alpha_order: 0,
            },
            Some(existing),
        );
        assert_eq!(choice.value, DirectoryTarget::Focus("w:existing".into()));
    }

    #[test]
    fn panes_view_orders_panes_most_recent_first() {
        let workspaces = vec![workspace("w:one", "one")];
        let panes = vec![
            pane("p:old", "w:one", "old shell"),
            pane("p:new", "w:one", "new shell"),
            pane("p:never", "w:one", "never focused"),
        ];
        // Recency log is most-recent-first: p:new was focused last.
        let recency = vec!["p:new".to_string(), "p:old".to_string()];
        let choices = picker_choices(workspaces, panes, &recency);
        // Only the flat panes-view rows carry view-2-only visibility.
        let panes_view: Vec<_> = choices
            .iter()
            .filter(|choice| choice.value.is_pane() && choice.visible_in(2))
            .collect();
        let rank = |id: &str| {
            panes_view
                .iter()
                .find(|choice| choice.value == FocusTarget::Pane(id.into()))
                .map(|choice| choice.sort_key())
                .unwrap()
        };
        // MRU rank: p:new=0, p:old=1, p:never=large fallback.
        assert_eq!(rank("p:new"), 0);
        assert_eq!(rank("p:old"), 1);
        assert!(rank("p:never") > 1);
        // The picker's empty-query alternate-order sort (covered by picker
        // tests) surfaces these ranks ascending, so p:new lands first.
    }

    #[test]
    fn panes_view_includes_shells_and_ignores_stale_recency_ids() {
        let workspaces = vec![workspace("w:one", "one")];
        let panes = vec![
            pane("p:shell", "w:one", "a shell"),
            pane("p:agent", "w:one", "an agent"),
        ];
        // p:closed no longer exists; it must be ignored, not crash, and not
        // shift the rank of the panes that do exist.
        let recency = vec!["p:closed".to_string(), "p:shell".to_string()];
        let choices = picker_choices(workspaces, panes, &recency);
        let panes_view: Vec<_> = choices
            .iter()
            .filter(|choice| choice.value.is_pane() && choice.visible_in(2))
            .collect();
        assert_eq!(panes_view.len(), 2);
        // p:shell is at recency rank 1 (after the stale p:closed entry); p:agent
        // is unseen and sorts after it.
        let shell = panes_view
            .iter()
            .find(|choice| choice.value == FocusTarget::Pane("p:shell".into()))
            .unwrap();
        assert_eq!(shell.sort_key(), 1);
    }

    #[test]
    fn workspace_picker_view_persistence_round_trips_index_and_label() {
        let _guard = crate::test_support::ENV_MUTEX.lock().unwrap();
        let dir =
            std::env::temp_dir().join(format!("cast-picker-view-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let previous = std::env::var_os("HERDR_PLUGIN_STATE_DIR");
        std::env::set_var("HERDR_PLUGIN_STATE_DIR", &dir);

        // Unknown / missing label defaults to the primary view (0).
        fs::write(dir.join(WORKSPACE_PICKER_VIEW_FILE), "bogus\n").unwrap();
        assert_eq!(load_workspace_picker_view(), 0);

        save_workspace_picker_view(2).unwrap();
        assert_eq!(load_workspace_picker_view(), 2);
        assert_eq!(
            fs::read_to_string(dir.join(WORKSPACE_PICKER_VIEW_FILE)).unwrap(),
            "panes\n"
        );

        // Backward compatibility: a pre-existing "agents" file loads as view 1.
        fs::write(dir.join(WORKSPACE_PICKER_VIEW_FILE), "agents\n").unwrap();
        assert_eq!(load_workspace_picker_view(), 1);

        if let Some(previous) = previous {
            std::env::set_var("HERDR_PLUGIN_STATE_DIR", previous);
        } else {
            std::env::remove_var("HERDR_PLUGIN_STATE_DIR");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    fn workspace(id: &str, label: &str) -> WorkspaceInfo {
        WorkspaceInfo {
            workspace_id: id.into(),
            number: 1,
            label: label.into(),
            focused: false,
            pane_count: 1,
            tab_count: 1,
            agent_status: "idle".into(),
            tokens: BTreeMap::new(),
            worktree: None,
        }
    }

    fn pane(id: &str, workspace_id: &str, title: &str) -> PaneInfo {
        PaneInfo {
            pane_id: id.into(),
            workspace_id: workspace_id.into(),
            focused: false,
            cwd: None,
            foreground_cwd: None,
            label: None,
            agent: Some("pi".into()),
            title: Some(title.into()),
            terminal_title: None,
            terminal_title_stripped: None,
            display_agent: Some("Pi".into()),
            agent_status: "working".into(),
            tokens: BTreeMap::new(),
        }
    }
}
