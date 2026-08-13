# herdr-cast

## Purpose and risk

This repository is a custom, unpublished Herdr plugin for Aliou's local setup.
It runs on macOS and Linux. Its plugin id is `ad.cast`. It is already linked,
enabled, and loaded from this checkout on this machine; linked plugins are
global to the local user and available to every Herdr session.

Do not run `herdr plugin link`, `herdr plugin unlink`, `herdr plugin install`,
`herdr plugin uninstall`, or change the plugin's enabled state unless the user
explicitly asks. Do not replace the local link with a managed GitHub install.

The plugin executes as the user and can post notifications, change Herdr
layout, focus panes, and raise terminal windows. Treat runtime testing as
state-changing work.

## Non-negotiable runtime isolation

Never test this plugin in the Herdr session containing the current agent or in
the default session. Use a fresh, uniquely named disposable session for every
runtime, event, pane, socket, focus, or layout test.

Read and follow `.agents/skills/herdr-throwaway-repro/SKILL.md` before any such
test. In particular:

- create the disposable session in a new outer pane;
- clear inherited socket, session, workspace, tab, and pane variables;
- explicitly set `HERDR_SESSION=<disposable-name>` on every control command;
- read all IDs from command output instead of constructing them;
- never stop, restart, delete, or kill the main Herdr server;
- close only the pane and named session created for the test; and
- complete cleanup even when the test fails.

Installed and linked plugins and their state are shared across named sessions.
Do not alter the existing `ad.cast` registration or real state to make a test
pass. If a manifest-registration test is necessary, copy the plugin to
`/var/tmp`, give the copy a unique temporary plugin id, link that id while
addressing only the disposable session, and unlink that exact temporary id
during cleanup. Do not run a live notification test unless the user explicitly
accepts the desktop notification and Launch Services side effects.

Read-only commands such as version, help, schema, plugin-list, and log
inspection are safe for discovery. Any command intended to exercise plugin
behavior belongs in the disposable session.

## Installed interface and protocol

The installed `herdr` binary is the authority for CLI syntax and protocol
shape. Inspect `herdr --version` and the relevant command group's help before
using it; do not assume the adjacent Herdr source checkout matches the running
version.

The JSON schema for the installed server's current protocol version is
available from:

```bash
herdr api schema
```

Use `herdr api schema --output /var/tmp/herdr-api-schema.json` when a file is
useful, and inspect `herdr api schema --help` for supported output options. Run
this before adding or changing raw socket requests, response parsing, event
payload assumptions, or plugin context fields. Do not copy the schema into this
repository or rely on a remembered shape.

This plugin deliberately talks to `HERDR_SOCKET_PATH` directly. Keep raw
requests aligned with the current schema and preserve the newline-delimited JSON
request/response contract.

## Repository map

- `herdr-plugin.toml`: plugin contract, build steps, event subscriptions, and
  pane entrypoints. Keep `min_herdr_version` aligned with the oldest protocol
  and manifest features actually used.
- `src/main.rs`: dispatches the Rust binary's `notify`, `palette`, `focus`,
  `sync-space`, `sync-title`, `sync-spaces`, and `shell-init` commands.
- `src/api.rs`: newline-delimited JSON client for the injected Unix socket.
- `src/notify.rs`: hard-coded personal notification behavior, event handling,
  state, Herdr enrichment, macOS frontmost-app detection, macOS notifier
  registration and delivery, Linux terminal notification requests, and macOS
  click-to-focus.
- `src/palette.rs`: popup layout palette. It uses `layout.export` and
  `pane.move` to flip a split or move the focused pane to a new workspace.
- `src/picker.rs`: reusable ratatui/crossterm fuzzy selector with readline
  editing, tree rows, and animated agent-status icons.
- `src/workspace.rs`: zoxide-backed workspace creation plus fuzzy workspace and
  pane focus through `workspace.create`, `workspace.list`, `pane.list`,
  `workspace.focus`, and `pane.focus`. The workspace picker has three views:
  `spaces` (workspace -> pane tree), `agents` (flat agent panes by status),
  and `panes` (every pane, most-recent-focus first via `src/recency.rs`).
- `src/recency.rs`: bounded move-to-front log of focused pane ids, recorded by
  the `record-focus` command on `pane.focused` events into the injected state
  directory. Read at picker open; stale ids for closed panes are filtered
  against `pane.list` and never name a pane.
- `src/space.rs`: Space sidebar metadata. Reports the `org`, `repos`, `host`,
  `hostkind`, and `pad` workspace tokens from the root pane's `cwd` and
  `pane.process_info`, and prints the zsh integration that triggers a sync.
  `space::describe` renders those tokens for one-line surfaces such as the
  workspace picker.
- `src/zoxide.rs`: filters zoxide to projects below `~/code/src`, adds `~/.dot`
  and top-level `~/tmp` directories, and persists the selected zoxide or
  alphabetical order.
- `assets/HerdrNotify.app`: bundled, rebranded `terminal-notifier`. Preserve its
  license in `assets/HerdrNotify.app.LICENSE.md`.

Herdr runs plugin commands with the plugin root as cwd and injects runtime
variables including `HERDR_BIN_PATH`, `HERDR_SOCKET_PATH`,
`HERDR_PLUGIN_CONFIG_DIR`, `HERDR_PLUGIN_STATE_DIR`,
`HERDR_PLUGIN_CONTEXT_JSON`, and entrypoint-specific event or pane variables.
The plugin has no config file or user-facing environment overrides. Personal
behavior is hard-coded in `src/notify.rs`. Store runtime artifacts only in the
injected state directory, never in the source checkout.

