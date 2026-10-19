# First disk image, built without a VM by systemd-repart. Only used once: the
# Mac copies it into its disk file, and the guest then registers its store and
# installs the bootloader itself (./default.nix, mac-guest-first-boot).
{
  config,
  lib,
  pkgs,
  modulesPath,
  ...
}:
let
  efiArch = config.nixpkgs.hostPlatform.efiArch;
  closure = pkgs.closureInfo { rootPaths = [ config.system.build.toplevel ]; };
in
{
  imports = [ (modulesPath + "/image/repart.nix") ];

  image.repart = {
    enable = true;
    name = config.networking.hostName;
    partitions = {
      esp = {
        contents = {
          "/EFI/BOOT/BOOT${lib.toUpper efiArch}.EFI".source =
            "${config.systemd.package}/lib/systemd/boot/efi/systemd-boot${efiArch}.efi";
          "/EFI/Linux/${config.system.boot.loader.ukiFile}".source =
            "${config.system.build.uki}/${config.system.boot.loader.ukiFile}";
        };
        repartConfig = {
          Type = "esp";
          Label = "esp";
          Format = "vfat";
          SizeMinBytes = "512M";
        };
      };
      root = {
        storePaths = [ config.system.build.toplevel ];
        contents."/nix-path-registration".source = "${closure}/registration";
        repartConfig = {
          Type = "root";
          Label = "root";
          Format = "ext4";
          Minimize = "guess";
        };
      };
    };
  };
}
