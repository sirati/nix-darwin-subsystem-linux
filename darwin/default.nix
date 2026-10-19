# A minimal NixOS VM on Apple silicon, run as a launchd agent of the logged-in
# user: libkrun (Hypervisor.framework) via ./launcher.c, user-mode networking
# via gvproxy. The guest is declared inline (services.linux-vm.config) and
# lives in one sparse disk file with its own store, so the Mac's /nix/store
# never holds Linux packages.
#
#   linux-vm-bootstrap  build the first disk image (needs bootstrap.enable)
#   linux-vm-switch     switch the VM to services.linux-vm.system (also run
#                       by launchd after each activation)
#   linux-vm-ssh        shell in the VM (root via LINUX_VM_USER=root)
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.linux-vm;
  inherit (cfg) stateDir name;
  disk = "${stateDir}/disk.img";
  shmMount = "/Volumes/${cfg.sharedMemory.volume}";
  agent = pkgs.callPackage ../agent/package.nix { };
  # socat, not libressl nc: nc outlives ssh and holds its stderr open.
  proxy = "${pkgs.socat}/bin/socat - UNIX-CONNECT:${stateDir}/ssh.sock";
  sshOpts = "-o ProxyCommand='${proxy}' -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile=${stateDir}/known_hosts";
  deployKey = "${stateDir}/deploy_ed25519";

  libkrun = pkgs.libkrun-efi.overrideAttrs (old: {
    patches = (old.patches or [ ]) ++ import ./libkrun/patches.nix;
  });
  nativeLauncher =
    pkgs.runCommandCC "linux-vm-launcher"
      {
        buildInputs = [ libkrun ];
        nativeBuildInputs = [ pkgs.darwin.sigtool ];
      }
      ''
        mkdir -p $out/bin
        $CC -O2 -Wall -o $out/bin/linux-vm-launcher ${./launcher.c} -lkrun-efi
        # hv_vm_create fails unless the binary carries the hypervisor entitlement.
        codesign --force --sign - --entitlements ${./launcher.entitlements} $out/bin/linux-vm-launcher
      '';
  launcher = cfg.launcherPackage;

  topologyArg =
    let
      t = cfg.topology;
    in
    lib.optionalString (t != null) (
      "--topology "
      + lib.concatMapStringsSep "," toString [
        t.bigCpus
        t.bigCapacity
        t.littleCapacity
        t.bigQos
        t.littleQos
      ]
    );

  # Guest-side ports these modules own; extraVsockPorts must not collide.
  vsockArgs = [
    "2222:${stateDir}/ssh.sock:listen"
    "4300:${stateDir}/agent.sock:connect"
    "4301:${stateDir}/open.sock:connect"
  ]
  ++ lib.optional cfg.audio.enable "4200:${stateDir}/pulse.sock:connect"
  ++ map (p: "${toString p.port}:${p.path}:${p.mode}") cfg.extraVsockPorts;

  shm = cfg.sharedMemory;
  shmSetup = lib.optionalString shm.enable ''
    # The arena is a file on a RAM disk (an APFS file would be written back to
    # the SSD), shared through a virtio-fs DAX window, so both sides map the
    # same pages.
    if [[ ! -d ${shmMount} ]]; then
      dev=$(/usr/bin/hdiutil attach -nomount ram://${
        toString (shm.sizeMiB * 2048 + 16384)
      } | ${pkgs.coreutils}/bin/tr -d '[:space:]')
      /usr/sbin/diskutil erasevolume HFS+ ${shm.volume} "$dev" >/dev/null
      touch ${shmMount}/.metadata_never_index
      chmod 0700 ${shmMount}
    fi
    # Kept across VM restarts rather than recreated: a RAM disk never hands
    # freed blocks back, and a crashed VM's arena may still be held, so a new
    # file would not fit. Users of the arena stamp their own run id anyway.
    (umask 077; ${pkgs.coreutils}/bin/truncate -s ${toString shm.sizeMiB}M ${shmMount}/arena)
    # What the guest sees as owner and mode (libkrun virtio-fs override_stat).
    /usr/bin/xattr -w user.containers.override_stat ${shm.guestOwner}:0700 ${shmMount}
    /usr/bin/xattr -w user.containers.override_stat ${shm.guestOwner}:0600 ${shmMount}/arena
  '';

  start = pkgs.writeShellScript "linux-vm-start" ''
    set -euo pipefail
    cd ${stateDir}
    [[ -e ${disk} ]] || exit 0
    # Sparse: only blocks the guest has written take space, and discard in
    # the guest punches them out again.
    ${pkgs.coreutils}/bin/truncate -s '>${cfg.diskSize}' ${disk}
    rm -f gvproxy-vfkit.sock gvproxy.sock ssh.sock ${
      lib.concatMapStringsSep " " (p: p.path) (lib.filter (p: p.mode == "listen") cfg.extraVsockPorts)
    }
    ${pkgs.gvproxy}/bin/gvproxy -mtu 1500 \
      -listen-vfkit unixgram://${stateDir}/gvproxy-vfkit.sock \
      -listen unix://${stateDir}/gvproxy.sock >gvproxy.log 2>&1 &
    gvproxy=$!
    trap 'kill $gvproxy' EXIT
    for _ in $(seq 50); do [[ -S gvproxy-vfkit.sock ]] && break; sleep 0.1; done
    mkdir -p -m 0700 bells
    ${shmSetup}
    ${launcher}/bin/linux-vm-launcher --cpus ${toString cfg.cpus} --mem ${toString cfg.memoryMiB} \
      ${topologyArg} \
      --disk ${disk} \
      --net-unixgram ${stateDir}/gvproxy-vfkit.sock \
      ${lib.concatMapStringsSep " " (a: "--vsock ${a}") vsockArgs} \
      ${lib.optionalString shm.enable "--virtiofs hostshm:${shmMount}:${toString shm.sizeMiB}"} \
      ${lib.concatMapStringsSep " " (b: "--bell ${b}:${stateDir}/bells/${b}") cfg.bells} \
      --console ${stateDir}/console.log
  '';

  # The guest, evaluated as part of this configuration the way NixOS
  # evaluates containers.<name>.config. What the Mac side already knows
  # (name, user, bells, the arena, audio) is set for it.
  guest = import "${cfg.nixpkgs}/nixos/lib/eval-config.nix" {
    system = null;
    inherit (cfg) specialArgs;
    modules = [
      ../guest
      {
        networking.hostName = lib.mkDefault name;
        virtualisation.macHost = {
          enable = true;
          user = lib.mkDefault cfg.user;
          audio.enable = lib.mkDefault cfg.audio.enable;
          inherit (cfg) bells;
          sharedMemory.enable = cfg.sharedMemory.enable;
        };
      }
      cfg.config
    ];
  };
  # Store paths without their context: the guest's closure lives on its own
  # disk, never in the Mac's store. Something with an aarch64-linux builder
  # puts it there, or the switch below builds it inside the VM.
  guestSystem = builtins.unsafeDiscardStringContext cfg.system.outPath;
  guestSystemDrv = builtins.unsafeDiscardStringContext cfg.system.drvPath;

  builderHostKey = (lib.head config.nix.buildMachines).publicHostKey;
  # Only the image's .drv closure is in the Mac's store, and only while
  # bootstrap is enabled.
  imageDrv =
    builtins.unsafeDiscardOutputDependency
      (guest.extendModules { modules = [ ../guest/image.nix ]; }).config.system.build.image.drvPath;
  bootstrap = pkgs.writeShellScriptBin "linux-vm-bootstrap" ''
    set -euo pipefail
    if [[ -e ${disk} ]]; then
      echo "${disk} exists; remove it first to start over" >&2
      exit 1
    fi
    mkdir -p ${stateDir}
    # Built and kept on the Linux builder; only the image bytes come over.
    knownHosts=$(mktemp)
    trap 'rm -f "$knownHosts"' EXIT
    echo "linux-builder $(echo ${builderHostKey} | base64 -d)" >"$knownHosts"
    builder=(sudo ssh -i /etc/nix/builder_ed25519 -o UserKnownHostsFile="$knownHosts" builder@linux-builder)
    image=$(sudo -H nix build --no-link --print-out-paths \
      --store 'ssh-ng://builder@linux-builder?ssh-key=/etc/nix/builder_ed25519&base64-ssh-public-host-key=${builderHostKey}' \
      '${imageDrv}^*')
    "''${builder[@]}" "cat $image/*.raw" | ${pkgs.coreutils}/bin/dd of=${disk}.tmp bs=4M conv=sparse status=none
    mv ${disk}.tmp ${disk}
    echo "${name} disk ready; the VM starts on its own" >&2
  '';

  # Bring the running guest to services.linux-vm.system. Run by launchd after
  # every activation (and by hand); waits for a VM that is still booting.
  switch = pkgs.writeShellScriptBin "linux-vm-switch" ''
    set -euo pipefail
    system=${guestSystem}
    ssh=(ssh -i ${deployKey} ${sshOpts} -o BatchMode=yes -o ConnectTimeout=10 root@${name})
    [[ -e ${disk} ]] || { echo "linux-vm-switch: no disk yet (linux-vm-bootstrap)"; exit 0; }
    for i in $(seq 60); do
      "''${ssh[@]}" true 2>/dev/null && break
      [[ $i == 60 ]] && { echo "linux-vm-switch: ${name} unreachable"; exit 1; }
      sleep 5
    done
    if [[ $("''${ssh[@]}" readlink -f /run/current-system) == "$system" ]]; then
      echo "linux-vm-switch: ${name} already runs $system"
    else
      if ! "''${ssh[@]}" test -e "$system"; then
        # Not pushed by a builder: build it in the VM from the Mac's .drv.
        if [[ ! -e ${guestSystemDrv} ]]; then
          echo "linux-vm-switch: $system is neither in ${name} nor buildable here (no ${guestSystemDrv}); copy it in from a builder" >&2
          exit 1
        fi
        NIX_SSHOPTS="-i ${deployKey} ${sshOpts}" nix build --no-link --store ssh-ng://root@${name} '${guestSystemDrv}^*'
      fi
      "''${ssh[@]}" "nix-env -p /nix/var/nix/profiles/system --set $system && $system/bin/switch-to-configuration switch"
      echo "linux-vm-switch: ${name} switched to $system"
    fi
    # Root in the VM, for postSwitch.
    vm() { "''${ssh[@]}" "$@"; }
    ${cfg.postSwitch}
  '';
