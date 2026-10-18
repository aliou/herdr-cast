# Dev shell for herdr-cast.
#
# The Rust toolchain comes from the global home-manager profile. This shell
# only exists to enter the project and let direnv/nix-shell set up.
#
# Enter with: nix-shell
{ pkgs ? import <nixpkgs> { } }:

pkgs.mkShell {
  RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
}
