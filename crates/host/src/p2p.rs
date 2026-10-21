//! Zero-setup peer-to-peer connections (like AnyDesk: just ID + password).
//!
//! * **Signaling** goes through the free public ntfy.sh pub/sub service — no Veyndesk server
//!   is needed to *find* a partner. A PC listens on topic `vdp-<hash(ID)>`; a viewer posts its
//!   WebRTC offer there with a private reply topic, and gets the answer back on it.
//! * **Media** then flows directly between the two PCs over a WebRTC data channel
//!   (UDP hole punching via public STUN servers, DTLS-encrypted end to end).
//! * On the host the data channel is bridged to this PC's own Veyndesk server
//!   (`/ws/viewer`), so the existing password check, video and input code is reused unchanged.
//!
//! Binary frames host→viewer are split into ≤16 KB chunks:
//! `u32 msg_id | u16 index | u16 count | payload` (little-endian).

use crate::{random_token, sha256_hex};
use base64::Engine;
use bytes::BytesMut;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::{
    io::{BufRead, BufReader, Read, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};
use webrtc::{
    data_channel::{DataChannel, DataChannelEvent},
    peer_connection::{
        PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCConfigurationBuilder, RTCIceGatheringState,
        RTCIceServer, RTCPeerConnectionState, RTCSessionDescription,
    },
};

pub const CHUNK: usize = 16_000;

pub fn stun_servers() -> Vec<String> {
    vec!["stun:stun.l.google.com:19302".into(), "stun:stun.cloudflare.com:3478".into()]
}

/// Public signaling relay (override with `VELODESK_SIGNAL_URL`, e.g. a self-hosted ntfy).
pub fn signal_base() -> String {
    std::env::var("VELODESK_SIGNAL_URL").unwrap_or_else(|_| "https://ntfy.sh".into())
}

pub fn host_topic(id: &str) -> String {
    format!("vdp-{}", &sha256_hex(&format!("velodesk-p2p:{id}"))[..24])
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct Envelope {
    t: String,
    #[serde(default)]
    reply: String,
    sdp: String,
    /// Host → viewer: public URL of a server the viewer can use if the direct path fails.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    relay: String,
}

static RELAY: Mutex<Option<String>> = Mutex::new(None);

/// Public relay URL (e.g. this PC's Cloudflare tunnel) offered to partners as a fallback.
pub fn set_relay(url: Option<String>) {
    *RELAY.lock().unwrap() = url;
}

fn pack(e: &Envelope) -> String {
    let json = serde_json::to_vec(e).unwrap();
    let mut z = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::best());
    z.write_all(&json).unwrap();
    format!("vd1:{}", base64::engine::general_purpose::STANDARD.encode(z.finish().unwrap()))
}

fn unpack(s: &str) -> Option<Envelope> {
    let raw = base64::engine::general_purpose::STANDARD.decode(s.strip_prefix("vd1:")?).ok()?;
    let mut out = Vec::new();
    flate2::read::DeflateDecoder::new(&raw[..]).take(1 << 20).read_to_end(&mut out).ok()?;
    serde_json::from_slice(&out).ok()
}

fn agent(read_timeout: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(10)).timeout_read(read_timeout).build()
}

fn post(topic: &str, body: &str) -> Result<(), String> {
    agent(Duration::from_secs(15))
        .post(&format!("{}/{topic}", signal_base()))
        .send_string(body)
        .map(|_| ())
        .map_err(|e| format!("signaling failed: {e}"))
}

/// Blocking subscriber: forwards every message on `topic` until `stop` is set. Reconnects on errors.
fn subscribe(topic: String, since_all: bool, tx: mpsc::UnboundedSender<String>, stop: Arc<AtomicBool>) {
    let url = format!("{}/{topic}/json{}", signal_base(), if since_all { "?since=all" } else { "" });
    while !stop.load(Ordering::SeqCst) && !tx.is_closed() {
        match agent(Duration::from_secs(90)).get(&url).call() {
            Ok(resp) => {
                for line in BufReader::new(resp.into_reader()).lines() {
                    if stop.load(Ordering::SeqCst) || tx.is_closed() {
                        return;
                    }
                    let Ok(line) = line else { break };
                    let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
                    if v["event"] == "message" {
                        if let Some(m) = v["message"].as_str() {
                            let _ = tx.send(m.to_string());
                        }
                    }
                }
            }
            Err(e) => debug!("signaling subscribe error: {e}"),
        }
        std::thread::sleep(Duration::from_secs(3));
    }
}

