use axum::extract::ws::Message;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    sync::Mutex,
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::mpsc::UnboundedSender;
use veyndesk_protocol::{random_token, sha256_hex};

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Persisted knowledge about a device (address book).
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct KnownHost {
    pub key_hash: String,
    pub name: String,
    pub os: String,
    pub version: String,
    pub last_ip: String,
    pub last_seen: u64,
    #[serde(default)]
    pub note: String,
}

#[allow(dead_code)]
pub struct OnlineHost {
    pub tx: UnboundedSender<Message>,
    pub conn_id: u64,
    pub since: u64,
    pub sessions: HashSet<u32>,
}

pub struct Session {
    pub host_id: String,
    pub viewer_tx: UnboundedSender<Message>,
    pub active: bool,
    pub viewer_ip: String,
    pub started: u64,
}

#[derive(Default)]
struct Live {
    hosts: HashMap<String, OnlineHost>,
    sessions: HashMap<u32, Session>,
    next_sid: u32,
    next_conn: u64,
    rate: HashMap<String, (Instant, u32)>,
}

pub struct App {
    live: Mutex<Live>,
    known: Mutex<HashMap<String, KnownHost>>,
    audit_file: Mutex<fs::File>,
    data_dir: PathBuf,
    pub admin_token: String,
    pub trust_proxy: bool,
}

#[derive(Serialize)]
pub struct HostView {
    pub id: String,
    pub name: String,
    pub os: String,
    pub version: String,
    pub online: bool,
    pub last_ip: String,
    pub last_seen: u64,
    pub sessions: usize,
    pub note: String,
}

#[derive(Serialize)]
pub struct SessionView {
    pub sid: u32,
    pub host_id: String,
    pub viewer_ip: String,
    pub active: bool,
    pub started: u64,
}

#[derive(Serialize, Deserialize)]
pub struct AuditEntry {
    pub ts: u64,
    pub event: String,
    pub host_id: Option<String>,
    pub sid: Option<u32>,
    pub ip: String,
    pub detail: String,
}

