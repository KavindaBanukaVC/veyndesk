use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use veyndesk_protocol::{password_secret, random_password, random_token};

/// Persistent host settings (`%APPDATA%\Veyndesk\host.json` on Windows, see [`config_path`]).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct HostConfig {
    /// Server base URL, e.g. `https://desk.example.com` or `ws://192.168.1.10:8080`.
    pub server: String,
    /// Assigned by the server on first connect.
    pub id: Option<String>,
    /// Device secret proving ownership of `id`. Never shown to anyone.
    pub key: String,
    /// Display name shown to viewers and in the admin panel.
    pub name: String,
    /// Permanent (unattended-access) password, stored as salt + hash only.
    pub permanent: Option<Credentials>,
    /// Desktop app: ask before accepting one-time-password sessions.
    #[serde(default = "yes")]
    pub ask_to_accept: bool,
    /// Desktop app: also run a Veyndesk server on this PC (LAN mode).
    #[serde(default)]
    pub run_server: bool,
    /// Desktop app (server PC): publish the built-in server on the internet (Cloudflare tunnel).
    #[serde(default)]
    pub internet: bool,
    /// Team code: the server PC publishes its internet address under it; other PCs follow it.
    #[serde(default)]
    pub team_code: Option<String>,
}

fn yes() -> bool {
    true
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Credentials {
    pub salt: String,
    pub secret: String,
}

impl Credentials {
    pub fn from_password(password: &str) -> Self {
        let salt = random_token(12);
        let secret = password_secret(&salt, password);
        Self { salt, secret }
    }
}

/// A freshly generated one-time password (shown in the UI, valid for this run).
pub fn one_time_password() -> (String, Credentials) {
    let pw = random_password(8);
    let creds = Credentials::from_password(&pw);
    (pw, creds)
}

pub fn config_path() -> PathBuf {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| app_dir(&dirs::config_dir().unwrap_or_else(|| PathBuf::from("."))))
        .join("host.json")
}

/// `<config>/Veyndesk`. Installs from before the rename kept everything (ID, settings, server data)
/// in `<config>/Veyndesk`; that folder is moved over once so the ID survives. If the move fails
/// (e.g. a file is still in use), the old folder keeps being used.
fn app_dir(base: &std::path::Path) -> PathBuf {
    let new = base.join("Veyndesk");
    let old = base.join("VeloDesk"); // name before the rename
    if !new.exists() && old.is_dir() {
        if std::fs::rename(&old, &new).is_err() {
            return old;
        }
    }
    new
}

impl HostConfig {
    pub fn load_or_default() -> Self {
        if let Ok(s) = std::fs::read_to_string(config_path()) {
            if let Ok(c) = serde_json::from_str::<HostConfig>(&s) {
                return c;
            }
        }
        HostConfig {
            server: String::new(),
            id: None,
            key: random_token(32),
            name: hostname(),
            permanent: None,
            ask_to_accept: true,
            run_server: false,
            internet: false,
            team_code: None,
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let p = config_path();
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(p, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    /// `https://x` → `wss://x/ws/host`, `http://x` → `ws://x/ws/host`.
    pub fn ws_url(&self) -> String {
        let s = self.server.trim().trim_end_matches('/');
        let s = if let Some(r) = s.strip_prefix("https://") {
            format!("wss://{r}")
        } else if let Some(r) = s.strip_prefix("http://") {
            format!("ws://{r}")
        } else if s.starts_with("ws://") || s.starts_with("wss://") {
            s.to_string()
        } else {
            format!("ws://{s}")
        };
        if s.ends_with("/ws/host") { s } else { format!("{s}/ws/host") }
    }
}

pub fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok().map(|s| s.trim().to_string()))
        .unwrap_or_else(|| "Veyndesk PC".into())
}

pub fn os_name() -> String {
    format!("{} {}", std::env::consts::OS, std::env::consts::ARCH)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn urls() {
        let mut c = HostConfig::load_or_default();
        c.server = "https://desk.example.com/".into();
        assert_eq!(c.ws_url(), "wss://desk.example.com/ws/host");
        c.server = "192.168.1.5:8080".into();
        assert_eq!(c.ws_url(), "ws://192.168.1.5:8080/ws/host");
    }

    #[test]
    fn migrates_old_folder() {
        let base = std::env::temp_dir().join(format!("vd-mig-{}", random_token(8)));
        std::fs::create_dir_all(base.join("VeloDesk")).unwrap();
        std::fs::write(base.join("VeloDesk").join("host.json"), "{}").unwrap();
        let dir = app_dir(&base);
        assert_eq!(dir, base.join("Veyndesk"));
        assert!(dir.join("host.json").exists());
        assert!(!base.join("VeloDesk").exists());
        // Second run: nothing to move.
        assert_eq!(app_dir(&base), base.join("Veyndesk"));
        // Fresh install: no old folder.
        let fresh = base.join("fresh");
        assert_eq!(app_dir(&fresh), fresh.join("Veyndesk"));
        std::fs::remove_dir_all(&base).ok();
    }
}