// ------------------------------------------------------------------ viewer side

/// Viewer: send our WebRTC offer to partner `id` and wait for its answer.
/// Returns `(answer_sdp, relay_url)`.
pub async fn request_answer(id: &str, offer_sdp: String) -> Result<(String, Option<String>), String> {
    let reply = format!("vdr-{}", random_token(12));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let stop = Arc::new(AtomicBool::new(false));
    {
        let (reply, stop) = (reply.clone(), stop.clone());
        std::thread::spawn(move || subscribe(reply, true, tx, stop));
    }
    let env = Envelope { t: "offer".into(), reply: reply.clone(), sdp: offer_sdp, relay: String::new() };
    let topic = host_topic(id);
    let body = pack(&env);
    let posted = tokio::task::spawn_blocking(move || post(&topic, &body)).await.map_err(|e| e.to_string());
    if let Err(e) = posted.and_then(|r| r) {
        stop.store(true, Ordering::SeqCst);
        return Err(e);
    }
    let result = tokio::time::timeout(Duration::from_secs(12), async {
        while let Some(m) = rx.recv().await {
            if let Some(e) = unpack(&m) {
                if e.t == "answer" {
                    return Some((e.sdp, Some(e.relay).filter(|r| r.starts_with("http"))));
                }
            }
        }
        None
    })
    .await;
    stop.store(true, Ordering::SeqCst);
    match result {
        Ok(Some(ans)) => Ok(ans),
        _ => Err("Partner not found. Check the ID, and that Veyndesk is open on the other PC.".into()),
    }
}

// ------------------------------------------------------------------ host side

/// What the host listener needs: our ID, and the viewer WebSocket of the server we're registered on.
pub type Target = Option<(String, String)>;

/// Host: answer incoming offers for our ID forever. Follows changes of `target`.
pub async fn serve(mut target: watch::Receiver<Target>) {
    loop {
        let current = target.borrow_and_update().clone();
        let Some((id, viewer_ws)) = current else {
            if target.changed().await.is_err() {
                return;
            }
            continue;
        };
        info!("p2p: listening for partners on {}", host_topic(&id));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        {
            let (topic, stop) = (host_topic(&id), stop.clone());
            std::thread::spawn(move || subscribe(topic, false, tx, stop));
        }
        loop {
            tokio::select! {
                m = rx.recv() => {
                    let Some(m) = m else { break };
                    if let Some(env) = unpack(&m) {
                        if env.t == "offer" && env.reply.starts_with("vdr-") {
                            let ws = viewer_ws.clone();
                            tokio::spawn(async move {
                                if let Err(e) = answer(env, ws).await {
                                    warn!("p2p session failed: {e}");
                                }
                            });
                        }
                    }
                }
                c = target.changed() => {
                    if c.is_err() { stop.store(true, Ordering::SeqCst); return; }
                    break;
                }
            }
        }
        stop.store(true, Ordering::SeqCst);
    }
}

struct Handler {
    gathered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    channel: mpsc::UnboundedSender<Arc<dyn DataChannel>>,
    dead: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            if let Some(tx) = self.gathered.lock().unwrap().take() {
                let _ = tx.send(());
            }
        }
    }
    async fn on_data_channel(&self, dc: Arc<dyn DataChannel>) {
        let _ = self.channel.send(dc);
    }
    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        debug!("p2p connection state: {state}");
        if matches!(state, RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed | RTCPeerConnectionState::Disconnected) {
            self.dead.notify_waiters();
            self.dead.notify_one();
        }
    }
}

pub async fn new_peer(handler: Arc<dyn PeerConnectionEventHandler>) -> Result<impl PeerConnection, String> {
    let config = RTCConfigurationBuilder::default()
        .with_ice_servers(vec![RTCIceServer { urls: stun_servers(), ..Default::default() }])
        .build();
    PeerConnectionBuilder::new()
        .with_configuration(config)
        .with_runtime(Arc::new(safe_rt::SafeRuntime::new()))
        .with_handler(handler)
        .with_udp_addrs(vec!["0.0.0.0:0"])
        .build()
        .await
        .map_err(|e| e.to_string())
}

