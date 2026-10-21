// Hide the console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Veyndesk desktop app — one program, like AnyDesk:
//! * shows this PC's ID + password and lets others control it (accept / deny prompt),
//! * opens remote screens of other PCs,
//! * optionally runs a Veyndesk server on this PC so a whole office/LAN needs nothing else.

mod discovery;
mod internet;

use serde::Serialize;
use std::sync::{Arc, Mutex};
use tauri::{Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use velodesk_host::{
    config::{config_path, one_time_password, Credentials, HostConfig},
    AuthConfig, HostCommand, UiEvent,
};

/// Port used when this PC runs the embedded server.
const LAN_PORT: u16 = 21118;

struct AppState {
    cfg: Arc<Mutex<HostConfig>>,
    auth: Arc<Mutex<AuthConfig>>,
    otp: Mutex<String>,
    id: Arc<Mutex<Option<String>>>,
    status: Arc<Mutex<String>>,
    cmd: UnboundedSender<HostCommand>,
    /// `Some(error)` if starting the embedded server failed; `None` while not attempted.
    server_state: Arc<Mutex<ServerState>>,
    net: Arc<internet::Internet>,
}

#[derive(Clone, Serialize, Default)]
#[serde(tag = "state", rename_all = "snake_case")]
enum ServerState {
    #[default]
    Off,
    Running { lan_url: String },
    Failed { error: String },
}

#[derive(Serialize)]
struct Info {
    id: Option<String>,
    password: String,
    server: String,
    name: String,
    permanent: bool,
    ask_to_accept: bool,
    run_server: bool,
    server_state: ServerState,
    status: String,
    lan_ip: String,
    internet: bool,
    internet_state: internet::NetState,
    team_code: Option<String>,
    version: &'static str,
}

fn rebuild_auth(state: &AppState) {
    let cfg = state.cfg.lock().unwrap().clone();
    let otp = state.otp.lock().unwrap().clone();
    let mut a = AuthConfig::build(&cfg, Some(&otp));
    a.ask_on_one_time = cfg.ask_to_accept;
    *state.auth.lock().unwrap() = a;
}

fn http_base(server: &str) -> String {
    let s = server.trim().trim_end_matches('/');
    let s = s.strip_suffix("/ws/host").unwrap_or(s);
    if let Some(r) = s.strip_prefix("wss://") {
        format!("https://{r}")
    } else if let Some(r) = s.strip_prefix("ws://") {
        format!("http://{r}")
    } else if s.starts_with("http://") || s.starts_with("https://") {
        s.to_string()
    } else {
        format!("http://{s}")
    }
}

/// This PC's LAN address (no packets are sent; UDP connect only picks a route).
fn lan_ip() -> String {
    std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("8.8.8.8:80")?;
            s.local_addr()
        })
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "127.0.0.1".into())
}

/// Starts the embedded server (idempotent). Returns the state after the attempt.
fn start_embedded_server(server_state: &Arc<Mutex<ServerState>>) -> ServerState {
    if let ServerState::Running { .. } = &*server_state.lock().unwrap() {
        return server_state.lock().unwrap().clone();
    }
    let new_state = match std::net::TcpListener::bind(("0.0.0.0", LAN_PORT)) {
        Ok(listener) => {
            let _ = listener.set_nonblocking(true);
            let data = config_path().parent().map(|p| p.join("server")).unwrap_or_else(|| "server-data".into());
            let st = server_state.clone();
            tauri::async_runtime::spawn(async move {
                let result = match tokio::net::TcpListener::from_std(listener) {
                    Ok(l) => velodesk_server::serve_on(l, data).await,
                    Err(e) => Err(e.into()),
                };
                if let Err(e) = result {
                    *st.lock().unwrap() = ServerState::Failed { error: e.to_string() };
                }
            });
            ServerState::Running { lan_url: format!("http://{}:{LAN_PORT}", lan_ip()) }
        }
        Err(e) => ServerState::Failed { error: format!("port {LAN_PORT} is busy: {e}") },
    };
    *server_state.lock().unwrap() = new_state.clone();
    new_state
}

