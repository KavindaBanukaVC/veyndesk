//! LAN discovery (like RustDesk's LAN discovery / AnyDesk on a local network).
//!
//! Every running Veyndesk app listens on UDP 21119. A PC can broadcast:
//! * `{"q":"id","id":"123456789"}` → the PC with that ID answers with the server it is registered on,
//! * `{"q":"server"}`              → PCs that run the built-in server answer with its address,
//! * `{"q":"any"}` (unicast to an IP) → that PC answers with its ID + server (direct-IP connect).
//!
//! So IDs work across the whole LAN even if PCs picked different servers, and nobody has to type
//! server addresses on a normal office / home network.

use serde::{Deserialize, Serialize};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::net::UdpSocket;

pub const PORT: u16 = 21119;

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "q", rename_all = "snake_case")]
enum Query {
    Id { id: String },
    Server,
    Any,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Answer {
    pub id: Option<String>,
    pub name: String,
    /// Server this PC is registered on, as reachable from the asker (`http://ip:port`).
    pub server: String,
    /// True if this PC runs the built-in server.
    pub is_server: bool,
}

/// What the responder needs to know about this PC (kept up to date by main).
#[derive(Default, Clone)]
pub struct LocalInfo {
    pub id: Option<String>,
    pub name: String,
    /// The server URL from the config (may be 127.0.0.1 when this PC is the server).
    pub server: String,
    /// `Some(port)` while the built-in server runs.
    pub server_port: Option<u16>,
}

/// The local address this PC uses to reach `peer` (picks the right network card).
fn local_ip_towards(peer: IpAddr) -> Option<IpAddr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect((peer, 9)).ok()?;
    s.local_addr().ok().map(|a| a.ip())
}

/// Rewrite loopback server URLs into an address the asker can reach.
fn reachable(url: &str, me: Option<IpAddr>) -> String {
    let Some(me) = me else { return url.to_string() };
    url.replace("127.0.0.1", &me.to_string()).replace("localhost", &me.to_string())
}

pub async fn responder(info: Arc<Mutex<LocalInfo>>) {
    let sock = match UdpSocket::bind(("0.0.0.0", PORT)).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("LAN discovery disabled (UDP {PORT}): {e}");
            return;
        }
    };
    let mut buf = [0u8; 1024];
    loop {
        let Ok((n, from)) = sock.recv_from(&mut buf).await else { continue };
        let Ok(q) = serde_json::from_slice::<Query>(&buf[..n]) else { continue };
        let i = info.lock().unwrap().clone();
        let me = local_ip_towards(from.ip());
        let server_url = i.server_port.map(|p| format!("http://{}:{p}", me.map(|m| m.to_string()).unwrap_or_default()));
        let answer = match q {
            Query::Id { id } if i.id.as_deref() == Some(id.as_str()) && !i.server.is_empty() => {
                Some(Answer { id: i.id, name: i.name, server: reachable(&i.server, me), is_server: i.server_port.is_some() })
            }
            Query::Server => server_url.map(|s| Answer { id: i.id, name: i.name, server: s, is_server: true }),
            Query::Any if !i.server.is_empty() => {
                Some(Answer { id: i.id, name: i.name, server: reachable(&i.server, me), is_server: i.server_port.is_some() })
            }
            _ => None,
        };
        if let Some(a) = answer {
            let _ = sock.send_to(&serde_json::to_vec(&a).unwrap(), from).await;
        }
    }
}

/// Broadcast addresses to try: limited broadcast + /24 directed broadcast of each likely interface.
fn broadcast_targets() -> Vec<SocketAddr> {
    let mut v = vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), PORT)];
    if let Some(IpAddr::V4(ip)) = local_ip_towards(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))) {
        let o = ip.octets();
        v.push(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(o[0], o[1], o[2], 255)), PORT));
    }
    v
}

async fn ask(targets: &[SocketAddr], q: &Query, wait: Duration, first_only: bool) -> Vec<Answer> {
    let Ok(sock) = UdpSocket::bind("0.0.0.0:0").await else { return vec![] };
    let _ = sock.set_broadcast(true);
    let msg = serde_json::to_vec(q).unwrap();
    let mut out: Vec<Answer> = vec![];
    // Send twice in case of packet loss.
    for _ in 0..2 {
        for t in targets {
            let _ = sock.send_to(&msg, t).await;
        }
        let deadline = tokio::time::Instant::now() + wait / 2;
        let mut buf = [0u8; 2048];
        while let Ok(Ok((n, _))) = tokio::time::timeout_at(deadline, sock.recv_from(&mut buf)).await {
            if let Ok(a) = serde_json::from_slice::<Answer>(&buf[..n]) {
                if !out.iter().any(|x| x.server == a.server && x.id == a.id) {
                    out.push(a);
                }
                if first_only {
                    return out;
                }
            }
        }
    }
    out
}

/// Which server is the PC with this ID registered on? (LAN only)
pub async fn find_id(id: &str) -> Option<Answer> {
    ask(&broadcast_targets(), &Query::Id { id: id.to_string() }, Duration::from_millis(900), true)
        .await
        .into_iter()
        .next()
}

/// Veyndesk servers running on the LAN.
pub async fn find_servers() -> Vec<Answer> {
    ask(&broadcast_targets(), &Query::Server, Duration::from_millis(1200), false).await
}

/// Ask a specific IP which ID / server it has (direct-IP connect).
pub async fn probe_ip(ip: IpAddr) -> Option<Answer> {
    ask(&[SocketAddr::new(ip, PORT)], &Query::Any, Duration::from_millis(1500), true).await.into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn responder_answers() {
        let info = Arc::new(Mutex::new(LocalInfo {
            id: Some("123456789".into()),
            name: "PC-A".into(),
            server: "http://127.0.0.1:21118".into(),
            server_port: Some(21118),
        }));
        tokio::spawn(responder(info));
        tokio::time::sleep(Duration::from_millis(200)).await;
        let lo: IpAddr = "127.0.0.1".parse().unwrap();
        let a = probe_ip(lo).await.expect("direct probe");
        assert_eq!(a.id.as_deref(), Some("123456789"));
        assert!(a.is_server);
        let t = [SocketAddr::new(lo, PORT)];
        let hit = ask(&t, &Query::Id { id: "123456789".into() }, Duration::from_millis(400), true).await;
        assert_eq!(hit.len(), 1);
        let miss = ask(&t, &Query::Id { id: "999999999".into() }, Duration::from_millis(400), true).await;
        assert!(miss.is_empty());
        let srv = ask(&t, &Query::Server, Duration::from_millis(400), false).await;
        assert_eq!(srv[0].server, "http://127.0.0.1:21118");
    }

    #[test]
    fn rewrite_loopback() {
        let me: IpAddr = "10.0.0.5".parse().unwrap();
        assert_eq!(reachable("http://127.0.0.1:21118", Some(me)), "http://10.0.0.5:21118");
        assert_eq!(reachable("https://desk.example.com", Some(me)), "https://desk.example.com");
    }
}
