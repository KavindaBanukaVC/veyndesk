//! Test harness for zero-setup P2P.
//!   VELODESK_TEST_PATTERN=1 VELODESK_SIGNAL_URL=http://127.0.0.1:8096 \
//!     cargo run -p velodesk-host --example p2p_demo -- http://127.0.0.1:8091 <password> <web/index.html>
//! Serves the viewer at http://127.0.0.1:8097/viewer.html and POST /signal/<id> for its offers.
use axum::{extract::Path, response::Html, routing::{get, post}, Router};
use std::sync::{Arc, Mutex};
use velodesk_host::{config::HostConfig, p2p, AuthConfig, UiEvent};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_env_filter("velodesk_host=debug").init();
    let a: Vec<String> = std::env::args().collect();
    let mut cfg = HostConfig::load_or_default();
    cfg.server = a[1].clone();
    cfg.permanent = None;
    let ws = format!("{}/ws/viewer", a[1].replace("http://", "ws://"));
    let auth = AuthConfig::build(&cfg, Some(&a[2]));
    let (ui_tx, mut ui_rx) = tokio::sync::mpsc::unbounded_channel();
    let (_c, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(velodesk_host::run(Arc::new(Mutex::new(cfg)), Arc::new(Mutex::new(auth)), ui_tx, cmd_rx, velodesk_host::default_source()));
    if let Ok(r) = std::env::var("VELODESK_TEST_RELAY") { p2p::set_relay(Some(r)); }
    let (t_tx, t_rx) = tokio::sync::watch::channel(None);
    tokio::spawn(p2p::serve(t_rx));
    let page = std::fs::read_to_string(&a[3]).unwrap();
    let app = Router::new()
        .route("/viewer.html", get(move || { let p = page.clone(); async move { Html(p) } }))
        .route("/signal/{id}", post(|Path(id): Path<String>, body: String| async move {
            match p2p::request_answer(&id, body).await {
                Ok((sdp, relay)) => (axum::http::StatusCode::OK, serde_json::json!({ "sdp": sdp, "relay": relay }).to_string()),
                Err(e) => (axum::http::StatusCode::NOT_FOUND, e) }
        }));
    tokio::spawn(async move {
        let l = tokio::net::TcpListener::bind("127.0.0.1:8097").await.unwrap();
        axum::serve(l, app).await.unwrap();
    });
    while let Some(ev) = ui_rx.recv().await {
        println!("{ev:?}");
        if let UiEvent::Registered { id } = ev {
            let _ = t_tx.send(Some((id, ws.clone())));
        }
    }
}
