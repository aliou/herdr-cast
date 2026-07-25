#!/usr/bin/env bash
# macOS window-server / Launch Services helpers (NOT herdr). Best-effort and
# FAIL OPEN: any failure (tool missing, unparsable output, empty result) yields
# empty output and never a non-zero exit, so a detection failure can only ever
# cause a MISSED suppression (a duplicate notification), never a crash or a
# silently swallowed event.

# frontmost_bundle_id -> bundle id of the frontmost macOS app, or empty.
# Uses `lsappinfo`, which reads the front app from the window server WITHOUT a
# TCC/Automation grant (unlike osascript, which prompts for control).
frontmost_bundle_id() {
  command -v lsappinfo >/dev/null 2>&1 || return 0
  local asn out
  asn="$(lsappinfo front 2>/dev/null || true)"
  [ -n "$asn" ] || return 0
  out="$(lsappinfo info -only bundleid "$asn" 2>/dev/null || true)"
  # Line looks like: "CFBundleIdentifier"="com.mitchellh.ghostty"
  printf '%s' "$out" | sed -n 's/.*=[[:space:]]*"\([^"]*\)".*/\1/p'
}