impl App {
    pub fn load(data_dir: PathBuf) -> anyhow::Result<Self> {
        fs::create_dir_all(&data_dir)?;
        let hosts_path = data_dir.join("hosts.json");
        let known = match fs::read_to_string(&hosts_path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
            Err(_) => HashMap::new(),
        };
        let audit_file = OpenOptions::new().create(true).append(true).open(data_dir.join("audit.log"))?;

        let admin_token = match std::env::var("VEYNDESK_ADMIN_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => {
                let p = data_dir.join("admin_token");
                match fs::read_to_string(&p) {
                    Ok(t) if !t.trim().is_empty() => t.trim().to_string(),
                    _ => {
                        let t = random_token(16);
                        fs::write(&p, &t)?;
                        t
                    }
                }
            }
        };
        Ok(Self {
            live: Mutex::new(Live { next_sid: 1, ..Default::default() }),
            known: Mutex::new(known),
            audit_file: Mutex::new(audit_file),
            data_dir,
            admin_token,
            trust_proxy: std::env::var("VEYNDESK_TRUST_PROXY").map(|v| v == "1").unwrap_or(false),
        })
    }

    fn save_known(&self, known: &HashMap<String, KnownHost>) {
        let path = self.data_dir.join("hosts.json");
        let tmp = self.data_dir.join("hosts.json.tmp");
        if let Ok(s) = serde_json::to_string_pretty(known) {
            if fs::write(&tmp, s).is_ok() {
                let _ = fs::rename(tmp, path);
            }
        }
    }

    // ---------------------------------------------------------- hosts

    #[allow(clippy::too_many_arguments)]
    pub fn register_host(
        &self,
        requested: Option<String>,
        key: &str,
        name: &str,
        os: &str,
        version: &str,
        ip: &str,
        tx: UnboundedSender<Message>,
    ) -> Result<(String, u64), String> {
        if key.len() < 32 {
            return Err("device key too short".into());
        }
        let key_hash = sha256_hex(key);
        let mut known = self.known.lock().unwrap();

        let id = match requested.filter(|s| !s.is_empty()) {
            Some(id) => {
                if !(6..=12).contains(&id.len()) || !id.chars().all(|c| c.is_ascii_digit()) {
                    return Err("invalid id format".into());
                }
                match known.get(&id) {
                    Some(k) if k.key_hash != key_hash => {
                        return Err("This ID belongs to another device. Delete your config to get a new ID.".into())
                    }
                    _ => id,
                }
            }
            None => loop {
                use rand::Rng;
                let candidate = rand::rng().random_range(100_000_000u32..999_999_999).to_string();
                if !known.contains_key(&candidate) {
                    break candidate;
                }
            },
        };

        let entry = known.entry(id.clone()).or_default();
        entry.key_hash = key_hash;
        entry.name = name.chars().take(64).collect();
        entry.os = os.chars().take(64).collect();
        entry.version = version.chars().take(32).collect();
        entry.last_ip = ip.to_string();
        entry.last_seen = now_secs();
        self.save_known(&known);
        drop(known);

        let mut live = self.live.lock().unwrap();
        live.next_conn += 1;
        let conn_id = live.next_conn;
        // A reconnecting host replaces its stale connection; drop its old sessions.
        if let Some(old) = live.hosts.remove(&id) {
            for sid in old.sessions {
                if let Some(s) = live.sessions.remove(&sid) {
                    let _ = s.viewer_tx.send(Message::Close(None));
                }
            }
            let _ = old.tx.send(Message::Close(None));
        }
        live.hosts.insert(id.clone(), OnlineHost { tx, conn_id, since: now_secs(), sessions: HashSet::new() });
        Ok((id, conn_id))
    }

    /// Returns the sessions that were closed.
    pub fn unregister_host(&self, id: &str, conn_id: u64) -> Vec<u32> {
        let mut live = self.live.lock().unwrap();
        let mut closed = vec![];
        if live.hosts.get(id).map(|h| h.conn_id) == Some(conn_id) {
            let h = live.hosts.remove(id).unwrap();
            for sid in h.sessions {
                if let Some(s) = live.sessions.remove(&sid) {
                    let msg = serde_json::to_string(&veyndesk_protocol::ServerToViewer::Closed {
                        reason: "Partner went offline".into(),
                    })
                    .unwrap();
                    let _ = s.viewer_tx.send(Message::Text(msg.into()));
                    let _ = s.viewer_tx.send(Message::Close(None));
                    closed.push(sid);
                }
            }
        }
        drop(live);
        if let Some(k) = self.known.lock().unwrap().get_mut(id) {
            k.last_seen = now_secs();
        }
        closed
    }

    pub fn host_tx(&self, id: &str) -> Option<UnboundedSender<Message>> {
        self.live.lock().unwrap().hosts.get(id).map(|h| h.tx.clone())
    }

    pub fn set_note(&self, id: &str, note: &str) -> bool {
        let mut known = self.known.lock().unwrap();
        match known.get_mut(id) {
            Some(k) => {
                k.note = note.chars().take(200).collect();
                self.save_known(&known);
                true
            }
            None => false,
        }
    }

    // ---------------------------------------------------------- sessions

    pub fn new_session(&self, s: Session) -> u32 {
        let mut live = self.live.lock().unwrap();
        let sid = live.next_sid;
        live.next_sid = live.next_sid.wrapping_add(1).max(1);
        if let Some(h) = live.hosts.get_mut(&s.host_id) {
            h.sessions.insert(sid);
        }
        live.sessions.insert(sid, s);
        sid
    }

    pub fn viewer_tx(&self, sid: u32, host_id: &str) -> Option<UnboundedSender<Message>> {
        let live = self.live.lock().unwrap();
        live.sessions.get(&sid).filter(|s| s.host_id == host_id).map(|s| s.viewer_tx.clone())
    }

    pub fn active_viewer_tx(&self, sid: u32, host_id: &str) -> Option<UnboundedSender<Message>> {
        let live = self.live.lock().unwrap();
        live.sessions
            .get(&sid)
            .filter(|s| s.host_id == host_id && s.active)
            .map(|s| s.viewer_tx.clone())
    }

    pub fn is_active(&self, sid: u32) -> bool {
        self.live.lock().unwrap().sessions.get(&sid).map(|s| s.active).unwrap_or(false)
    }

    pub fn activate(&self, sid: u32, host_id: &str) -> Option<(UnboundedSender<Message>, String)> {
        let mut live = self.live.lock().unwrap();
        let s = live.sessions.get_mut(&sid).filter(|s| s.host_id == host_id)?;
        s.active = true;
        s.started = now_secs();
        Some((s.viewer_tx.clone(), s.viewer_ip.clone()))
    }

    pub fn remove_session(&self, sid: u32, host_id: Option<&str>) -> Option<Session> {
        let mut live = self.live.lock().unwrap();
        if let Some(h) = host_id {
            if live.sessions.get(&sid).map(|s| s.host_id.as_str()) != Some(h) {
                return None;
            }
        }
        let s = live.sessions.remove(&sid)?;
        if let Some(h) = live.hosts.get_mut(&s.host_id) {
            h.sessions.remove(&sid);
        }
        Some(s)
    }

    /// Max 20 connection attempts per IP per minute.
    pub fn rate_ok(&self, ip: &str) -> bool {
        let mut live = self.live.lock().unwrap();
        let now = Instant::now();
        live.rate.retain(|_, (t, _)| now.duration_since(*t).as_secs() < 60);
        let e = live.rate.entry(ip.to_string()).or_insert((now, 0));
        e.1 += 1;
        e.1 <= 20
    }

    // ---------------------------------------------------------- admin views

    pub fn host_list(&self) -> Vec<HostView> {
        let live = self.live.lock().unwrap();
        let known = self.known.lock().unwrap();
        let mut v: Vec<HostView> = known
            .iter()
            .map(|(id, k)| {
                let online = live.hosts.get(id);
                HostView {
                    id: id.clone(),
                    name: k.name.clone(),
                    os: k.os.clone(),
                    version: k.version.clone(),
                    online: online.is_some(),
                    last_ip: k.last_ip.clone(),
                    last_seen: if online.is_some() { now_secs() } else { k.last_seen },
                    sessions: online
                        .map(|h| h.sessions.iter().filter(|s| live.sessions.get(s).map(|x| x.active).unwrap_or(false)).count())
                        .unwrap_or(0),
                    note: k.note.clone(),
                }
            })
            .collect();
        v.sort_by(|a, b| b.online.cmp(&a.online).then(b.last_seen.cmp(&a.last_seen)));
        v
    }

    pub fn session_list(&self) -> Vec<SessionView> {
        let live = self.live.lock().unwrap();
        let mut v: Vec<SessionView> = live
            .sessions
            .iter()
            .map(|(sid, s)| SessionView {
                sid: *sid,
                host_id: s.host_id.clone(),
                viewer_ip: s.viewer_ip.clone(),
                active: s.active,
                started: s.started,
            })
            .collect();
        v.sort_by_key(|s| std::cmp::Reverse(s.started));
        v
    }

    // ---------------------------------------------------------- audit log

    pub fn audit(&self, event: &str, host_id: Option<&str>, sid: Option<u32>, ip: &str, detail: &str) {
        let e = AuditEntry {
            ts: now_secs(),
            event: event.into(),
            host_id: host_id.map(Into::into),
            sid,
            ip: ip.into(),
            detail: detail.into(),
        };
        if let Ok(line) = serde_json::to_string(&e) {
            let mut f = self.audit_file.lock().unwrap();
            let _ = writeln!(f, "{line}");
        }
    }

    pub fn audit_tail(&self, limit: usize) -> Vec<AuditEntry> {
        let Ok(f) = fs::File::open(self.data_dir.join("audit.log")) else { return vec![] };
        let lines: Vec<String> = BufReader::new(f).lines().map_while(Result::ok).collect();
        lines
            .iter()
            .rev()
            .take(limit)
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }
}
