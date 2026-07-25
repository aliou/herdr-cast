#!/usr/bin/env bash
# Cast event handler: turn a pane.agent_status_changed event into a
# macOS notification that, on click, focuses the right herdr pane.
#
# herdr runs this with the plugin directory as cwd and injects:
#   HERDR_PLUGIN_EVENT, HERDR_PLUGIN_EVENT_JSON, HERDR_PLUGIN_CONTEXT_JSON,
#   HERDR_BIN_PATH, HERDR_PLUGIN_CONFIG_DIR, HERDR_PLUGIN_STATE_DIR, ...
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=lib/config.sh
. "$ROOT/lib/config.sh"
# shellcheck source=lib/herdr.sh
. "$ROOT/lib/herdr.sh"
# shellcheck source=lib/macos.sh
. "$ROOT/lib/macos.sh"

STATE_DIR="${HERDR_PLUGIN_STATE_DIR:-${TMPDIR:-/tmp}/cast}"
mkdir -p "$STATE_DIR"

log() { printf '[cast] %s\n' "$*" >&2; }

DEBUG_FILE="$STATE_DIR/last-event.json"
dbg() { [ "$DEBUG" = "1" ] && printf '%s\n' "$*" >>"$DEBUG_FILE"; return 0; }

# drop [--loud] <reason...>: record the decision and exit 0. The reason goes to
# stderr only under DEBUG=1 by default; pass --loud for rare/anomalous drops
# that always warrant a line in herdr's log.
drop() {
  local loud=0
  [ "${1:-}" = "--loud" ] && { loud=1; shift; }
  { [ "$loud" = 1 ] || [ "$DEBUG" = "1" ]; } && log "drop $*"
  dbg "decision=drop $*"
  exit 0
}

# jq is required for everything we read out of the event/context and out of
# live herdr responses. Without it the helpers below degrade to empty strings
# and the event would drop for a bogus reason. Fail loudly.
command -v jq >/dev/null 2>&1 \
  || { log "fatal: jq not found on PATH; install jq"; exit 1; }

# --- 0. resolve the notifier binary -----------------------------------------
BUNDLED_APP="$ROOT/assets/HerdrNotify.app"
BUNDLED_BIN="$BUNDLED_APP/Contents/MacOS/terminal-notifier"
if [ -n "$NOTIFIER" ] && [ -x "$NOTIFIER" ]; then
  NOTIFIER_BIN="$NOTIFIER"
elif [ -x "$BUNDLED_BIN" ]; then
  NOTIFIER_BIN="$BUNDLED_BIN"
  # Keep the bundle registered with Launch Services so macOS attributes the
  # notification (and its LEFT icon) to HerdrNotify.app instead of the parent
  # terminal. Ad-hoc-signed helpers can lose their LS registration over time
  # (reboots, OS updates), so we self-heal on a TTL; verify the signature and
  # re-sign ONLY if invalid (a needless re-sign mints a fresh CDHash, which
  # resets the per-app notification grant). A failed sign/register is logged
  # and retried on the next TTL expiry; delivery stays best-effort.
  sentinel="$STATE_DIR/.notifier-registered"
  lsregister="${LSREGISTER:-/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister}"
  register_ttl="${REGISTER_TTL_SECONDS:-21600}"
  # Sentinel holds the epoch seconds of its last successful register, so we avoid
  # `stat -f %m` (BSD-only; this box's PATH ships GNU stat, which misreads -f).
  now_s="$(date +%s)"
  sentinel_ts=""
  [ -f "$sentinel" ] && sentinel_ts="$(cat "$sentinel" 2>/dev/null || true)"
  case "$sentinel_ts" in ''|*[!0-9]*) sentinel_ts=0 ;; esac
  needs_register=0
  if [ ! -f "$sentinel" ] || [ "$BUNDLED_BIN" -nt "$sentinel" ]; then
    needs_register=1
  else
    [ "$((now_s - 10#$sentinel_ts))" -ge "$((register_ttl))" ] && needs_register=1
  fi
  if [ "$needs_register" = 1 ]; then
    if ! codesign --verify --deep "$BUNDLED_APP" >/dev/null 2>&1; then
      if codesign --force --deep -s - "$BUNDLED_APP" >/dev/null 2>&1; then
        log "re-signed HerdrNotify.app (ad-hoc); if notifications stop, re-approve \"herdr\" in System Settings -> Notifications"
      else
        log "codesign FAILED for HerdrNotify.app; will retry within ${register_ttl}s"
      fi
    fi
    "$lsregister" -f "$BUNDLED_APP" >/dev/null 2>&1 || true
    printf '%s' "$now_s" >"$sentinel"
  fi
elif command -v terminal-notifier >/dev/null 2>&1; then
  NOTIFIER_BIN="terminal-notifier"
else
  log "no notifier found (bundled HerdrNotify.app missing and no terminal-notifier on PATH)"
  exit 0
fi

EVENT_JSON="${HERDR_PLUGIN_EVENT_JSON:-}";   [ -n "$EVENT_JSON"   ] || EVENT_JSON='{}'
CONTEXT_JSON="${HERDR_PLUGIN_CONTEXT_JSON:-}"; [ -n "$CONTEXT_JSON" ] || CONTEXT_JSON='{}'

