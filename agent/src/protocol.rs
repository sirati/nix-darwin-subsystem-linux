//! Wire format shared by host and guest: newline-delimited JSON.

use std::io::{self, BufRead, Read, Write};

use serde::{de::DeserializeOwned, Deserialize, Serialize};

/// vsock port of the persistent channel.
pub const CHANNEL_PORT: u32 = 4300;
/// vsock port for one-shot open requests.
pub const OPEN_PORT: u32 = 4301;

/// Cap on a single line; the peer may be hostile, so never buffer unbounded input.
pub const MAX_LINE: usize = 256 * 1024;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Device {
    pub pulse_name: String,
    pub description: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct AudioState {
    pub sinks: Vec<Device>,
    pub sources: Vec<Device>,
    pub default_sink: Option<String>,
    pub default_source: Option<String>,
}

/// host -> guest on the channel.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "type")]
pub enum HostMessage {
    AudioDevices(AudioState),
}

/// guest -> host on the open socket.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "type")]
pub enum OpenRequest {
    OpenUrl { url: String },
}

/// host -> guest reply on the open socket.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "type")]
pub enum OpenReply {
    Opened,
    Refused { reason: String },
}

pub fn write_line<T: Serialize>(w: &mut impl Write, msg: &T) -> io::Result<()> {
    let mut buf = serde_json::to_vec(msg).map_err(io::Error::other)?;
    buf.push(b'\n');
    w.write_all(&buf)?;
    w.flush()
}

/// Reads one line of at most `MAX_LINE` bytes. `Ok(None)` on clean EOF.
pub fn read_line_raw(r: &mut impl BufRead) -> io::Result<Option<String>> {
    let mut buf = Vec::new();
    let n = r.by_ref().take(MAX_LINE as u64 + 1).read_until(b'\n', &mut buf)?;
    if n == 0 {
        return Ok(None);
    }
    if buf.last() != Some(&b'\n') {
        if buf.len() > MAX_LINE {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
        }
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated line"));
    }
    buf.pop();
    String::from_utf8(buf)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub fn read_message<T: DeserializeOwned>(r: &mut impl BufRead) -> io::Result<Option<T>> {
    match read_line_raw(r)? {
        None => Ok(None),
        Some(line) => serde_json::from_str(&line)
            .map(Some)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn roundtrip<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(msg: T) -> String {
        let mut buf = Vec::new();
        write_line(&mut buf, &msg).unwrap();
        let text = String::from_utf8(buf.clone()).unwrap();
        assert!(text.ends_with('\n') && !text[..text.len() - 1].contains('\n'));
        let back: T = read_message(&mut Cursor::new(buf)).unwrap().unwrap();
        assert_eq!(back, msg);
        text.trim_end().to_string()
    }

    #[test]
    fn audio_devices_roundtrip() {
        let msg = HostMessage::AudioDevices(AudioState {
            sinks: vec![Device {
                pulse_name: "Channel_1__Channel_2".into(),
                description: "WH-1000XM5".into(),
            }],
            sources: vec![],
            default_sink: Some("Channel_1__Channel_2".into()),
            default_source: None,
        });
        let text = roundtrip(msg);
        assert_eq!(
            text,
            r#"{"type":"AudioDevices","sinks":[{"pulse_name":"Channel_1__Channel_2","description":"WH-1000XM5"}],"sources":[],"default_sink":"Channel_1__Channel_2","default_source":null}"#
        );
    }

    #[test]
    fn open_roundtrip() {
        assert_eq!(
            roundtrip(OpenRequest::OpenUrl { url: "https://example.org/".into() }),
            r#"{"type":"OpenUrl","url":"https://example.org/"}"#
        );
        assert_eq!(roundtrip(OpenReply::Opened), r#"{"type":"Opened"}"#);
        assert_eq!(
            roundtrip(OpenReply::Refused { reason: "nope".into() }),
            r#"{"type":"Refused","reason":"nope"}"#
        );
    }

    #[test]
    fn parses_spec_examples() {
        let r: OpenRequest = serde_json::from_str(r#"{ "type": "OpenUrl", "url": "..." }"#).unwrap();
        assert_eq!(r, OpenRequest::OpenUrl { url: "...".into() });
        assert!(serde_json::from_str::<OpenRequest>(r#"{"type":"Other"}"#).is_err());
    }

    #[test]
    fn line_limits() {
        let mut r = Cursor::new(Vec::<u8>::new());
        assert!(read_line_raw(&mut r).unwrap().is_none());
        let mut r = Cursor::new(b"abc".to_vec());
        assert!(read_line_raw(&mut r).is_err());
        let mut r = Cursor::new(vec![b'a'; MAX_LINE + 10]);
        assert_eq!(read_line_raw(&mut r).unwrap_err().kind(), io::ErrorKind::InvalidData);
        let mut r = Cursor::new(b"one\ntwo\n".to_vec());
        assert_eq!(read_line_raw(&mut r).unwrap().as_deref(), Some("one"));
        assert_eq!(read_line_raw(&mut r).unwrap().as_deref(), Some("two"));
    }
}
