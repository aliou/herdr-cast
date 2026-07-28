# herdr-cast

Aliou's personal [herdr](https://herdr.dev) customization hub, starting with
native macOS notifications when a turn ends or an agent needs you that jump to
the right pane/tab on click.

Named for *casting* — both broadcasting a signal and casting a flock of sheep
(tipping one onto its back to work it). It started as a notifier and is now the
dumping ground for all my herdr customizations. Not published; just a local
`herdr plugin link`.

## What it does

When an agent pane's status changes, herdr fires a
`pane.agent_status_changed` event. Cast listens for `blocked` and `done`, then
posts a native macOS notification through the bundled `HerdrNotify.app` (a
rebranded `terminal-notifier` whose bundle id is `codes.dot.herdr-notify`, so
the **left** icon is the herdr logo). Clicking the notification runs the same
Rust binary, which raises Ghostty and focuses the pane through Herdr's socket.

Status semantics in herdr (relevant for pi):

- `blocked` — the agent needs your input mid-turn (attention / dangerous /
  error). Reported by the pi integration via `herdr:blocked`.
- `done` — a turn ended and you haven't looked at the pane yet. herdr
  re-classifies a fresh `idle` as `done` in `agent_view.rs`, so pi's turn-end
  `idle` arrives here as `done`. Exactly the "turn end" trigger.

## Requirements

- macOS. Tested on macOS 26.
- Rust and Cargo to build the plugin. On this machine they are provided through
  Nix.

## Install (local dev; not published)

```sh
nix-shell -p cargo rustc --run 'cargo build --release'
herdr plugin link "$PWD"
```

This checkout is already linked as `aliou.cast` on Aliou's machine. Do not
relink it during normal development. The Rust event handler verifies the app's
signature and refreshes its Launch Services registration before notification
delivery. Trigger one event, then grant notifications once under **System
Settings → Notifications → herdr → Allow**. The grant is keyed to the bundle
id, so it persists across plugin updates and moves unless the app is re-signed.

## Avoid double notifications

Keep herdr's toast INSIDE the herdr TUI so it doesn't double-post to the
desktop. Cast handles the native visual notification, while pi-harness owns
sound playback independently:

```toml
# ~/.config/herdr/config.toml
[ui.toast]
delivery = "herdr"   # in-app toast only; "terminal"/"system" double with cast

[ui.sound]
enabled = false      # pi-harness owns notification sounds
```

## Behavior

Cast has no configuration file or user-facing environment overrides. Its
personal defaults live in `src/notify.rs` and take effect with the next build:

- notify for `blocked` and `done`;
- suppress the banner only when its Herdr workspace is focused and a supported
  terminal is the frontmost macOS app;
- debounce the same pane and status for two seconds;
- group notifications by pane;
- use the bundled status icons; and
- raise Ghostty before focusing a clicked pane.

Cast intentionally does not play sounds. Pi-harness owns sound playback, so
focused-workspace suppression hides only the native banner.

## Architecture

The plugin has two executable surfaces:

- `assets/HerdrNotify.app` displays the macOS notification.
- `target/release/herdr-cast` handles events, state, socket requests, click
  focus, and the layout palette.

`herdr-plugin.toml` invokes the Rust binary with `notify` or `palette`. The
notification's click action invokes it with `focus`. The binary uses the
current `HERDR_SOCKET_PATH` and request shapes from `herdr api schema`; no shell
scripts, `jq`, or separate config parser are involved.

## Caveat: click focuses the pane inside herdr, raising the terminal window

The Rust `focus` command performs a server-side workspace/tab/pane switch. It
first runs `open -a Ghostty` so clicking from another app also raises the
terminal window.

## License

MIT for this plugin's own code. The bundled `assets/HerdrNotify.app` is a copy
of [`terminal-notifier`](https://github.com/julienXX/terminal-notifier) (MIT);
see `assets/HerdrNotify.app.LICENSE.md`.
