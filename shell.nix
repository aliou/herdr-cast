# Dev shell for herdr-cast.
#
# Provides the Rust toolchain used by the checks in AGENTS.md:
#   cargo fmt -- --check
#   cargo test
#   cargo build --release
#
# Enter with: nix-shell
{ pkgs ? import <nixpkgs> { } }:

pkgs.mkShell {
  packages = with pkgs; [
    cargo
    rustc
    rustfmt
    clippy
  ];

  RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
}
