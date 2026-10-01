#!/usr/bin/env bash
# Delete all but the newest KEEP `build-<epoch>-<sha8>` releases and their
# tags. Tags sort by their fixed-width epoch, so the greatest is the newest.
# `unstable` is never touched.
#
# Usage: prune.sh [keep]. Needs GH_TOKEN.
set -euo pipefail

keep=${1:-3}

gh release list --limit 200 --json tagName --jq '.[].tagName' |
	grep -E '^build-[0-9]+-[0-9a-f]{8}$' |
	sort -r |
	tail -n "+$((keep + 1))" |
	while read -r tag; do
		echo "deleting $tag"
		gh release delete "$tag" --cleanup-tag --yes
	done
