//! Veyndesk server library: rendezvous + relay + web client + admin API.
//!
//! Env vars:
//!   VELODESK_BIND         listen address           (default 0.0.0.0:8080)
//!   VELODESK_DATA         data directory           (default ./data)
//!   VELODESK_ADMIN_TOKEN  admin API/panel token    (generated + printed if unset)
//!   VELODESK_TRUST_PROXY  "1" to trust X-Forwarded-For (when behind Caddy/nginx)

mod state;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        ConnectInfo, Path, Query, State,
    },
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures_util::{SinkExt, StreamExt};
use state::{now_secs, App, Session};
use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};
use tokio::sync::mpsc;
use tracing::{info, warn};
use velodesk_protocol::*;

const INDEX_HTML: &str = include_str!("../../../web/index.html");
const ADMIN_HTML: &str = include_str!("../../../web/admin.html");

/// Runs the server until the process exits. Used by the `velodesk-server` binary and
/// embedded in the desktop app ("run a server on this PC").
pub async fn serve(bind: &str, data_dir: std::path::PathBuf) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(bind).await?;
    serve_on(listener, data_dir).await
}

pub async fn serve_on(listener: tokio::net::TcpListener, data_dir: std::path::PathBuf) -> anyhow::Result<()> {
    let bind = listener.local_addr()?;
    let app = Arc::new(App::load(data_dir)?);

    let router = Router::new()
        .route("/", get(|| async { Html(INDEX_HTML) }))
        .route("/admin", get(|| async { Html(ADMIN_HTML) }))
        .route("/healthz", get(|| async { "ok" }))
        .route("/ws/host", get(ws_host))
        .route("/ws/viewer", get(ws_viewer))
        .route("/api/hosts", get(api_hosts))
        .route("/api/sessions", get(api_sessions))
        .route("/api/sessions/{sid}/kick", post(api_kick))
        .route("/api/hosts/{id}/note", post(api_note))
        .route("/api/audit", get(api_audit))
        .with_state(app.clone());

    info!("Veyndesk server listening on http://{bind}");
    info!("Admin panel: http://{bind}/admin  (token: {})", app.admin_token);
    axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>()).await?;
    Ok(())
}

// ------------------------------------------------------------------ helpers

fn client_ip(app: &App, headers: &HeaderMap, addr: SocketAddr) -> String {
    if app.trust_proxy {
        if let Some(v) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
            if let Some(first) = v.split(',').next() {
                return first.trim().to_string();
            }
        }
    }
    addr.ip().to_string()
}

fn json_msg<T: serde::Serialize>(v: &T) -> Message {
    Message::Text(serde_json::to_string(v).expect("serializable").into())
}

/// Spawns a task that drains `rx` into the socket sink (plus keepalive pings),
/// and returns the stream half for reading.
fn split_socket(
    socket: WebSocket,
) -> (mpsc::UnboundedSender<Message>, futures_util::stream::SplitStream<WebSocket>) {
    let (mut sink, stream) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    tokio::spawn(async move {
        let mut ping = tokio::time::interval(Duration::from_secs(25));
        ping.tick().await;
        loop {
            tokio::select! {
                m = rx.recv() => match m {
                    Some(m) => {
                        let is_close = matches!(m, Message::Close(_));
                        if sink.send(m).await.is_err() || is_close { break; }
                    }
                    None => break,
                },
                _ = ping.tick() => {
                    if sink.send(Message::Ping(Default::default())).await.is_err() { break; }
                }
            }
        }
        let _ = sink.close().await;
    });
    (tx, stream)
}

async fn next_text(
    stream: &mut futures_util::stream::SplitStream<WebSocket>,
    timeout: Duration,
) -> Option<String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match tokio::time::timeout_at(deadline, stream.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => return Some(t.to_string()),
            Ok(Some(Ok(Message::Close(_)))) | Ok(Some(Err(_))) | Ok(None) | Err(_) => return None,
            Ok(Some(Ok(_))) => continue,
        }
    }
}

// ------------------------------------------------------------------ host socket

async fn ws_host(
    ws: WebSocketUpgrade,
    State(app): State<Arc<App>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let ip = client_ip(&app, &headers, addr);
    ws.max_message_size(32 << 20)
        .on_upgrade(move |s| handle_host(s, app, ip))
}

