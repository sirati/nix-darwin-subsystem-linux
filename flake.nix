{
  description = "A minimal NixOS VM on Apple silicon: libkrun, integrated with macOS";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    # Only for the bootstrap Linux builder: a revision Hydra built for
    # aarch64-darwin, so darwin.linux-builder is substituted. Building it would
    # itself need an aarch64-linux builder.
    nixpkgs-darwin-builder.url = "github:NixOS/nixpkgs/f20cd90e28fe0403873f473b29e42662cd86e8c4";
    # Only for cross-building the Mac launcher on x86_64-linux.
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      nixpkgs-darwin-builder,
      rust-overlay,
    }:
    let
      systems = [
        "aarch64-darwin"
        "aarch64-linux"
      ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
      cross = import ./darwin/cross {
        pkgs = import nixpkgs {
          system = "x86_64-linux";
          overlays = [ rust-overlay.overlays.default ];
        };
      };
    in
    {
      darwinModules.default = {
        imports = [ ./darwin ];
        services.linux-vm.bootstrap.builderPackage =
          nixpkgs-darwin-builder.legacyPackages.aarch64-darwin.darwin.linux-builder;
      };
      nixosModules.default = ./guest;
      # Only for building the first disk image; linux-vm-bootstrap adds it.
      nixosModules.image = ./guest/image.nix;

      packages =
        forAll (pkgs: {
          agent = pkgs.callPackage ./agent/package.nix { };
        })
        // {
          # For services.linux-vm.launcherPackage: built on Linux, run on the Mac.
          x86_64-linux = {
            launcher-aarch64-darwin = cross.launcher;
            libkrun-efi-aarch64-darwin = cross.libkrun-efi;
          };
        };

      formatter = forAll (pkgs: pkgs.nixfmt);
    };
}
