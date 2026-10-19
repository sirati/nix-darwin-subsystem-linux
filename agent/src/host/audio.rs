//! Tracks the Mac's PulseAudio devices and CoreAudio defaults, and pushes
//! `AudioDevices` to the connected guest whenever that state changes.

use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::protocol::{read_line_raw, write_line, AudioState, Device, HostMessage};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PulseDevice {
    pub name: String,
    pub description: String,
    pub device_string: Option<String>,
}

#[derive(Deserialize)]
struct RawPulse {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    monitor_source: Option<String>,
    #[serde(default)]
    properties: serde_json::Map<String, serde_json::Value>,
}

/// Parses `pactl -f json list sinks|sources`. For sources, `monitor_source`
/// names the sink being monitored, so a non-empty value marks a monitor.
pub fn parse_pactl(json: &str, skip_monitors: bool) -> Result<Vec<PulseDevice>> {
    let raw: Vec<RawPulse> = serde_json::from_str(json).context("parsing pactl JSON")?;
    Ok(raw
        .into_iter()
        .filter(|d| !skip_monitors || d.monitor_source.as_deref().unwrap_or("").is_empty())
        .map(|d| PulseDevice {
            description: d.description.unwrap_or_else(|| d.name.clone()),
            device_string: d
                .properties
                .get("device.string")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            name: d.name,
        })
        .collect())
}

/// module-coreaudio-detect sets `device.string` to the CoreAudio device name;
/// fall back to the description for entries that lack it.
pub fn map_default<'a>(devices: &'a [PulseDevice], coreaudio_name: Option<&str>) -> Option<&'a str> {
    let want = coreaudio_name?;
    devices
        .iter()
        .find(|d| d.device_string.as_deref() == Some(want))
        .or_else(|| devices.iter().find(|d| d.device_string.is_none() && d.description == want))
        .map(|d| d.name.as_str())
}

pub fn compute_state(
    sinks: &[PulseDevice],
    sources: &[PulseDevice],
    default_output: Option<&str>,
    default_input: Option<&str>,
) -> AudioState {
    let conv = |ds: &[PulseDevice]| {
        ds.iter()
            .map(|d| Device { pulse_name: d.name.clone(), description: d.description.clone() })
            .collect()
    };
    AudioState {
        sinks: conv(sinks),
        sources: conv(sources),
        default_sink: map_default(sinks, default_output).map(str::to_string),
        default_source: map_default(sources, default_input).map(str::to_string),
    }
}

/// `pactl subscribe` prints `Event 'change' on sink #3`. Only sink, source and
/// server facilities matter; `sink-input` and `source-output` don't.
pub fn subscribe_line_relevant(line: &str) -> bool {
    let Some((_, rest)) = line.split_once(" on ") else { return false };
    let facility = rest.split_whitespace().next().unwrap_or("");
    matches!(facility, "sink" | "source" | "server")
}

/// Holds the latest state and the current guest connection. Dedups sends.
#[derive(Default)]
pub struct Hub {
    inner: Mutex<HubInner>,
}

#[derive(Default)]
struct HubInner {
    state: Option<AudioState>,
    client: Option<(u64, UnixStream)>,
    next_id: u64,
}

impl HubInner {
    fn send_current(&mut self) {
        let (Some(state), Some((id, stream))) = (&self.state, &mut self.client) else { return };
        let msg = HostMessage::AudioDevices(state.clone());
        if let Err(e) = write_line(stream, &msg) {
            log!("channel {id}: send failed ({e}), dropping");
            let _ = stream.shutdown(std::net::Shutdown::Both);
            self.client = None;
        }
    }
}

impl Hub {
    pub fn publish(&self, state: AudioState) {
        let mut g = self.inner.lock().unwrap();
        if g.state.as_ref() == Some(&state) {
            return;
        }
        log!("audio state: {state:?}");
        g.state = Some(state);
        g.send_current();
    }

