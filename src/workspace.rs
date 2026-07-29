use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::api::SocketClient;
use crate::picker::{pick_with_detail, Choice, ChoiceStatus, OrderToggle, Picker};
use crate::zoxide::{self, RankedDirectory};

const SOCKET_TIMEOUT: Duration = Duration::from_secs(3);
const WORKSPACE_PICKER_VIEW_FILE: &str = "workspace-picker-view";

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
    let (target, alphabetical) = pick_with_detail(
        Picker {
            placeholder: "Search ranked directories",
            empty_message: "No matching directories",
            order: Some(OrderToggle {
                primary: "zoxide",
                alternate: "alpha",
                initial_alternate: zoxide::load_alphabetical_order(),
            }),
        },
        choices,
        |_| None,
    )?;
    zoxide::save_alphabetical_order(alphabetical)?;
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
    let choices = workspace_tree_choices(workspaces, panes);
    let (target, agents_view) = pick_with_detail(
        Picker {
            placeholder: "Search workspaces and panes",
            empty_message: "No matching workspaces or panes",
            order: Some(OrderToggle {
                primary: "spaces",
                alternate: "agents",
                initial_alternate: load_workspace_picker_agents_view(),
            }),
        },
        choices,
        |_| None,
    )?;
    save_workspace_picker_agents_view(agents_view)?;
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

fn load_workspace_picker_agents_view() -> bool {
    workspace_picker_view_file()
        .and_then(|path| fs::read_to_string(path).ok())
        .is_some_and(|value| value.trim() == "agents")
}

fn save_workspace_picker_agents_view(agents: bool) -> Result<(), String> {
    let Some(path) = workspace_picker_view_file() else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create workspace picker state: {error}"))?;
    }
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&temporary, if agents { "agents\n" } else { "spaces\n" })
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
        .unwrap_or_else(|| DirectoryTarget::Create(directory.path));
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
}

fn workspace_tree_choices(
    workspaces: Vec<WorkspaceInfo>,
    panes: Vec<PaneInfo>,
) -> Vec<Choice<FocusTarget>> {
    let mut choices = Vec::new();
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
    choices
}

fn workspace_choice(workspace: WorkspaceInfo) -> Choice<FocusTarget> {
    let mut metadata = vec![format!("#{}", workspace.number)];
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
            tokens: BTreeMap::from([("branch".into(), "main".into())]),
            worktree: Some(WorkspaceWorktreeInfo {
                checkout_path: "/tmp/herdr-cast".into(),
            }),
        });
        assert_eq!(choice.value, FocusTarget::Workspace("w:opaque".into()));
        assert!(choice.search_text.contains("cast"));
        assert!(choice.search_text.contains("main"));
        assert!(choice.search_text.contains("/tmp/herdr-cast"));
        assert!(choice.detail.unwrap().contains("current"));
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
        assert_eq!(choices.len(), 4);
        assert_eq!(choices[0].value, FocusTarget::Workspace("w:one".into()));
        assert_eq!(choices[1].value, FocusTarget::Workspace("w:two".into()));
        assert_eq!(choices[2].value, FocusTarget::Pane("p:two".into()));
        assert_eq!(choices[3].value, FocusTarget::Pane("p:two".into()));
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
