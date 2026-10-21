//! "Internet access" — makes the built-in server reachable from ANY network, for free.
//!
//! * The server PC runs Cloudflare's free quick tunnel (`cloudflared`), which gives a public
//!   `https://<random>.trycloudflare.com` address (works behind NAT / mobile data / office Wi-Fi
//!   with client isolation, no port forwarding).
//! * That address changes every time the tunnel restarts, so the server PC publishes the current
//!   address under a short **team code** (via the free ntfy.sh pub/sub service).
//! * Other PCs enter the team code once; Veyndesk looks the address up and follows it when it changes.

use serde::Serialize;
use std::{
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

const CLOUDFLARED_URL: &str =
    "https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-windows-amd64.exe";
const NTFY: &str = "https://ntfy.sh";
const REPUBLISH_EVERY: Duration = Duration::from_secs(3 * 3600); // ntfy.sh keeps messages 12 h

#[derive(Serialize, Clone, Default, Debug)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum NetState {
    #[default]
    Off,
    Downloading,
    Starting,
    Online { url: String },
    Failed { error: String },
}

pub struct Internet {
    pub state: Mutex<NetState>,
    child: Mutex<Option<Child>>,
    running: AtomicBool,
    /// Team code to publish the address under (None = tunnel only used as P2P relay).
    code: Mutex<Option<String>>,
}

// ------------------------------------------------------------------ team codes

const CODE_CHARS: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// e.g. `K7QM-3XPA`
pub fn new_code() -> String {
    let t = velodesk_protocol::random_token(8);
    let b = t.as_bytes();
    let c: String = (0..8).map(|i| CODE_CHARS[(b[i] as usize * 7 + b[i + 8] as usize) % CODE_CHARS.len()] as char).collect();
    format!("{}-{}", &c[..4], &c[4..])
}

/// Accepts `k7qm3xpa`, `K7QM-3XPA`, ` k7qm 3xpa ` → `K7QM-3XPA`.
pub fn normalize_code(s: &str) -> Option<String> {
    let c: String = s.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_uppercase()).collect();
    (c.len() == 8).then(|| format!("{}-{}", &c[..4], &c[4..]))
}

fn topic(code: &str) -> String {
    format!("velodesk-{}", code.replace('-', "").to_ascii_lowercase())
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout(Duration::from_secs(15)).build()
}

pub fn publish(code: &str, url: &str) -> Result<(), String> {
    agent()
        .post(&format!("{NTFY}/{}", topic(code)))
        .send_string(url)
        .map(|_| ())
        .map_err(|e| format!("could not publish address: {e}"))
}

/// Latest server address published for this team code.
pub fn resolve(code: &str) -> Result<String, String> {
    let body = agent()
        .get(&format!("{NTFY}/{}/json?poll=1&since=12h", topic(code)))
        .call()
        .map_err(|e| format!("lookup failed: {e}"))?
        .into_string()
        .map_err(|e| e.to_string())?;
    latest_url(&body).ok_or_else(|| "No Veyndesk server is online for this team code. Is the server PC on, with Internet access turned on?".into())
}

fn latest_url(ndjson: &str) -> Option<String> {
    ndjson
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["event"] == "message")
        .filter_map(|v| v["message"].as_str().map(str::to_string))
        .filter(|m| m.starts_with("https://"))
        .last()
}

fn tunnel_url(line: &str) -> Option<String> {
    let i = line.find("https://")?;
    let rest = &line[i..];
    let end = rest.find(|c: char| c.is_whitespace() || c == '|' || c == '"').unwrap_or(rest.len());
    let url = &rest[..end];
    url.ends_with(".trycloudflare.com").then(|| url.to_string())
}

// ------------------------------------------------------------------ tunnel

fn cloudflared_path() -> PathBuf {
    velodesk_host::config::config_path()
        .parent()
        .map(|p| p.join("cloudflared.exe"))
        .unwrap_or_else(|| "cloudflared.exe".into())
}

fn ensure_cloudflared(me: &Internet) -> Result<PathBuf, String> {
    // Prefer a copy next to Veyndesk.exe (portable / bundled), then the downloaded one.
    if let Some(p) = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("cloudflared.exe"))) {
        if p.exists() {
            return Ok(p);
        }
    }
    let path = cloudflared_path();
    if path.exists() && std::fs::metadata(&path).map(|m| m.len() > 1_000_000).unwrap_or(false) {
        return Ok(path);
    }
    *me.state.lock().unwrap() = NetState::Downloading;
    let resp = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(600))
        .build()
        .get(CLOUDFLARED_URL)
        .call()
        .map_err(|e| format!("download of cloudflared failed: {e}"))?;
    let tmp = path.with_extension("download");
    if let Some(d) = path.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let mut f = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
    std::io::copy(&mut resp.into_reader(), &mut f).map_err(|e| format!("download interrupted: {e}"))?;
    f.flush().ok();
    drop(f);
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    Ok(path)
}

