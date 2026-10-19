# Store apps are ad-hoc signed, so their designated requirement is a cdhash and
# every rebuild looks like a new app to TCC. Copy them out of the store and sign
# them with a local key: the requirement becomes `identifier X and certificate
# leaf = H"..."`, which survives rebuilds and only this key can satisfy.
# The identity lives in the System keychain, where root can import it without a
# prompt and codesign finds it untrusted; trusting it would need a password
# dialog. A private keychain would have to join the search list, which is
# shared with the GUI session that runs darwin-rebuild. rcodesign avoids the
# keychain but leaves Electron's nested helper apps out of the outer seal.
{
  config,
  lib,
  ...
}:
let
  cfg = config.stableCodesign;
  cert = "/var/db/nix-codesign/cert.pem";
  systemKeychain = "/Library/Keychains/System.keychain";
  appDir = "/Applications/Nix Signed";
  sign = name: app: ''
    src=${lib.escapeShellArg "${app.package}/Applications/${name}.app"}
    dest=${lib.escapeShellArg "${appDir}/${name}.app"}
    stamp=${lib.escapeShellArg "${appDir}/.${name}.source"}
    if [[ ! -d "$dest" || "$(cat "$stamp" 2>/dev/null)" != "$identity $src" ]]; then
      echo "signing $dest" >&2
      tmp=$(mktemp -d "${appDir}/.tmp.XXXXXX")
      /usr/bin/ditto "$src" "$tmp/app"
      chmod -R u+w "$tmp/app"
      /usr/bin/codesign --force --deep --sign "$identity" --keychain ${systemKeychain} "$tmp/app"
      rm -rf "$dest"
      mv "$tmp/app" "$dest"
      rmdir "$tmp"
      printf '%s' "$identity $src" > "$stamp"
      ${lib.optionalString (app.launchdLabel != null) ''
        # The agent's plist doesn't change, so launchd keeps the old binary running.
        /bin/launchctl kickstart -k "gui/$(id -u ${config.system.primaryUser})/${app.launchdLabel}" || true
      ''}
    fi
  '';
in
{
  options.stableCodesign.apps = lib.mkOption {
    type = lib.types.attrsOf (
      lib.types.submodule {
        options = {
          package = lib.mkOption {
            type = lib.types.package;
            description = "Package providing Applications/<name>.app.";
          };
          launchdLabel = lib.mkOption {
            type = lib.types.nullOr lib.types.str;
            default = null;
            description = "User agent running the signed copy, restarted after re-signing.";
          };
        };
      }
    );
    default = { };
    description = "Apps copied to ${appDir} and signed with the local key.";
  };

  config = lib.mkIf (cfg.apps != { }) {
    system.activationScripts.postActivation.text = lib.mkAfter ''
      if [[ ! -f ${cert} ]]; then
        echo "creating local code signing key" >&2
        /usr/bin/install -d -m 0755 -o root -g wheel "$(dirname ${cert})"
        (
          umask 077
          work=$(mktemp -d)
          printf '%s\n' '[req]' 'distinguished_name=dn' 'prompt=no' '[dn]' \
            'CN=${config.networking.hostName} Nix code signing' '[ext]' \
            'basicConstraints=critical,CA:false' 'keyUsage=critical,digitalSignature' \
            'extendedKeyUsage=critical,codeSigning' > "$work/conf"
          /usr/bin/openssl req -x509 -newkey rsa:3072 -nodes -days 36500 \
            -keyout "$work/key.pem" -out "$work/cert.pem" \
            -config "$work/conf" -extensions ext 2>/dev/null
          pw=$(/usr/bin/openssl rand -hex 16)
          /usr/bin/openssl pkcs12 -export -inkey "$work/key.pem" -in "$work/cert.pem" \
            -out "$work/id.p12" -passout "pass:$pw"
          /usr/bin/security import "$work/id.p12" -k ${systemKeychain} -P "$pw" \
            -T /usr/bin/codesign >/dev/null
          install -m 0644 "$work/cert.pem" ${cert}
          rm -rf "$work"
        )
      fi
      identity=$(/usr/bin/openssl x509 -in ${cert} -noout -fingerprint -sha1 | cut -d= -f2 | tr -d :)
      /usr/bin/install -d -m 0755 -o root -g admin ${lib.escapeShellArg appDir}
      ${lib.concatStrings (lib.mapAttrsToList sign cfg.apps)}
      for app in ${lib.escapeShellArg appDir}/*.app; do
        [[ -e "$app" ]] || continue
        name=$(basename "$app" .app)
        case "$name" in
          ${lib.concatMapStringsSep "|" lib.escapeShellArg (lib.attrNames cfg.apps)}) ;;
          *) rm -rf "$app" ${lib.escapeShellArg appDir}/."$name".source ;;
        esac
      done
    '';
  };
}
