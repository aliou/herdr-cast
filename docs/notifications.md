# Notifications

Cast notifies on agent status changes. The trigger set (`blocked`, `done`)
and every presentation constant are hard-coded in `src/notify/mod.rs` and
`src/notify/compose.rs`; there is no config file and no environment
override.

## Pipeline

```text
pane.agent_status_changed (manifest hook)
  → notify::run                                        src/notify/hook.rs
    → events::PluginEvent::from_environment            src/events.rs
    → filter: status in TRIGGER_STATUSES (blocked, done)
    → enrich: pane.get / workspace.get over the Herdr socket
    → debounce: 2 s per pane+status (state dir)
    → deliver (first path that takes it wins)
        1. bridge::send_notification                    src/bridge/mod.rs
        2. notification.show on the Herdr socket (sound = "none")
        3. local macOS notifier (macOS only)
```

Every delivery step fails open to the next, and Herdr enrichment is
best-effort: a missed enrichment never loses the notification. Every
triggered notification is delivered regardless of pane, tab, workspace, or
frontmost-app focus; the trigger set and the debounce window are the only
filters.

## Layout

`compose` (`src/notify/compose.rs`) renders the two-line layout every
surface shares:

- title: the project, falling back to the workspace label then `herdr`;
  forwarded notifications carry `PROJECT@HOST`;
- subtitle: the action (`<agent> needs input` / `<agent> done`), extended
  with `· <workspace>` only when the label differs from the title;
- no body line. Status is visual (per-status bundle icon) and audible
  (`Glass` for blocked, `Funk` for done), never an emoji or glyph.

Text is glyph-free in every path, including the forwarder, so the two can
never drift.

## Delivery paths

### Bridge (first)

See docs/bridge.md. The remote sends a `Notify` frame; the host applies it
through the status bundle with grouping `host:pane` and acks after apply.
A missing relay socket is the normal case on the host itself and falls back
silently.

### Terminal notification (second)

`notification.show` on the Herdr socket, `sound = "none"`. The body carries
the forwarded payload (below). When the server reports `shown: true`, the
notification already rendered in an attached client, and a macOS sender
skips local delivery. On Linux this is the final path.

### Local notifier (macOS fallback)

`notify::local` runs the bundled notifier for the status, grouped per
pane and session, and records an outstanding-notification marker. Herdr
requests never set a sound field; the bundled notifier owns the status
sound.

## Clearing

- `pane.focused` on a pane clears that pane's delivered local macOS
  notification (`clear_delivered_for_pane`, called from the coordinator in
  `src/main.rs`).
- `pane.closed` clears it because it is no longer actionable
  (`clear-notification` → `clear_from_event`).
- Delivery uses one bundle identity per triggering status, so each
  outstanding bundle removes its own pane group. Local group ids include a
  hash of the Herdr socket (or session name), so one session cannot clear
  another's notification.

Clearing is best-effort: a notifier that cannot remove keeps the
outstanding marker.

## Identity bundles

macOS draws a notification's icon from the sender bundle's registered icon
and offers no per-notification override, so each status ships as its own
app identity in `assets/`:

- `HerdrNotify.app` — neutral (pass-through, Herdr toasts);
- `HerdrNotify-blocked.app` — orange dot icon;
- `HerdrNotify-done.app` — green dot icon.

`scripts/gen-notify-bundles.py` generates the two status variants from the
base (composited icon, own bundle id, ad-hoc signed) and the bundles are
committed. Registration (codesign verify-then-sign, `lsregister`) runs
lazily under a lock with a six-hour TTL sentinel; re-signing can change the
identity and reset the macOS grant, so verify runs before sign.

## Forwarder (`forward-notify`)

Herdr's macOS client renders `notification.show` by running whatever
`terminal-notifier` it finds on `PATH`. The Nix package installs a shim of
that name next to `herdr-cast`, which execs `herdr-cast forward-notify`
(`src/notify/forwarder.rs`):

- an invocation matching Herdr's client grammar whose body decodes as a
  forwarded payload (`v: 1`) is rebuilt: layout recomposed from
  `a`/`w`/`p`/`h` parts, grouping from `g`, sound from `s`, rendered
  through the status bundle;
- legacy `t`/`b` payloads still render (title/body with the host as
  subtitle) so older remote senders keep working;
- everything else passes through the neutral bundle verbatim, so Herdr's
  own toasts and generic callers are untouched.

The payload caps each field so the encoded JSON always fits Herdr's
240-character body cap; the caps are pinned by tests. The shim resolves the
bundles from `libexec/` next to the canonicalized executable — it runs in
client context, never in plugin context — and execs the bundle binary so
its NSBundle identity holds.

## Click-to-focus (macOS)

A local notification's click runs `herdr-cast focus <socket> <pane>`
(`src/notify/focus.rs`):

1. `agent.focus` on the notification's socket — the pane focuses inside
   Herdr first and is never delayed by window work;
2. best-effort raise of the Ghostty window or tab serving that session:
   the client pid is read from the socket's peer pid and its parent, then
   matched against Ghostty's terminal surfaces via AppleScript `focus`;
3. no match (remote session, socket gone): `open -a Ghostty`.

Linux terminal notifications are not click-to-focus.

## State

All state lives in the injected plugin state directory: debounce files,
outstanding-notification markers, lifecycle locks, and registration
sentinels. Nothing is written to the source checkout.
