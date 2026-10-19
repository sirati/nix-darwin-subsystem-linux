//! linux-vm-agent: integration between a macOS host and a libkrun NixOS guest.

/// Logs to stderr; launchd and journald add timestamps.
#[macro_export]
macro_rules! log {
    ($($t:tt)*) => { eprintln!("linux-vm-agent: {}", format_args!($($t)*)) };
}

#[cfg(target_os = "linux")]
mod guest;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod host;
mod protocol;
#[cfg(target_os = "linux")]
mod vsock;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// macOS side: serve the guest's channel and open sockets.
    Host(host::HostArgs),
    /// Linux side: mirror the Mac's audio devices into PipeWire.
    Guest {
        /// PulseAudio server address inside the guest that reaches the Mac.
        #[arg(long, value_name = "SERVER")]
        pulse_server: String,
        #[arg(long, default_value_t = protocol::CHANNEL_PORT)]
        port: u32,
    },
    /// Linux side: open a URL in the Mac's browser (xdg-open replacement).
    Open {
        #[arg(long, default_value_t = protocol::OPEN_PORT)]
        port: u32,
        url: String,
    },
}

fn main() {
    let cli = Cli::parse();
    let code = match cli.cmd {
        Cmd::Host(args) => report(run_host(args)),
        Cmd::Guest { pulse_server, port } => report(run_guest(pulse_server, port)),
        Cmd::Open { port, url } => run_open(&url, port),
    };
    std::process::exit(code);
}

fn report(r: anyhow::Result<()>) -> i32 {
    match r {
        Ok(()) => 0,
        Err(e) => {
            log!("error: {e:#}");
            1
        }
    }
}

#[cfg(target_os = "macos")]
fn run_host(args: host::HostArgs) -> anyhow::Result<()> {
    host::run(args)
}
#[cfg(not(target_os = "macos"))]
fn run_host(_: host::HostArgs) -> anyhow::Result<()> {
    anyhow::bail!("`host` only runs on macOS")
}

#[cfg(target_os = "linux")]
fn run_guest(pulse_server: String, port: u32) -> anyhow::Result<()> {
    guest::run(guest::GuestConfig { pulse_server, port })
}
#[cfg(not(target_os = "linux"))]
fn run_guest(_: String, _: u32) -> anyhow::Result<()> {
    anyhow::bail!("`guest` only runs on Linux")
}

#[cfg(target_os = "linux")]
fn run_open(url: &str, port: u32) -> i32 {
    guest::open(url, port)
}
#[cfg(not(target_os = "linux"))]
fn run_open(_: &str, _: u32) -> i32 {
    log!("`open` only runs in the Linux guest");
    1
}