// ------------------------------------------------------------------ commands

#[tauri::command]
fn get_info(state: State<'_, AppState>) -> Info {
    let cfg = state.cfg.lock().unwrap();
    Info {
        id: state.id.lock().unwrap().clone().or_else(|| cfg.id.clone()),
        password: state.otp.lock().unwrap().clone(),
        server: cfg.server.clone(),
        name: cfg.name.clone(),
        permanent: cfg.permanent.is_some(),
        ask_to_accept: cfg.ask_to_accept,
        run_server: cfg.run_server,
        server_state: state.server_state.lock().unwrap().clone(),
        status: state.status.lock().unwrap().clone(),
        lan_ip: lan_ip(),
        internet: cfg.internet,
        internet_state: state.net.state.lock().unwrap().clone(),
        team_code: cfg.team_code.clone(),
        version: env!("CARGO_PKG_VERSION"),
    }
}

/// Server PC: make the built-in server reachable from any network (free Cloudflare tunnel).
#[tauri::command]
fn enable_internet(state: State<'_, AppState>, enable: bool) -> Result<Option<String>, String> {
    let code = {
        let mut cfg = state.cfg.lock().unwrap();
        cfg.internet = enable;
        if enable {
            cfg.run_server = true;
            if !matches!(*state.server_state.lock().unwrap(), ServerState::Running { .. }) {
                cfg.server = format!("http://127.0.0.1:{LAN_PORT}");
            }
            if cfg.team_code.is_none() {
                cfg.team_code = Some(internet::new_code());
            }
        }
        cfg.save().map_err(|e| e.to_string())?;
        cfg.team_code.clone()
    };
    if enable {
        if let ServerState::Failed { error } = start_embedded_server(&state.server_state) {
            return Err(error);
        }
        let _ = state.cmd.send(HostCommand::Reconnect);
        state.net.set_code(code.clone());
        state.net.start(LAN_PORT);
    } else {
        // Keep the tunnel itself: it is also the automatic relay for direct connections.
        state.net.set_code(None);
    }
    Ok(code)
}

/// Other PCs: join the team's server from anywhere using the team code.
#[tauri::command]
async fn join_team(state: State<'_, AppState>, code: String) -> Result<String, String> {
    let code = internet::normalize_code(&code).ok_or("A team code looks like ABCD-2345 (8 letters/digits).")?;
    let c2 = code.clone();
    let url = tauri::async_runtime::spawn_blocking(move || internet::resolve(&c2))
        .await
        .map_err(|e| e.to_string())??;
    {
        let mut cfg = state.cfg.lock().unwrap();
        cfg.team_code = Some(code);
        cfg.server = url.clone();
        cfg.save().map_err(|e| e.to_string())?;
    }
    let _ = state.cmd.send(HostCommand::Reconnect);
    Ok(url)
}

#[tauri::command]
fn new_password(state: State<'_, AppState>) -> String {
    let (pw, _) = one_time_password();
    *state.otp.lock().unwrap() = pw.clone();
    rebuild_auth(&state);
    pw
}

#[tauri::command]
fn set_server(state: State<'_, AppState>, server: String, name: String) -> Result<(), String> {
    {
        let mut cfg = state.cfg.lock().unwrap();
        cfg.server = server.trim().to_string();
        if !name.trim().is_empty() {
            cfg.name = name.trim().to_string();
        }
        cfg.save().map_err(|e| e.to_string())?;
    }
    let _ = state.cmd.send(HostCommand::Reconnect);
    Ok(())
}

