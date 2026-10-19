# The guest side: a plain NixOS on one sparse disk file (ESP + ext4 root,
# /nix included), booted by libkrun's EFI firmware through systemd-boot, and
# integrated with the Mac over vsock (../darwin/default.nix).
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.virtualisation.macHost;
  agent = pkgs.callPackage ../agent/package.nix { };
  uid = toString config.users.users.${cfg.user}.uid;
  pulseSocket = "/run/user/${uid}/mac-pulse.sock";

  # xdg-open for a guest without a browser: URLs open on the Mac.
  xdgOpen = pkgs.writeShellScriptBin "xdg-open" ''
    exec ${lib.getExe agent} open "$@"
  '';
  macBrowser = pkgs.makeDesktopItem {
    name = "mac-browser";
    desktopName = "Mac browser";
    exec = "${lib.getExe agent} open %u";
    noDisplay = true;
    mimeTypes = [
      "x-scheme-handler/http"
      "x-scheme-handler/https"
      "text/html"
    ];
  };
in
{
  options.virtualisation.macHost = {
    enable = lib.mkEnableOption "running as the Mac's libkrun guest";
    user = lib.mkOption {
      type = lib.types.str;
      description = "The desktop user: auto-login, audio, the shared arena.";
    };
    deployKeys = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      description = ''
        Root's authorized keys, including the Mac's deploy key
        (<stateDir>/deploy_ed25519.pub on the Mac).
      '';
    };
    audio.enable = lib.mkEnableOption "Mac speakers and microphones via PipeWire" // {
      default = true;
    };
    bells = lib.mkOption {
      type = lib.types.listOf (lib.types.strMatching "[A-Za-z0-9._-]+");
      default = [ ];
      description = ''
        The Mac's virtio-serial wake ports (services.linux-vm.bells there),
        made readable and writable by `user` at /dev/virtio-ports/NAME.
      '';
    };
    sharedMemory.enable = lib.mkEnableOption ''
      the Mac shared memory arena at /run/host-shm/arena (a virtio-fs DAX
      mapping of a file on a Mac RAM disk: both sides map the same pages)
    '';
  };

  config = lib.mkIf cfg.enable (
    lib.mkMerge [
      {
        nixpkgs.hostPlatform = lib.mkDefault "aarch64-linux";

        boot.loader.systemd-boot.enable = true;
        boot.loader.efi.canTouchEfiVariables = false;
        boot.loader.timeout = 0;
        boot.initrd.systemd.enable = true;
        boot.initrd.availableKernelModules = [
          "virtio_blk"
          "virtio_mmio"
          "virtio_net"
          "virtio_console"
        ];
        # In the initrd so vsock exists before the generators and sockets of
        # the real system start.
        boot.initrd.kernelModules = [ "vmw_vsock_virtio_transport" ];
        # No init_on_free/page_poison: virtio_balloon drops free page reporting
        # when freed pages are poisoned, and then nothing goes back to the Mac.
        boot.kernelParams = [ "console=hvc0" ];
        boot.kernel.sysctl = {
          "vm.swappiness" = 10;
          # Cached pages count as used and are never reported free, so a big
          # page cache stays charged to the Mac.
          "vm.vfs_cache_pressure" = 200;
        };

        # The Mac grows the disk file; repart grows the root partition into it
        # and growfs the filesystem. `discard` hands freed blocks back, so the
        # file shrinks again.
        boot.initrd.systemd.repart.enable = true;
        systemd.repart.partitions.root.Type = "root";
        fileSystems."/" = {
          device = "/dev/disk/by-partlabel/root";
          fsType = "ext4";
          options = [
            "discard"
            "noatime"
            "x-systemd.growfs"
          ];
        };
        fileSystems."/boot" = {
          device = "/dev/disk/by-partlabel/esp";
          fsType = "vfat";
          options = [ "umask=0077" ];
        };

        networking.useNetworkd = true;
        systemd.network.networks."10-uplink" = {
          matchConfig.Type = "ether";
          networkConfig.DHCP = "ipv4";
        };

        users.users.${cfg.user}.linger = true;
        services.getty.autologinUser = cfg.user;
        users.users.root.openssh.authorizedKeys.keys = cfg.deployKeys;

        # The Mac's way in, on vsock port 2222. Not systemd-ssh-generator's
        # port 22: it only appears if vsock existed when generators ran, which
        # after a reboot it doesn't. TCP 22 stays shut by the firewall.
        services.openssh = {
          enable = true;
          startWhenNeeded = true;
          openFirewall = false;
          settings.PasswordAuthentication = false;
        };
        systemd.sockets.sshd-mac = {
          wantedBy = [ "sockets.target" ];
          socketConfig = {
            ListenStream = "vsock::2222";
            Accept = true;
          };
        };
        systemd.services."sshd-mac@".serviceConfig = {
          ExecStart = "${config.services.openssh.package}/bin/sshd -i -f /etc/ssh/sshd_config";
          StandardInput = "socket";
        };

        nix.settings = {
          experimental-features = [
            "nix-command"
            "flakes"
          ];
          trusted-users = [ "root" ];
        };

        # No browser here: xdg-open and the http(s) handlers open on the Mac.
        environment.systemPackages = [
          (lib.hiPrio xdgOpen)
          macBrowser
        ];
        xdg.mime.defaultApplications = {
          "x-scheme-handler/http" = "mac-browser.desktop";
          "x-scheme-handler/https" = "mac-browser.desktop";
          "text/html" = "mac-browser.desktop";
        };
        environment.sessionVariables.BROWSER = "xdg-open";

        # The first image (./image.nix) has its store copied in but not
        # registered. Register it, make it the system profile, and install the
        # bootloader for real; afterwards the image's bootstrap UKI is dropped.
        systemd.services.mac-guest-first-boot = {
          wantedBy = [ "multi-user.target" ];
          unitConfig.ConditionPathExists = "/nix-path-registration";
          serviceConfig.Type = "oneshot";
          path = [ config.nix.package ];
          script = ''
            nix-store --load-db < /nix-path-registration
            system=$(readlink -f /run/current-system)
            nix-env -p /nix/var/nix/profiles/system --set "$system"
            "$system"/bin/switch-to-configuration boot
            rm -rf /boot/EFI/Linux
            rm /nix-path-registration
          '';
        };
      }

      (lib.mkIf cfg.audio.enable {
        security.rtkit.enable = true;
        services.pipewire = {
          enable = true;
          pulse.enable = true;
        };
        # The pulse socket the tunnels dial: the guest connects to vsock port
        # 4200 on the host (CID 2), which libkrun connects to the Mac's
        # PulseAudio. User units apply to every user manager, hence the
        # condition (root's from ssh logins would grab them too).
        systemd.user.sockets.mac-pulse = {
          wantedBy = [ "sockets.target" ];
          unitConfig.ConditionUser = cfg.user;
          socketConfig = {
            ListenStream = "%t/mac-pulse.sock";
            Accept = true;
          };
        };
        systemd.user.services."mac-pulse@".serviceConfig = {
          ExecStart = "${pkgs.socat}/bin/socat STDIO VSOCK-CONNECT:2:4200";
          StandardInput = "socket";
        };
        # One PipeWire tunnel per Mac speaker and microphone, and the Mac's
        # default devices mirrored as PipeWire's defaults.
        # Started with the session (lingering), so the devices exist before
        # any app asks for them.
        systemd.user.services.mac-audio = {
          wantedBy = [ "default.target" ];
          requires = [ "pipewire.service" ];
          after = [ "pipewire.service" ];
          unitConfig.ConditionUser = cfg.user;
          path = [ config.services.pipewire.package ];
          serviceConfig = {
            ExecStart = "${lib.getExe agent} guest --pulse-server unix:${pulseSocket}";
            Restart = "always";
            RestartSec = 2;
          };
        };
      })

      (lib.mkIf (cfg.bells != [ ]) {
        services.udev.extraRules = lib.concatMapStrings (name: ''
          SUBSYSTEM=="virtio-ports", ATTR{name}=="${name}", OWNER="${cfg.user}", MODE="0600"
        '') cfg.bells;
      })

      (lib.mkIf cfg.sharedMemory.enable {
        boot.kernelModules = [ "virtiofs" ];
        fileSystems."/run/host-shm" = {
          device = "hostshm";
          fsType = "virtiofs";
          options = [
            "dax=always"
            "nofail"
          ];
        };
      })
    ]
  );
}
