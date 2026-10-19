{ lib, rustPlatform }:

rustPlatform.buildRustPackage {
  pname = "linux-vm-agent";
  version = "0.1.0";

  src = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.unions [
      ./Cargo.toml
      ./Cargo.lock
      ./src
    ];
  };

  cargoLock.lockFile = ./Cargo.lock;

  meta = {
    description = "Integration agent between a macOS host and a libkrun NixOS guest";
    mainProgram = "linux-vm-agent";
    platforms = lib.platforms.linux ++ lib.platforms.darwin;
  };
}
