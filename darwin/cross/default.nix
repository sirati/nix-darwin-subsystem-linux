# The launcher and its libkrun, cross-built for aarch64-darwin on x86_64-linux,
# so the Mac never compiles them. clang + lld against the nixpkgs Apple SDK,
# rustc from rust-overlay; rcodesign signs, as macOS codesign would.
# `pkgs` is x86_64-linux nixpkgs with rust-overlay applied.
{ pkgs }:
let
  inherit (pkgs) lib;
  target = "aarch64-apple-darwin";
  deploymentTarget = "14.0";

  sdkVersions = lib.importJSON (pkgs.path + "/pkgs/by-name/ap/apple-sdk/metadata/versions.json");
  sdk = pkgs.callPackage (
    pkgs.path + "/pkgs/by-name/ap/apple-sdk/common/fetch-sdk.nix"
  ) { } sdkVersions."26";

  cc = pkgs.runCommand "aarch64-apple-darwin-cc" { } ''
    mkdir -p $out/bin
    cat > $out/bin/${target}-cc <<EOF
    #!${pkgs.runtimeShell}
    exec ${pkgs.llvmPackages.clang-unwrapped}/bin/clang --target=arm64-apple-macosx${deploymentTarget} \
      -isysroot ${sdk} -B${pkgs.llvmPackages.lld}/bin -fuse-ld=lld "\$@"
    EOF
    chmod +x $out/bin/${target}-cc
  '';

  rust = pkgs.rust-bin.stable.latest.minimal.override { targets = [ target ]; };

  version = "1.19.4";
  src = pkgs.fetchFromGitHub {
    owner = "libkrun";
    repo = "libkrun";
    tag = "v${version}";
    hash = "sha256-X/VGKOfbLN/3rj1Af7HEprfJB99Mh3UwfikXtS1dkXI=";
  };

  # Embedded by the default init-blob feature, as in nixpkgs' libkrun-efi.
  initBinary = pkgs.pkgsCross.aarch64-multiplatform.pkgsStatic.stdenv.mkDerivation {
    pname = "libkrun-init";
    inherit version src;
    dontConfigure = true;
    buildPhase = ''
      cd src/init_blob/init
      $CC -O2 -static -Wall -o init init.c dhcp.c
    '';
    installPhase = "install -D init $out/init";
  };

  # A Mach-O loses its signature to any edit; arm64 macOS refuses to load an
  # unsigned one.
  sign = "${pkgs.rcodesign}/bin/rcodesign sign";

  # nixpkgs' libkrun-efi with EFI=1 and without GPU=1. Cargo directly rather
  # than the Makefile, which keys everything on `uname -s`.
  libkrun-efi = pkgs.stdenv.mkDerivation {
    pname = "libkrun-efi-${target}";
    inherit version src;
    patches = import ../libkrun/patches.nix;

    cargoDeps = pkgs.rustPlatform.fetchCargoVendor {
      inherit src;
      hash = "sha256-ZNSpsxCzCUKkjsDt7Sd5HcfqiQ13kaZSo7w/Vy6HXtY=";
    };
    nativeBuildInputs = [
      rust
      pkgs.rustPlatform.cargoSetupHook
      pkgs.removeReferencesTo
    ];

    env = {
      # The firmware comes from the tree: src/vmm/edk2/KRUN_EFI.silent.fd.
      KRUN_INIT_BINARY_PATH = "${initBinary}/init";
      SDKROOT = "${sdk}";
      MACOSX_DEPLOYMENT_TARGET = deploymentTarget;
      CC_aarch64_apple_darwin = "${cc}/bin/${target}-cc";
      CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER = "${cc}/bin/${target}-cc";
    };

    # build.rs picks its link args by #[cfg(target_os)], i.e. the build
    # machine's: keep only the macOS ones, with the install name in the store.
    postPatch = ''
      sed -i '/cfg(target_os = "linux")/,/);/d; /cfg(target_os = "macos")/d' src/libkrun/build.rs
      substituteInPlace src/libkrun/build.rs \
        --replace-fail '-install_name,libkrun.{}.dylib' "-install_name,$out/lib/libkrun-efi.{}.dylib"
    '';

    dontConfigure = true;
    buildPhase = ''
      runHook preBuild
      cargo build --offline --release --target ${target} --features efi -p libkrun
      runHook postBuild
    '';
    installPhase = ''
      runHook preInstall
      mkdir -p $out/lib $out/include
      lib=$out/lib/libkrun-efi.${version}.dylib
      install -m 755 target/${target}/release/libkrun.dylib $lib
      ln -s libkrun-efi.${version}.dylib $out/lib/libkrun-efi.1.dylib
      ln -s libkrun-efi.1.dylib $out/lib/libkrun-efi.dylib
      ln -s libkrun-efi.dylib $out/lib/libkrun.dylib
      install -m 644 include/libkrun.h $out/include/
      runHook postInstall
    '';
    dontStrip = true;
    dontPatchELF = true;
    # Panic locations name the toolchain and vendored crates; keep Linux
    # store paths out of the Mac's closure.
    postFixup = ''
      remove-references-to -t ${rust} -t $cargoDeps $out/lib/libkrun-efi.${version}.dylib
      ${sign} $out/lib/libkrun-efi.${version}.dylib
    '';
    meta.platforms = [ "x86_64-linux" ];
  };

  launcher = pkgs.runCommand "linux-vm-launcher-${target}" { passthru.libkrun = libkrun-efi; } ''
    mkdir -p $out/bin
    ${cc}/bin/${target}-cc -O2 -Wall -o $out/bin/linux-vm-launcher ${../launcher.c} \
      -I${libkrun-efi}/include -L${libkrun-efi}/lib -lkrun-efi
    # hv_vm_create fails unless the binary carries the hypervisor entitlement.
    ${sign} --entitlements-xml-file ${../launcher.entitlements} $out/bin/linux-vm-launcher
  '';
in
{
  inherit libkrun-efi launcher;
  toolchain = cc;
}
