# Cast

Cast is a collection of personal customizations for
[Herdr](https://herdr.dev). It adds agent notifications, keyboard-first
workspace navigation, zoxide-backed workspace creation, Space sidebar
metadata, and a small layout command palette.

The plugin id is `ad.cast`. Source repository:
[`aliou/herdr-cast`](https://github.com/aliou/herdr-cast). Cast is unpublished
and intended to run from a local linked checkout.

## Demos

Click a GIF to play the MP4.

### Workspace picker

Switch between the nested spaces view, the flat agents view, and the
most-recent panes view, filter, and focus a pane.

[![Workspace picker](https://assets.aliou.me/github/aliou/herdr-cast/workspace-picker-v3.gif)](https://assets.aliou.me/github/aliou/herdr-cast/workspace-picker.mp4)

### New workspace

Filter ranked directories, toggle zoxide and alphabetical order, then create
the workspace.

[![New workspace](https://assets.aliou.me/github/aliou/herdr-cast/directory-workspace-v4.gif)](https://assets.aliou.me/github/aliou/herdr-cast/directory-workspace.mp4)

### Layout palette

Flip a two-pane split, then move a pane into a new workspace.

[![Layout palette](https://assets.aliou.me/github/aliou/herdr-cast/layout-palette.gif)](https://assets.aliou.me/github/aliou/herdr-cast/layout-palette.mp4)

## Features

### Agent notifications

- Posts notifications for Herdr's `blocked` and `done` agent statuses.
- On macOS, uses the bundled, rebranded `HerdrNotify.app`.
- On Linux, asks Herdr to show the notification through the attached terminal
  client with sound disabled. Herdr handles the terminal notification path.
- Includes the agent, workspace, and working-directory context.
- Groups notifications by pane and debounces duplicate pane/status events for
  two seconds.
- Delivers every triggered notification regardless of pane, tab, workspace, or
  frontmost-app focus; nothing is suppressed for being on screen.
- Plays a status sound on macOS (`Glass` for blocked, `Funk` for done).
- Fails open when Herdr enrichment is unavailable.
- Verifies the notifier signature before signing and refreshes Launch Services
  registration on a six-hour TTL on macOS.

Clicking a notification raises Ghostty and asks Herdr to focus the exact pane
from the original event on macOS. Linux terminal notifications are not
click-to-focus. Event pane ids and workspace ids remain opaque; Cast does not
substitute the currently focused pane.

### Workspace and agent picker

The workspace picker opens with `prefix+space` in the local Herdr config.

- **Spaces view:** shows workspaces with their panes nested below them.
- **Agents view:** shows only agent panes as flat rows containing the workspace
  name, pane title, status, and agent name.
- **Panes view:** shows every pane (shells and agents) flat, ordered by the
  most recently focused pane first. Cast records each focused pane from the
  `pane.focused` event into a bounded recency log in its plugin state
  directory; stale ids for closed panes are ignored at read time.
- Press `Tab` to switch views.
- The picker reopens on the last-used spaces, agents, or panes view.
- On an empty query the picker lands on the first visible row that is not the
  current pane, so confirming jumps elsewhere; press `Escape` to stay put.
- Workspace matches retain their panes; pane matches retain their workspace as
  context.
- Agent mode uses Herdr's status priority: blocked, done, working, idle, then
  unknown. Press `Ctrl+S` to rank filtered results by fuzzy score instead.
- Working agents use an animated braille gutter indicator and a static yellow
  status label.
- The current-pane diamond takes precedence over the working animation.
- Workspace rows lead with the same location the sidebar shows, so a sandbox
  reads `tmp (1)  sbx \u00b7 copper-eva-stratt  \u00b7 #4 \u00b7 1 pane`.
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
- Zoxide mode filters with zoxide's keyword matcher and displays frecency scores
  inline: all keywords must appear in order, and the last keyword must match
  the last path component.
- Alphabetical mode hides scores and fuzzy-ranks filtered results.
- A teal diamond marks directories already represented by a Herdr workspace.
- Selecting a marked directory focuses its workspace instead of creating a
  duplicate.
- Selecting an unmarked directory creates and focuses a workspace there.

Existing workspaces are matched through managed worktree checkout paths and
exact pane working directories, with paths canonicalized when possible.

### Space sidebar metadata

Herdr builds a Space's second sidebar row from `branch` and `git_status`, both
derived from the first tab's root pane. Directories that are not repositories
render nothing, and remote sessions never show at all. Cast reports four
custom workspace tokens to fill that row:

- `$org`: the organization or client owning the root pane's directory, using
  the same taxonomy as the shell prompt. Dropped when it only repeats the
  Space name.
- `$repos`: how many repositories a container directory holds, for Spaces that
  are not repositories themselves. Reported only from two repositories up,
  because a lone repository says less than the Space name already does.
- `$host`: the first label of the host of a remote session running in the root
  pane, such as `donut` for `donut.tetra-albacore.ts.net`.
- `$hostkind`: `sbx` when that host is a lab sandbox, so the sidebar can color
  the marker.
- `$pad`: a braille blank reported when a Space has nothing else to show. Herdr
  hides a row whose tokens are all empty and trims whitespace out of metadata
  values, so holding the row open takes a character that prints as nothing
  without being whitespace. Every Space then keeps the same height.

The workspace picker renders the same tokens in the same order, so a Space
reads the same in the sidebar and in the popup.

Nothing here repeats the Space name. Herdr names a Space after the repository
or directory its root pane sits in and renames it when that pane moves, so the
name already answers what, and these tokens answer where.

A remote session replaces the local tokens, since the local directory no
longer answers where the pane is. Cast reads the host from the pane's
foreground process list rather than the typed command, so wrappers such as
`sbxctl connect` still resolve to the real destination.

Only the root pane counts, matching how Herdr picks a Space's Git identity and
name. A second pane running SSH does not relabel the Space.

The organization taxonomy is hard-coded personal policy in `src/space.rs`.

Refreshes come from three places: a startup hook that rebuilds every Space,
because Herdr drops metadata tokens when a new server restores a session;
`workspace.created` and `workspace.focused` event hooks; and the zsh
integration below. Herdr withholds `pane.updated`, its own live signal, from
plugin hooks as a high-volume event.

### Terminal window title

Cast sets Herdr's foreground client window title to the focused Space label on
`workspace.focused`, and refreshes it when that focused Space is renamed.
Herdr sends this as an OSC title update to the attached terminal client. The
title is client/window state, not per-workspace state, so Cast reapplies it on
focus changes rather than storing title state of its own.

### Layout palette

The layout palette opens with `prefix+p` in the local Herdr config. It provides:

- **Flip split direction:** toggles a two-pane tab between side-by-side and
  stacked while preserving the split ratio.
- **Move pane to new tab:** moves the focused pane into a new tab in the
  current workspace and focuses it.
- **Move pane to new workspace:** detaches the focused pane, creates a
  workspace, moves the pane there, and focuses it.
- **Rename current tab:** sets a custom label for the tab containing the
  focused pane.
- **Rename current workspace:** sets a custom label for the workspace
  containing the focused pane.
- **Rename Terminal title for current workspace:** sets the foreground Herdr
  client window title. Herdr does not store this as per-workspace title state.

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

When a result list has more matches than fit, a solid accent-colored badge
overlays the corner of the list: an up arrow and count top-right when earlier
matches are scrolled out of view, a down arrow and count bottom-right when
later ones are. Only one badge shows if just one row is visible. This doesn't
reserve a row — it paints over the edge of the first/last visible row, so long
paths or titles can run under it.

## Requirements

- macOS 26 or newer, or Linux with a terminal/client that supports Herdr
  notifications
- Herdr 0.8.0 or newer
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

On macOS, after the first notification event, grant notification access under
**System Settings → Notifications → herdr → Allow**. The grant is tied to the
bundled app's `me.aliou.herdr-cast.notify` bundle id.

Clicking a notification also asks macOS to let the bundled app control
Ghostty (Automation access), so it can raise the exact window/tab a
notification came from instead of just activating the app. Grant this the
first time a system dialog asks; if it's missed or times out, macOS caches a
denial and clicking silently falls back to activating Ghostty without
switching tabs. Reset a stuck denial with:

```sh
tccutil reset AppleEvents me.aliou.herdr-cast.notify
```

This repository's local development checkout may already be linked. Do not
relink it during routine development.

## Herdr configuration

Cast owns visual notifications and keeps sound disabled. On macOS it uses the
bundled notifier, so keep Herdr's own toast inside the TUI:

```toml
[ui.toast]
delivery = "herdr"

[ui.sound]
enabled = false
```

On Linux, Cast asks Herdr to show notifications through the terminal client:

```toml
[ui.toast]
delivery = "terminal"

[ui.sound]
enabled = false
```

Space rows need the custom tokens. Missing tokens drop their separator, so one
row serves every case:

```toml
[ui.sidebar.spaces]
rows = [
  ["state_icon", "workspace"],
  [
    { token = "$hostkind", fg = "#d98870" },
    "$host",
    "$org",
    "$repos",
    "branch",
    "git_status",
    "$pad",
  ],
]
```

That renders `378 · main` for a repository inside a known organization, `main`
for one outside it, `378 · 11 repos` for a directory of repositories, and
`sbx · copper-eva-stratt` for a sandbox session. Anything else gets a blank
second line rather than none.

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

[[keys.command]]
command = '"${HERDR_BIN_PATH:-herdr}" plugin pane open --plugin ad.cast --entrypoint lazygit'
description = "open lazygit"
key = "prefix+g"
type = "shell"
```

The `lazygit` entrypoint replaces a bare `command = "lazygit"` popup key. It
opens lazygit directly when the focused pane sits in (or is) a repository,
and otherwise fuzzy-picks one from the repositories found up to 3 levels
below it, instead of lazygit's own no-repository error.

## Shell integration

The zsh hooks keep Space metadata current between plugin events. `precmd`
syncs after a directory change; `preexec` schedules a second pass for commands
that hand the terminal to another machine, because the remote process does not
exist yet when it runs.

```sh
cast=herdr-cast
command -v $cast >/dev/null && eval "$($cast shell-init zsh)"
```

The snippet points at the binary that printed it and does nothing outside a
Herdr pane. The shell only triggers a sync; every value still comes from
Herdr's API.

## Architecture

- `src/main.rs` dispatches the `notify`, `focus`, `palette`,
  `directory-workspace`, `workspace-picker`, `sync-space`, `sync-title`,
  `sync-spaces`, and `shell-init` commands.
- `src/api.rs` implements newline-delimited JSON requests over Herdr's injected
  Unix socket.
- `src/notify.rs` owns notification policy, state, macOS focus detection,
  macOS notifier registration and delivery, Linux terminal notification
  requests, and macOS click-to-focus behavior.
- `src/picker.rs` provides the reusable fuzzy picker and rendering.
- `src/palette.rs` implements layout actions.
- `src/workspace.rs` implements workspace creation and workspace/pane focus.
- `src/space.rs` reports Space sidebar metadata and prints the zsh
  integration.
- `src/zoxide.rs` builds and orders directory candidates.
- `assets/HerdrNotify.app` is the bundled macOS notification application.

`herdr-plugin.toml` defines the startup hook, event subscriptions, pane
entrypoints, popup sizes, and build command. Runtime artifacts live only in
`HERDR_PLUGIN_STATE_DIR`.

## Development

```sh
nix-shell -p cargo rustc rustfmt --run 'cargo fmt -- --check'
nix-shell -p cargo rustc --run 'cargo test'
nix-shell -p cargo rustc --run 'cargo build --release'
```

The linked plugin resolves `herdr-cast` through `PATH`, so it runs whatever
binary that name resolves to for the Herdr server's process — a Nix-installed
build by default, or a local `target/release/herdr-cast` placed earlier on
`PATH` for testing. Rust changes require a release rebuild; they don't need a
relink or `PATH` change unless you're switching which binary is active.
Manifest changes require a registration refresh or a newly loaded Herdr
server.

## CI binaries

GitHub Actions builds binaries on every push and pull request for:

- `herdr-cast-darwin-arm64`
- `herdr-cast-linux-arm64`
- `herdr-cast-linux-x64`

The CI workflow uploads each binary as a workflow artifact with a `.sha256`
file containing the Nix-compatible `sha256-...` hash. Linux binaries target
musl so they run on NixOS without a dynamic loader patch.

## License

MIT for this plugin's code. The bundled `assets/HerdrNotify.app` is based on
[`terminal-notifier`](https://github.com/julienXX/terminal-notifier) (MIT); see
`assets/HerdrNotify.app.LICENSE.md`.