async fn answer(offer: Envelope, viewer_ws: String) -> Result<(), String> {
    let (g_tx, g_rx) = tokio::sync::oneshot::channel();
    let (dc_tx, mut dc_rx) = mpsc::unbounded_channel();
    let dead = Arc::new(tokio::sync::Notify::new());
    let handler = Arc::new(Handler { gathered: Mutex::new(Some(g_tx)), channel: dc_tx, dead: dead.clone() });
    let pc = new_peer(handler).await?;
    pc.set_remote_description(RTCSessionDescription::offer(offer.sdp).map_err(|e| e.to_string())?)
        .await
        .map_err(|e| e.to_string())?;
    let ans = pc.create_answer(None).await.map_err(|e| e.to_string())?;
    pc.set_local_description(ans).await.map_err(|e| e.to_string())?;
    let _ = tokio::time::timeout(Duration::from_secs(5), g_rx).await;
    let sdp = pc.local_description().await.ok_or("no local description")?.sdp;
    let relay = RELAY.lock().unwrap().clone().unwrap_or_default();
    let body = pack(&Envelope { t: "answer".into(), reply: String::new(), sdp, relay });
    let reply = offer.reply.clone();
    tokio::task::spawn_blocking(move || post(&reply, &body)).await.map_err(|e| e.to_string())??;

    let dc = tokio::select! {
        dc = dc_rx.recv() => dc.ok_or("no data channel")?,
        _ = tokio::time::sleep(Duration::from_secs(30)) => { let _ = pc.close().await; return Err("partner never connected (network blocks direct connections)".into()) }
        _ = dead.notified() => { let _ = pc.close().await; return Err("direct connection failed".into()) }
    };
    info!("p2p: partner connected directly");
    let r = tokio::select! {
        r = bridge(dc.clone(), &viewer_ws) => r,
        _ = dead.notified() => Ok(()),
    };
    let _ = dc.close().await;
    let _ = pc.close().await;
    r
}

/// Pipe a data channel to the local server's viewer WebSocket.
pub async fn bridge(dc: Arc<dyn DataChannel>, viewer_ws: &str) -> Result<(), String> {
    let (ws, _) = tokio_tungstenite::connect_async(viewer_ws).await.map_err(|e| format!("local server: {e}"))?;
    let (mut sink, mut stream) = ws.split();
    let up = async {
        while let Some(ev) = dc.poll().await {
            match ev {
                DataChannelEvent::OnMessage(m) if m.is_string => {
                    let t = String::from_utf8_lossy(&m.data).into_owned();
                    if sink.send(Message::Text(t.into())).await.is_err() {
                        break;
                    }
                }
                DataChannelEvent::OnClose => break,
                _ => {}
            }
        }
        let _ = sink.close().await;
    };
    let down = async {
        let mut msg_id = 0u32;
        while let Some(Ok(m)) = stream.next().await {
            let ok = match m {
                Message::Text(t) => dc.send_text(t.as_str()).await.is_ok(),
                Message::Binary(b) => {
                    msg_id = msg_id.wrapping_add(1);
                    let mut ok = true;
                    for (i, part) in split_chunks(&b).enumerate() {
                        let n = b.len().div_ceil(CHUNK - 8).max(1) as u16;
                        let mut buf = BytesMut::with_capacity(part.len() + 8);
                        buf.extend_from_slice(&msg_id.to_le_bytes());
                        buf.extend_from_slice(&(i as u16).to_le_bytes());
                        buf.extend_from_slice(&n.to_le_bytes());
                        buf.extend_from_slice(part);
                        if dc.send(buf).await.is_err() {
                            ok = false;
                            break;
                        }
                    }
                    ok
                }
                Message::Close(_) => false,
                _ => true,
            };
            if !ok {
                break;
            }
        }
    };
    tokio::select! { _ = up => {}, _ = down => {} }
    Ok(())
}

/// Tokio runtime whose UDP sockets fall back to plain send_to/recv_from when the OS rejects
/// the batching / ECN socket options quinn-udp enables (seen with Wine, some VPN / antivirus
/// network filters on Windows: WSAEOPNOTSUPP 10045).
mod safe_rt {
    use std::{
        fmt, future::Future, io, io::IoSliceMut, net::SocketAddr, pin::Pin, sync::Arc,
        task::{Context, Poll},
        time::Duration,
    };
    use webrtc::runtime::{AsyncInterval, AsyncTcpListener, AsyncTcpStream, AsyncUdpSocket, JoinHandle, RecvMeta, Runtime, TokioRuntime, Transmit};