# --- 1. optional debug dump -------------------------------------------------
if [ "$DEBUG" = "1" ]; then
  {
    printf 'event=%s\n' "${HERDR_PLUGIN_EVENT:-}"
    printf 'EVENT_JSON=%s\n' "$EVENT_JSON"
    printf 'CONTEXT_JSON=%s\n' "$CONTEXT_JSON"
  } >"$DEBUG_FILE"
  log "debug dump written to $DEBUG_FILE"
fi

# jq readers over the two JSON env vars. // empty collapses missing keys to "".
ev()  { printf '%s' "$EVENT_JSON"   | jq -r "$1 // empty" 2>/dev/null || true; }
ctx() { printf '%s' "$CONTEXT_JSON" | jq -r "$1 // empty" 2>/dev/null || true; }

# --- 2. resolve pane_id + new_status from the EVENT before any CLI call -----
# Event shape: {"event":...,"data":{"pane_id","workspace_id","agent_status","agent"}}
# Identity fields come from the EVENT ONLY (no focused-pane fallback): the
# event can be about a background pane, so guessing from what the user is looking
# at would misattribute titles and silence real background notifications.
pane_id="$(ev '.data.pane_id')"
[ -n "$pane_id" ] || drop --loud "reason=nopane (event carried no .data.pane_id)"
new_status="$(ev '.data.agent_status')"
# Anomalous-only fallback: a well-formed event carries the status. If absent,
# one live `herdr pane get` so the gate has a value. The normal churn path
# stays CLI-free before the gate.
[ -n "$new_status" ] || new_status="$(pane_field "$pane_id" '.agent_status')"
dbg "pane_id=$pane_id"
dbg "new_status=$new_status"
[ -n "$new_status" ] || drop --loud "reason=nostatus (event/herdr carried no agent_status)"

# Per-pane previous status: the event carries only the new status, so we track
# the last-seen value ourselves to give {old_status} meaning. Updated before
# the trigger filter so transitions are recorded faithfully.
pane_key="$(printf '%s' "$pane_id" | tr -c 'A-Za-z0-9._-' '_')"
old_status=""
laststatus_file="$STATE_DIR/laststatus-$pane_key"
[ -f "$laststatus_file" ] && old_status="$(cat "$laststatus_file" 2>/dev/null || true)"
printf '%s' "$new_status" >"$laststatus_file"
dbg "old_status=$old_status"

# --- 3. should this transition notify? -------------------------------------
# Placed BEFORE live enrichment so a non-triggering status short-circuits with
# zero herdr socket round-trips.
case " $TRIGGER_STATUSES " in
  *" $new_status "*) : ;;
  *) drop "reason=trigger status=$new_status not in [$TRIGGER_STATUSES]" ;;
esac

# --- 4. enrich from live herdr state (only reached by triggering events) ----
agent="$(ev '.data.agent')"
workspace_id="$(ev '.data.workspace_id')"
workspace="$(ctx '.workspace_label')"
tab_id="$(ctx '.tab_id')"
cwd="$(ctx '.workspace_cwd')"; [ -n "$cwd" ] || cwd="$(ctx '.focused_pane_cwd')"
session="$(pane_field "$pane_id" '.agent_session.value')"
[ -n "$agent" ]        || agent="$(pane_field "$pane_id" '.agent')"
[ -n "$workspace_id" ] || workspace_id="$(pane_field "$pane_id" '.workspace_id')"
[ -n "$tab_id" ]       || tab_id="$(pane_field "$pane_id" '.tab_id')"
[ -n "$cwd" ]          || cwd="$(pane_field "$pane_id" '.cwd')"
[ -n "$workspace" ]    || workspace="$(workspace_label "$workspace_id")"
tab="$(tab_label "$tab_id")"
[ -n "$workspace" ] || workspace="$workspace_id"
[ -n "$agent" ]     || agent="agent"
worktree="$([ -n "$cwd" ] && basename "$cwd" || printf '%s' "$workspace")"
dbg "agent=$agent workspace_id=$workspace_id workspace=$workspace cwd=$cwd"

# --- 5. suppress the workspace you are currently looking at ----------------
# Two conditions must BOTH hold to suppress: event's workspace is the focused
# one inside herdr, AND a herdr-hosting terminal is the frontmost macOS app.
# Frontmost detection FAILS OPEN (empty/missing -> notify).
if [ "$SUPPRESS_FOCUSED" = "1" ] && [ -n "$workspace_id" ]; then
  if [ "$(focused_workspace_id)" = "$workspace_id" ]; then
    front="$(frontmost_bundle_id)"
    if [ -n "$front" ] && [ -n "$TERMINAL_APP_IDS" ] \
       && case " $TERMINAL_APP_IDS " in *" $front "*) true ;; *) false ;; esac; then
      drop "reason=focused ws=$workspace_id frontmost=$front"
    fi
    dbg "focus-suppress skipped: ws=$workspace_id focused but terminal not frontmost (front=${front:-<none>})"
  fi
