//! VeloDesk wire protocol.
//!
//! Every peer talks to the VeloDesk server over a WebSocket.
//! * **Text frames** carry JSON control messages (tagged by the `"t"` field).
//! * **Binary frames** carry video. Host → server frames are prefixed with a
//!   little-endian `u32` session id; the server strips it and forwards the
//!   rest to that session's viewer unchanged.
//!
//! ## Authentication (the server never sees the password)
//! ```text
//! viewer --Connect{id}------> server --Incoming{sid}---> host
//! viewer <-Challenge{salt,n}- server <-Challenge{..}---- host
//! viewer --Auth{proof}------> server --Auth{proof}-----> host
//! viewer <-Accepted/Rejected- server <-Accept/Reject---- host
//! ```
//! `secret = hex(sha256(salt ":" password))`, `proof = hex(sha256(secret ":" nonce))`.
//! The host only stores `salt` and `secret`, and every attempt uses a fresh nonce,
//! so a captured proof cannot be replayed.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const PROTOCOL_VERSION: u32 = 1;

/// Messages a host agent sends to the server.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum HostToServer {
    /// First message after connecting. `id` is `None` on first run; the server assigns one.
    /// `key` is a long random secret that proves ownership of the id on later runs.
    Register {
        id: Option<String>,
        key: String,
        name: String,
        os: String,
        version: String,
    },
    Challenge { sid: u32, salt: String, nonce: String },
    Accept { sid: u32, width: u32, height: u32, name: String, os: String },
    Reject { sid: u32, reason: String },
    ToViewer { sid: u32, msg: HostEvent },
    Close { sid: u32, reason: String },
    Ping,
}

/// Messages the server sends to a host agent.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ServerToHost {
    Registered { id: String },
    Error { message: String },
    Incoming { sid: u32, ip: String },
    Auth { sid: u32, proof: String },
    FromViewer { sid: u32, msg: ViewerInput },
    ViewerLeft { sid: u32 },
    Pong,
}

/// Control messages a viewer sends before a session is established.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ViewerControl {
    Connect { id: String },
    Auth { proof: String },
}

/// Input / session messages a viewer sends once connected. Relayed to the host as-is.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ViewerInput {
    /// Absolute position in host frame pixels.
    MouseMove { x: i32, y: i32 },
    /// 0 = left, 1 = middle, 2 = right (same as DOM `MouseEvent.button`).
    MouseButton { button: u8, down: bool },
    /// Wheel "clicks" (positive = down / right).
    Wheel { dx: i32, dy: i32 },
    /// `code` is DOM `KeyboardEvent.code` (physical key), `key` is `KeyboardEvent.key`.
    Key { code: String, key: String, down: bool },
    /// Type a whole string (paste-as-keystrokes).
    Text { text: String },
    /// Acknowledges a decoded+painted video frame. Drives adaptive quality.
    Ack { seq: u32 },
    Clipboard { text: String },
    /// Ask the host to resend a full frame.
    Refresh,
    /// Switch captured monitor.
    SelectMonitor { index: u32 },
    /// Sends Ctrl+Alt+Del style combos the browser cannot capture.
    SpecialKey { name: String },
}

/// Messages the server sends to a viewer.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ServerToViewer {
    Challenge { salt: String, nonce: String },
    Accepted { width: u32, height: u32, name: String, os: String },
    Rejected { reason: String },
    Error { message: String },
    Host { msg: HostEvent },
    Closed { reason: String },
}

/// Host → viewer events (wrapped by the server in `ServerToViewer::Host`).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum HostEvent {
    Resize { width: u32, height: u32 },
    Clipboard { text: String },
    Monitors { list: Vec<MonitorInfo>, active: u32 },
    Stats { fps: f32, quality: u8, kbps: u32 },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct MonitorInfo {
    pub index: u32,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

// ---------------------------------------------------------------- video frames

/// Binary video frame payload (what the viewer receives).
///
/// ```text
/// u8  kind = 1
/// u32 seq
/// u16 frame_width, u16 frame_height
/// u16 tile_count
/// tile_count × { u16 x, u16 y, u16 w, u16 h, u32 len, [len] JPEG bytes }
/// ```
/// All integers little-endian.
pub const FRAME_KIND_TILES: u8 = 1;

pub struct Tile<'a> {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
    pub jpeg: &'a [u8],
}