    pub struct SafeRuntime(TokioRuntime);
    impl SafeRuntime {
        pub fn new() -> Self {
            Self(TokioRuntime)
        }
    }
    impl fmt::Debug for SafeRuntime {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("SafeRuntime")
        }
    }

    impl Runtime for SafeRuntime {
        fn spawn(&self, f: Pin<Box<dyn Future<Output = ()> + Send>>) -> Box<dyn JoinHandle> {
            self.0.spawn(f)
        }
        fn spawn_reactor(&self, n: usize, f: Pin<Box<dyn Future<Output = ()> + Send>>) -> Box<dyn JoinHandle> {
            self.0.spawn_reactor(n, f)
        }
        fn wrap_udp_socket(&self, sock: std::net::UdpSocket) -> io::Result<Arc<dyn AsyncUdpSocket>> {
            let backup = sock.try_clone()?;
            match self.0.wrap_udp_socket(sock) {
                Ok(s) => Ok(s),
                Err(e) => {
                    tracing::info!("p2p: using plain UDP socket ({e})");
                    backup.set_nonblocking(true)?;
                    Ok(Arc::new(PlainUdp(tokio::net::UdpSocket::from_std(backup)?)))
                }
            }
        }
        fn wrap_tcp_listener(&self, l: std::net::TcpListener) -> io::Result<Arc<dyn AsyncTcpListener>> {
            self.0.wrap_tcp_listener(l)
        }
        fn connect_tcp<'a>(&'a self, a: SocketAddr) -> Pin<Box<dyn Future<Output = io::Result<Arc<dyn AsyncTcpStream>>> + Send + 'a>> {
            self.0.connect_tcp(a)
        }
        fn resolve_host<'a>(&'a self, h: &'a str) -> Pin<Box<dyn Future<Output = io::Result<Vec<SocketAddr>>> + Send + 'a>> {
            self.0.resolve_host(h)
        }
        fn sleep(&self, d: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
            self.0.sleep(d)
        }
        fn interval(&self, p: Duration) -> Box<dyn AsyncInterval> {
            self.0.interval(p)
        }
        fn block_on(&self, f: Pin<Box<dyn Future<Output = ()> + '_>>) {
            self.0.block_on(f)
        }
    }

    #[derive(Debug)]
    struct PlainUdp(tokio::net::UdpSocket);

    impl AsyncUdpSocket for PlainUdp {
        fn local_addr(&self) -> io::Result<SocketAddr> {
            self.0.local_addr()
        }
        fn poll_send(&self, cx: &mut Context<'_>, t: &Transmit<'_>) -> Poll<io::Result<usize>> {
            self.0.poll_send_to(cx, t.contents, t.destination)
        }
        fn poll_recv(&self, cx: &mut Context<'_>, bufs: &mut [IoSliceMut<'_>], meta: &mut [RecvMeta]) -> Poll<io::Result<usize>> {
            let mut rb = tokio::io::ReadBuf::new(&mut bufs[0]);
            match self.0.poll_recv_from(cx, &mut rb) {
                Poll::Ready(Ok(addr)) => {
                    let len = rb.filled().len();
                    let mut m = RecvMeta::default();
                    m.addr = addr;
                    m.len = len;
                    m.stride = len.max(1);
                    meta[0] = m;
                    Poll::Ready(Ok(1))
                }
                Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
                Poll::Pending => Poll::Pending,
            }
        }
    }
}

fn split_chunks(b: &[u8]) -> impl Iterator<Item = &[u8]> {
    let n = b.len().div_ceil(CHUNK - 8).max(1);
    (0..n).map(move |i| &b[i * (CHUNK - 8)..((i + 1) * (CHUNK - 8)).min(b.len())])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_roundtrip_is_small() {
        let sdp = "v=0\r\no=- 123 2 IN IP4 127.0.0.1\r\n".repeat(40);
        let e = Envelope { t: "offer".into(), reply: "vdr-abc".into(), sdp: sdp.clone(), relay: String::new() };
        let p = pack(&e);
        assert!(p.len() < 4096, "fits in one ntfy message: {}", p.len());
        assert_eq!(unpack(&p).unwrap().sdp, sdp);
        assert!(unpack("garbage").is_none());
    }

    #[test]
    fn chunks_cover_everything() {
        let data: Vec<u8> = (0..100_000u32).map(|i| i as u8).collect();
        let parts: Vec<&[u8]> = split_chunks(&data).collect();
        assert_eq!(parts.len(), data.len().div_ceil(CHUNK - 8));
        assert_eq!(parts.concat(), data);
        assert_eq!(split_chunks(&[]).count(), 1);
    }

    #[test]
    fn topics_are_stable_and_opaque() {
        assert_eq!(host_topic("123456789"), host_topic("123456789"));
        assert!(!host_topic("123456789").contains("123456789"));
    }
}
