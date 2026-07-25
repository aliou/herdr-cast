#!/usr/bin/env bash
# Register the bundled HerdrNotify.app with Launch Services so macOS shows the
# herdr logo (not the parent terminal's icon) on notifications and allows
# `-execute` click actions. Idempotent: verify-then-sign only on invalid sig.
#
# Called from [[build]] on `herdr plugin install`, and from scripts/link.sh on
# local dev. bin/notify.sh also re-registers on a TTL, so a skipped/manual run
# here is not fatal — the icon self-heals on the next event.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP="$ROOT/assets/HerdrNotify.app"

[ -d "$APP" ] || { echo "setup: $APP not found" >&2; exit 0; }

if ! codesign --verify --deep "$APP" >/dev/null 2>&1; then
  if codesign --force --deep -s - "$APP" >/dev/null 2>&1; then
    echo "setup: re-signed $APP (ad-hoc)"
  else
    echo "setup: codesign FAILED for $APP" >&2
  fi
fi

lsregister="/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"
"$lsregister" -f "$APP" >/dev/null 2>&1 || true
echo "setup: registered $APP with Launch Services"
