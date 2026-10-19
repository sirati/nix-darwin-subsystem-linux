//! OpenUrl handling. The guest is less trusted than the host: validate
//! everything, never involve a shell.

use std::collections::VecDeque;
use std::io::BufReader;
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use url::Url;

use crate::protocol::{read_message, write_line, OpenReply, OpenRequest};

pub const MAX_URL_LEN: usize = 8 * 1024;

/// Returns the re-serialized URL to hand to `open`, or a refusal reason.
pub fn validate_url(raw: &str, allowed_schemes: &[String]) -> Result<String, String> {
    if raw.len() > MAX_URL_LEN {
        return Err(format!("URL longer than {MAX_URL_LEN} bytes"));
    }
    // Check before parsing: the url crate silently strips tabs and newlines.
    if raw.chars().any(char::is_control) {
        return Err("URL contains control characters".into());
    }
    let url = Url::parse(raw).map_err(|e| format!("not a valid URL: {e}"))?;
    // The parser lowercases the scheme; compare against lowercased config too.
    let scheme = url.scheme();
    if !allowed_schemes.iter().any(|s| s.eq_ignore_ascii_case(scheme)) {
        return Err(format!("scheme '{scheme}' is not allowed"));
    }
    let out = url.as_str();
    if out.len() > MAX_URL_LEN || out.chars().any(char::is_control) {
        return Err("URL invalid after normalisation".into());
    }
    Ok(out.to_string())
}

/// Sliding-window limiter: at most `max` events per `window`.
pub struct RateLimiter {
    max: usize,
    window: Duration,
    events: VecDeque<Instant>,
}

impl RateLimiter {
    pub fn new(max: usize, window: Duration) -> Self {
        Self { max, window, events: VecDeque::new() }
    }

    pub fn allow(&mut self, now: Instant) -> bool {
        while let Some(&t) = self.events.front() {
            if now.duration_since(t) >= self.window {
                self.events.pop_front();
            } else {
                break;
            }
        }
        if self.events.len() >= self.max {
            return false;
        }
        self.events.push_back(now);
        true
    }
}

pub struct OpenConfig {
    pub browser_app: String,
    pub allowed_schemes: Vec<String>,
}

pub fn decide(cfg: &OpenConfig, limiter: &mut RateLimiter, req: &OpenRequest) -> OpenReply {
    let OpenRequest::OpenUrl { url } = req;
    let url = match validate_url(url, &cfg.allowed_schemes) {
        Ok(u) => u,
        Err(reason) => return OpenReply::Refused { reason },
    };
    if !limiter.allow(Instant::now()) {
        return OpenReply::Refused { reason: "rate limit exceeded".into() };
    }
    // Separate argv entries; `--` ends option parsing before the URL.
    let status = Command::new("/usr/bin/open")
        .arg("-a")
        .arg(&cfg.browser_app)
        .arg("--")
        .arg(&url)
        .stdin(Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() => OpenReply::Opened,
        Ok(s) => OpenReply::Refused { reason: format!("open exited with {s}") },
        Err(e) => OpenReply::Refused { reason: format!("failed to run open: {e}") },
    }
}

fn handle(cfg: &OpenConfig, limiter: &mut RateLimiter, stream: UnixStream) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut writer = stream.try_clone()?;
    let reply = match read_message::<OpenRequest>(&mut BufReader::new(stream)) {
        Ok(Some(req)) => {
            let reply = decide(cfg, limiter, &req);
            let OpenRequest::OpenUrl { url } = &req;
            // Debug-format so control characters can't forge log lines.
            log!("open request {:?}: {:?}", url.chars().take(200).collect::<String>(), reply);
            reply
        }
        Ok(None) => return Ok(()),
        Err(e) => {
            log!("open request malformed: {e}");
            OpenReply::Refused { reason: format!("malformed request: {e}") }
        }
    };
    write_line(&mut writer, &reply)
}

/// Serves one request at a time; a slow guest only delays its own requests.
pub fn serve(listener: UnixListener, cfg: OpenConfig) {
    let mut limiter = RateLimiter::new(10, Duration::from_secs(10));
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                if let Err(e) = handle(&cfg, &mut limiter, stream) {
                    log!("open connection error: {e}");
                }
            }
            Err(e) => {
                log!("open socket accept failed: {e}");
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn web() -> Vec<String> {
        vec!["http".into(), "https".into()]
    }

    #[test]
    fn accepts_web_urls() {
        assert_eq!(validate_url("https://example.org/a?b=c#d", &web()).unwrap(), "https://example.org/a?b=c#d");
        assert_eq!(validate_url("http://example.org", &web()).unwrap(), "http://example.org/");
        assert_eq!(validate_url("HTTPS://Example.ORG/X", &web()).unwrap(), "https://example.org/X");
        // Config is case-insensitive too.
        assert!(validate_url("https://example.org/", &["HTTPS".into()]).is_ok());
        // Spaces get percent-encoded by the re-serialisation.
        assert_eq!(validate_url("https://example.org/a b", &web()).unwrap(), "https://example.org/a%20b");
    }

    #[test]
    fn rejects_bad_schemes() {
        for u in [
            "javascript:alert(1)",
            "JavaScript:alert(1)",
            "file:///etc/passwd",
            "ftp://example.org/",
            "x-apple-systempreferences:com.apple.preference",
            "data:text/html,<script>",
        ] {
            let err = validate_url(u, &web()).unwrap_err();
            assert!(err.contains("not allowed"), "{u}: {err}");
        }
    }

    #[test]
    fn rejects_malformed() {
        assert!(validate_url("/etc/passwd", &web()).is_err());
        assert!(validate_url("", &web()).is_err());
        assert!(validate_url("https://exa mple.org/", &web()).is_err());
        assert!(validate_url("-a Terminal", &web()).is_err());
    }

    #[test]
    fn rejects_control_characters() {
        for u in ["https://example.org/\n", "https://ex\tample.org/", "https://example.org/\u{7f}", "https://example.org/\u{85}", "https://example.org/\0"] {
            assert!(validate_url(u, &web()).unwrap_err().contains("control"), "{u:?}");
        }
    }

    #[test]
    fn rejects_long() {
        let long = format!("https://example.org/{}", "a".repeat(9 * 1024));
        assert!(validate_url(&long, &web()).unwrap_err().contains("longer"));
        let ok = format!("https://example.org/{}", "a".repeat(MAX_URL_LEN - 20));
        assert_eq!(ok.len(), MAX_URL_LEN);
        assert!(validate_url(&ok, &web()).is_ok());
    }

    #[test]
    fn rate_limiter_window() {
        let mut rl = RateLimiter::new(10, Duration::from_secs(10));
        let t0 = Instant::now();
        for i in 0..10 {
            assert!(rl.allow(t0 + Duration::from_millis(i)));
        }
        assert!(!rl.allow(t0 + Duration::from_secs(5)));
        assert!(rl.allow(t0 + Duration::from_secs(10)));
        // Only the oldest slot has expired.
        assert!(!rl.allow(t0 + Duration::from_secs(10)));
        assert!(rl.allow(t0 + Duration::from_millis(10_001)));
    }

    #[test]
    fn refused_requests_do_not_spawn_or_count() {
        let cfg = OpenConfig { browser_app: "Firefox".into(), allowed_schemes: web() };
        let mut rl = RateLimiter::new(1, Duration::from_secs(10));
        let r = decide(&cfg, &mut rl, &OpenRequest::OpenUrl { url: "file:///".into() });
        assert!(matches!(r, OpenReply::Refused { .. }));
        assert!(rl.allow(Instant::now()));
    }
}
