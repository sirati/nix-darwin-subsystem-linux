//! macOS half. Only `run` and the CoreAudio FFI are macOS-gated; the rest is
//! portable so it can be tested on Linux.

pub mod audio;
#[cfg(target_os = "macos")]
pub mod coreaudio;
pub mod open;

use std::io::ErrorKind;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

#[derive(clap::Args, Debug)]
pub struct HostArgs {
    /// Unix socket libkrun connects guest vsock port 4300 to.
    #[arg(long, value_name = "PATH")]
    pub channel_socket: PathBuf,
    /// Unix socket libkrun connects guest vsock port 4301 to.
    #[arg(long, value_name = "PATH")]
    pub open_socket: PathBuf,
    /// Application passed to `open -a`.
    #[arg(long, value_name = "NAME", default_value = "Firefox")]
    pub browser_app: String,
    /// URL schemes the guest may open.
    #[arg(long, value_name = "LIST", value_delimiter = ',', default_value = "http,https")]
    pub allowed_schemes: Vec<String>,
    #[arg(long, value_name = "PATH", default_value = "pactl")]
    pub pactl: String,
    /// PulseAudio server pactl queries, e.g. unix:/path/to/native.
    #[arg(long, value_name = "SERVER")]
    pub pulse_server: String,
}

/// Binds a 0600 Unix socket, replacing a stale socket file but refusing to
/// clobber a live listener or a non-socket file.
pub fn bind_private(path: &Path) -> Result<UnixListener> {
    match std::fs::symlink_metadata(path) {
        Ok(md) if md.file_type().is_socket() => {
            if UnixStream::connect(path).is_ok() {
                bail!("{} is in use by another process", path.display());
            }
            std::fs::remove_file(path).with_context(|| format!("removing stale {}", path.display()))?;
        }
        Ok(_) => bail!("{} exists and is not a socket", path.display()),
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("stat {}", path.display())),
    }
    // umask closes the window between bind and chmod. Called before any threads start.
    let old = unsafe { libc::umask(0o177) };
    let res = UnixListener::bind(path);
    unsafe { libc::umask(old) };
    let listener = res.with_context(|| format!("binding {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

#[cfg(target_os = "macos")]
pub fn run(args: HostArgs) -> Result<()> {
    use std::sync::Arc;


    let channel = bind_private(&args.channel_socket)?;
    let open_listener = bind_private(&args.open_socket)?;
    let schemes: Vec<String> = args.allowed_schemes.iter().map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty()).collect();
    log!("host agent: browser {:?}, schemes {:?}, pulse {}", args.browser_app, schemes, args.pulse_server);

    let open_cfg = open::OpenConfig { browser_app: args.browser_app, allowed_schemes: schemes };
    std::thread::spawn(move || open::serve(open_listener, open_cfg));

    let hub = Arc::new(audio::Hub::default());
    {
        let hub = Arc::clone(&hub);
        let cfg = audio::PactlConfig { pactl: args.pactl, server: args.pulse_server };
        std::thread::spawn(move || audio::track(cfg, hub, coreaudio::defaults));
    }

    for conn in channel.incoming() {
        match conn {
            Ok(stream) => {
                if let Err(e) = hub.attach(stream) {
                    log!("channel attach failed: {e:#}");
                }
            }
            Err(e) => {
                log!("channel accept failed: {e}");
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn bind_private_mode_and_stale() {
        let dir = std::env::temp_dir().join(format!("lva-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s.sock");
        let l = bind_private(&p).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().mode() & 0o777, 0o600);
        // Live listener: refuse.
        assert!(bind_private(&p).is_err());
        drop(l);
        // Stale socket: replace.
        let _l = bind_private(&p).unwrap();
        // Regular file: refuse.
        let f = dir.join("file");
        std::fs::write(&f, b"x").unwrap();
        assert!(bind_private(&f).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
