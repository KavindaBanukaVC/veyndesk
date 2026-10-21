//! Veyndesk host agent core: used by the CLI (`veyndesk-host`) and the Tauri desktop app.

pub mod config;
pub mod input;
pub mod video;
pub mod p2p;

use config::HostConfig;
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};
use veyndesk_protocol::*;
use video::{Shared, SourceFactory, VideoCmd};

pub use input::InputCmd;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Messages queued for the server by any thread.
pub enum Outgoing {
    Text(String),
    Binary(Vec<u8>),
}

/// Status updates for a UI (CLI prints them, Tauri forwards them to the webview).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UiEvent {
    Connecting { url: String },
    Registered { id: String },
    Disconnected { reason: String },
    Incoming { sid: u32, ip: String },
    SessionStarted { sid: u32, ip: String },
    SessionEnded { sid: u32 },
    AuthFailed { sid: u32, ip: String },
    /// Correct one-time password; the local user must accept or deny (AnyDesk-style).
    ApprovalRequest { sid: u32, ip: String },
    Error { message: String },
}

/// Commands a UI can send to a running host.
#[derive(Debug, Clone)]
pub enum HostCommand {
    DisconnectAll,
    /// Drop the server connection and reconnect (e.g. after the server URL changed).
    Reconnect,
    /// Answer to an [`UiEvent::ApprovalRequest`].
    Respond { sid: u32, accept: bool },
}

/// Which passwords are currently accepted. All share one salt so a single challenge works.
#[derive(Clone, Debug)]
pub struct AuthConfig {
    pub salt: String,
    /// `(secret, is_permanent)`
    pub secrets: Vec<(String, bool)>,
    /// Ask the local user to accept sessions opened with the one-time password.
    /// Permanent-password (unattended) sessions never ask.
    pub ask_on_one_time: bool,
}

impl AuthConfig {
    /// Accepts the permanent password (if set) and/or the given one-time password.
    pub fn build(cfg: &HostConfig, one_time: Option<&str>) -> Self {
        let salt = cfg.permanent.as_ref().map(|p| p.salt.clone()).unwrap_or_else(|| random_token(12));
        let mut secrets = vec![];
        if let Some(p) = &cfg.permanent {
            secrets.push((p.secret.clone(), true));
        }
        if let Some(otp) = one_time {
            secrets.push((password_secret(&salt, otp), false));
        }
        Self { salt, secrets, ask_on_one_time: false }
    }
    /// `Some(is_permanent)` if the proof matches any accepted password.
    fn verify(&self, nonce: &str, proof: &str) -> Option<bool> {
        self.secrets.iter().find(|(s, _)| ct_eq(&auth_proof(s, nonce), proof)).map(|(_, p)| *p)
    }
}

/// Call once at startup (makes Windows report real pixels so mouse coordinates match capture).
pub fn init_platform() {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

pub fn default_source() -> SourceFactory {
    if std::env::var("VEYNDESK_TEST_PATTERN").map(|v| v == "1").unwrap_or(false) {
        Box::new(|| Box::new(video::TestPattern::new()))
    } else {
        Box::new(|| Box::new(video::XcapSource::new()))
    }
}

struct Ctx {
    cfg: Arc<Mutex<HostConfig>>,
    auth: Arc<Mutex<AuthConfig>>,
    ui: UnboundedSender<UiEvent>,
    video: std::sync::mpsc::Sender<VideoCmd>,
    input: std::sync::mpsc::Sender<InputCmd>,
    shared: Arc<Shared>,
    active: Arc<Mutex<HashSet<u32>>>,
    pending: HashMap<u32, (String, String)>, // sid -> (nonce, ip)
    ips: HashMap<u32, String>,
    awaiting: HashMap<u32, (String, Instant)>, // sid -> (ip, since): waiting for local approval
    failures: Vec<Instant>,
}

/// Runs forever, reconnecting with backoff.
pub async fn run(
    cfg: Arc<Mutex<HostConfig>>,
    auth: Arc<Mutex<AuthConfig>>,
    ui: UnboundedSender<UiEvent>,
    mut commands: UnboundedReceiver<HostCommand>,
    source: SourceFactory,
) {
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Outgoing>();
    let shared = Arc::new(Shared::default());
    let active = Arc::new(Mutex::new(HashSet::new()));
    let video = video::spawn_video(source, out_tx.clone(), shared.clone());
    let input = input::spawn_input(shared.clone(), out_tx.clone(), active.clone());

    let mut ctx = Ctx {
        cfg,
        auth,
        ui,
        video,
        input,
        shared,
        active,
        pending: HashMap::new(),
        ips: HashMap::new(),
        awaiting: HashMap::new(),
        failures: vec![],
    };

    let mut backoff = 1u64;
    loop {
        if ctx.cfg.lock().unwrap().server.trim().is_empty() {
            let _ = ctx.ui.send(UiEvent::Disconnected { reason: "No server set — open Settings".into() });
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(3)) => {}
                _ = commands.recv() => {}
            }
            continue;
        }
        let url = ctx.cfg.lock().unwrap().ws_url();
        let _ = ctx.ui.send(UiEvent::Connecting { url: url.clone() });
        let reason = match tokio_tungstenite::connect_async(url.as_str()).await {
            Ok((ws, _)) => {
                backoff = 1;
                while out_rx.try_recv().is_ok() {} // drop anything queued for a dead connection
                connection(&mut ctx, ws, &mut out_rx, &mut commands).await
            }
            Err(e) => format!("cannot reach server: {e}"),
        };
        warn!("disconnected: {reason}");
        let _ = ctx.ui.send(UiEvent::Disconnected { reason });
        end_all_sessions(&mut ctx);
        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(15);
    }
}

