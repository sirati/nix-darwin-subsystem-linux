# Speakers and microphones for the guest: PulseAudio on the Mac drives every
# CoreAudio device, and the guest's agent tunnels each one into PipeWire over
# vsock (../guest/default.nix). PulseAudio runs from a signed app bundle so
# macOS asks for the microphone once and keeps the grant across rebuilds.
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.linux-vm;
  label = "org.nixos.linux-vm-audio";
  app = "Linux VM Audio";
  bundle = pkgs.runCommand "linux-vm-audio-app" { } ''
    contents="$out/Applications/${app}.app/Contents"
    mkdir -p "$contents/MacOS"
    cp ${pkgs.pulseaudio}/bin/pulseaudio "$contents/MacOS/pulseaudio"
    cat > "$contents/Info.plist" <<'PLIST'
    <?xml version="1.0" encoding="UTF-8"?>
    <!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
    <plist version="1.0"><dict>
      <key>CFBundleExecutable</key><string>pulseaudio</string>
      <key>CFBundleIdentifier</key><string>${label}</string>
      <key>CFBundleName</key><string>${app}</string>
      <key>CFBundlePackageType</key><string>APPL</string>
      <key>LSUIElement</key><true/>
      <key>NSMicrophoneUsageDescription</key>
      <string>Apps in the Linux VM use the microphone.</string>
    </dict></plist>
    PLIST
  '';
  # Only this socket, which only the user and the VM can reach; hence the
  # anonymous auth. The client is on another kernel, so --disable-shm (which
  # also drops the shm ringbuffer): samples go through the socket.
  script = pkgs.writeText "linux-vm-audio.pa" ''
    load-module module-coreaudio-detect
    load-module module-native-protocol-unix socket=${cfg.stateDir}/pulse.sock auth-anonymous=1
  '';
in
{
  options.services.linux-vm.audio.enable =
    lib.mkEnableOption "Mac speakers and microphones in the guest"
    // {
      default = true;
    };

  config = lib.mkIf (cfg.enable && cfg.audio.enable) {
    stableCodesign.apps.${app} = {
      package = bundle;
      launchdLabel = label;
    };

    # nix-darwin names it org.nixos.linux-vm-audio, matching `label`.
    launchd.user.agents.linux-vm-audio.serviceConfig = {
      ProgramArguments = [
        "/Applications/Nix Signed/${app}.app/Contents/MacOS/pulseaudio"
        "--daemonize=no"
        "--exit-idle-time=-1"
        "--disallow-exit"
        "--disable-shm=yes"
        "-n"
        "--file=${script}"
        "--dl-search-path=${pkgs.pulseaudio}/lib/pulseaudio/modules"
      ];
      RunAtLoad = true;
      KeepAlive = true;
      ThrottleInterval = 10;
      ProcessType = "Interactive";
      StandardOutPath = "${cfg.stateDir}/audio.log";
      StandardErrorPath = "${cfg.stateDir}/audio.log";
    };
  };
}
