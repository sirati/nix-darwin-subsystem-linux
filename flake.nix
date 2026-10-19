{
  description = "A minimal NixOS VM on Apple silicon: libkrun, integrated with macOS";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    # Only for the bootstrap Linux builder: a revision Hydra built for
    # aarch64-darwin, so darwin.linux-builder is substituted. Building it would
    # itself need an aarch64-linux builder.
    nixpkgs-darwin-builder.url = "github:NixOS/nixpkgs/f20cd90e28fe0403873f473b29e42662cd86e8c4";
  };

  outputs =
    {
      self,
      nixpkgs,
      nixpkgs-darwin-builder,
    }:
    let
      systems = [
        "aarch64-darwin"
        "aarch64-linux"
      ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
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

      packages = forAll (pkgs: {
        agent = pkgs.callPackage ./agent/package.nix { };
      });

      formatter = forAll (pkgs: pkgs.nixfmt);
    };
}
