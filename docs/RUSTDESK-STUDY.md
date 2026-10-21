# What Veyndesk can learn from RustDesk's code

Studied: `rustdesk/rustdesk` @ 4b92080 (2026-10-20) incl. `libs/hbb_common`, and `rustdesk/rustdesk-server`.
RustDesk is AGPL-3.0 (Veyndesk is AGPL too, so ideas *and* code may be reused with attribution + AGPL compliance).

## 1. How a connection is found (the part that makes "any Wi-Fi" work)

| Step | RustDesk | Veyndesk today |
|---|---|---|
| Register | Client sends `RegisterPeer` over **UDP** to `hbbs` every few seconds (`REG_INTERVAL`); peer is "offline" after 30 s (`REG_TIMEOUT`). ID ownership proven by an Ed25519 key (`RegisterPk`). | WebSocket to our server, ID bound to a device key (same idea). |
| Find partner | `PunchHoleRequest{id, nat_type}` → server (`rendezvous_server.rs::handle_punch_hole_request`). | Viewer asks server, server relays everything. |
| Same network | Server compares the two peers' **public IPs**; if equal it sends `FetchLocalAddr` and they connect via **LAN addresses** directly (routers drop hairpin traffic). | UDP-21119 broadcast discovery. |
| Different networks | Server forwards `PunchHole` with each side's public address → **UDP hole punching**, **TCP punching**, IPv6, and now **WebRTC/ICE**, all raced against a relay (`client.rs` lines ~210-330: a direct result wins outright, a relay is held briefly in case direct succeeds). | Always relayed. |
| Fallback | `RequestRelay` → `hbbr` relay server pairs both sides by a `uuid` and copies bytes, with bandwidth limits and an IP blacklist (`relay_server.rs`). | Our server relays (no limits). |
| NAT detection | `TestNatRequest` to two ports → `NatType` (ASYMMETRIC / SYMMETRIC); symmetric NAT skips punching and goes to relay immediately. | — |
| Public servers | Hard-coded `RENDEZVOUS_SERVERS` (rs-ny…), run by the company. | Team server + free Cloudflare tunnel + team code. |

**Lesson:** relaying everything works, but P2P is what makes RustDesk/AnyDesk feel fast. Our server should become a *rendezvous* (introduce peers), with relay only as fallback.

## 2. Security
- Every peer has an Ed25519 identity key; server signs `(id, pk)` so clients can't be spoofed (`decode_id_pk`).
- Session key: viewer generates a random `secretbox` key, seals it with the host's Curve25519 public key (`create_symmetric_key_msg`) → **end-to-end encrypted** even through the relay. 
- Optional server key (`-k`) so only your clients can use your server; 2FA (`auth_2fa.rs`); login-failure lockout.
- **Veyndesk gap:** our relay sees screen data (only TLS to the server). Adopt the same seal-a-session-key scheme (e.g. `crypto_box`/`chacha20poly1305` crates).

## 3. Video
- Codecs: VP8 / **VP9 (default)** / AV1 in software; **H.264/H.265 hardware** (`hwcodec`, NVENC/QSV/AMF) and a **VRAM** path that encodes directly from the GPU texture (no copy to RAM).
- Capture: **DXGI Desktop Duplication** on Windows (`libs/scrap/src/dxgi`), plus X11/Wayland/macOS/Android.
- QoS (`video_qos.rs`): viewer replies to `TestDelay`; host tracks delay history minus RTT and adjusts **FPS (1–120, start 15) and bitrate ratio**; cautious recovery after congestion.
- **Veyndesk today:** GDI capture + JPEG tiles + ack-based quality. Good for text, heavy for video/games.
  Next: DXGI capture + H.264 (Media Foundation) decoded in the browser with WebCodecs; keep JPEG tiles as fallback.

## 4. Other features worth copying (in order of user value)
1. **Windows service** (`service.rs`, `platform/windows.rs`): runs as SYSTEM, sees the login/UAC screen, real Ctrl+Alt+Del via `SendSAS`.
2. **File transfer**, clipboard files, **audio** (`audio_service`), multi-monitor, **privacy mode** (black screen on host).
3. **Keyboard modes** (`message.proto`: Legacy / Map / Translate) — fixes non-US layouts.
4. **LAN discovery** sends broadcasts from **every network interface** (`lan.rs::create_broadcast_sockets`), not just one — our discovery should do the same (cheap fix, likely why discovery is flaky on some PCs).
5. Address book, online status (`OnlineRequest` bitmask), Wake-on-LAN, port forwarding, terminal.

## 5. Recommended Veyndesk roadmap from this study
1. **Quick wins (days):** broadcast on all interfaces; same-public-IP → LAN address (like `FetchLocalAddr`); keyboard layout mode.
2. **P2P (1–2 weeks):** STUN-style NAT test + UDP hole punching (or WebRTC via `webrtc-rs`/`str0m`), relay fallback; race them like `client.rs`.
3. **E2E encryption** with per-device keys.
4. **DXGI + H.264 + WebCodecs** video path.
5. **Windows service** + code signing (needed for Smart App Control anyway).
6. Own small VPS rendezvous/relay (replaces Cloudflare tunnel dependency).
