# CI binaries and releases

GitHub Actions (`.github/workflows/ci.yml`) builds binaries on every push
and pull request:

- `herdr-cast-darwin-arm64`
- `herdr-cast-linux-arm64`
- `herdr-cast-linux-x64`

Linux binaries target musl so NixOS consumers run them without patching a
dynamic loader. `Cargo.lock` is tracked because CI builds this binary; keep
it current when `Cargo.toml` dependencies change.

## Release scheme

On pushes to main, the build jobs and the `assets` job publish through
`.github/scripts/publish.sh` to two prereleases:

- `unstable` — the rolling latest build. Every file is replaced with
  `--clobber` on each push.
- `build-<epoch>-<sha8>` — this commit's build. The epoch is the commit's
  committer timestamp, so the greatest tag is the newest build. Files are
  published without clobbering existing assets, so re-runs reuse the tag
  and never replace a hashed file.

`herdr-cast-assets-darwin.tar.gz` carries the three bundled `HerdrNotify*.app`
identities alongside the binaries.

A `prune` job (`.github/scripts/prune.sh`) keeps the newest three `build-*`
releases and deletes older ones with their tags. Older commits rebuild from
source.

## Rules

- Never upload with `--clobber` to a `build-*` release: a pinned package
  hashed those files.
- The `build-*` scheme exists so a package can pin an immutable set of
  bytes; `unstable` alone invalidates pins on every push.
