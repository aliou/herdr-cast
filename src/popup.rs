// Opens a plugin popup pane sized as the larger of a percentage of the
// current terminal area and a fixed minimum cell size. Percent-only sizing
// (as set directly in herdr-plugin.toml) looks fine on a 27" display but
// becomes cramped on a 14" laptop screen, so this command measures the
// terminal first and clamps up to a usable minimum.

use serde::Serialize;
use serde_json::Value;

use crate::api::SocketClient;

const PLUGIN_ID: &str = "ad.cast";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PopupDimensions {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct PopupSizeSpec {
    pub min_width: u32,
    pub min_height: u32,
    pub pct_width: u32,
    pub pct_height: u32,
}

#[derive(Serialize)]
struct PaneLayoutParams {
    pane_id: Option<String>,
}

#[derive(Serialize)]
struct PluginPaneOpenParams {
    plugin_id: &'static str,
    entrypoint: String,
    placement: &'static str,
    width: u32,
    height: u32,
    focus: bool,
}

/// Clamp a popup dimension to the larger of a percentage of the available
/// terminal cells and a fixed minimum, without exceeding the available space.
fn clamp_dimension(available: u32, pct: u32, min: u32) -> u32 {
    let scaled = (available.saturating_mul(pct)) / 100;
    scaled.max(min).min(available.max(1))
}

pub fn resolve_dimensions(area: PopupDimensions, spec: PopupSizeSpec) -> PopupDimensions {
    PopupDimensions {
        width: clamp_dimension(area.width, spec.pct_width, spec.min_width),
        height: clamp_dimension(area.height, spec.pct_height, spec.min_height),
    }
}

fn terminal_area(
    client: &SocketClient,
    pane_id: Option<String>,
) -> Result<PopupDimensions, String> {
    let response = client.send(
        "cast:pane-layout",
        "pane.layout",
        PaneLayoutParams { pane_id },
    )?;
    let width = response
        .pointer("/result/layout/area/width")
        .and_then(Value::as_u64)
        .ok_or_else(|| "pane.layout missing layout area width".to_string())?;
    let height = response
        .pointer("/result/layout/area/height")
        .and_then(Value::as_u64)
        .ok_or_else(|| "pane.layout missing layout area height".to_string())?;
    Ok(PopupDimensions {
        width: width as u32,
        height: height as u32,
    })
}

fn active_pane_id() -> Option<String> {
    std::env::var("HERDR_ACTIVE_PANE_ID")
        .ok()
        .or_else(|| std::env::var("HERDR_PANE_ID").ok())
        .filter(|value| !value.is_empty())
}

pub fn run(entrypoint: &str, spec: PopupSizeSpec) -> Result<(), String> {
    let socket =
        std::env::var("HERDR_SOCKET_PATH").map_err(|_| "HERDR_SOCKET_PATH not set".to_string())?;
    let client = SocketClient::new(socket);

    let area = terminal_area(&client, active_pane_id())?;
    let dimensions = resolve_dimensions(area, spec);

    client.send(
        "cast:plugin-pane-open",
        "plugin.pane.open",
        PluginPaneOpenParams {
            plugin_id: PLUGIN_ID,
            entrypoint: entrypoint.to_owned(),
            placement: "popup",
            width: dimensions.width,
            height: dimensions.height,
            focus: true,
        },
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_the_minimum_when_the_percentage_is_smaller() {
        // A 14" laptop terminal: 37% of 90 cols is only 33 cols, well under
        // a usable minimum.
        let dimensions = resolve_dimensions(
            PopupDimensions {
                width: 90,
                height: 30,
            },
            PopupSizeSpec {
                min_width: 60,
                min_height: 20,
                pct_width: 37,
                pct_height: 33,
            },
        );
        assert_eq!(
            dimensions,
            PopupDimensions {
                width: 60,
                height: 20,
            }
        );
    }

    #[test]
    fn uses_the_percentage_when_it_exceeds_the_minimum() {
        // A 27" display: 37% comfortably clears the minimum.
        let dimensions = resolve_dimensions(
            PopupDimensions {
                width: 220,
                height: 60,
            },
            PopupSizeSpec {
                min_width: 60,
                min_height: 20,
                pct_width: 37,
                pct_height: 33,
            },
        );
        assert_eq!(
            dimensions,
            PopupDimensions {
                width: 81,
                height: 20,
            }
        );
    }

    #[test]
    fn never_exceeds_the_available_area() {
        let dimensions = resolve_dimensions(
            PopupDimensions {
                width: 40,
                height: 15,
            },
            PopupSizeSpec {
                min_width: 60,
                min_height: 20,
                pct_width: 37,
                pct_height: 33,
            },
        );
        assert_eq!(
            dimensions,
            PopupDimensions {
                width: 40,
                height: 15,
            }
        );
    }
}
