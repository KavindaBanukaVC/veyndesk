//! `velodesk-host` — headless / console host agent.
//!
//!   velodesk-host --server https://desk.example.com
//!   velodesk-host --set-password "my unattended password"
//!   velodesk-host --no-one-time          (only accept the permanent password)

use clap::Parser;
use std::sync::{Arc, Mutex};
use velodesk_host::{config::*, AuthConfig, UiEvent};

#[derive(Parser)]
#[command(name = "velodesk-host", version, about = "Veyndesk remote desktop host agent")]
struct Args {
    /// Server URL (http(s):// or ws(s)://). Saved for next time.
    #[arg(long, env = "VELODESK_SERVER")]
    server: Option<String>,
    /// Display name for this computer. Saved for next time.
    #[arg(long)]
    name: Option<String>,
    /// Set a permanent password for unattended access, then exit.
    #[arg(long)]
    set_password: Option<String>,
    /// Remove the permanent password, then exit.
    #[arg(long)]
    clear_password: bool,
    /// Do not generate a one-time password (permanent password only).
    #[arg(long)]
    no_one_time: bool,
    /// Print the config file location and exit.
    #[arg(long)]
    where_config: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "velodesk_host=info".into()),
        )
        .init();
    velodesk_host::init_platform();
    let args = Args::parse();
    let mut cfg = HostConfig::load_or_default();

    if args.where_config {
        println!("{}", config_path().display());
        return Ok(());
    }
    if let Some(pw) = &args.set_password {
        anyhow::ensure!(pw.len() >= 8, "use at least 8 characters for a permanent password");
        cfg.permanent = Some(Credentials::from_password(pw));
        cfg.save()?;
        println!("Permanent password saved.");
        return Ok(());
    }
    if args.clear_password {
        cfg.permanent = None;
        cfg.save()?;
        println!("Permanent password removed.");
        return Ok(());
    }
    if let Some(s) = args.server {
        cfg.server = s;
    }
    if let Some(n) = args.name {
        cfg.name = n;
    }
    if cfg.server.trim().is_empty() {
        anyhow::bail!("no server configured: run with --server http://SERVER:PORT (it is remembered)");
    }
    cfg.save()?;

    let otp = if args.no_one_time { None } else { Some(one_time_password().0) };
    let auth = AuthConfig::build(&cfg, otp.as_deref());
    if auth.secrets.is_empty() {
        anyhow::bail!("no password configured: remove --no-one-time or use --set-password");
    }

    println!("──────────────────────────────────────────");
    println!(" Veyndesk host v{}  —  {}", velodesk_host::VERSION, cfg.name);
    println!(" Server   : {}", cfg.server);
    if let Some(p) = &otp {
        println!(" Password : {p}   (one-time, changes on restart)");
    }
    if cfg.permanent.is_some() {
        println!(" Permanent password: set");
    }
    println!("──────────────────────────────────────────");

    let (ui_tx, mut ui_rx) = tokio::sync::mpsc::unbounded_channel();
    let (_cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    let cfg = Arc::new(Mutex::new(cfg));
    tokio::spawn(velodesk_host::run(cfg, Arc::new(Mutex::new(auth)), ui_tx, cmd_rx, velodesk_host::default_source()));

    while let Some(ev) = ui_rx.recv().await {
        match ev {
            UiEvent::Registered { id } => println!(">> Your ID: {}", format_id(&id)),
            UiEvent::Connecting { url } => println!("   connecting to {url} ..."),
            UiEvent::Disconnected { reason } => println!("   offline: {reason}"),
            UiEvent::Incoming { ip, .. } => println!("   incoming connection from {ip}"),
            UiEvent::SessionStarted { sid, ip } => println!("** session {sid} started ({ip}) — your screen is being shared"),
            UiEvent::SessionEnded { sid } => println!("   session {sid} ended"),
            UiEvent::AuthFailed { ip, .. } => println!("!! wrong password from {ip}"),
            UiEvent::Error { message } => println!("!! {message}"),
            UiEvent::ApprovalRequest { .. } => {} // the CLI never asks (ask_on_one_time = false)
        }
    }
    Ok(())
}

fn format_id(id: &str) -> String {
    id.as_bytes()
        .chunks(3)
        .map(|c| std::str::from_utf8(c).unwrap())
        .collect::<Vec<_>>()
        .join(" ")
}
