#!/usr/bin/env bash
# Publish one build file to two releases:
#
# - `unstable`: the rolling latest build of main, overwritten on every push.
# - `build-<epoch>-<sha8>`: this commit's build. Never overwritten, so a
#   package pinned to it keeps fetching the exact bytes it hashed. The epoch
#   is the commit's committer timestamp: identical in every job and re-run,
#   and the greatest tag is the newest build. Only the newest few are kept
#   (see prune.sh); older commits can be rebuilt from source.
#
# Usage: publish.sh <file>. Needs GH_TOKEN and GITHUB_SHA.
set -euo pipefail

file=$1
name=$(basename "$file")
build_tag="build-$(git show -s --format=%ct "$GITHUB_SHA")-${GITHUB_SHA::8}"

# Several jobs race to create each release. Losing the race is fine as long
# as the release exists by the time we upload.
ensure_release() {
	local tag=$1 notes=$2
	gh release view "$tag" >/dev/null 2>&1 && return
	gh release create "$tag" --prerelease --title "$tag" --target "$GITHUB_SHA" --notes "$notes" ||
		gh release view "$tag" >/dev/null
}

ensure_release unstable "Rolling build of the main branch. Do not use as a changelog."
gh release upload unstable "$file" --clobber

ensure_release "$build_tag" "Build of ${GITHUB_SHA}. Pinned by packages; pruned once newer builds exist."
# A re-run of this commit must not replace files a package already hashed.
if gh release view "$build_tag" --json assets --jq '.assets[].name' | grep -qx "$name"; then
	echo "$build_tag already has $name; keeping it"
	exit 0
fi
gh release upload "$build_tag" "$file"
