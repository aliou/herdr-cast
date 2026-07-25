#!/usr/bin/env bash
# Config loading for the Cast herdr plugin.
#
# Resolution order (later wins), so the source of truth can live in
# dotfiles (nix/chezmoi) instead of herdr's per-machine config dir:
#   1. built-in default  (the literal in this file)
#   2. environment variable  (exported before the handler runs)
#   3. $HERDR_PLUGIN_CONFIG_DIR/config.env  (herdr-managed, per machine)
#   4. $CAST_CONFIG                   (a dotfiles-managed file you point at)
#
# `_cast_default VAR "default"` sets VAR only when unset OR empty, so an exported
# env var wins over the default; the two files are sourced after these
# assignments and therefore still override the environment.
#
# Caveat of `:-` semantics: exporting a var EMPTY (e.g. `export NOTIFIER=`)
# counts as unset, so the built-in default applies. To force "empty means empty",
# set the key in a config file instead.
# shellcheck disable=SC2034

_cast_default() { [ -n "${!1:-}" ] || printf -v "$1" '%s' "$2"; }

# herdr binary the handler calls back into. herdr injects HERDR_BIN_PATH.
[ -n "${HERDR_BIN:-}" ] || HERDR_BIN="${HERDR_BIN_PATH:-herdr}"

# Which NEW agent statuses trigger a notification (space separated).
# Valid herdr statuses: working blocked idle done unknown
#   blocked = agent needs your input mid-turn (attention / dangerous / error)
#   done    = a turn ended and you haven't looked at the pane yet
#             (herdr re-classifies a fresh idle as `done` in agent_view.rs,
#              so pi's turn-end `idle` shows up here as `done`)
_cast_default TRIGGER_STATUSES "blocked done"

# Stay quiet for the workspace you are CURRENTLY looking at. Fires only when
# BOTH the event's workspace is focused inside herdr AND a terminal listed in
# TERMINAL_APP_IDS is the frontmost macOS app. So starting an agent and
# switching to the browser still delivers the notification (the workspace stays
# "focused" inside herdr even though you left the terminal).
# 1 = enable, 0 = always notify.
_cast_default SUPPRESS_FOCUSED "1"

# Bundle ids (space separated) of terminal apps that can host herdr, used by the
# frontmost check above. Empty / undetectable -> suppress is skipped (fail
# open: a duplicate beats a missed alert). Find an app's id with:
#   osascript -e 'id of app "Ghostty"'
_cast_default TERMINAL_APP_IDS "com.mitchellh.ghostty com.apple.Terminal com.googlecode.iterm2 net.kovidgoyal.kitty com.github.wez.wezterm org.alacritty"

# Drop a repeated (pane,status) seen within this many seconds (flap guard).
_cast_default DEBOUNCE_SECONDS "2"

# Click the notification to jump to the agent that changed.
# 1 = enable click-to-jump, 0 = post a quiet notification.
_cast_default ACTIVATE_ON_CLICK "1"

# How to focus on click. {pane}/{workspace}/{agent} are substituted before run.
# `agent focus {pane}` lands on the exact agent pane. The template's literal
# words are shell-word-split into command args; substituted VALUES are
# shell-quoted, so each is exactly one literal argument and can't inject shell
# syntax. Do NOT quote placeholders ("{pane}") yourself.
_cast_default CLICK_COMMAND "agent focus {pane}"

