# Veyndesk architecture & roadmap

## How it works today

```
 ┌──────────── Host (Windows) ────────────┐            ┌──── velodesk-server ────┐         ┌── Viewer ──┐
 │ capture thread (xcap, 30 fps cap)      │            │ /ws/host   rendezvous   │         │ browser /  │
 │   → 64×64 tile diff vs previous frame  │  WSS       │ /ws/viewer relay        │  WSS    │ Tauri      │
 │   → per-viewer dirty set               │──binary──▶ │ strips sid, forwards    │──────▶  │ canvas     │
 │   → JPEG (rayon, SIMD) only if <2      │            │ / (web viewer)          │         │ decode →   │
 │     frames un-acked                    │◀──JSON──── │ /admin + /api/*         │◀─────── │ paint→ack  │
 │ input thread (enigo, VK codes)         │            │ data/hosts.json, audit  │  input  │            │
 └────────────────────────────────────────┘            └─────────────────────────┘         └────────────┘
```

* **Latency control** — viewer acks each painted frame; host keeps ≤ 2 frames in flight, so on a
  slow link you get fewer *fresh* frames instead of a growing backlog (the classic "lag builds up" bug).
* **Adaptive quality** — every 500 ms the host checks how often it was blocked on acks and the RTT;
  quality moves between 30 and 80 (AIMD).
* **Sharpening pass** — 350 ms after the screen stops changing, tiles sent at lower quality are
  re-sent at q92 with 4:4:4 chroma, so text is crisp when you're reading.
* **Idle = free** — no viewers → capture thread blocks; still screen → no frames.

## Phase 2 — performance (beat RustDesk on quality/latency)

1. **H.264 via WebCodecs**: hardware encoder on host (Media Foundation / NVENC / QuickSync / AMF),
   `VideoDecoder` in the browser. Keep JPEG tiles as fallback for old browsers and for text-heavy refine.
2. **DXGI Desktop Duplication** capture on Windows (GPU-side, gives dirty rects + cursor shape for free;
   much faster than GDI).
3. **Cursor channel**: send cursor shape/position separately and draw it locally → zero-latency pointer.
4. **WebRTC data/video channels** with STUN/TURN for **P2P** — server only does signaling,
   relay is a fallback. (Rust: `webrtc-rs` or `str0m`.)
5. Audio forwarding (WASAPI loopback → Opus).

## Phase 3 — product features

* **Accept/Deny prompt** in the desktop app for attended sessions (AnyDesk-style), view-only mode,
  per-session permissions (keyboard, clipboard, files).
* **File transfer** (drag & drop in viewer, chunked over the data channel).
* **Windows service** for unattended access: runs as SYSTEM, can capture the login screen / UAC
  secure desktop and send real Ctrl+Alt+Del via `SendSAS`.
* **End-to-end encryption**: X25519 key agreement (host key pinned in address book) + ChaCha20-Poly1305;
  server can no longer read relayed traffic.
* **Accounts & teams**: users, groups, device ownership, 2FA, SSO (OIDC), address book sync,
  per-group access policies — all in the free self-hosted server.
* Session recording, chat, wake-on-LAN, multi-viewer whiteboard.
* Linux (X11 + PipeWire/Wayland portal) and macOS hosts (ScreenCaptureKit), Android viewer.

## Phase 4 — scale

* Stateless relays behind a load balancer, Redis/Postgres for presence + audit.
* Prometheus metrics, structured audit export (SIEM).
