use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use skim::prelude::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Request {
    id: String,
    #[serde(flatten)]
    method: Method,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params")]
enum Method {
    #[serde(rename = "layout.export")]
    LayoutExport(LayoutExportParams),
    #[serde(rename = "pane.move")]
    PaneMove(PaneMoveParams),
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct LayoutExportParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pane_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tab_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PaneMoveParams {
    pane_id: String,
    destination: PaneMoveDestination,
    #[serde(default)]
    focus: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum PaneMoveDestination {
    Tab {
        tab_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        target_pane_id: Option<String>,
        split: SplitDirection,
        #[serde(skip_serializing_if = "Option::is_none")]
        ratio: Option<f32>,
    },
    NewTab {
        #[serde(skip_serializing_if = "Option::is_none")]
        workspace_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    NewWorkspace {
        #[serde(skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tab_label: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum SplitDirection {
    Right,
    Down,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Direction {
    Right,
    Down,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
enum LayoutNode {
    Pane {
        #[serde(skip_serializing_if = "Option::is_none")]
        pane_id: Option<String>,
    },
    Split {
        direction: Direction,
        ratio: f32,
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
}

struct SocketClient {
    path: String,
}

impl SocketClient {
    fn send(&self, req: &Request) -> io::Result<String> {
        let mut stream = UnixStream::connect(&self.path)?;
        let mut payload = serde_json::to_vec(req)?;
        payload.push(b'\n');
        stream.write_all(&payload)?;
        stream.shutdown(std::net::Shutdown::Write)?;

        let mut response = String::new();
        stream.read_to_string(&mut response)?;
        Ok(response)
    }
}

fn socket_path() -> Option<String> {
    std::env::var("HERDR_SOCKET_PATH").ok()
}

fn plugin_context() -> Value {
    std::env::var("HERDR_PLUGIN_CONTEXT_JSON")
        .ok()
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default()
}

fn focused_pane_id() -> Option<String> {
    if let Ok(id) = std::env::var("HERDR_PANE_ID") {
        return Some(id);
    }
    plugin_context()
        .get("focused_pane_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn error(message: &str) -> ! {
    eprintln!("[layout-palette] {}", message);
    std::process::exit(1);
}

fn rpc_send(client: &SocketClient, req: Request) -> Value {
    let response = client
        .send(&req)
        .unwrap_or_else(|err| error(&format!("socket request failed: {}", err)));
    let value: Value = serde_json::from_str(&response)
        .unwrap_or_else(|err| error(&format!("invalid json response: {}", err)));
    if value.get("error").is_some() {
        error(&format!("request failed: {}", response));
    }
    value
}

fn layout_export(client: &SocketClient, pane_id: &str) -> Value {
    rpc_send(
        client,
        Request {
            id: "export".into(),
            method: Method::LayoutExport(LayoutExportParams {
                pane_id: Some(pane_id.into()),
                tab_id: None,
            }),
        },
    )
}

fn pane_move(client: &SocketClient, pane_id: String, destination: PaneMoveDestination) -> Value {
    rpc_send(
        client,
        Request {
            id: "move".into(),
            method: Method::PaneMove(PaneMoveParams {
                pane_id,
                destination,
                focus: true,
            }),
        },
    )
}

fn pane_id_matches(node: &LayoutNode, target: &str) -> bool {
    match node {
        LayoutNode::Pane { pane_id, .. } => pane_id.as_deref() == Some(target),
        LayoutNode::Split { first, second, .. } => {
            pane_id_matches(first, target) || pane_id_matches(second, target)
        }
    }
}

fn current_split_direction(node: &LayoutNode, target_pane_id: &str) -> Option<Direction> {
    match node {
        LayoutNode::Pane { .. } => None,
        LayoutNode::Split {
            direction,
            first,
            second,
            ..
        } => {
            if pane_id_matches(first, target_pane_id)
                || pane_id_matches(second, target_pane_id)
            {
                Some(*direction)
            } else {
                current_split_direction(first, target_pane_id)
                    .or_else(|| current_split_direction(second, target_pane_id))
            }
        }
    }
}

fn move_to_new_workspace(client: &SocketClient, pane_id: &str) {
    pane_move(
        client,
        pane_id.into(),
        PaneMoveDestination::NewWorkspace {
            label: None,
            tab_label: None,
        },
    );
}

fn move_to_tab(
    client: &SocketClient,
    pane_id: String,
    tab_id: String,
    split: SplitDirection,
) {
    pane_move(
        client,
        pane_id,
        PaneMoveDestination::Tab {
            tab_id,
            target_pane_id: None,
            split,
            ratio: None,
        },
    );
}

fn flip_action(socket: &str, pane_id: &str) {
    let client = SocketClient {
        path: socket.to_string(),
    };

    let exported = layout_export(&client, pane_id);
    let layout = exported
        .get("result")
        .and_then(|r| r.get("layout"))
        .unwrap_or_else(|| error("layout.export missing layout"));
    let tab_id = layout
        .get("tab_id")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| error("layout.export missing tab_id"))
        .to_string();

    let root: LayoutNode = serde_json::from_value(layout.get("root").cloned().unwrap_or_else(
        || error("layout.export missing root"),
    ))
    .unwrap_or_else(|err| error(&format!("failed to parse layout root: {}", err)));

    let current = current_split_direction(&root, pane_id)
        .unwrap_or_else(|| error("focused pane is not inside a split"));

    let opposite = match current {
        Direction::Right => SplitDirection::Down,
        Direction::Down => SplitDirection::Right,
    };

    // Move focused pane to a temp workspace, then back into the original tab
    // with the opposite split direction. pane.move preserves the PTY.
    move_to_new_workspace(&client, pane_id);

    let moved_pane_id = pane_id.to_string();
    move_to_tab(&client, moved_pane_id, tab_id, opposite);
}

fn choose_action() -> Option<String> {
    if let Ok(choice) = std::env::var("PALETTE_CHOICE") {
        return if choice.is_empty() { None } else { Some(choice) };
    }

    let options = SkimOptionsBuilder::default()
        .height("50%")
        .prompt("layout: ")
        .header("choose an action")
        .build()
        .expect("valid skim options");

    let items = vec![
        "flip split direction",
        "move pane to new workspace",
    ];

    Skim::run_items(options, items)
        .ok()
        .and_then(|out| {
            if out.is_abort {
                None
            } else {
                out.selected_items.first().map(|item| item.output().to_string())
            }
        })
}

fn main() {
    let pane_id = focused_pane_id().unwrap_or_else(|| error("focused pane not available"));
    let socket = socket_path().unwrap_or_else(|| error("HERDR_SOCKET_PATH not set"));

    let action = match choose_action() {
        Some(action) => action,
        None => return,
    };

    match action.as_str() {
        "flip split direction" => flip_action(&socket, &pane_id),
        "move pane to new workspace" => move_to_new_workspace(
            &SocketClient {
                path: socket.to_string(),
            },
            &pane_id,
        ),
        other => error(&format!("unknown action: {}", other)),
    }
}