impl Internet {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(NetState::Off),
            child: Mutex::new(None),
            running: AtomicBool::new(false),
            code: Mutex::new(None),
        })
    }

    /// Public URL while the tunnel is online.
    pub fn url(&self) -> Option<String> {
        match &*self.state.lock().unwrap() {
            NetState::Online { url } => Some(url.clone()),
            _ => None,
        }
    }

    /// Set / clear the team code; publishes right away if the tunnel is already online.
    pub fn set_code(&self, code: Option<String>) {
        *self.code.lock().unwrap() = code.clone();
        if let (Some(c), Some(u)) = (code, self.url()) {
            std::thread::spawn(move || {
                let _ = publish(&c, &u);
            });
        }
    }

    /// Start (idempotent) the tunnel to `http://127.0.0.1:port`.
    pub fn start(self: &Arc<Self>, port: u16) {
        if self.running.swap(true, Ordering::SeqCst) {
            return;
        }
        let me = self.clone();
        std::thread::spawn(move || {
            while me.running.load(Ordering::SeqCst) {
                if let Err(e) = me.run_once(port) {
                    *me.state.lock().unwrap() = NetState::Failed { error: e };
                }
                if me.running.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_secs(10)); // restart the tunnel
                }
            }
            *me.state.lock().unwrap() = NetState::Off;
        });
    }

    fn run_once(self: &Arc<Self>, port: u16) -> Result<(), String> {
        let exe = ensure_cloudflared(self)?;
        *self.state.lock().unwrap() = NetState::Starting;
        let mut cmd = Command::new(exe);
        cmd.args(["tunnel", "--no-autoupdate", "--url", &format!("http://127.0.0.1:{port}")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let mut child = cmd.spawn().map_err(|e| format!("could not start cloudflared: {e}"))?;
        let stderr = child.stderr.take().unwrap();
        *self.child.lock().unwrap() = Some(child);

        let mut published: Option<(String, Instant)> = None;
        let mut last_lines: Vec<String> = vec![];
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if let Some(url) = tunnel_url(&line) {
                *self.state.lock().unwrap() = NetState::Online { url: url.clone() };
                let code = self.code.lock().unwrap().clone();
                if let Some(code) = code {
                    if let Err(e) = publish(&code, &url) {
                        *self.state.lock().unwrap() = NetState::Failed { error: e };
                    }
                }
                published = Some((url, Instant::now()));
            }
            // Re-publish periodically (log lines arrive regularly enough to piggy-back on).
            let code = self.code.lock().unwrap().clone();
            if let (Some((url, t)), Some(code)) = (&published, code) {
                if t.elapsed() > REPUBLISH_EVERY && publish(&code, url).is_ok() {
                    published = Some((url.clone(), Instant::now()));
                }
            }
            last_lines.push(line);
            if last_lines.len() > 5 {
                last_lines.remove(0);
            }
        }
        if let Some(mut c) = self.child.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        Err(format!("tunnel stopped: {}", last_lines.last().cloned().unwrap_or_default()))
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(mut c) = self.child.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Republish keep-alive for long quiet periods (cloudflared may not log for hours).
pub fn spawn_republisher(net: Arc<Internet>, code: impl Fn() -> Option<String> + Send + 'static) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(3600));
        if let (NetState::Online { url }, Some(c)) = (net.state.lock().unwrap().clone(), code()) {
            let _ = publish(&c, &url);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes() {
        let c = new_code();
        assert_eq!(c.len(), 9);
        assert_eq!(normalize_code(&c.to_lowercase().replace('-', " ")), Some(c.clone()));
        assert_eq!(normalize_code("abc"), None);
        assert_eq!(topic("K7QM-3XPA"), "velodesk-k7qm3xpa");
    }

    #[test]
    fn parse_cloudflared_banner() {
        let l = "2026-10-20T10:00:00Z INF |  https://quiet-river-1234.trycloudflare.com                                  |";
        assert_eq!(tunnel_url(l).as_deref(), Some("https://quiet-river-1234.trycloudflare.com"));
        assert_eq!(tunnel_url("INF Requesting new quick Tunnel on trycloudflare.com..."), None);
        assert_eq!(tunnel_url("see https://developers.cloudflare.com/x"), None);
    }

    #[test]
    fn parse_ntfy_poll() {
        let body = r#"{"id":"a","event":"message","message":"https://old.trycloudflare.com"}
{"id":"b","event":"message","message":"https://new.trycloudflare.com"}
"#;
        assert_eq!(latest_url(body).as_deref(), Some("https://new.trycloudflare.com"));
        assert_eq!(latest_url(""), None);
    }
}