/// Turn this PC into the server for the local network.
#[tauri::command]
fn enable_lan_server(state: State<'_, AppState>, enable: bool) -> Result<ServerState, String> {
    {
        let mut cfg = state.cfg.lock().unwrap();
        cfg.run_server = enable;
        if enable {
            cfg.server = format!("http://127.0.0.1:{LAN_PORT}");
        }
        cfg.save().map_err(|e| e.to_string())?;
    }
    if !enable {
        // Keeps running until the app restarts (sessions may depend on it).
        return Ok(state.server_state.lock().unwrap().clone());
    }
    let st = start_embedded_server(&state.server_state);
    let _ = state.cmd.send(HostCommand::Reconnect);
    Ok(st)
}

#[tauri::command]
fn set_options(state: State<'_, AppState>, ask_to_accept: bool) -> Result<(), String> {
    {
        let mut cfg = state.cfg.lock().unwrap();
        cfg.ask_to_accept = ask_to_accept;
        cfg.save().map_err(|e| e.to_string())?;
    }
    rebuild_auth(&state);
    Ok(())
}

#[tauri::command]
fn set_permanent_password(state: State<'_, AppState>, password: Option<String>) -> Result<(), String> {
    {
        let mut cfg = state.cfg.lock().unwrap();
        match password {
            Some(p) if p.len() >= 8 => cfg.permanent = Some(Credentials::from_password(&p)),
            Some(_) => return Err("Use at least 8 characters.".into()),
            None => cfg.permanent = None,
        }
        cfg.save().map_err(|e| e.to_string())?;
    }
    rebuild_auth(&state);
    Ok(())
}

#[tauri::command]
fn respond(state: State<'_, AppState>, sid: u32, accept: bool) {
    let _ = state.cmd.send(HostCommand::Respond { sid, accept });
}

#[tauri::command]
fn disconnect_all(state: State<'_, AppState>) {
    let _ = state.cmd.send(HostCommand::DisconnectAll);
}

/// Opens the remote screen of partner `id` in a new window.
///
/// Must be `async`: creating a window from a synchronous command deadlocks on Windows
/// (the window stays white). The viewer page is bundled with the app, so a firewall or
/// wrong address shows a readable error instead of a blank window.
/// LAN servers found by broadcast (for first-run setup).
#[tauri::command]
async fn discover_servers() -> Vec<discovery::Answer> {
    discovery::find_servers().await
}

