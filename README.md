# herdr-cast

Aliou's personal [herdr](https://herdr.dev) customization hub, starting with
native macOS notifications that ring when a turn ends or an agent needs you,
and on click jump to the right pane/tab.

Named for *casting* — both broadcasting a signal (a notification you can hear
across the field) and casting a flock of sheep (tipping one onto its back to
work it). It started as a notifier and is now the dumping ground for all my
herdr customizations. Not published; just a local `herdr plugin link`.

## What it does

When an agent pane's status changes, herdr fires a
`pane.agent_status_changed` event. Cast listens, and for the statuses in
`TRIGGER_STATUSES` (default `blocked done`) it posts a native macOS notification
through the bundled `HerdrNotify.app` (a rebranded `terminal-notifier` whose
bundle id is `codes.dot.herdr-notify`, so the **left** icon is the herdr logo).
Clicking the notification runs `herdr agent focus <pane>`, which switches herdr
to that pane's workspace + tab + pane.

Status semantics in herdr (relevant for pi):

- `blocked` — the agent needs your input mid-turn (attention / dangerous /
  error). Reported by the pi integration via `herdr:blocked`.
- `done` — a turn ended and you haven't looked at the pane yet. herdr
  re-classifies a fresh `idle` as `done` in `agent_view.rs`, so pi's turn-end
  `idle` arrives here as `done`. Exactly the "turn end" trigger.

## Requirements

- macOS. Tested on macOS 26.
- `jq` (the only runtime dep). On this machine it's in PATH via Nix.

## Install (local dev; not published)

```sh
bash scripts/link.sh          # links this repo as a plugin + registers the notifier app
# or, equivalently:
herdr plugin link "$PWD"
bash scripts/setup.sh
```

Then trigger one event (let an agent go blocked or finish a turn) and grant
notifications once: **System Settings → Notifications → herdr → Allow**. The
grant is keyed to the bundle id, so it persists across plugin updates and moves.

## Avoid double notifications

Keep herdr's toast INSIDE the herdr TUI so it doesn't double-post to the
desktop (cast handles the OS notification):

```toml
# ~/.config/herdr/config.toml
[ui.toast]
delivery = "herdr"   # in-app toast only; "terminal"/"system" double with cast
```

## Configuration

Optional. All keys have built-in defaults (see `lib/config.sh`). Resolution
order (later wins): defaults → env var →
`$HERDR_PLUGIN_CONFIG_DIR/config.env` → `$CAST_CONFIG`.

Point your dotfiles at a config file:

```sh
export CAST_CONFIG="$HOME/.config/herdr-cast/config.env"
```

Key settings:

| Key | Default | Meaning |
| --- | --- | --- |
| `TRIGGER_STATUSES` | `blocked done` | which new statuses ring |
| `SUPPRESS_FOCUSED` | `1` | mute only when the workspace is focused in herdr **and** a terminal from `TERMINAL_APP_IDS` is frontmost |
| `DEBOUNCE_SECONDS` | `2` | drop repeated `(pane,status)` within window |
| `ACTIVATE_ON_CLICK` | `1` | click → focus the agent |
| `CLICK_COMMAND` | `agent focus {pane}` | `herdr` subcommand run on click |
| `ACTIVATE_APP` | _(auto)_ | terminal app name `open -a` raises before focusing the pane; auto-detected from `TERM_PROGRAM` (Ghostty/WezTerm), falls back to `ACTIVATE_APP_FALLBACK` (default `Ghostty`) |
| `GROUP` | `{pane}` | `-group` key; widen to `{pane}-{new_status}` so `done` doesn't hide unread `blocked` |
| `TITLE_<STATUS>` / `BODY_<STATUS>` / `ICON_<STATUS>` / `SOUND_<STATUS>` | see config | per-status templates |
| `NOTIFIER` | _(bundled app)_ | override the notifier binary |
| `REGISTER_TTL_SECONDS` | `21600` | self-heal the Launch Services registration (left icon) |
| `DEBUG` | `0` | dump event/context JSON + decision trace to the state dir |

Template placeholders: `{agent} {workspace} {worktree} {tab} {tab_label}
{pane} {session} {old_status} {new_status} {cwd}`. `<STATUS>` is upper-cased;
`*_DEFAULT` covers the rest.

Sound mapping (mirrors `pi-harness/hooks/chrome/hooks/notification.ts`):

- `blocked` → **Glass** (attention / dangerous / error all funnel to blocked
  via `hooks/herdr/index.ts`)
- `done` → **Funk** (clean turn end; herdr re-classifies a fresh `idle` as
  `done` in `agent_view.rs`)
- an errored turn surfaces as `blocked`, so an `ERROR_SOUND` (Basso) path is
  unreachable as `done`; kept as the DEFAULT fallback.

## Caveat: click focuses the pane inside herdr, raising the terminal window

`herdr agent focus <pane>` is a server-side workspace/tab/pane switch. To also
raise the terminal window to the foreground when you click from another app,
cast prepends `open -a <ACTIVATE_APP>` to the click command. `ACTIVATE_APP` is
auto-detected from `TERM_PROGRAM` (Ghostty and WezTerm are recognized; anything
else falls back to `ACTIVATE_APP_FALLBACK`, default `Ghostty`). Set `ACTIVATE_APP`
explicitly to force a value, or empty to disable window activation.

## License

MIT for this plugin's own code. The bundled `assets/HerdrNotify.app` is a copy
of [`terminal-notifier`](https://github.com/julienXX/terminal-notifier) (MIT);
see `assets/HerdrNotify.app.LICENSE.md`.
