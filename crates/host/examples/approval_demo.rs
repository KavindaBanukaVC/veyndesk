//! Test harness: host with "ask to accept" on; accepts every odd request, denies every even one.
//! VEYNDESK_TEST_PATTERN=1 cargo run -p veyndesk-host --example approval_demo -- http://127.0.0.1:8080 <password>
use std::sync::{Arc, Mutex};
use veyndesk_host::{config::HostConfig, AuthConfig, HostCommand, UiEvent};

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut cfg = HostConfig::load_or_default();
    cfg.server = args[1].clone();
    cfg.permanent = None;
    let mut auth = AuthConfig::build(&cfg, Some(&args[2]));
    auth.ask_on_one_time = true;
    let (ui_tx, mut ui_rx) = tokio::sync::mpsc::unbounded_channel();
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(veyndesk_host::run(Arc::new(Mutex::new(cfg)), Arc::new(Mutex::new(auth)), ui_tx, cmd_rx, veyndesk_host::default_source()));
    let mut n = 0;
    while let Some(ev) = ui_rx.recv().await {
        println!("{ev:?}");
        if let UiEvent::ApprovalRequest { sid, .. } = ev {
            n += 1;
            tokio::time::sleep(std::time::Duration::from_millis(800)).await;
            let _ = cmd_tx.send(HostCommand::Respond { sid, accept: n % 2 == 1 });
        }
    }
}
