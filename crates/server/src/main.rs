//! `veyndesk-server` binary.
//!
//! Env vars:
//!   VEYNDESK_BIND         listen address           (default 0.0.0.0:8080)
//!   VEYNDESK_DATA         data directory           (default ./data)
//!   VEYNDESK_ADMIN_TOKEN  admin API/panel token    (generated + printed if unset)
//!   VEYNDESK_TRUST_PROXY  "1" to trust X-Forwarded-For (when behind Caddy/nginx)

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "veyndesk_server=info".into()),
        )
        .init();
    let bind = std::env::var("VEYNDESK_BIND").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let data = std::env::var("VEYNDESK_DATA").unwrap_or_else(|_| "./data".into());
    veyndesk_server::serve(&bind, data.into()).await
}
