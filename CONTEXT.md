# Context

Glossary for herdr-cast. Read this before naming or changing anything that
touches these terms. Each entry states the distinction that prevents a real
misunderstanding.

## Herdr surfaces

- **Herdr** — the terminal multiplexer this plugin extends. A **server**
  runs per session; **clients** attach to it (Ghostty directly, over ssh, or
  via `herdr --remote`).
- **Session** — one running Herdr server and its socket. Herdr sessions have
  names; the default session has none. Cast code must always resolve ids
  from the injected event or socket, never from the focused pane.
- **Workspace** — a Herdr workspace: a group of tabs with a label and its
  own metadata tokens. Colloquially a **Space**, the name Herdr's UI uses.
- **Tab** — one tab of a workspace. **Pane** — one terminal region in a tab.
  The **root pane** is a tab's first pane; Space identity and Git metadata
  derive from the first tab's root pane only.
- **Token** — a named metadata value a plugin reports to a workspace or pane
  (for example `$host`, `$pad`, `$session`). Herdr renders tokens in
  configurable sidebar rows; a missing token renders as absent.

## The bridge

- **Host** — the Mac running the Herdr client the user looks at. One host
  bridge daemon runs per user (and per Herdr server) there.
- **Remote** — any machine a `herdr --remote <target>` client is attached
  to. The remote runs no daemon; it runs a relay per active link.
- **Link** — one `ssh -T` connection from the host daemon to a remote, over
  which the relay and host exchange frames. Targets come from scanning
  running `herdr --remote` processes; there is no machine list.
- **Relay** — `herdr-cast bridge-relay`, the remote end of a link. It owns
  `~/.local/state/herdr-cast/bridge.sock` while it lives; senders on the
  remote connect to that socket.
- **Tunnel** — the host-side session for one link (`src/bridge/tunnel.rs`):
  spawn, hello handshake, pings, backoff, and takeover parking.
- **Sender** — code on the remote that tries the bridge first (the notify
  hook, the pasteboard watcher) and falls back to non-bridge paths.
- **Frame** — one wire message: a JSON header line plus an optional raw
  body. `Hello`, `Ping`, `Bye`, `Notify`, `Pasteboard`, `Ack`, `Focus`.
- **Ack** — the host's reply after applying a request, never before.
- **Control socket** — `bridge-control.sock` in the host's plugin state
  directory; the notification click talks to it so the host daemon sends a
  Focus frame down the right link.

## Notifications

- **Trigger** — an agent status in the hard-coded set (`blocked`, `done`).
  Nothing else notifies, and nothing suppresses a trigger: focus, tab, and
  frontmost-app state are irrelevant by design.
- **Debounce** — the two-second per pane-and-status window that collapses
  duplicate events. The only filter besides the trigger set.
- **Delivery chain** — bridge first, then the server's `notification.show`
  (rendered by an attached client), then the local macOS notifier. Every
  step fails open to the next.
- **Bridged delivery** — the host applying a notification that arrived over
  a link, through the status's `HerdrNotify` bundle.
- **Forwarder** — `herdr-cast forward-notify`, exec'd by the
  `terminal-notifier` shim the Nix package installs. It renders forwarded
  remote payloads through the status bundle and passes every other
  invocation through the neutral bundle verbatim.
- **Forwarded payload** — the compact JSON body (`a`/`w`/`p`/`h`/`g`/`s`,
  legacy `t`/`b`) that rides inside a `notification.show` body so layout
  parts, grouping, and sound survive Herdr's title/body-only protocol.
- **Identity bundle** — one `HerdrNotify*.app` directory per status
  (`HerdrNotify`, `-blocked`, `-done`). macOS takes a notification's icon
  from the sender bundle, so the status is visual, never a glyph in text.
- **Outstanding notification** — the state marker recorded after local macOS
  delivery so a later focus or pane close can remove that group.

## Runtime

- **Resident daemon** — a long-lived `herdr-cast` process: the host bridge,
  the title daemon (`daemon`), the pasteboard watcher (`forward-daemon`),
  and remote relays. They keep the binary they started with; a deploy
  reaches them only after a Herdr restart or a kill.
- **State directory** — `HERDR_PLUGIN_STATE_DIR`, injected per hook. All
  runtime artifacts live there except the remote bridge socket.
- **Popup entrypoint** — a manifest `[[panes]]` id opened through
  `herdr-cast open-popup` (`palette`, `directory-workspace`,
  `workspace-picker`, `lazygit`, `hunk`, `hunk-log`). Only these may wait
  for a keypress on error.
- **Picker** — the shared fuzzy selector (`src/picker.rs`) used by every
  popup; themed from Herdr's config by `src/theme.rs`.
- **Recency log** — the bounded focus-ordered pane list the `pane.focused`
  coordinator records, read by the workspace picker's panes view.