pub fn encode_frame(sid: Option<u32>, seq: u32, fw: u16, fh: u16, tiles: &[Tile]) -> Vec<u8> {
    let size: usize = 4 + 11 + tiles.iter().map(|t| 12 + t.jpeg.len()).sum::<usize>();
    let mut out = Vec::with_capacity(size);
    if let Some(sid) = sid {
        out.extend_from_slice(&sid.to_le_bytes());
    }
    out.push(FRAME_KIND_TILES);
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&fw.to_le_bytes());
    out.extend_from_slice(&fh.to_le_bytes());
    out.extend_from_slice(&(tiles.len() as u16).to_le_bytes());
    for t in tiles {
        out.extend_from_slice(&t.x.to_le_bytes());
        out.extend_from_slice(&t.y.to_le_bytes());
        out.extend_from_slice(&t.w.to_le_bytes());
        out.extend_from_slice(&t.h.to_le_bytes());
        out.extend_from_slice(&(t.jpeg.len() as u32).to_le_bytes());
        out.extend_from_slice(t.jpeg);
    }
    out
}

/// Parsed frame header + tiles (used by tests and native viewers).
#[derive(Debug)]
pub struct DecodedFrame {
    pub seq: u32,
    pub width: u16,
    pub height: u16,
    pub tiles: Vec<(u16, u16, u16, u16, Vec<u8>)>,
}

pub fn decode_frame(buf: &[u8]) -> Option<DecodedFrame> {
    let mut p = 0usize;
    let mut take = |n: usize| -> Option<&[u8]> {
        let s = buf.get(p..p + n)?;
        p += n;
        Some(s)
    };
    if take(1)?[0] != FRAME_KIND_TILES {
        return None;
    }
    let u16_ = |b: &[u8]| u16::from_le_bytes([b[0], b[1]]);
    let seq = u32::from_le_bytes(take(4)?.try_into().ok()?);
    let width = u16_(take(2)?);
    let height = u16_(take(2)?);
    let n = u16_(take(2)?);
    let mut tiles = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let x = u16_(take(2)?);
        let y = u16_(take(2)?);
        let w = u16_(take(2)?);
        let h = u16_(take(2)?);
        let len = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
        tiles.push((x, y, w, h, take(len)?.to_vec()));
    }
    Some(DecodedFrame { seq, width, height, tiles })
}

// ---------------------------------------------------------------- auth helpers

pub fn sha256_hex(data: &str) -> String {
    hex::encode(Sha256::digest(data.as_bytes()))
}

/// What the host stores for a password (never the password itself).
pub fn password_secret(salt: &str, password: &str) -> String {
    sha256_hex(&format!("{salt}:{password}"))
}

pub fn auth_proof(secret: &str, nonce: &str) -> String {
    sha256_hex(&format!("{secret}:{nonce}"))
}

/// Constant-time string compare.
pub fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn random_token(bytes: usize) -> String {
    use rand::RngCore;
    let mut buf = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

/// Human-friendly one-time password: no 0/O/1/l/I confusion.
pub fn random_password(len: usize) -> String {
    use rand::Rng;
    const CHARS: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
    let mut rng = rand::rng();
    (0..len).map(|_| CHARS[rng.random_range(0..CHARS.len())] as char).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip() {
        let jpeg = vec![1, 2, 3, 4];
        let tiles = [Tile { x: 64, y: 128, w: 64, h: 64, jpeg: &jpeg }];
        let buf = encode_frame(None, 7, 1920, 1080, &tiles);
        let f = decode_frame(&buf).unwrap();
        assert_eq!((f.seq, f.width, f.height), (7, 1920, 1080));
        assert_eq!(f.tiles[0], (64, 128, 64, 64, jpeg));
    }

    #[test]
    fn auth_flow() {
        let secret = password_secret("salt", "hunter2");
        let proof = auth_proof(&secret, "nonce");
        assert!(ct_eq(&proof, &auth_proof(&password_secret("salt", "hunter2"), "nonce")));
        assert!(!ct_eq(&proof, &auth_proof(&password_secret("salt", "wrong"), "nonce")));
        // Known vector so the JS implementation can be checked against it.
        assert_eq!(sha256_hex("abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn json_tags() {
        let s = serde_json::to_string(&ViewerInput::MouseMove { x: 1, y: 2 }).unwrap();
        assert_eq!(s, r#"{"t":"mouse_move","x":1,"y":2}"#);
        let v: ViewerInput = serde_json::from_str(r#"{"t":"ack","seq":3}"#).unwrap();
        assert_eq!(v, ViewerInput::Ack { seq: 3 });
    }
}
