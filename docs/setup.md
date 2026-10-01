# Setup and configuration

## Requirements

- macOS 26 or newer, or Linux with a terminal/client that supports Herdr
  notifications
- Herdr 0.8.0 or newer
- `zoxide` (optional; the new-workspace picker falls back to a filesystem
  scan when it is absent)
- Rust and Cargo for local builds (this checkout uses Nix when tooling is
  not already available)
- For the bridge: key-based ssh from the host Mac to each remote, and
  `herdr-cast` on the remote's non-interactive ssh `PATH`. The host connects
  with `BatchMode=yes`, so a password prompt fails the link. See
  docs/bridge.md.

## Install

```sh
git clone https://github.com/aliou/herdr-cast.git
cd herdr-cast
nix-shell -p cargo rustc --run 'cargo build --release'
herdr plugin link "$PWD"
```

On macOS, grant notification access under System Settings → Notifications →
herdr after the first notification event; the grant is tied to the bundled
app's `me.aliou.herdr-cast.notify` bundle id. Clicking a notification also
asks for Automation access so it can raise the exact Ghostty window/tab; if
that grant is missed or cached as denied, reset it with:

```sh
tccutil reset AppleEvents me.aliou.herdr-cast.notify
```

`~/.local/bin/herdr-cast` should be a symlink to the Nix-packaged binary
(managed by the homelab Home Manager module). For local runtime testing,
point it at this checkout's `target/release/herdr-cast` temporarily and
restore it afterwards. Rust changes require a release rebuild to be visible
at runtime; they need no relink. Manifest changes need a registration
refresh or a newly loaded Herdr server (`herdr plugin link` does not run
manifest build commands).

## Herdr configuration

Cast owns visual notifications and keeps sound disabled. On macOS keep
Herdr's own toast inside the TUI:

```toml
[ui.toast]
delivery = "herdr"

[ui.sound]
enabled = false
```

On Linux, show notifications through the terminal client:

```toml
[ui.toast]
delivery = "terminal"

[ui.sound]
enabled = false
```

Space rows need the custom tokens; missing tokens drop their separator, so
one row serves every case:

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

An Agents sidebar row can show the session token:

```toml
[ui.sidebar.agents]
rows = [["state_icon", "$session"], ["workspace", "tab", "pane"]]
```

Popup bindings. The pickers open through `herdr-cast open-popup`, which
clamps the popup to the larger of a percentage of the terminal area and a
fixed minimum (docs/popups.md):

```toml
[[keys.command]]
command = 'herdr-cast open-popup --entrypoint layout-palette --pct-width 40 --pct-height 40 --min-width 72 --min-height 24'
description = "open layout command palette"
key = "prefix+p"
type = "shell"

[[keys.command]]
command = '"${HERDR_BIN_PATH:-herdr}" herdr-cast open-popup --entrypoint directory-workspace --min-width 60 --min-height 20 --pct-width 37 --pct-height 33'
description = "create workspace from a ranked directory"
key = "prefix+shift+c"
type = "shell"

[[keys.command]]
command = '"${HERDR_BIN_PATH:-herdr}" herdr-cast open-popup --entrypoint workspace-picker --min-width 60 --min-height 20 --pct-width 37 --pct-height 33'
description = "focus an existing workspace or pane"
key = "prefix+space"
type = "shell"

[[keys.command]]
command = 'herdr-cast open-popup --entrypoint hunk --pct-width 90 --pct-height 90 --min-width 70 --min-height 22'
description = "review changes with hunk"
key = "prefix+g"
type = "shell"

[[keys.command]]
command = 'herdr-cast open-popup --entrypoint hunk-log --pct-width 90 --pct-height 90 --min-width 70 --min-height 22'
description = "browse commits with hunk"
key = "prefix+shift+g"
type = "shell"
```

The `lazygit` entrypoint binds like a plain `lazygit` popup but resolves
the focused pane's repository first (docs/popups.md).

## Shell integration

The zsh hooks keep Space metadata current between plugin events: `precmd`
syncs after a directory change, `preexec` schedules a second pass for
commands that hand the terminal to another machine (the remote process does
not exist yet when the command runs).

```sh
cast=herdr-cast
command -v $cast >/dev/null && eval "$($cast shell-init zsh)"
```

The snippet does nothing outside a Herdr pane.

## Starting the pasteboard watcher by hand

`forward-start` runs at plugin startup. To start the watcher after
installing Cast into an already-running session, run this inside a Herdr
pane on the pasteboard-forwarding Mac (the pane supplies
`HERDR_SOCKET_PATH`; the command supplies the state directory normally
injected by startup hooks):

```sh
HERDR_PLUGIN_STATE_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/herdr/plugins/ad.cast" \
  ~/.local/bin/herdr-cast forward-start
```

No server restart is needed; the command exits quietly when a watcher
already holds the session's lock.

## Resident daemons and deploys

Resident daemons (bridge, title, pasteboard watcher, remote relays) keep the
binary they started with. After deploying a new build, restart Herdr.
Killing them also works: the next pane focus restarts the bridge and title
daemons, the host reconnects each remote's relay, and the pasteboard watcher
restarts from `forward-start`.

## License

MIT for this plugin's code. The bundled `assets/HerdrNotify.app` is based on
[`terminal-notifier`](https://github.com/julienXX/terminal-notifier) (MIT);
see `assets/HerdrNotify.app.LICENSE.md`.