# terminal-notifier's click action runs `herdr <CLICK_COMMAND>`, which focuses
# the pane server-side but does NOT raise the terminal window to the foreground.
# When you click a notification from another app, prepend `open -a <name>` so
# the terminal window comes forward first. Use the app name `open -a` accepts.
#
# Auto-detected from TERM_PROGRAM (the env var the terminal sets, inherited by
# herdr and its plugin handlers). Only Ghostty and WezTerm are handled; if
# TERM_PROGRAM is neither or unset, falls back to ACTIVATE_APP_FALLBACK below.
# Set ACTIVATE_APP explicitly in a config file to force a value (or empty to
# disable window activation).
_cast_default ACTIVATE_APP_FALLBACK "Ghostty"
case "${TERM_PROGRAM:-}" in
  ghostty) _cast_default ACTIVATE_APP "Ghostty" ;;
  WezTerm) _cast_default ACTIVATE_APP "WezTerm" ;;
  *)       _cast_default ACTIVATE_APP "$ACTIVATE_APP_FALLBACK" ;;
esac

# Notifier binary. Empty = use the bundled assets/HerdrNotify.app (herdr icon),
# falling back to a system `terminal-notifier` on PATH. Set an absolute path to
# use a different notifier build.
_cast_default NOTIFIER ""

# How often (seconds) to refresh the bundled app's Launch Services
# registration. Ad-hoc-signed helpers can lose registration over time
# (reboots, OS updates), which makes macOS show the parent terminal's icon
# instead of the herdr logo. notify.sh re-registers when the sentinel is older
# than this, so the icon self-heals. Default 6h.
_cast_default REGISTER_TTL_SECONDS "21600"

# Right-side image mode (terminal-notifier -contentImage vs -appIcon).
# contentImage is reliable on modern macOS; appIcon is often ignored.
# The LEFT icon is always the notifier app's (the bundled app = herdr logo).
_cast_default ICON_MODE "contentImage"

# Notification group key (template, same placeholders as titles).
# terminal-notifier REPLACES any earlier notification sharing this -group, so
# the default "{pane}" keeps one live notification per pane. Widen it so a
# later transition does not hide an earlier still-unread one, e.g.
# "{pane}-{new_status}" gives blocked and done distinct groups. Set GROUP=""
# in a config file to disable grouping entirely (every notification stacks).
_cast_default GROUP "{pane}"

# Per-status presentation. Placeholders:
#   {agent} {workspace} {worktree} {tab} {tab_label} {pane} {session}
#   {old_status} {new_status} {cwd}
_cast_default TITLE_BLOCKED "⏳ {agent} needs input"
_cast_default BODY_BLOCKED  "{workspace} · {worktree}"
_cast_default ICON_BLOCKED  "assets/icons/blocked.png"
# Harness sound mapping (hooks/chrome/hooks/notification.ts):
#   ATTENTION (Glass) -> blocked: needs input / dangerous / error (all funnel
#                        to blocked via hooks/herdr/index.ts)
#   DONE-OK   (Funk)  -> done: clean turn end (herdr re-classifies a fresh
#                        idle as done in agent_view.rs)
#   DONE-ERR  (Basso) -> unreachable as `done` (an errored turn becomes
#                        `blocked`); kept as the DEFAULT fallback just in case.
_cast_default SOUND_BLOCKED "Glass"

_cast_default TITLE_DONE "✅ {agent} done"
_cast_default BODY_DONE  "{workspace} · {worktree}"
_cast_default ICON_DONE  "assets/icons/done.png"
_cast_default SOUND_DONE "Funk"

# Catch-all for any other triggered status.
_cast_default TITLE_DEFAULT "{agent}: {new_status}"
_cast_default BODY_DEFAULT  "{workspace} · {worktree}"
_cast_default ICON_DEFAULT  "assets/icons/working.png"
_cast_default SOUND_DEFAULT "none"

# Set DEBUG=1 to dump the raw event/context JSON + the decision trace to the
# state dir (handy after a herdr upgrade).
_cast_default DEBUG "0"

# --- overrides --------------------------------------------------------------

_cast_load() {
  local f="$1"
  [ -n "$f" ] && [ -f "$f" ] || return 0
  # shellcheck disable=SC1090
  . "$f"
}

_cast_load "${HERDR_PLUGIN_CONFIG_DIR:-}/config.env"
_cast_load "${CAST_CONFIG:-}"