fn end_all_sessions(ctx: &mut Ctx) {
    ctx.pending.clear();
    ctx.awaiting.clear();
    let sids: Vec<u32> = ctx.active.lock().unwrap().drain().collect();
    for sid in sids {
        end_session(ctx, sid);
    }
}

fn end_session(ctx: &mut Ctx, sid: u32) {
    ctx.active.lock().unwrap().remove(&sid);
    ctx.ips.remove(&sid);
    let _ = ctx.video.send(VideoCmd::Remove(sid));
    let _ = ctx.input.send(InputCmd::SessionEnded(sid));
    let _ = ctx.ui.send(UiEvent::SessionEnded { sid });
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn text<T: Serialize>(v: &T) -> Message {
    Message::Text(serde_json::to_string(v).unwrap().into())
}

async fn connection(
    ctx: &mut Ctx,
    ws: Ws,
    out_rx: &mut UnboundedReceiver<Outgoing>,
    commands: &mut UnboundedReceiver<HostCommand>,
) -> String {
    let (mut sink, mut stream) = ws.split();
    let register = {
        let c = ctx.cfg.lock().unwrap();
        HostToServer::Register {
            id: c.id.clone(),
            key: c.key.clone(),
            name: c.name.clone(),
            os: config::os_name(),
            version: VERSION.into(),
        }
    };
    if let Err(e) = sink.send(text(&register)).await {
        return e.to_string();
    }
    let mut ping = tokio::time::interval(Duration::from_secs(20));

    loop {
        tokio::select! {
            m = out_rx.recv() => {
                let Some(m) = m else { return "internal channel closed".into() };
                let msg = match m {
                    Outgoing::Text(t) => Message::Text(t.into()),
                    Outgoing::Binary(b) => Message::Binary(b.into()),
                };
                if let Err(e) = sink.send(msg).await { return e.to_string(); }
            }
            m = stream.next() => {
                let msg = match m {
                    Some(Ok(m)) => m,
                    Some(Err(e)) => return e.to_string(),
                    None => return "connection closed".into(),
                };
                match msg {
                    Message::Text(t) => {
                        match serde_json::from_str::<ServerToHost>(t.as_str()) {
                            Ok(m) => {
                                for reply in handle(ctx, m) {
                                    if let Err(e) = sink.send(reply).await { return e.to_string(); }
                                }
                            }
                            Err(e) => warn!("bad server message: {e}"),
                        }
                    }
                    Message::Close(_) => return "server closed connection".into(),
                    _ => {}
                }
            }
            c = commands.recv() => match c {
                Some(HostCommand::DisconnectAll) => {
                    let sids: Vec<u32> = ctx.active.lock().unwrap().iter().copied().collect();
                    for sid in sids {
                        let _ = sink.send(text(&HostToServer::Close { sid, reason: "Host ended the session".into() })).await;
                        end_session(ctx, sid);
                    }
                }
                Some(HostCommand::Respond { sid, accept }) => {
                    if let Some((ip, _)) = ctx.awaiting.remove(&sid) {
                        let replies = if accept {
                            accept_session(ctx, sid, ip)
                        } else {
                            vec![text(&HostToServer::Reject { sid, reason: "The remote user declined the connection.".into() })]
                        };
                        for r in replies {
                            if let Err(e) = sink.send(r).await { return e.to_string(); }
                        }
                    }
                }
                Some(HostCommand::Reconnect) => {
                    let _ = sink.close().await;
                    return "reconnecting".into();
                }
                None => {}
            },
            _ = ping.tick() => {
                // The server drops unauthenticated viewers after 60 s; give up before that.
                let expired: Vec<u32> = ctx.awaiting.iter()
                    .filter(|(_, (_, t))| t.elapsed() > Duration::from_secs(45)).map(|(s, _)| *s).collect();
                for sid in expired {
                    ctx.awaiting.remove(&sid);
                    let _ = ctx.ui.send(UiEvent::SessionEnded { sid });
                    let _ = sink.send(text(&HostToServer::Reject { sid, reason: "No one accepted the connection.".into() })).await;
                }
                if let Err(e) = sink.send(text(&HostToServer::Ping)).await { return e.to_string(); }
            }
        }
    }
}

fn accept_session(ctx: &mut Ctx, sid: u32, ip: String) -> Vec<Message> {
    ctx.active.lock().unwrap().insert(sid);
    ctx.ips.insert(sid, ip.clone());
    let (w, h) = *ctx.shared.size.lock().unwrap();
    let name = ctx.cfg.lock().unwrap().name.clone();
    let _ = ctx.video.send(VideoCmd::Add(sid));
    let _ = ctx.ui.send(UiEvent::SessionStarted { sid, ip });
    vec![text(&HostToServer::Accept { sid, width: w, height: h, name, os: config::os_name() })]
}

fn handle(ctx: &mut Ctx, m: ServerToHost) -> Vec<Message> {
    match m {
        ServerToHost::Registered { id } => {
            info!("registered as {id}");
            {
                let mut c = ctx.cfg.lock().unwrap();
                if c.id.as_deref() != Some(&id) {
                    c.id = Some(id.clone());
                    if let Err(e) = c.save() {
                        warn!("could not save config: {e}");
                    }
                }
            }
            let _ = ctx.ui.send(UiEvent::Registered { id });
            vec![]
        }
        ServerToHost::Error { message } => {
            let _ = ctx.ui.send(UiEvent::Error { message });
            vec![]
        }
        ServerToHost::Incoming { sid, ip } => {
            ctx.failures.retain(|t| t.elapsed() < Duration::from_secs(300));
            if ctx.failures.len() >= 8 {
                return vec![text(&HostToServer::Reject {
                    sid,
                    reason: "Too many wrong passwords. Try again in a few minutes.".into(),
                })];
            }
            let auth = ctx.auth.lock().unwrap().clone();
            if auth.secrets.is_empty() {
                return vec![text(&HostToServer::Reject { sid, reason: "Remote access is disabled on this device.".into() })];
            }
            let nonce = random_token(16);
            ctx.pending.insert(sid, (nonce.clone(), ip.clone()));
            let _ = ctx.ui.send(UiEvent::Incoming { sid, ip });
            vec![text(&HostToServer::Challenge { sid, salt: auth.salt, nonce })]
        }
        ServerToHost::Auth { sid, proof } => {
            let Some((nonce, ip)) = ctx.pending.remove(&sid) else { return vec![] };
            let (result, ask) = {
                let a = ctx.auth.lock().unwrap();
                (a.verify(&nonce, &proof), a.ask_on_one_time)
            };
            if let Some(permanent) = result {
                if !permanent && ask {
                    ctx.awaiting.insert(sid, (ip.clone(), Instant::now()));
                    let _ = ctx.ui.send(UiEvent::ApprovalRequest { sid, ip });
                    return vec![];
                }
                accept_session(ctx, sid, ip)
            } else {
                ctx.failures.push(Instant::now());
                let _ = ctx.ui.send(UiEvent::AuthFailed { sid, ip });
                vec![text(&HostToServer::Reject { sid, reason: "Wrong password".into() })]
            }
        }
        ServerToHost::FromViewer { sid, msg } => {
            if !ctx.active.lock().unwrap().contains(&sid) {
                return vec![];
            }
            match msg {
                ViewerInput::Ack { seq } => {
                    let _ = ctx.video.send(VideoCmd::Ack(sid, seq));
                }
                ViewerInput::Refresh => {
                    let _ = ctx.video.send(VideoCmd::Refresh(sid));
                }
                ViewerInput::SelectMonitor { index } => {
                    let _ = ctx.video.send(VideoCmd::SelectMonitor(index));
                }
                other => {
                    let _ = ctx.input.send(InputCmd::Event(sid, other));
                }
            }
            vec![]
        }
        ServerToHost::ViewerLeft { sid } => {
            ctx.pending.remove(&sid);
            if ctx.awaiting.remove(&sid).is_some() {
                let _ = ctx.ui.send(UiEvent::SessionEnded { sid });
            }
            if ctx.active.lock().unwrap().contains(&sid) {
                end_session(ctx, sid);
            }
            vec![]
        }
        ServerToHost::Pong => vec![],
    }
}