/// Opens the remote screen of a partner, given an ID or an IP address, in a new window.
///
/// * IP address → asks that PC directly (UDP 21119) for its ID and server.
/// * ID → first asks the LAN who has it (so IDs work even if PCs use different servers),
///   then falls back to the configured server (internet / VPS server).
///
/// Must be `async`: creating a window from a synchronous command deadlocks on Windows.
#[tauri::command]
async fn open_viewer(app: tauri::AppHandle, state: State<'_, AppState>, id: String) -> Result<(), String> {
    let raw = id.trim().to_string();
    let configured = state.cfg.lock().unwrap().server.clone();
    let (server, id) = if raw.contains('.') {
        let host = raw.split(':').next().unwrap_or("").trim();
        let ip: std::net::IpAddr = host.parse().map_err(|_| format!("'{raw}' is not a valid IP address or ID"))?;
        let a = discovery::probe_ip(ip).await.ok_or_else(|| {
            format!("No Veyndesk answered at {ip}. Make sure Veyndesk is open there and allowed in Windows Firewall.")
        })?;
        let id = a.id.ok_or("That PC is not ready yet (no ID). Try again in a few seconds.")?;
        (a.server, id)
    } else {
        let id: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
        if id.len() < 6 {
            return Err("Enter a remote ID (9 digits) or an IP address".into());
        }
        // Fallback server if a direct connection is impossible: the partner's LAN server,
        // else our team server (a loopback server only knows this PC, so it's no fallback).
        let fallback = match discovery::find_id(&id).await {
            Some(a) => a.server,
            None if !is_loopback(&configured) => http_base(&configured),
            None => String::new(),
        };
        (fallback, id)
    };
    let p2p = !raw.contains('.');
    let server = if server.is_empty() { String::new() } else { http_base(&server) };
    let init = format!(
        "window.__VELODESK__ = {};",
        serde_json::json!({ "server": if server.is_empty() { serde_json::Value::Null } else { server.into() }, "id": id, "embedded": true, "p2p": p2p })
    );
    let label = format!("remote-{id}-{}", app.webview_windows().len());
    WebviewWindowBuilder::new(&app, label, WebviewUrl::App("viewer.html".into()))
        .initialization_script(&init)
        .title(format!("Veyndesk — {id}"))
        .inner_size(1280.0, 800.0)
        .build()
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn is_loopback(server: &str) -> bool {
    server.trim().is_empty() || server.contains("127.0.0.1") || server.contains("localhost")
}

fn viewer_ws(server: &str) -> String {
    let mut c = HostConfig::load_or_default();
    c.server = server.to_string();
    c.ws_url().replace("/ws/host", "/ws/viewer")
}

/// Viewer window: deliver its WebRTC offer to partner `id` (via public signaling) and return the answer.
#[tauri::command]
async fn p2p_signal(id: String, sdp: String) -> Result<serde_json::Value, String> {
    let (sdp, relay) = velodesk_host::p2p::request_answer(&id, sdp).await?;
    Ok(serde_json::json!({ "sdp": sdp, "relay": relay }))
}

fn main() {
    tracing_subscriber::fmt::init();
    velodesk_host::init_platform();

    let mut cfg = HostConfig::load_or_default();
    // Zero setup: every PC runs its own small server and is reachable peer-to-peer by ID.
    if cfg.server.trim().is_empty() {
        cfg.server = format!("http://127.0.0.1:{LAN_PORT}");
        cfg.run_server = true;
    }
    let _ = cfg.save();
    let (otp, _) = one_time_password();
    let mut auth = AuthConfig::build(&cfg, Some(&otp));
    auth.ask_on_one_time = cfg.ask_to_accept;
    let run_server = cfg.run_server;
    let auth = Arc::new(Mutex::new(auth));
    let cfg = Arc::new(Mutex::new(cfg));
    let (cmd_tx, cmd_rx) = unbounded_channel();
    let (ui_tx, mut ui_rx) = unbounded_channel::<UiEvent>();
    let id = Arc::new(Mutex::new(None));
    let status = Arc::new(Mutex::new("Starting…".to_string()));
    let server_state = Arc::new(Mutex::new(ServerState::Off));
    let lan = Arc::new(Mutex::new(discovery::LocalInfo::default()));

    let net = internet::Internet::new();
    let follow_tx = cmd_tx.clone();
    let net_exit = net.clone();
    let state = AppState {
        cfg: cfg.clone(),
        auth: auth.clone(),
        otp: Mutex::new(otp),
        id: id.clone(),
        status: status.clone(),
        cmd: cmd_tx,
        server_state: server_state.clone(),
        net: net.clone(),
    };

    tauri::Builder::default()
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            get_info,
            new_password,
            set_server,
            enable_lan_server,
            set_options,
            set_permanent_password,
            respond,
            disconnect_all,
            discover_servers,
            p2p_signal,
            enable_internet,
            join_team,
            open_viewer
        ])
        .setup(move |app| {
            if run_server {
                start_embedded_server(&server_state);
            }
            // Server PC: internet tunnel. Other PCs: follow the team's current address.
            {
                let c = cfg.lock().unwrap().clone();
                if c.run_server {
                    // Free relay for when a direct (P2P) connection is impossible.
                    if c.internet {
                        net.set_code(c.team_code.clone());
                    }
                    net.start(LAN_PORT);
                }
                // Tell the P2P listener which public relay partners can fall back to.
                let (cfg4, net4) = (cfg.clone(), net.clone());
                std::thread::spawn(move || loop {
                    let server = cfg4.lock().unwrap().server.clone();
                    let relay = if is_loopback(&server) {
                        net4.url()
                    } else if server.starts_with("https://") {
                        Some(http_base(&server))
                    } else {
                        None
                    };
                    velodesk_host::p2p::set_relay(relay);
                    std::thread::sleep(std::time::Duration::from_secs(3));
                });
                let cfg2 = cfg.clone();
                internet::spawn_republisher(net.clone(), move || cfg2.lock().unwrap().team_code.clone());
                let cfg3 = cfg.clone();
                std::thread::spawn(move || loop {
                    let (code, server, is_server) = {
                        let c = cfg3.lock().unwrap();
                        (c.team_code.clone(), c.server.clone(), c.run_server)
                    };
                    if let (Some(code), false) = (code, is_server) {
                        if let Ok(url) = internet::resolve(&code) {
                            if url != server {
                                {
                                    let mut c = cfg3.lock().unwrap();
                                    c.server = url;
                                    let _ = c.save();
                                }
                                let _ = follow_tx.send(HostCommand::Reconnect);
                            }
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_secs(60));
                });
            }
            // Keep the LAN-discovery answer up to date and serve it.
            {
                let (l2, cfg, id, st) = (lan.clone(), cfg.clone(), id.clone(), server_state.clone());
                tauri::async_runtime::spawn(async move {
                    let lan = l2;
                    loop {
                        {
                            let c = cfg.lock().unwrap();
                            let mut l = lan.lock().unwrap();
                            l.id = id.lock().unwrap().clone().or_else(|| c.id.clone());
                            l.name = c.name.clone();
                            l.server = http_base(&c.server);
                            if c.server.trim().is_empty() {
                                l.server.clear();
                            }
                            l.server_port = matches!(*st.lock().unwrap(), ServerState::Running { .. }).then_some(LAN_PORT);
                        }
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    }
                });
                tauri::async_runtime::spawn(discovery::responder(lan.clone()));
            }
            let (p2p_tx, p2p_rx) = tokio::sync::watch::channel(None);
            tauri::async_runtime::spawn(velodesk_host::p2p::serve(p2p_rx));
            let p2p_cfg = cfg.clone();
            tauri::async_runtime::spawn(velodesk_host::run(cfg, auth, ui_tx, cmd_rx, velodesk_host::default_source()));
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                while let Some(ev) = ui_rx.recv().await {
                    match &ev {
                        UiEvent::Registered { id: new_id } => {
                            *id.lock().unwrap() = Some(new_id.clone());
                            // Listen for direct (P2P) partners on our ID, bridged to the server we're on.
                            let ws = viewer_ws(&p2p_cfg.lock().unwrap().server);
                            let _ = p2p_tx.send(Some((new_id.clone(), ws)));
                            *status.lock().unwrap() = "Ready".into();
                        }
                        UiEvent::Connecting { .. } => *status.lock().unwrap() = "Connecting…".into(),
                        UiEvent::Disconnected { reason } => *status.lock().unwrap() = format!("Offline: {reason}"),
                        UiEvent::ApprovalRequest { .. } => {
                            // Pop the window to the front so the user sees the prompt.
                            if let Some(w) = handle.get_webview_window("main") {
                                let _ = w.unminimize();
                                let _ = w.show();
                                let _ = w.set_focus();
                                let _ = w.request_user_attention(Some(tauri::UserAttentionType::Critical));
                            }
                        }
                        _ => {}
                    }
                    let _ = handle.emit("host-event", &ev);
                }
            });
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.set_title(&format!("Veyndesk {}", app.package_info().version));
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while starting Veyndesk")
        .run(move |_, ev| {
            if let tauri::RunEvent::Exit = ev {
                net_exit.stop(); // don't leave cloudflared running
            }
        });
}

#[cfg(test)]
mod tests {
    #[test]
    fn http_base() {
        assert_eq!(super::http_base("ws://1.2.3.4:8080"), "http://1.2.3.4:8080");
        assert_eq!(super::http_base("wss://desk.example.com/ws/host"), "https://desk.example.com");
        assert_eq!(super::http_base("desk.local:8080/"), "http://desk.local:8080");
    }
}