async fn handle_host(socket: WebSocket, app: Arc<App>, ip: String) {
    let (tx, mut stream) = split_socket(socket);

    let Some(first) = next_text(&mut stream, Duration::from_secs(10)).await else { return };
    let Ok(HostToServer::Register { id, key, name, os, version }) = serde_json::from_str(&first) else {
        let _ = tx.send(json_msg(&ServerToHost::Error { message: "expected register".into() }));
        return;
    };

    let (host_id, conn_id) = match app.register_host(id, &key, &name, &os, &version, &ip, tx.clone()) {
        Ok(v) => v,
        Err(e) => {
            let _ = tx.send(json_msg(&ServerToHost::Error { message: e }));
            let _ = tx.send(Message::Close(None));
            return;
        }
    };
    let _ = tx.send(json_msg(&ServerToHost::Registered { id: host_id.clone() }));
    info!(%host_id, %ip, %name, "host online");
    app.audit("host_online", Some(&host_id), None, &ip, &format!("{name} ({os}) v{version}"));

    while let Some(Ok(msg)) = stream.next().await {
        match msg {
            Message::Binary(buf) => {
                if buf.len() < 5 {
                    continue;
                }
                let sid = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
                if let Some(vtx) = app.active_viewer_tx(sid, &host_id) {
                    let _ = vtx.send(Message::Binary(buf.slice(4..)));
                }
            }
            Message::Text(t) => {
                let Ok(m) = serde_json::from_str::<HostToServer>(&t) else {
                    warn!(%host_id, "bad host message");
                    continue;
                };
                handle_host_msg(&app, &host_id, &tx, m);
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    for sid in app.unregister_host(&host_id, conn_id) {
        app.audit("session_end", Some(&host_id), Some(sid), "", "host went offline");
    }
    info!(%host_id, "host offline");
    app.audit("host_offline", Some(&host_id), None, &ip, "");
}

fn handle_host_msg(app: &App, host_id: &str, tx: &mpsc::UnboundedSender<Message>, m: HostToServer) {
    match m {
        HostToServer::Register { .. } => {}
        HostToServer::Ping => {
            let _ = tx.send(json_msg(&ServerToHost::Pong));
        }
        HostToServer::Challenge { sid, salt, nonce } => {
            if let Some(v) = app.viewer_tx(sid, host_id) {
                let _ = v.send(json_msg(&ServerToViewer::Challenge { salt, nonce }));
            }
        }
        HostToServer::Accept { sid, width, height, name, os } => {
            if let Some((v, vip)) = app.activate(sid, host_id) {
                let _ = v.send(json_msg(&ServerToViewer::Accepted { width, height, name, os }));
                app.audit("session_start", Some(host_id), Some(sid), &vip, "password accepted");
            }
        }
        HostToServer::Reject { sid, reason } => {
            if let Some(s) = app.remove_session(sid, Some(host_id)) {
                let _ = s.viewer_tx.send(json_msg(&ServerToViewer::Rejected { reason: reason.clone() }));
                let _ = s.viewer_tx.send(Message::Close(None));
                app.audit("auth_failed", Some(host_id), Some(sid), &s.viewer_ip, &reason);
            }
        }
        HostToServer::ToViewer { sid, msg } => {
            if let Some(v) = app.active_viewer_tx(sid, host_id) {
                let _ = v.send(json_msg(&ServerToViewer::Host { msg }));
            }
        }
        HostToServer::Close { sid, reason } => {
            if let Some(s) = app.remove_session(sid, Some(host_id)) {
                let _ = s.viewer_tx.send(json_msg(&ServerToViewer::Closed { reason: reason.clone() }));
                app.audit("session_end", Some(host_id), Some(sid), &s.viewer_ip, &reason);
            }
        }
    }
}

// ------------------------------------------------------------------ viewer socket

async fn ws_viewer(
    ws: WebSocketUpgrade,
    State(app): State<Arc<App>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let ip = client_ip(&app, &headers, addr);
    ws.max_message_size(1 << 20).on_upgrade(move |s| handle_viewer(s, app, ip))
}

async fn handle_viewer(socket: WebSocket, app: Arc<App>, ip: String) {
    let (tx, mut stream) = split_socket(socket);
    let fail = |msg: &str| {
        let _ = tx.send(json_msg(&ServerToViewer::Error { message: msg.into() }));
        let _ = tx.send(Message::Close(None));
    };

    let Some(first) = next_text(&mut stream, Duration::from_secs(15)).await else { return };
    let Ok(ViewerControl::Connect { id }) = serde_json::from_str(&first) else {
        return fail("expected connect");
    };
    let id: String = id.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    if !app.rate_ok(&ip) {
        return fail("Too many connection attempts. Wait a minute and try again.");
    }
    let Some(host_tx) = app.host_tx(&id) else {
        return fail("Partner not found on this server. Check the ID, and that the partner's Veyndesk is open and shows Ready on the same network or server.");
    };

    let sid = app.new_session(Session {
        host_id: id.clone(),
        viewer_tx: tx.clone(),
        active: false,
        viewer_ip: ip.clone(),
        started: now_secs(),
    });
    let _ = host_tx.send(json_msg(&ServerToHost::Incoming { sid, ip: ip.clone() }));
    app.audit("connect_attempt", Some(&id), Some(sid), &ip, "");

    // Pending sessions must authenticate within 60 s.
    let auth_deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let next = if app.is_active(sid) {
            stream.next().await
        } else {
            match tokio::time::timeout_at(auth_deadline, stream.next()).await {
                Ok(n) => n,
                Err(_) => {
                    let _ = tx.send(json_msg(&ServerToViewer::Error { message: "Authentication timed out".into() }));
                    break;
                }
            }
        };
        let Some(Ok(msg)) = next else { break };
        let Message::Text(t) = msg else {
            if matches!(msg, Message::Close(_)) { break; }
            continue;
        };
        let Some(host_tx) = app.host_tx(&id) else { break };
        if app.is_active(sid) {
            if let Ok(input) = serde_json::from_str::<ViewerInput>(&t) {
                let _ = host_tx.send(json_msg(&ServerToHost::FromViewer { sid, msg: input }));
            }
        } else if let Ok(ViewerControl::Auth { proof }) = serde_json::from_str(&t) {
            let _ = host_tx.send(json_msg(&ServerToHost::Auth { sid, proof }));
        }
    }

    if let Some(s) = app.remove_session(sid, None) {
        if let Some(h) = app.host_tx(&s.host_id) {
            let _ = h.send(json_msg(&ServerToHost::ViewerLeft { sid }));
        }
        app.audit("session_end", Some(&s.host_id), Some(sid), &ip, "viewer disconnected");
    }
}

// ------------------------------------------------------------------ admin API

fn authorized(app: &App, headers: &HeaderMap, q: &HashMap<String, String>) -> bool {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let token = bearer.or(q.get("token").map(|s| s.as_str())).unwrap_or("");
    ct_eq(token, &app.admin_token)
}

macro_rules! guard {
    ($app:expr, $h:expr, $q:expr) => {
        if !authorized(&$app, &$h, &$q) {
            return (StatusCode::UNAUTHORIZED, "invalid admin token").into_response();
        }
    };
}

async fn api_hosts(State(app): State<Arc<App>>, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    guard!(app, headers, q);
    Json(app.host_list()).into_response()
}

async fn api_sessions(State(app): State<Arc<App>>, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    guard!(app, headers, q);
    Json(app.session_list()).into_response()
}

async fn api_kick(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
    Path(sid): Path<u32>,
) -> Response {
    guard!(app, headers, q);
    match app.remove_session(sid, None) {
        Some(s) => {
            let _ = s.viewer_tx.send(json_msg(&ServerToViewer::Closed { reason: "Disconnected by administrator".into() }));
            let _ = s.viewer_tx.send(Message::Close(None));
            if let Some(h) = app.host_tx(&s.host_id) {
                let _ = h.send(json_msg(&ServerToHost::ViewerLeft { sid }));
            }
            app.audit("session_kicked", Some(&s.host_id), Some(sid), &s.viewer_ip, "by admin");
            StatusCode::NO_CONTENT.into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[derive(serde::Deserialize)]
struct NoteBody {
    note: String,
}

async fn api_note(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
    Path(id): Path<String>,
    Json(body): Json<NoteBody>,
) -> Response {
    guard!(app, headers, q);
    if app.set_note(&id, &body.note) {
        StatusCode::NO_CONTENT.into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

async fn api_audit(State(app): State<Arc<App>>, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    guard!(app, headers, q);
    let limit = q.get("limit").and_then(|v| v.parse().ok()).unwrap_or(300usize).min(5000);
    Json(app.audit_tail(limit)).into_response()
}
