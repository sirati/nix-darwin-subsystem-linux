# nix-darwin-subsystem-linux

A minimal NixOS VM on Apple silicon, run by libkrun (Hypervisor.framework)
from nix-darwin, and integrated with macOS.

- **Memory comes back.** libkrun's balloon does free page reporting, so the Mac
  only holds what the guest uses; `memoryMiB` is a ceiling. Apple's
  Virtualization.framework never returns guest memory.
- **One sparse disk file.** ESP + ext4 root, `/nix` included. It grows as the
  guest writes and shrinks again (ext4 `discard`). The guest has its own store,
  so the Mac's `/nix/store` never holds Linux packages; it is also the Mac's
  `aarch64-linux` builder.
- **big.LITTLE.** Optional `topology`: Linux gets `capacity-dmips-mhz` and a
  `cpu-map`, and each vCPU thread a macOS QoS class (`BACKGROUND` keeps a thread
  on efficiency cores).
- **Audio.** Every Mac speaker and microphone appears in the guest's PipeWire,
  and the guest follows the Mac's default input and output.
- **Browser.** The guest has none: `xdg-open` and http(s) links open in a Mac
  app (Firefox by default).
- **Bells.** Optional named virtio-serial ports for cross-side wakeups:
  `/dev/virtio-ports/NAME` in the guest, a FIFO pair on the Mac.
- **Shared memory.** Optional: `/run/host-shm/arena` in the guest and
  `/Volumes/<volume>/arena` on the Mac are the same pages (a virtio-fs DAX
  window onto a RAM disk file).

There is no display path here; bring your own compositor.

## Use

```nix
# flake inputs
subsystem-linux.url = "github:sirati/nix-darwin-subsystem-linux";

# nix-darwin
imports = [ inputs.subsystem-linux.darwinModules.default ];
services.linux-vm = {
  enable = true;
  name = "linux-vm";
  # The guest, inline, as in containers.<name>.config.
  config = { pkgs, ... }: {
    system.stateVersion = "26.11";
    virtualisation.macHost.deployKeys = [ "<~/.local/state/linux-vm/deploy_ed25519.pub on the Mac>" ];
    users.users.me.isNormalUser = true;
    environment.systemPackages = [ pkgs.foot ];
  };
};
```

The host name, the desktop user (the Mac user), bells, the shared arena and
audio are passed to the guest from `services.linux-vm`; the guest is evaluated
from the Mac's nixpkgs (`services.linux-vm.nixpkgs`), and
`services.linux-vm.specialArgs` reaches its modules.

First run: set `services.linux-vm.bootstrap.enable = true`, switch, run
`linux-vm-bootstrap` (builds the first image on a temporary Linux builder),
then disable `bootstrap` again. The VM starts on its own while you are logged
in.

The Mac's configuration holds only the guest system's store path
(`services.linux-vm.system`), not its closure. After each activation, launchd
runs `linux-vm-switch`, which switches the VM to that system: if a builder
already copied it in (`nix copy --to ssh-ng://root@linux-vm`), nothing is
built; otherwise it is built inside the VM. `linux-vm-ssh` opens a shell
(`LINUX_VM_USER=root` for root).

## Layout

| Path | |
|---|---|
| `darwin/` | nix-darwin module, launcher (`launcher.c`), libkrun patches |
| `guest/` | NixOS module and the first-image module |
| `agent/` | `linux-vm-agent`: Mac side (URLs, audio devices), guest side (PipeWire tunnels, `xdg-open`) |

## libkrun patches

- `topology.patch`: `krun_set_cpu_topology` (device tree cpu-map and
  capacities, per-vCPU QoS).
- `dax-worker.patch`, `dax-readonly.patch`: macOS virtio-fs DAX never answered
  without a GPU configured, and read faults failed with SIGBUS.

## vsock ports

| Port | Direction | Use |
|---|---|---|
| 2222 | Mac → guest | sshd |
| 4200 | guest → Mac | PulseAudio |
| 4300 | guest → Mac | agent channel |
| 4301 | guest → Mac | open URL |

More via `services.linux-vm.extraVsockPorts`.
