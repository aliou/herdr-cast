# Spaces, session tokens, and the window title

Cast reports Herdr sidebar metadata and owns the foreground terminal window
title. Every value derives from the server's API; the shell integration only
triggers a sync and passes no values.

## Space sidebar tokens

`src/space.rs` reports workspace tokens that fill a Space's second sidebar
row. They describe where a Space lives, never what it is — Herdr names the
Space after the root pane's repository or directory and derives `branch` and
`git_status` from that same pane, so Cast never reports a branch token or
the Space's own name.

| Token | Meaning |
| --- | --- |
| `$org` | organization or client owning the root pane's directory; dropped when it repeats the Space name |
| `$repos` | how many repositories a container directory holds; reported from two up |
| `$host` | first label of the host of a remote session in the root pane |
| `$hostkind` | `sbx` when that host is a lab sandbox |
| `$pad` | braille blank holding the row open when nothing else would render |

Rules:

- Only the first tab's root pane counts. That is the pane Herdr uses for the
  Space's Git identity, so a second pane running ssh never relabels the
  Space.
- A remote session replaces the local tokens; the host is read from the
  pane's `pane.process_info`, never from a typed command line, so wrappers
  such as `sbxctl connect` still resolve.
- Absent values are reported as `null` so stale tokens clear.
- Reports carry the current epoch milliseconds as their sequence, so a slow
  background sync cannot overwrite a newer one.

`space::describe` renders the tokens for one-line surfaces; the workspace
picker reads the server-held tokens rather than recomputing them, so the
sidebar and the popup cannot drift.

Refreshes come from the `sync-spaces` startup hook (Herdr drops tokens when
a new server restores a session), the `workspace.created` and
`workspace.focused` event hooks, and the zsh integration (`shell-init`).
Herdr withholds `pane.updated` from plugin hooks, so there is no
per-command event to subscribe to.

## Session token

`src/session.rs` reports a pane's `$session` token through
`pane.report_metadata` under the `plugin:ad.cast` source. Only the title
daemon writes it — Herdr tracks the report sequence per terminal and source,
so a second writer under the same source would fight the daemon's monotonic
counter.

The value is the pi session name: pi writes titles as
`π - <session name> - <cwd>` and Herdr's stripped title keeps the `π`, so
the token strips that prefix and the trailing cwd basename. An unnamed pi
session is labeled from the first user message of its session file — the
skill name for a skill opener, else the prompt's first words — then the
title body, then the agent kind. Reports carry only the `session` token, so
they never bind to an agent lifecycle.

Reporter state is keyed by terminal id, pruned against `pane.list`, fully
resent every 60 seconds, and sequenced by a monotonic counter.

## Window title

`src/title.rs` owns the foreground terminal window title, composed as
`HOSTNAME › SESSION_NAME › terminal_title`:

- the hostname fragment appears only when the server's inherited environment
  carries `SSH_CONNECTION` or `SSH_TTY` (interactive ssh or `herdr
  --remote`);
- the session fragment appears only for named sessions;
- the tail is the focused pane's `terminal_title_stripped`.

Absent fragments drop out with their separators. A server started outside
ssh and attached later keeps no hostname fragment, because that cannot be
told apart from a local server.

Because Herdr withholds `pane.updated` from plugin hooks, nothing announces
a title change. A resident `herdr-cast daemon` per session polls `pane.list`
and reapplies the title when it changes; `sync-title` (startup hook and the
`pane.focused` coordinator) pushes it immediately and respawns the daemon if
it died. The daemon holds an `flock` keyed by the socket path in the state
directory and exits after persistent socket loss.

Only `title.rs` may write the title: an explicit title suppresses Herdr's
template, and a second writer would fight the daemon within one poll.
