//! Linux half: receives the Mac's device list and runs a PipeWire child that
//! hosts one pulse-tunnel per Mac device. Also the `open` client.

pub mod tunnels;

use std::io::BufReader;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::protocol::{self, AudioState, HostMessage, OpenReply, OpenRequest};
use crate::vsock;
use tunnels::{Defaults, Mode, Reconciler, Tunnel};

pub struct GuestConfig {
    pub pulse_server: String,
    pub port: u32,
}

pub fn run(cfg: GuestConfig) -> Result<()> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is not set")?;
    let dir = PathBuf::from(runtime).join("linux-vm-agent");
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;

    let (tx, rx) = mpsc::channel();
    let port = cfg.port;
    std::thread::spawn(move || channel_loop(port, tx));

    let mut sup = Supervisor {
        conf_path: dir.join("tunnels.conf"),
        pulse_server: cfg.pulse_server,
        child: None,
        started_at: Instant::now(),
        desired: Vec::new(),
        retry_at: None,
        backoff: MIN_BACKOFF,
        recon: Reconciler::default(),
        defaults_gen: Arc::new(AtomicU64::new(0)),
    };
    sup.run(rx)
}

/// Reads `AudioDevices` from the host forever, reconnecting with backoff.
fn channel_loop(port: u32, tx: mpsc::Sender<AudioState>) {
    let mut backoff = Duration::from_millis(250);
    let mut last_err = String::new();
    loop {
        match vsock::connect(vsock::VMADDR_CID_HOST, port) {
            Ok(f) => {
                log!("channel connected");
                last_err.clear();
                backoff = Duration::from_millis(250);
                let mut r = BufReader::new(f);
                loop {
                    match protocol::read_line_raw(&mut r) {
                        Ok(Some(line)) => match serde_json::from_str::<HostMessage>(&line) {
                            Ok(HostMessage::AudioDevices(s)) => {
                                if tx.send(s).is_err() {
                                    return;
                                }
                            }
                            // Unknown messages are skipped so the host can grow the protocol.
                            Err(e) => log!("ignoring host message: {e}"),
                        },
                        Ok(None) => {
                            log!("channel closed by host");
                            break;
                        }
                        Err(e) => {
                            log!("channel read error: {e}");
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                let msg = e.to_string();
                if msg != last_err {
                    log!("connecting to vsock port {port}: {msg} (retrying)");
                    last_err = msg;
                }
            }
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(Duration::from_secs(10));
    }
}

const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

struct Supervisor {
    conf_path: PathBuf,
    pulse_server: String,
    child: Option<Child>,
    started_at: Instant,
    desired: Vec<Tunnel>,
    retry_at: Option<Instant>,
    backoff: Duration,
    recon: Reconciler,
    defaults_gen: Arc<AtomicU64>,
}

impl Supervisor {
    fn run(&mut self, rx: mpsc::Receiver<AudioState>) -> Result<()> {
        loop {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(state) => {
                    let d = self.recon.update(&state);
                    if let Some(t) = d.restart {
                        log!("device set changed: {} tunnel(s)", t.len());
                        self.desired = t;
                        self.backoff = MIN_BACKOFF;
                        self.restart();
                    } else if d.apply_defaults {
                        self.apply_defaults();
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => anyhow::bail!("channel thread exited"),
            }
            self.check_child();
        }
    }

    fn stop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    fn restart(&mut self) {
        self.stop();
        self.retry_at = None;
        if self.desired.is_empty() {
            let _ = std::fs::remove_file(&self.conf_path);
            return;
        }
        match self.spawn() {
            Ok(c) => {
                log!("started pipewire tunnels (pid {})", c.id());
                self.child = Some(c);
                self.started_at = Instant::now();
                self.apply_defaults();
            }
            Err(e) => {
                log!("starting pipewire tunnels: {e:#}");
                self.schedule_retry();
            }
        }
    }

    fn spawn(&self) -> Result<Child> {
        let conf = tunnels::render_config(&self.desired, &self.pulse_server);
        write_atomic(&self.conf_path, &conf)?;
        let mut cmd = Command::new("pipewire");
        cmd.arg("-c").arg(&self.conf_path).stdin(Stdio::null());
        // Don't leave orphaned tunnels if the agent dies. This thread lives as long as the process.
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        cmd.spawn().context("spawning pipewire")
    }

    fn schedule_retry(&mut self) {
        self.retry_at = Some(Instant::now() + self.backoff);
        self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
    }

    fn check_child(&mut self) {
        if let Some(c) = &mut self.child {
            match c.try_wait() {
                Ok(None) => {
                    if self.started_at.elapsed() > MAX_BACKOFF {
                        self.backoff = MIN_BACKOFF;
                    }
                    return;
                }
                Ok(Some(status)) => log!("pipewire tunnels exited unexpectedly: {status}"),
                Err(e) => log!("waiting for pipewire tunnels: {e}"),
            }
            self.child = None;
            self.schedule_retry();
        }
        if self.retry_at.is_some_and(|t| Instant::now() >= t) {
            self.restart();
        }
    }

    /// Sets the configured defaults in a background thread, retrying until the
    /// nodes appear. A newer call supersedes an older one.
    fn apply_defaults(&self) {
        let d: Defaults = self.recon.defaults().clone();
        let generation = self.defaults_gen.fetch_add(1, Ordering::SeqCst) + 1;
        if d.is_empty() {
            return;
        }
        let gen = Arc::clone(&self.defaults_gen);
        std::thread::spawn(move || {
            let mut pending: Vec<(Mode, String)> = [(Mode::Sink, d.sink), (Mode::Source, d.source)]
                .into_iter()
                .filter_map(|(m, n)| n.map(|n| (m, n)))
                .collect();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if gen.load(Ordering::SeqCst) != generation {
                    return;
                }
                let nodes = Command::new("pw-dump")
                    .stdin(Stdio::null())
                    .stderr(Stdio::null())
                    .output()
                    .map(|o| tunnels::pw_dump_node_names(&String::from_utf8_lossy(&o.stdout)))
                    .unwrap_or_default();
                pending.retain(|(mode, node)| {
                    if !nodes.contains(node) {
                        return true;
                    }
                    let st = Command::new("pw-metadata")
                        .args(tunnels::metadata_args(*mode, node))
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .status();
                    match st {
                        Ok(s) if s.success() => {
                            log!("default {} -> {node}", mode.as_str());
                            false
                        }
                        other => {
                            log!("pw-metadata for {node} failed: {other:?}");
                            true
                        }
                    }
                });
                if pending.is_empty() {
                    return;
                }
                if Instant::now() >= deadline {
                    log!("gave up setting defaults, nodes missing: {pending:?}");
                    return;
                }
                std::thread::sleep(Duration::from_millis(300));
            }
        });
    }
}

fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    let tmp = path.with_extension("conf.tmp");
    std::fs::write(&tmp, contents).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming to {}", path.display()))?;
    Ok(())
}

/// Validates the `open` argument locally. Err = (message) for exit code 2.
pub fn check_open_arg(arg: &str) -> Result<String, String> {
    const LOCAL: &str = "local files can't be opened on the Mac; only URLs are forwarded";
    match url::Url::parse(arg) {
        Ok(u) if u.scheme() == "file" => Err(format!("{arg}: is a file: URL; {LOCAL}")),
        Ok(_) => Ok(arg.to_string()),
        Err(_) if arg.starts_with('/') || arg.starts_with('.') || arg.starts_with('~') || Path::new(arg).exists() => {
            Err(format!("{arg}: looks like a local path; {LOCAL}"))
        }
        Err(e) => Err(format!("{arg}: not a URL with a scheme ({e})")),
    }
}

/// Returns the process exit code.
pub fn open(url: &str, port: u32) -> i32 {
    let url = match check_open_arg(url) {
        Ok(u) => u,
        Err(msg) => {
            eprintln!("linux-vm-agent open: {msg}");
            return 2;
        }
    };
    let res = (|| -> std::io::Result<Option<OpenReply>> {
        let mut f = vsock::connect(vsock::VMADDR_CID_HOST, port)?;
        vsock::set_timeouts(&f, Some(Duration::from_secs(10)))?;
        protocol::write_line(&mut f, &OpenRequest::OpenUrl { url: url.clone() })?;
        protocol::read_message(&mut BufReader::new(f))
    })();
    match res {
        Ok(Some(OpenReply::Opened)) => {
            println!("opened on the Mac: {url}");
            0
        }
        Ok(Some(OpenReply::Refused { reason })) => {
            eprintln!("linux-vm-agent open: refused by the Mac: {reason}");
            1
        }
        Ok(None) => {
            eprintln!("linux-vm-agent open: host closed the connection without a reply");
            1
        }
        Err(e) => {
            eprintln!("linux-vm-agent open: talking to the host on vsock port {port}: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_arg_classification() {
        assert_eq!(check_open_arg("https://example.org").unwrap(), "https://example.org");
        assert!(check_open_arg("mailto:a@b.c").is_ok(), "host decides on schemes");
        assert!(check_open_arg("/home/user/x.pdf").unwrap_err().contains("local path"));
        assert!(check_open_arg("./x.pdf").unwrap_err().contains("local path"));
        assert!(check_open_arg("file:///home/user/x.pdf").unwrap_err().contains("file: URL"));
        assert!(check_open_arg("example.org").unwrap_err().contains("not a URL"));
    }
}
