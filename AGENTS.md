# herdr-cast

## Purpose and risk

This repository is a custom, unpublished Herdr plugin for Aliou's local macOS
setup. Its plugin id is `ad.cast`. It is already linked, enabled, and loaded
from this checkout on this machine; linked plugins are global to the local
user and available to every Herdr session.

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

This macOS-only plugin deliberately talks to `HERDR_SOCKET_PATH` directly. Keep
raw requests aligned with the current schema and preserve the newline-delimited
JSON request/response contract.

## Repository map

- `herdr-plugin.toml`: plugin contract, build steps, event subscriptions, and
  pane entrypoints. Keep `min_herdr_version` aligned with the oldest protocol
  and manifest features actually used.
- `src/main.rs`: dispatches the Rust binary's `notify`, `palette`, and `focus`
  commands.
- `src/api.rs`: newline-delimited JSON client for the injected Unix socket.
- `src/notify.rs`: hard-coded personal notification behavior, event handling,
  state, Herdr enrichment, macOS frontmost-app detection, notifier registration,
  delivery, and click-to-focus.
- `src/palette.rs`: popup layout palette. It uses `layout.export` and
  `pane.move` to flip a split or move the focused pane to a new workspace.
- `src/picker.rs`: reusable ratatui/crossterm fuzzy selector with readline
  editing, tree rows, and animated agent-status icons.
- `src/workspace.rs`: zoxide-backed workspace creation plus fuzzy workspace and
  pane focus through `workspace.create`, `workspace.list`, `pane.list`,
  `workspace.focus`, and `pane.focus`.
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

- Rust changes require rebuilding `target/release/herdr-cast`; do not relink.
- `herdr plugin link` does not run manifest `[[build]]` commands.
- Manifest changes require registration refresh or a newly loaded server to be
  observed. Test them with the temporary-id workflow above, not by disturbing
  `ad.cast` in the current session.
- The `notify` command refreshes Launch Services registration on a six-hour
  TTL. Re-signing can change the app identity and reset the macOS notification
  grant, so preserve verify-before-sign behavior.

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
- `terminal-notifier -execute` evaluates one command string through the system
  shell. Keep every generated argument single-quoted and unit-test paths and
  pane IDs containing spaces, quotes, and shell metacharacters.
- Keep notification sound out of this plugin; it owns visual delivery only.
- Preserve focused-workspace suppression: suppress only when both the Herdr
  workspace is focused and a supported terminal app is frontmost.
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

## Documentation triggers

Update `README.md` when behavior, setup, requirements, or hard-coded personal
policy changes. Update this file when the architecture, checks, protocol
workflow, installation state, or safety constraints change. Keep docs about
current behavior; use git history instead of leaving migration commentary.