    /// A new guest connection replaces the old one (the guest reconnected).
    pub fn attach(self: &Arc<Self>, stream: UnixStream) -> Result<()> {
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        let reader = stream.try_clone()?;
        let id = {
            let mut g = self.inner.lock().unwrap();
            g.next_id += 1;
            let id = g.next_id;
            if let Some((old, s)) = g.client.take() {
                log!("channel {old}: replaced by {id}");
                let _ = s.shutdown(std::net::Shutdown::Both);
            }
            g.client = Some((id, stream));
            g.send_current();
            id
        };
        log!("channel {id}: guest connected");
        let hub = Arc::clone(self);
        std::thread::spawn(move || {
            // Nothing is expected from the guest yet; drain until EOF to notice the drop.
            let mut r = BufReader::new(reader);
            loop {
                match read_line_raw(&mut r) {
                    Ok(Some(line)) => log!("channel {id}: ignoring {:?}", line.chars().take(200).collect::<String>()),
                    Ok(None) => break,
                    Err(e) => {
                        log!("channel {id}: read error {e}");
                        break;
                    }
                }
            }
            let mut g = hub.inner.lock().unwrap();
            if matches!(&g.client, Some((cur, _)) if *cur == id) {
                g.client = None;
                log!("channel {id}: guest disconnected");
            }
        });
        Ok(())
    }
}

pub struct PactlConfig {
    pub pactl: String,
    pub server: String,
}

impl PactlConfig {
    fn command(&self) -> Command {
        let mut c = Command::new(&self.pactl);
        // subscribe output is translated; keep it parseable.
        c.env("LC_ALL", "C").arg("-s").arg(&self.server);
        c
    }

    fn list(&self, what: &str) -> Result<String> {
        let out = self
            .command()
            .args(["-f", "json", "list", what])
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("running {}", self.pactl))?;
        if !out.status.success() {
            bail!("pactl list {what}: {} {}", out.status, String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8(out.stdout)?)
    }

    fn read_devices(&self) -> Result<(Vec<PulseDevice>, Vec<PulseDevice>)> {
        Ok((parse_pactl(&self.list("sinks")?, false)?, parse_pactl(&self.list("sources")?, true)?))
    }
}

