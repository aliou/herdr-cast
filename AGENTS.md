# herdr-cast

## Purpose and risk

This repository is a custom, unpublished Herdr plugin for Aliou's local macOS
setup. Its plugin id is `aliou.cast`. It is already linked, enabled, and loaded
from this checkout on this machine; linked plugins are global to the local
user and available to every Herdr session.

Do not run `scripts/link.sh`, `herdr plugin link`, `herdr plugin unlink`,
`herdr plugin install`, `herdr plugin uninstall`, or change the plugin's enabled
state unless the user explicitly asks. Do not replace the local link with a
managed GitHub install.

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

Installed and linked plugins and their config are shared across named sessions.
Do not alter the existing `aliou.cast` registration or its real config to make a
test pass. If a manifest-registration test is necessary, copy the plugin to
`/var/tmp`, give the copy a unique temporary plugin id, link that id while
addressing only the disposable session, and unlink that exact temporary id
during cleanup. Use a mock notifier and temporary config/state paths so tests
do not post real desktop notifications or overwrite user state.

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

Prefer calls through `HERDR_BIN_PATH` in portable plugin code. Raw socket code
may use `HERDR_SOCKET_PATH`; it must follow the current schema and preserve the
newline-delimited JSON request/response contract.

## Repository map

- `herdr-plugin.toml`: plugin contract, build steps, event subscriptions, and
  pane entrypoints. Keep `min_herdr_version` aligned with the oldest protocol
  and manifest features actually used.
- `bin/notify.sh`: `pane.agent_status_changed` handler. It filters status,
  enriches event data through Herdr, suppresses/debounces, expands templates,
  and invokes the notifier.
- `lib/config.sh`: defaults and config precedence. Later sources win: built-in
  defaults, environment, Herdr's plugin config file, then `CAST_CONFIG`.
- `lib/herdr.sh`: best-effort Herdr CLI queries parsed with `jq`.
- `lib/macos.sh`: best-effort frontmost-app detection. Detection failures must
  fail open so a notification is duplicated rather than silently lost.
- `src/main.rs`: Rust popup layout palette. It reads injected plugin context
  and uses `layout.export` and `pane.move` to flip a split or move the focused
  pane to a new workspace.
- `assets/HerdrNotify.app`: bundled, rebranded `terminal-notifier`. Preserve its
  license in `assets/HerdrNotify.app.LICENSE.md`.
- `scripts/setup.sh`: signs when needed and registers the notifier app with
  Launch Services.
- `scripts/link.sh`: one-time local-link helper, not a routine development or
  test command on this machine.
- `config/config.example.env`: user-facing configuration example. Keep it and
  `README.md` synchronized with configuration or behavior changes.

Herdr runs plugin commands with the plugin root as cwd and injects runtime
variables including `HERDR_BIN_PATH`, `HERDR_SOCKET_PATH`,
`HERDR_PLUGIN_CONFIG_DIR`, `HERDR_PLUGIN_STATE_DIR`,
`HERDR_PLUGIN_CONTEXT_JSON`, and entrypoint-specific event or pane variables.
Store editable config in the config directory and runtime artifacts in the
state directory, never in the source checkout.

The shell handler's runtime dependency is `jq`; it is available through Nix on
this machine and must be on the plugin process's `PATH`.

## Development behavior

- Linked shell-script changes apply on the next invocation; do not relink.
- Rust changes require rebuilding `target/release/layout-palette`; do not
  relink.
- Set `PALETTE_CHOICE` to `flip split direction` or
  `move pane to new workspace` to drive the Rust palette without the interactive
  skim UI in a disposable-session test.
- `herdr plugin link` does not run manifest `[[build]]` commands.
- Manifest changes require registration refresh or a newly loaded server to be
  observed. Test them with the temporary-id workflow above, not by disturbing
  `aliou.cast` in the current session.
- Run `scripts/setup.sh` only when notifier app signing or Launch Services
  registration is part of the work. Re-signing can change the app identity and
  reset the macOS notification grant.

Useful discovery and diagnostics commands include:

```bash
herdr plugin list --plugin aliou.cast --json
herdr plugin config-dir aliou.cast
herdr plugin log list --plugin aliou.cast
herdr plugin pane --help
herdr api schema
```

Use the disposable-session environment prefix from the copied skill for every
runtime invocation.

## Implementation invariants

- Keep Bash entrypoints compatible with Bash and under `set -euo pipefail`.
- Keep `jq` as the explicit parser for runtime JSON; fail loudly when it is
  missing instead of making decisions from empty fields.
- Resolve event identity from the event payload. Never substitute the currently
  focused pane for a background event's pane.
- Filter non-triggering statuses before live Herdr enrichment to avoid needless
  socket calls.
- Keep Herdr enrichment and macOS focus detection best-effort where documented.
- Preserve shell quoting around substituted click-command values. Event,
  context, pane, workspace, agent, and config values must not become shell
  syntax.
- Keep notification sound out of this plugin; it owns visual delivery only.
- Preserve the `SUPPRESS_FOCUSED` rule: suppress only when both the Herdr
  workspace is focused and a configured terminal app is frontmost.
- Use injected context and opaque IDs. Never infer workspace, tab, or pane IDs.
- In Rust, represent protocol methods and payloads with serializable types,
  report malformed/error responses clearly, and consult `herdr api schema`
  before changing them.
- Add dependencies only when the existing shell, Rust standard library, and
  current crates cannot cover the need.

## Checks

Run checks from the repository root. Cargo is not normally on PATH on this
machine, so use Nix when needed:

```bash
bash -n bin/notify.sh lib/*.sh scripts/*.sh
nix-shell -p shellcheck --run \
  'shellcheck bin/notify.sh lib/*.sh scripts/*.sh'
nix-shell -p cargo rustc rustfmt --run 'cargo fmt -- --check'
nix-shell -p cargo rustc --run 'cargo test'
nix-shell -p cargo rustc --run 'cargo build --release'
```

Add focused unit tests for pure Rust behavior and shell tests with mocked
`HERDR_BIN_PATH`, notifier, event/context JSON, config, and state directories.
Static checks may run in this checkout. Tests that connect to Herdr, invoke a
plugin pane, post a notification, focus or move panes, or consume live events
must run only in a disposable named session.

After runtime validation, inspect the temporary session's plugin logs and state,
record the Herdr version and observed result, then perform the skill's full
cleanup procedure.

## Documentation triggers

Update `README.md` and `config/config.example.env` when behavior, setup,
requirements, placeholders, defaults, or configuration changes. Update this
file when the architecture, checks, protocol workflow, installation state, or
safety constraints change. Keep docs about current behavior; use git history
instead of leaving migration commentary.