## Development behavior

- `~/.local/bin/herdr-cast` is the single command path Herdr and shells should
  resolve. In normal use it must be a symlink to the Nix-packaged
  `herdr-cast`, managed by the homelab Home Manager module at
  `~/code/src/code.378labs.dev/homelab/modules/modules/programs/herdr/default.nix`.
- Rust changes do not affect runtime until tested through that command path.
  For local runtime testing only, build `target/release/herdr-cast`, temporarily
  point `~/.local/bin/herdr-cast` at this checkout's release binary, run the
  disposable-session test, then restore `~/.local/bin/herdr-cast` to the Nix
  store path. Do not leave the symlink pointing at `target/release/herdr-cast`.
- `herdr plugin link` does not run manifest `[[build]]` commands.
- Manifest changes require registration refresh or a newly loaded server to be
  observed. Test them with the temporary-id workflow above, not by disturbing
  `ad.cast` in the current session.
- On macOS, the `notify` command refreshes Launch Services registration on a
  six-hour TTL. Re-signing can change the app identity and reset the macOS
  notification grant, so preserve verify-before-sign behavior.

Useful discovery and diagnostics commands include:

```bash
herdr plugin list --plugin ad.cast --json
herdr plugin config-dir ad.cast
herdr plugin log list --plugin ad.cast
herdr plugin pane --help
herdr api schema
```

Use the disposable-session environment prefix from the copied skill for every
runtime invocation.

## Implementation invariants

- Resolve event identity from the event payload. Never substitute the currently
  focused pane for a background event's pane.
- Filter non-triggering statuses before live Herdr enrichment to avoid needless
  socket calls.
- Keep Herdr enrichment and macOS focus detection best-effort. Detection
  failures must fail open so a duplicate notification is preferred over a
  silently missed notification.
- On non-macOS platforms, request terminal notifications through Herdr's
  `notification.show` socket method with `sound = "none"`. Do not write OSC
  directly to a pane PTY; Herdr owns the client notification path.
- `terminal-notifier -execute` evaluates one command string through the system
  shell. Keep every generated argument single-quoted and unit-test paths and
  pane IDs containing spaces, quotes, and shell metacharacters.
- Keep notification sound out of this plugin; it owns visual delivery only.
- Preserve macOS focused-workspace suppression: suppress only when both the
  Herdr workspace is focused and a supported terminal app is frontmost. Linux
  does not run this frontmost-app check.
- Space metadata describes the first tab's root pane, which is the pane Herdr
  uses for a Space's own Git identity and the first entry `pane.list` returns
  for a workspace. Never relabel a Space from a secondary pane.
- Never report a branch token, and never report the repository or directory a
  space sits in. Herdr derives `branch` and `git_status` from that same pane,
  and names the space after that repository or directory, renaming it when the
  pane moves. Space tokens answer where a space lives, not what it is.
- Derive remote sessions from `pane.process_info`, never from a typed command
  line. The shell integration only triggers a sync and passes no values.
- Sequence every metadata report with the current epoch milliseconds so a slow
  background sync cannot overwrite a newer one.
- Report absent values as `null` so stale tokens clear instead of lingering.
- Keep every Space two rows tall. Herdr hides a row whose tokens are all empty
  and trims whitespace out of metadata, so report the braille-blank `pad`
  token whenever nothing else, including Herdr's own branch, would render.
- Render Space tokens through `space::describe` everywhere outside the
  sidebar, so the picker and the sidebar cannot drift apart.
- Use injected context and opaque IDs. Never infer workspace, tab, or pane IDs.
- In Rust, represent protocol methods and payloads with serializable types,
  report malformed/error responses clearly, and consult `herdr api schema`
  before changing them.
- Keep personal policy constants in `src/notify.rs`. Do not add a config file or
  per-setting environment overrides without an explicit request.
- Add dependencies only when the Rust standard library and current crates
  cannot cover the need.

## Checks

Run checks from the repository root. Cargo is not normally on PATH on this
machine, so use Nix when needed:

```bash
nix-shell -p cargo rustc rustfmt --run 'cargo fmt -- --check'
nix-shell -p cargo rustc --run 'cargo test'
nix-shell -p cargo rustc --run 'cargo build --release'
```

Add focused unit tests for protocol serialization, event decisions, state-file
logic, shell quoting, frontmost-app parsing, and layout behavior. Static checks
may run in this checkout. Tests that connect to Herdr, invoke a plugin pane,
post a notification, focus or move panes, consume live events, sign the app, or
touch Launch Services must run only with explicit user approval and in a
disposable named session.

After runtime validation, inspect the temporary session's plugin logs and state,
record the Herdr version and observed result, then perform the skill's full
cleanup procedure.

## CI binaries

`Cargo.lock` is tracked because this binary is built by GitHub Actions.
Keep it current when `Cargo.toml` dependencies change.

`.github/workflows/ci.yml` builds these binaries on every push and pull
request:

- `herdr-cast-darwin-arm64`
- `herdr-cast-linux-arm64`
- `herdr-cast-linux-x64`

Linux artifacts target musl so NixOS consumers can fetch and run them without
patching a dynamic loader. Each workflow artifact has a matching `.sha256` file
with a Nix-compatible hash for downstream package definitions.

## Documentation triggers

Update `README.md` when behavior, setup, requirements, or hard-coded personal
policy changes. Update this file when the architecture, checks, protocol
workflow, installation state, or safety constraints change. Keep docs about
current behavior; use git history instead of leaving migration commentary.