/// Runs `pactl subscribe` forever, poking `tx` on relevant events.
fn subscribe_loop(cfg: &PactlConfig, tx: mpsc::Sender<()>) {
    let mut backoff = Duration::from_millis(500);
    loop {
        let child = cfg.command().arg("subscribe").stdin(Stdio::null()).stdout(Stdio::piped()).spawn();
        match child {
            Ok(mut child) => {
                // The server may have restarted while we were down.
                let _ = tx.send(());
                let mut saw_output = false;
                for line in BufReader::new(child.stdout.take().unwrap()).lines() {
                    let Ok(line) = line else { break };
                    saw_output = true;
                    if subscribe_line_relevant(&line) && tx.send(()).is_err() {
                        let _ = child.kill();
                        let _ = child.wait();
                        return;
                    }
                }
                let status = child.wait();
                log!("pactl subscribe exited ({status:?}), restarting");
                if saw_output {
                    backoff = Duration::from_millis(500);
                }
            }
            Err(e) => log!("spawning pactl subscribe failed: {e}"),
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(Duration::from_secs(10));
    }
}

/// Main tracking loop. `defaults` returns the CoreAudio default output and
/// input device names; it is polled once a second.
pub fn track(cfg: PactlConfig, hub: Arc<Hub>, defaults: fn() -> (Option<String>, Option<String>)) -> ! {
    let cfg = Arc::new(cfg);
    let (tx, rx) = mpsc::channel();
    {
        let cfg = Arc::clone(&cfg);
        std::thread::spawn(move || subscribe_loop(&cfg, tx));
    }
    let mut devices: Option<(Vec<PulseDevice>, Vec<PulseDevice>)> = None;
    let mut dirty = true;
    loop {
        if dirty {
            match cfg.read_devices() {
                Ok(d) => {
                    devices = Some(d);
                    dirty = false;
                }
                // Keep dirty so the next tick retries.
                Err(e) => log!("reading pulse devices: {e:#}"),
            }
        }
        if let Some((sinks, sources)) = &devices {
            let (out, inp) = defaults();
            hub.publish(compute_state(sinks, sources, out.as_deref(), inp.as_deref()));
        }
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(()) => {
                dirty = true;
                // Coalesce event bursts into one re-read.
                std::thread::sleep(Duration::from_millis(100));
                while rx.try_recv().is_ok() {}
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => panic!("subscribe thread died"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Trimmed real output from the Mac (PulseAudio 17, module-coreaudio-detect).
    const SINKS: &str = r#"[
      {"index":1,"state":"SUSPENDED","name":"Channel_1__Channel_2","description":"WH-1000XM5","driver":"module-coreaudio-device.c",
       "monitor_source":"Channel_1__Channel_2.monitor",
       "properties":{"device.string":"WH-1000XM5","device.description":"WH-1000XM5"}},
      {"index":2,"state":"RUNNING","name":"1__2","description":"MacBook Neo Speakers","driver":"module-coreaudio-device.c",
       "monitor_source":"1__2.monitor",
       "properties":{"device.string":"MacBook Neo Speakers"}}
    ]"#;

    const SOURCES: &str = r#"[
      {"index":1,"name":"Channel_1__Channel_2.monitor","description":"Monitor of WH-1000XM5",
       "monitor_source":"Channel_1__Channel_2","properties":{"device.class":"monitor"}},
      {"index":2,"name":"Channel_1","description":"WH-1000XM5","monitor_source":"",
       "properties":{"device.string":"WH-1000XM5"}},
      {"index":3,"name":"1__2.monitor","description":"Monitor of MacBook Neo Speakers",
       "monitor_source":"1__2","properties":{}},
      {"index":4,"name":"1","description":"MacBook Neo Microphone","monitor_source":null,
       "properties":{"device.string":"MacBook Neo Microphone"}}
    ]"#;

    #[test]
    fn parses_and_skips_monitors() {
        let sinks = parse_pactl(SINKS, false).unwrap();
        let sources = parse_pactl(SOURCES, true).unwrap();
        assert_eq!(sinks.len(), 2);
        assert_eq!(
            sinks[0],
            PulseDevice {
                name: "Channel_1__Channel_2".into(),
                description: "WH-1000XM5".into(),
                device_string: Some("WH-1000XM5".into())
            }
        );
        let names: Vec<_> = sources.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["Channel_1", "1"]);
    }

    #[test]
    fn maps_coreaudio_defaults() {
        let sinks = parse_pactl(SINKS, false).unwrap();
        let sources = parse_pactl(SOURCES, true).unwrap();
        let s = compute_state(&sinks, &sources, Some("WH-1000XM5"), Some("MacBook Neo Microphone"));
        assert_eq!(s.default_sink.as_deref(), Some("Channel_1__Channel_2"));
        assert_eq!(s.default_source.as_deref(), Some("1"));
        assert_eq!(s.sinks[1], Device { pulse_name: "1__2".into(), description: "MacBook Neo Speakers".into() });
        assert_eq!(s.sources.len(), 2);

        let s = compute_state(&sinks, &sources, Some("MacBook Neo Speakers"), Some("WH-1000XM5"));
        assert_eq!(s.default_sink.as_deref(), Some("1__2"));
        assert_eq!(s.default_source.as_deref(), Some("Channel_1"));

        let s = compute_state(&sinks, &sources, Some("AirPods"), None);
        assert_eq!((s.default_sink, s.default_source), (None, None));
    }

    #[test]
    fn falls_back_to_description_without_device_string() {
        // The source sample as given in the spec has no properties.
        let src = parse_pactl(r#"[{"name":"Channel_1","description":"WH-1000XM5"},{"name":"1","description":"MacBook Neo Microphone"}]"#, true).unwrap();
        assert_eq!(map_default(&src, Some("MacBook Neo Microphone")), Some("1"));
    }

    #[test]
    fn subscribe_filter() {
        assert!(subscribe_line_relevant("Event 'change' on sink #3"));
        assert!(subscribe_line_relevant("Event 'new' on source #7"));
        assert!(subscribe_line_relevant("Event 'change' on server #4294967295"));
        assert!(!subscribe_line_relevant("Event 'change' on sink-input #12"));
        assert!(!subscribe_line_relevant("Event 'remove' on source-output #2"));
        assert!(!subscribe_line_relevant("Event 'change' on client #9"));
        assert!(!subscribe_line_relevant("garbage"));
    }

    #[test]
    fn hub_dedups_and_replays_on_attach() {
        let hub = Arc::new(Hub::default());
        let state = AudioState { default_sink: Some("x".into()), ..Default::default() };
        hub.publish(state.clone());
        let (a, b) = UnixStream::pair().unwrap();
        hub.attach(a).unwrap();
        hub.publish(state.clone());
        hub.publish(AudioState::default());
        drop(hub);
        b.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        let mut r = BufReader::new(b);
        let first: HostMessage = crate::protocol::read_message(&mut r).unwrap().unwrap();
        assert_eq!(first, HostMessage::AudioDevices(state));
        let second: HostMessage = crate::protocol::read_message(&mut r).unwrap().unwrap();
        assert_eq!(second, HostMessage::AudioDevices(AudioState::default()));
        assert!(crate::protocol::read_message::<HostMessage>(&mut r).is_err());
    }
}
