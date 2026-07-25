#!/usr/bin/env bash
# Link this checkout as a local herdr plugin, then register the bundled
# notifier app. Idempotent. Use this for local dev (we're not publishing).
#
#   scripts/link.sh          # link this repo
#   scripts/link.sh --force  # unlink first, then re-link
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
HERDR="${HERDR_BIN_PATH:-herdr}"
PLUGIN_ID="aliou.cast"

if ! command -v "$HERDR" >/dev/null 2>&1; then
  echo "herdr not found on PATH; skipping" >&2; exit 0
fi

force=0
[ "${1:-}" = "--force" ] && force=1

# `herdr plugin list` prints "- <id> (...) <state>". Match the exact id field.
if [ "$force" -eq 0 ] && "$HERDR" plugin list 2>/dev/null \
    | awk -v id="$PLUGIN_ID" '$1 == "-" && $2 == id { found=1 } END { exit !found }'; then
  echo "$PLUGIN_ID already linked; pass --force to re-link"
  exit 0
fi

if [ "$force" -eq 1 ]; then
  "$HERDR" plugin unlink "$PLUGIN_ID" 2>/dev/null || true
fi

"$HERDR" plugin link "$ROOT"
bash "$ROOT/scripts/setup.sh"
echo "linked $PLUGIN_ID at $ROOT"
echo "next: trigger an event, then allow notifications for \"herdr\" in System Settings -> Notifications"
