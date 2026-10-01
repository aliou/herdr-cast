# Popups

Cast's popup entrypoints run TUIs inside Herdr plugin panes. The manifest
declares them under `[[panes]]`; key bindings open them through
`herdr-cast open-popup`, which sizes the popup before opening it.

## Sizing and lifecycle

```text
key binding (herdr config)
  → herdr-cast open-popup --entrypoint <id> --min-width .. --min-height ..
        --pct-width .. --pct-height ..        src/main.rs, src/popup.rs
    → pane.layout: current terminal area
    → dimensions: larger of pct of area and the fixed minimum
    → plugin.pane.open with concrete cell counts
```

Percentage sizing alone only applies to a bare `plugin.pane.open`; the
`open-popup` flags guarantee a usable minimum on small screens. A child CLI
streamed into the popup runs through `src/popup_cli.rs`, which inherits
stdin/stdout/stderr and renders a non-zero exit as a bold-red
`[cast] <program> exited with <status>` line.

On a failed interactive entrypoint, `src/main.rs` waits for a keypress
before the popup closes so the error is readable. The wait is an allowlist
over the popup entrypoints (`palette`, `directory-workspace`,
`workspace-picker`, `lazygit`, `hunk`, `hunk-log`); background hooks and
non-interactive commands never wait.

## The shared picker

`src/picker.rs` is one ratatui/crossterm fuzzy selector reused by every
popup: readline-style editing, tree rows, animated agent-status icons, and
`pick_nav`, the wizard-level variant whose Escape means "clear the query,
then go back one level" while Ctrl-C cancels outright.

Colors come from `src/theme.rs`: it reads Herdr's config
(`[theme.custom.dark]` / `[theme.custom.light]`, dark or light chosen by
querying the terminal background through OSC 11 with `COLORFGBG` fallback)
and maps the ten picker tokens onto it. An absent or unparsable config
falls back to the hard-coded Senzu palette; a bad config can never make a
picker unreadable.

| Action | Keys |
| --- | --- |
| Filter | type |
| Move | Up/Down, Ctrl+P/N, Ctrl+K/J |
| Select | Enter |
| Close | Esc, Ctrl+C |
| Toggle picker mode | Tab |
| Toggle priority/fuzzy sorting | Ctrl+S |
| Start/end of query | Home/End, Ctrl+A/E |
| Move by character | Left/Right, Ctrl+B/F |
| Delete previous word | Ctrl+W |
| Delete to start | Ctrl+U |

## Entry points

### Layout palette (`palette`)

`src/palette.rs`: flip a two-pane split (`layout.export` + `pane.move`,
rejecting nested layouts and restoring on failure), rename the current tab
or workspace (`tab.rename` / `workspace.rename`), and dispatch "Move pane…"
to the move wizard. The window title is owned by `src/title.rs`; nothing
here writes it.

### Move wizard (`palette` → "Move pane…")

`src/move_wizard.rs` keeps an explicit stack of picker levels:

```text
destination kind
  → fuzzy workspace → tab tree → split direction (tabs always ask)
  → or session picker (move to another Herdr session)
```

- Escape clears the query, then pops one level; Esc on the first level
  returns to the palette's root action list; Ctrl-C aborts from any depth.
- One snapshot of `workspace.list`, `tab.list`, the pane's location and
  details, and `herdr session list --json` serves every level.
- Same-session moves are one `pane.move` followed by an explicit
  `pane.focus` on the resulting pane: the popup closes on top of the move,
  and the post-move focus is what lands the user on the moved pane.
- Cross-session pane moves recreate the pane in the target session (same
  label, cwd, renamed) and resume the agent with the same per-agent resume
  flags Herdr itself uses, close the source pane, and end with a toast.
- Cross-session workspace moves replay every tab's layout from
  `layout.export` (`plan_replay` turns the tree into ordered `pane.split`
  steps), verify every pane was recreated, and close the source workspace
  only after full coverage. A failure before the close keeps the original.

### Workspace picker (`workspace-picker`)

`src/workspace.rs` focuses an existing workspace or pane. Three views:
`spaces` (workspace → pane tree), `agents` (flat agent panes by status),
and `panes` (every pane, most-recent focus first via `src/recency.rs`).
Rows carry their Space's machine tokens in search text and context labels,
read from server-held tokens via `space::describe` — a consumer never
re-derives a label from raw fields.

### New workspace (`directory-workspace`)

`src/workspace.rs` + `src/zoxide.rs`: candidates from `zoxide query -ls`
filtered below `~/code/src`, always including `~/.dot` and top-level
`~/tmp` directories, order persisted between zoxide and alphabetical.
Without zoxide (or with no ranked entries), it falls back to a filesystem
scan of `~/code/src` and `/workspace/code/src` for git repositories,
reusing lazygit's scanner. A directory already represented by a workspace
is focused instead of duplicated.

### lazygit (`lazygit`) and hunk (`hunk`, `hunk-log`)

`src/lazygit.rs` resolves the repository: the focused pane's repository, or
a fuzzy pick from repositories up to three levels below the pane's
directory. `resolve_repository` is shared with `src/hunk.rs`, which runs
`hunk diff --watch` (the whole working-tree changeset, reloaded while
agents edit) and `hunk log` at the repository root — hunk has no repository
flag.

Hunk review notes survive the popup: a generated extension
(`src/hunk-review-dump.mjs`, written into the plugin state directory and
loaded with `--extension`) dumps the review's notes (`hunk session comment
list --json` output) to a temp file on every change and at hunk's shutdown.
When hunk exits, the dump path is copied to the clipboard (best-effort) and
Herdr toasts about it through `notification.show` with `sound = "none"`.
With no notes, neither happens.
