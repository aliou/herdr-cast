use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api::SocketClient;
use crate::picker::{pick, Choice, Picker};

#[derive(Debug, Clone, Serialize)]
struct LayoutExportParams {
    pane_id: String,
}

#[derive(Debug, Clone, Serialize)]
struct PaneMoveParams {
    pane_id: String,
    destination: PaneMoveDestination,
    focus: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum PaneMoveDestination {
    Tab {
        tab_id: String,
        target_pane_id: Option<String>,
        split: SplitDirection,
        ratio: Option<f32>,
    },
    NewWorkspace {
        label: Option<String>,
        tab_label: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum SplitDirection {
    Right,
    Down,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Direction {
    Right,
    Down,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
enum LayoutNode {
    Pane {
        pane_id: Option<String>,
    },
    Split {
        direction: Direction,
        ratio: f32,
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
}

pub fn run() -> Result<(), String> {
    let pane_id = focused_pane_id().ok_or_else(|| "focused pane not available".to_string())?;
    let socket =
        std::env::var("HERDR_SOCKET_PATH").map_err(|_| "HERDR_SOCKET_PATH not set".to_string())?;
    let client = SocketClient::new(socket);

    let Some(action) = choose_action()? else {
        return Ok(());
    };

    match action {
        LayoutAction::FlipSplit => flip_action(&client, &pane_id),
        LayoutAction::MoveToNewWorkspace => {
            move_to_new_workspace(&client, &pane_id, true).map(|_| ())
        }
    }
}

fn plugin_context() -> Value {
    std::env::var("HERDR_PLUGIN_CONTEXT_JSON")
        .ok()
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default()
}

fn focused_pane_id() -> Option<String> {
    std::env::var("HERDR_PANE_ID").ok().or_else(|| {
        plugin_context()
            .get("focused_pane_id")
            .and_then(Value::as_str)
            .map(str::to_owned)
    })
}

fn layout_export(client: &SocketClient, pane_id: &str) -> Result<Value, String> {
    client.send(
        "cast:layout-export",
        "layout.export",
        LayoutExportParams {
            pane_id: pane_id.to_owned(),
        },
    )
}

fn pane_move(
    client: &SocketClient,
    pane_id: &str,
    destination: PaneMoveDestination,
    focus: bool,
) -> Result<String, String> {
    let response = client.send(
        "cast:pane-move",
        "pane.move",
        PaneMoveParams {
            pane_id: pane_id.to_owned(),
            destination,
            focus,
        },
    )?;
    response
        .pointer("/result/move_result/pane/pane_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| "pane.move missing resulting pane id".to_string())
}

fn move_to_new_workspace(
    client: &SocketClient,
    pane_id: &str,
    focus: bool,
) -> Result<String, String> {
    pane_move(
        client,
        pane_id,
        PaneMoveDestination::NewWorkspace {
            label: None,
            tab_label: None,
        },
        focus,
    )
}

struct FlipPlan {
    stationary_pane_id: String,
    moved_pane_id: String,
    original: SplitDirection,
    opposite: SplitDirection,
    ratio: f32,
}

fn flip_plan(root: &LayoutNode, focused_pane_id: &str) -> Result<FlipPlan, String> {
    let LayoutNode::Split {
        direction,
        ratio,
        first,
        second,
    } = root
    else {
        return Err("flip split requires a two-pane tab".to_string());
    };
    let (
        LayoutNode::Pane {
            pane_id: Some(first_pane_id),
        },
        LayoutNode::Pane {
            pane_id: Some(second_pane_id),
        },
    ) = (first.as_ref(), second.as_ref())
    else {
        return Err("flip split requires a two-pane tab".to_string());
    };
    if focused_pane_id != first_pane_id && focused_pane_id != second_pane_id {
        return Err("focused pane is not in the exported layout".to_string());
    }
    let (original, opposite) = match direction {
        Direction::Right => (SplitDirection::Right, SplitDirection::Down),
        Direction::Down => (SplitDirection::Down, SplitDirection::Right),
    };
    Ok(FlipPlan {
        stationary_pane_id: first_pane_id.clone(),
        moved_pane_id: second_pane_id.clone(),
        original,
        opposite,
        ratio: *ratio,
    })
}

fn flip_action(client: &SocketClient, pane_id: &str) -> Result<(), String> {
    let exported = layout_export(client, pane_id)?;
    let layout = exported
        .pointer("/result/layout")
        .ok_or_else(|| "layout.export missing layout".to_string())?;
    let tab_id = layout
        .get("tab_id")
        .and_then(Value::as_str)
        .ok_or_else(|| "layout.export missing tab_id".to_string())?;
    let root: LayoutNode = serde_json::from_value(
        layout
            .get("root")
            .cloned()
            .ok_or_else(|| "layout.export missing root".to_string())?,
    )
    .map_err(|error| format!("failed to parse layout root: {error}"))?;
    let plan = flip_plan(&root, pane_id)?;

    let moved_pane_id = move_to_new_workspace(client, &plan.moved_pane_id, false)?;
    let move_back = pane_move(
        client,
        &moved_pane_id,
        PaneMoveDestination::Tab {
            tab_id: tab_id.to_owned(),
            target_pane_id: Some(plan.stationary_pane_id.clone()),
            split: plan.opposite,
            ratio: Some(plan.ratio),
        },
        false,
    );
    if let Err(error) = move_back {
        let rollback = pane_move(
            client,
            &moved_pane_id,
            PaneMoveDestination::Tab {
                tab_id: tab_id.to_owned(),
                target_pane_id: Some(plan.stationary_pane_id),
                split: plan.original,
                ratio: Some(plan.ratio),
            },
            false,
        );
        return match rollback {
            Ok(_) => Err(format!(
                "failed to flip split and restored the pane: {error}"
            )),
            Err(rollback_error) => Err(format!(
                "failed to flip split ({error}); rollback also failed ({rollback_error})"
            )),
        };
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum LayoutAction {
    FlipSplit,
    MoveToNewWorkspace,
}

fn choose_action() -> Result<Option<LayoutAction>, String> {
    pick(
        Picker {
            placeholder: "Search actions",
            empty_message: "No matching layout actions",
            order: None,
        },
        vec![
            Choice::new(
                LayoutAction::FlipSplit,
                "Flip split direction",
                Some("Toggle a two-pane tab between side-by-side and stacked"),
                "flip split direction side by side stacked",
            ),
            Choice::new(
                LayoutAction::MoveToNewWorkspace,
                "Move pane to new workspace",
                Some("Detach and focus the selected pane in a new workspace"),
                "move pane detach new workspace",
            ),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_a_stable_flip_plan_for_a_two_pane_tab() {
        let layout = LayoutNode::Split {
            direction: Direction::Right,
            ratio: 0.4,
            first: Box::new(LayoutNode::Pane {
                pane_id: Some("w1:p1".into()),
            }),
            second: Box::new(LayoutNode::Pane {
                pane_id: Some("w1:p2".into()),
            }),
        };

        let plan = flip_plan(&layout, "w1:p1").unwrap();
        assert_eq!(plan.stationary_pane_id, "w1:p1");
        assert_eq!(plan.moved_pane_id, "w1:p2");
        assert_eq!(plan.original, SplitDirection::Right);
        assert_eq!(plan.opposite, SplitDirection::Down);
        assert_eq!(plan.ratio, 0.4);
    }

    #[test]
    fn refuses_to_mutate_a_nested_layout() {
        let layout = LayoutNode::Split {
            direction: Direction::Right,
            ratio: 0.5,
            first: Box::new(LayoutNode::Pane {
                pane_id: Some("w1:p1".into()),
            }),
            second: Box::new(LayoutNode::Split {
                direction: Direction::Down,
                ratio: 0.5,
                first: Box::new(LayoutNode::Pane {
                    pane_id: Some("w1:p2".into()),
                }),
                second: Box::new(LayoutNode::Pane {
                    pane_id: Some("w1:p3".into()),
                }),
            }),
        };

        assert!(flip_plan(&layout, "w1:p3").is_err());
    }

    #[test]
    fn move_request_uses_the_current_protocol_shape() {
        let params = PaneMoveParams {
            pane_id: "w1:p1".into(),
            destination: PaneMoveDestination::NewWorkspace {
                label: None,
                tab_label: None,
            },
            focus: true,
        };

        assert_eq!(
            serde_json::to_value(params).unwrap(),
            serde_json::json!({
                "pane_id": "w1:p1",
                "destination": {
                    "type": "new_workspace",
                    "label": null,
                    "tab_label": null
                },
                "focus": true
            })
        );
    }
}