in
{
  imports = [
    ./stable-codesign.nix
    ./audio.nix
  ];

  options.services.linux-vm = {
    enable = lib.mkEnableOption "the libkrun Linux VM";
    name = lib.mkOption {
      type = lib.types.str;
      default = "linux-vm";
      description = "Host name of the guest; also its ssh name on the Mac.";
    };
    user = lib.mkOption {
      type = lib.types.str;
      default = config.system.primaryUser;
      defaultText = lib.literalExpression "config.system.primaryUser";
      description = "Mac user whose launchd session runs the VM.";
    };
    stateDir = lib.mkOption {
      type = lib.types.str;
      default = "${config.users.users.${cfg.user}.home}/.local/state/linux-vm";
      defaultText = lib.literalExpression ''"''${home}/.local/state/linux-vm"'';
      description = "Disk image, sockets, logs and the deploy key.";
    };
    launcherPackage = lib.mkOption {
      type = lib.types.package;
      default = nativeLauncher;
      defaultText = lib.literalMD "built on the Mac from `launcher.c` and patched libkrun-efi";
      example = lib.literalExpression "subsystem-linux.packages.x86_64-linux.launcher-aarch64-darwin";
      description = ''
        Provides bin/linux-vm-launcher, signed with the hypervisor entitlement.
        The x86_64-linux flake output is the same launcher cross-built, so a
        Linux machine can build it and the Mac only copies it.
      '';
    };
    cpus = lib.mkOption {
      type = lib.types.ints.between 1 255;
      default = 4;
    };
    memoryMiB = lib.mkOption {
      type = lib.types.ints.positive;
      default = 4096;
      description = ''
        Ceiling, not a reservation: libkrun's balloon does free page reporting,
        so the Mac only holds what the guest is using.
      '';
    };
    topology = lib.mkOption {
      default = null;
      description = ''
        Expose a big.LITTLE topology: vCPUs 0..bigCpus-1 form the big cluster.
        Linux gets capacity-dmips-mhz and a cpu-map, and each vCPU thread gets
        a macOS QoS class (BACKGROUND keeps a thread on efficiency cores).
      '';
      type = lib.types.nullOr (
        lib.types.submodule {
          options = {
            bigCpus = lib.mkOption { type = lib.types.ints.positive; };
            bigCapacity = lib.mkOption {
              type = lib.types.ints.positive;
              default = 1024;
            };
            littleCapacity = lib.mkOption { type = lib.types.ints.positive; };
            bigQos = lib.mkOption {
              type = lib.types.int;
              default = 33;
              description = "qos_class_t for big vCPUs (QOS_CLASS_USER_INTERACTIVE).";
            };
            littleQos = lib.mkOption {
              type = lib.types.int;
              default = 9;
              description = "qos_class_t for little vCPUs (QOS_CLASS_BACKGROUND).";
            };
          };
        }
      );
    };
    diskSize = lib.mkOption {
      type = lib.types.str;
      default = "64G";
      description = "Size of the sparse disk file; it only takes what is used.";
    };
    config = lib.mkOption {
      type = lib.types.deferredModule;
      default = { };
      example = lib.literalExpression ''
        { pkgs, ... }: {
          system.stateVersion = "26.11";
          users.users.me.isNormalUser = true;
          environment.systemPackages = [ pkgs.foot ];
        }
      '';
      description = ''
        The guest's NixOS configuration, as in `containers.<name>.config`.
        The guest module (virtualisation.macHost) is imported and set from
        these options; host name, user, bells, the arena and audio need not
        be repeated.
      '';
    };
    specialArgs = lib.mkOption {
      type = lib.types.attrsOf lib.types.raw;
      default = { };
      example = lib.literalExpression "{ inherit inputs; }";
      description = "Extra arguments for the guest's modules.";
    };
    nixpkgs = lib.mkOption {
      type = lib.types.path;
      default = pkgs.path;
      defaultText = lib.literalExpression "pkgs.path";
      description = "The nixpkgs the guest is evaluated from.";
    };
    guestConfig = lib.mkOption {
      type = lib.types.raw;
      readOnly = true;
      default = guest.config;
      defaultText = lib.literalMD "the guest's evaluated `config`";
      description = "The guest's configuration, for reading its values on the Mac side.";
    };
    postSwitch = lib.mkOption {
      type = lib.types.lines;
      default = "";
      description = ''
        Shell run by linux-vm-switch (as the Mac user) once the VM runs
        services.linux-vm.system; `vm CMD...` runs a command in it as root.
      '';
    };
    system = lib.mkOption {
      type = lib.types.package;
      readOnly = true;
      default = guest.config.system.build.toplevel;
      defaultText = lib.literalMD "the guest's `system.build.toplevel`";
      description = ''
        The guest system. The Mac's configuration holds only its store path:
        build it on an aarch64-linux builder and copy it into the VM
        (`nix copy --to ssh-ng://root@<name>`), or let linux-vm-switch build
        it inside the VM.
      '';
    };
    browser = {
      app = lib.mkOption {
        type = lib.types.str;
        default = "Firefox";
        description = "Mac app the guest's xdg-open opens URLs in.";
      };
      schemes = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [
          "http"
          "https"
        ];
        description = "URL schemes the guest may open on the Mac.";
      };
    };
    sharedMemory = {
      enable = lib.mkEnableOption "a host/guest shared memory arena";
      sizeMiB = lib.mkOption {
        type = lib.types.ints.positive;
        default = 128;
      };
      volume = lib.mkOption {
        type = lib.types.str;
        default = "linux-vm-shm";
        description = "Name of the RAM disk under /Volumes holding the arena.";
      };
      guestOwner = lib.mkOption {
        type = lib.types.str;
        default = "1000:100";
        description = "uid:gid the guest sees as the arena's owner.";
      };
    };
    bells = lib.mkOption {
      type = lib.types.listOf (lib.types.strMatching "[A-Za-z0-9._-]+");
      default = [ ];
      example = [ "org.example.bell.0" ];
      description = ''
        Named virtio-serial ports for cross-side wakeups: /dev/virtio-ports/NAME
        in the guest, and on the Mac the FIFOs <stateDir>/bells/NAME.to-guest
        (write to wake the guest) and NAME.from-guest (read the guest's wakes).
        The guest needs the same names in virtualisation.macHost.bells.
      '';
    };
    extraVsockPorts = lib.mkOption {
      default = [ ];
      description = ''
        More vsock ports mapped to Unix sockets on the Mac. `listen`: the guest
        listens and the Mac connects to `path`; `connect`: the guest connects
        and libkrun dials the Mac's listener at `path`. 2222, 4200, 4300 and
        4301 are taken.
      '';
      type = lib.types.listOf (
        lib.types.submodule {
          options = {
            port = lib.mkOption { type = lib.types.port; };
            path = lib.mkOption { type = lib.types.str; };
            mode = lib.mkOption {
              type = lib.types.enum [
                "listen"
                "connect"
              ];
            };
          };
        }
      );
    };
    bootstrap.enable = lib.mkEnableOption ''
      the temporary QEMU Linux builder (3 GB of RAM while running) and
      linux-vm-bootstrap, to create the first disk image
    '';
    bootstrap.builderPackage = lib.mkOption {
      type = lib.types.package;
      description = ''
        darwin.linux-builder from a nixpkgs revision Hydra built for
        aarch64-darwin, so it is substituted rather than built (building it
        would itself need a Linux builder).
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = lib.all (
          p:
          !(lib.elem p.port [
            2222
            4200
            4300
            4301
          ])
        ) cfg.extraVsockPorts;
        message = "services.linux-vm.extraVsockPorts: 2222, 4200, 4300 and 4301 are taken";
      }
    ];

    launchd.user.agents.linux-vm.serviceConfig = {
      ProgramArguments = [ "${start}" ];
      # Runs whenever the disk exists, i.e. once linux-vm-bootstrap made it.
      KeepAlive.PathState.${disk} = true;
      ThrottleInterval = 10;
      # Background would clamp every vCPU thread to efficiency cores.
      ProcessType = "Interactive";
      StandardOutPath = "${stateDir}/launcher.log";
      StandardErrorPath = "${stateDir}/launcher.log";
    };

    launchd.user.agents.linux-vm-agent.serviceConfig = {
      ProgramArguments = [
        (lib.getExe agent)
        "host"
        "--channel-socket"
        "${stateDir}/agent.sock"
        "--open-socket"
        "${stateDir}/open.sock"
        "--browser-app"
        cfg.browser.app
        "--allowed-schemes"
        (lib.concatStringsSep "," cfg.browser.schemes)
        "--pactl"
        "${pkgs.pulseaudio}/bin/pactl"
        "--pulse-server"
        "unix:${stateDir}/pulse.sock"
      ];
      RunAtLoad = true;
      KeepAlive = true;
      ThrottleInterval = 5;
      ProcessType = "Interactive";
      StandardOutPath = "${stateDir}/agent.log";
      StandardErrorPath = "${stateDir}/agent.log";
    };

    # Reloaded, and so run, whenever the guest system changes.
    launchd.user.agents.linux-vm-switch.serviceConfig = {
      ProgramArguments = [ (lib.getExe switch) ];
      RunAtLoad = true;
      StandardOutPath = "${stateDir}/switch.log";
      StandardErrorPath = "${stateDir}/switch.log";
    };

    # A deploy key for root in the guest; the guest module needs its public
    # half (virtualisation.macHost.deployKeys).
    system.activationScripts.postActivation.text = ''
      sudo -u ${cfg.user} mkdir -p ${stateDir}
      if [[ ! -f ${deployKey} ]]; then
        sudo -u ${cfg.user} /usr/bin/ssh-keygen -q -t ed25519 -N "" \
          -C "deploy to ${name}" -f ${deployKey}
        echo "linux-vm: new deploy key, add to the guest's deployKeys: $(cat ${deployKey}.pub)" >&2
      fi
    '';

    # The VM is the Mac's aarch64-linux builder; nix-daemon (root) reaches it
    # through the user's vsock socket.
    nix.distributedBuilds = true;
    nix.buildMachines = lib.mkIf (!cfg.bootstrap.enable) [
      {
        hostName = name;
        protocol = "ssh-ng";
        sshUser = "root";
        sshKey = deployKey;
        systems = [ "aarch64-linux" ];
        maxJobs = cfg.cpus;
        supportedFeatures = [
          "big-parallel"
          "benchmark"
        ];
      }
    ];
    nix.linux-builder = lib.mkIf cfg.bootstrap.enable {
      enable = true;
      package = cfg.bootstrap.builderPackage;
      maxJobs = 4;
    };
    programs.ssh.extraConfig = ''
      Host ${name}
        ProxyCommand ${proxy}
        StrictHostKeyChecking accept-new
        UserKnownHostsFile ${stateDir}/known_hosts
    '';

    environment.systemPackages = [
      switch
      (pkgs.writeShellScriptBin "linux-vm-ssh" ''
        exec ssh ${sshOpts} "''${LINUX_VM_USER:-${cfg.user}}@${name}" "$@"
      '')
    ]
    ++ lib.optional cfg.bootstrap.enable bootstrap;
  };
}
