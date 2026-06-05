{ pkgs, ... }:

{
  packages = [
    pkgs.cargo
    pkgs.cargo-nextest
    pkgs.clippy
    pkgs.curl
    pkgs.gcc
    pkgs.gh
    pkgs.git
    pkgs.pkg-config
    pkgs.rustc
    pkgs.rustfmt
  ];

  enterTest = ''
    cargo fmt --check
    cargo clippy -- -D warnings
    cargo test
  '';
}
