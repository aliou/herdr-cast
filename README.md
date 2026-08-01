# Cast

Cast is a collection of personal customizations for
[Herdr](https://herdr.dev). It adds native macOS agent notifications,
keyboard-first workspace navigation, zoxide-backed workspace creation, and a
small layout command palette.

The plugin id is `ad.cast`. Source repository:
[`aliou/herdr-cast`](https://github.com/aliou/herdr-cast). Cast is unpublished
and intended to run from a local linked checkout.

## Demos

Click a GIF to play the MP4.

### Workspace picker

Switch between the nested spaces view and the flat agents view, filter, and
focus a pane.

[![Workspace picker](https://assets.aliou.me/github/aliou/herdr-cast/workspace-picker-v3.gif)](https://assets.aliou.me/github/aliou/herdr-cast/workspace-picker.mp4)

### New workspace

Filter ranked directories, toggle zoxide and alphabetical order, then create
the workspace.

[![New workspace](https://assets.aliou.me/github/aliou/herdr-cast/directory-workspace-v4.gif)](https://assets.aliou.me/github/aliou/herdr-cast/directory-workspace.mp4)

### Layout palette

Flip a two-pane split, then move a pane into a new workspace.

[![Layout palette](https://assets.aliou.me/github/aliou/herdr-cast/layout-palette.gif)](https://assets.aliou.me/github/aliou/herdr-cast/layout-palette.mp4)

## Features

### Native agent notifications

- Posts native macOS notifications for Herdr's `blocked` and `done` agent
  statuses.
- Uses the bundled, rebranded `HerdrNotify.app` with Herdr status artwork.
- Includes the agent, workspace, and working-directory context.
- Groups notifications by pane and debounces duplicate pane/status events for
  two seconds.
- Suppresses a notification only when its Herdr workspace is focused and a
  supported terminal is the frontmost macOS app.
- Fails open when Herdr enrichment or frontmost-app detection is unavailable.
- Verifies the notifier signature before signing and refreshes Launch Services
  registration on a six-hour TTL.
- Keeps sound out of the plugin so another integration can own audio playback.

Clicking a notification raises Ghostty and asks Herdr to focus the exact pane
from the original event. Event pane ids and workspace ids remain opaque; Cast
does not substitute the currently focused pane.

### Workspace and agent picker

The workspace picker opens with `prefix+space` in the local Herdr config.

- **Spaces view:** shows workspaces with their panes nested below them.
- **Agents view:** shows only agent panes as flat rows containing the workspace
  name, pane title, status, and agent name.
- Press `Tab` to switch views.
- The picker reopens on the last-used spaces or agents view.
- Agents view starts at the first row; spaces view selects the current pane when
  there is no query.
- Workspace matches retain their panes; pane matches retain their workspace as
  context.
- Agent mode uses Herdr's status priority: blocked, done, working, idle, then
  unknown. Press `Ctrl+S` to rank filtered results by fuzzy score instead.
- Working agents use an animated braille gutter indicator and a static yellow
  status label.
- The current-pane diamond takes precedence over the working animation.
- Selecting a workspace or pane focuses it through Herdr's socket API.

Search covers workspace labels, pane titles, opaque ids, agent names and
statuses, metadata tokens, working directories, and managed worktree paths.

### New workspace picker

The directory picker opens with `prefix+shift+c` in the local Herdr config.

- Reads candidates and frecency scores from `zoxide query -ls`.
- Keeps zoxide entries below `~/code/src`.
- Always includes `~/.dot` and top-level directories below `~/tmp`.
- Uses compact labels such as `aliou/herdr-cast` and `tmp/repro` while keeping
  the full tilde path visible.
- Opens in zoxide mode. Press `Tab` to toggle zoxide and alphabetical modes for
  the current invocation.
- Zoxide mode keeps frecency order while filtering and displays scores inline.
- Alphabetical mode hides scores and fuzzy-ranks filtered results.
- A teal diamond marks directories already represented by a Herdr workspace.
- Selecting a marked directory focuses its workspace instead of creating a
  duplicate.
- Selecting an unmarked directory creates and focuses a workspace there.

Existing workspaces are matched through managed worktree checkout paths and
exact pane working directories, with paths canonicalized when possible.

### Layout palette

The layout palette opens with `prefix+p` in the local Herdr config. It provides:

- **Flip split direction:** toggles a two-pane tab between side-by-side and
  stacked while preserving the split ratio.
- **Move pane to new workspace:** detaches the focused pane, creates a
  workspace, moves the pane there, and focuses it.

Split flipping rejects nested layouts and attempts to restore the original
layout if the second move fails.

### Shared picker controls

All palettes use the same Ratatui/Crossterm picker with Senzu colors and
Herdr-owned popup chrome.

| Action | Keys |
| --- | --- |
| Filter | Type normally |
| Move | Up/Down, Ctrl+P/Ctrl+N, or Ctrl+K/Ctrl+J |
| Select | Enter |
| Close | Esc or Ctrl+C |
| Toggle picker mode | Tab |
| Toggle priority and fuzzy sorting | Ctrl+S |
| Start/end of query | Home/End or Ctrl+A/Ctrl+E |
| Move by character | Left/Right or Ctrl+B/Ctrl+F |
| Delete previous word | Ctrl+W |
| Delete to start | Ctrl+U |
| Delete | Backspace/Delete |

The query cursor accounts for Unicode display width.

Pickers with tabs reserve one row above the query in every view, so switching
views never shifts the layout. View tabs sit on the left of that row and sort
tabs on the right.

Below 50 content columns, or 8 rows (9 with a tab row), the picker shows a
centered "Popup too small" hint with the current and needed dimensions
colored red or green per axis, instead of a half-rendered query and results,
the same idea as btop's terminal-too-small screen. Esc/Ctrl+C still close the
popup and typing still narrows matches, but Enter is ignored until the popup
grows back above the floor.

When a result list has more matches than fit, a solid accent-colored badge
overlays the corner of the list: an up arrow and count top-right when earlier
matches are scrolled out of view, a down arrow and count bottom-right when
later ones are. Only one badge shows if just one row is visible. This doesn't
reserve a row — it paints over the edge of the first/last visible row, so long
paths or titles can run under it.

## Requirements

- macOS 26 or newer
- Herdr 0.7.0 or newer
- `zoxide`
- Rust and Cargo for local builds

This checkout uses Nix when Rust tooling is not already available.

## Install

```sh
git clone https://github.com/aliou/herdr-cast.git
cd herdr-cast
nix-shell -p cargo rustc --run 'cargo build --release'
herdr plugin link "$PWD"
```

After the first notification event, grant notification access under **System
Settings → Notifications → herdr → Allow**. The grant is tied to the bundled
app's `codes.dot.herdr-notify` bundle id.

This repository's local development checkout may already be linked. Do not
relink it during routine development.

## Herdr configuration

Cast owns native visual notifications. Keep Herdr's toast inside the TUI and
leave sound to the integration that owns audio playback:

```toml
[ui.toast]
delivery = "herdr"

[ui.sound]
enabled = false
```

Example pane bindings:

```toml
[[keys.command]]
command = '"${HERDR_BIN_PATH:-herdr}" plugin pane open --plugin ad.cast --entrypoint layout-palette'
description = "open layout command palette"
key = "prefix+p"
type = "shell"

[[keys.command]]
command = '"${HERDR_BIN_PATH:-herdr}" plugin pane open --plugin ad.cast --entrypoint directory-workspace'
description = "create workspace from a ranked directory"
key = "prefix+shift+c"
type = "shell"

[[keys.command]]
command = '"${HERDR_BIN_PATH:-herdr}" plugin pane open --plugin ad.cast --entrypoint workspace-picker'
description = "focus an existing workspace or pane"
key = "prefix+space"
type = "shell"
```

## Architecture

- `src/main.rs` dispatches the `notify`, `focus`, `palette`,
  `directory-workspace`, and `workspace-picker` commands.
- `src/api.rs` implements newline-delimited JSON requests over Herdr's injected
  Unix socket.
- `src/notify.rs` owns notification policy, state, macOS focus detection,
  notifier registration, delivery, and click-to-focus behavior.
- `src/picker.rs` provides the reusable fuzzy picker and rendering.
- `src/palette.rs` implements layout actions.
- `src/workspace.rs` implements workspace creation and workspace/pane focus.
- `src/zoxide.rs` builds and orders directory candidates.
- `assets/HerdrNotify.app` is the bundled notification application.

`herdr-plugin.toml` defines the event subscription, pane entrypoints, popup
sizes, and build command. Runtime artifacts live only in
`HERDR_PLUGIN_STATE_DIR`.

## Development

```sh
nix-shell -p cargo rustc rustfmt --run 'cargo fmt -- --check'
nix-shell -p cargo rustc --run 'cargo test'
nix-shell -p cargo rustc --run 'cargo build --release'
```

The linked plugin executes `target/release/herdr-cast` directly, so Rust
changes require a release rebuild but not a relink. Manifest changes require a
registration refresh or a newly loaded Herdr server.

## License

MIT for this plugin's code. The bundled `assets/HerdrNotify.app` is based on
[`terminal-notifier`](https://github.com/julienXX/terminal-notifier) (MIT); see
`assets/HerdrNotify.app.LICENSE.md`.