fi

# --- 6. debounce repeated (pane,status) within DEBOUNCE_SECONDS ------------
stamp_file="$STATE_DIR/debounce-$pane_key"
now="$(date +%s)"
if [ -f "$stamp_file" ]; then
  read -r last_ts last_status <"$stamp_file" || true
  case "$last_ts" in ''|*[!0-9]*) last_ts=0 ;; esac
  debounce_window="${DEBOUNCE_SECONDS:-0}"
  case "$debounce_window" in ''|*[!0-9]*) debounce_window=0 ;; esac
  if [ "$last_status" = "$new_status" ] && [ "$((now - 10#$last_ts))" -lt "$debounce_window" ]; then
    drop "reason=debounce pane=$pane_id status=$new_status within ${debounce_window}s"
  fi
fi
printf '%s %s\n' "$now" "$new_status" >"$stamp_file"

# --- 7. pick per-status template and expand placeholders --------------------
# status_uc is used only to build a variable NAME (${PREFIX}_${status_uc}); map
# any non-[A-Z0-9] to '_' so a future status like "needs-input" can't trip a
# fatal "bad substitution" under set -e.
status_uc="$(printf '%s' "$new_status" | tr '[:lower:]' '[:upper:]' | tr -c 'A-Z0-9' '_')"
pick() { # pick VAR_PREFIX -> value of ${PREFIX_STATUS} or ${PREFIX_DEFAULT}
  local var="${1}_${status_uc}" def="${1}_DEFAULT"
  printf '%s' "${!var:-${!def:-}}"
}
expand() {
  local s="$1"
  s="${s//\{agent\}/$agent}"
  s="${s//\{workspace\}/$workspace}"
  s="${s//\{worktree\}/$worktree}"
  s="${s//\{tab\}/$tab_id}"
  s="${s//\{tab_label\}/$tab}"
  s="${s//\{pane\}/$pane_id}"
  s="${s//\{session\}/$session}"
  s="${s//\{cwd\}/$cwd}"
  s="${s//\{old_status\}/$old_status}"
  s="${s//\{new_status\}/$new_status}"
  printf '%s' "$s"
}

title="$(expand "$(pick TITLE)")"
body="$(expand "$(pick BODY)")"
icon="$(pick ICON)"
sound="$(pick SOUND)"

# --- 8. fire terminal-notifier ---------------------------------------------
# -group REPLACES any earlier notification sharing the key; per-pane keeps one
# live notification per pane. GROUP="" (set in a config file) disables grouping.
args=(-title "$title" -message "$body")
# shellcheck disable=SC2153
group="$(expand "$GROUP")"
[ -n "$group" ] && args+=(-group "$group")
[ -n "$sound" ] && [ "$sound" != "none" ] && args+=(-sound "$sound")

if [ -n "$icon" ]; then
  case "$icon" in /*) : ;; *) icon="$ROOT/$icon" ;; esac
  if [ -f "$icon" ]; then
    case "$ICON_MODE" in
      appIcon) args+=(-appIcon "$icon") ;;
      *)       args+=(-contentImage "$icon") ;;
    esac
  fi
fi

if [ "$ACTIVATE_ON_CLICK" = "1" ] && [ -n "$pane_id" ]; then
  # terminal-notifier runs -execute through a shell, so this string is
  # re-parsed. CLICK_COMMAND's literal words (`agent focus`) must stay
  # unquoted; the binary path and every substituted VALUE are `printf %q`-quoted
  # so they become exactly one literal argument and can't inject shell syntax.
  bin="$(command -v "$HERDR_BIN" || printf '%s' "$HERDR_BIN")"
  click="${CLICK_COMMAND//\{pane\}/$(printf '%q' "$pane_id")}"
  click="${click//\{workspace\}/$(printf '%q' "$workspace_id")}"
  click="${click//\{agent\}/$(printf '%q' "$agent")}"
  # `herdr agent focus` switches the pane server-side but does NOT raise the
  # terminal window to the foreground. When the click comes from another app,
  # prepend `open -a <ACTIVATE_APP>` so the terminal window comes forward first,
  # then herdr focuses the pane. Empty ACTIVATE_APP = no window activation
  # (the old behavior, useful if you run herdr headless / in a non-macOS terminal
  # that `open -a` can't address).
  if [ -n "${ACTIVATE_APP:-}" ]; then
    full="open -a $(printf '%q' "$ACTIVATE_APP") && $(printf '%q' "$bin") $click"
  else
    full="$(printf '%q' "$bin") $click"
  fi
  args+=(-execute "$full")
fi

dbg "decision=notify title=$title"
notifier_err="$("$NOTIFIER_BIN" "${args[@]}" 2>&1 >/dev/null)" && notifier_rc=0 || notifier_rc=$?
if [ "$notifier_rc" -ne 0 ]; then
  notifier_err="$(printf '%s' "${notifier_err:-<no stderr>}" | tr '\n' ' ' | cut -c1-500)"
  log "notifier failed (exit $notifier_rc): $notifier_err"
fi
