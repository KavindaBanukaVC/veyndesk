//! Capture → dirty-tile detection → adaptive JPEG encoding → per-session send.
//!
//! Why this beats "send a full frame every time":
//! * Only 64×64 tiles that actually changed are encoded (a typing user changes ~1% of the screen).
//! * Each viewer acks every painted frame; at most `MAX_INFLIGHT` frames are unacked, so a
//!   slow link never builds up seconds of lag — it just gets fewer, fresher frames.
//! * Quality adapts to congestion (fast link → sharp, slow link → lighter JPEGs), and when the
//!   screen goes still, blurry tiles are re-sent at high quality with 4:4:4 chroma
//!   ("progressive refinement"), so text ends up crisp.
//! * Capture stops entirely when nobody is watching.

use crate::Outgoing;
use jpeg_encoder::{ColorType, Encoder, SamplingFactor};
use rayon::prelude::*;
use std::{
    collections::{HashMap, VecDeque},
    sync::{mpsc, Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, warn};
use veyndesk_protocol::{encode_frame, HostEvent, HostToServer, MonitorInfo, Tile};

const TILE: usize = 64;
const MAX_INFLIGHT: usize = 2;
const FRAME_INTERVAL: Duration = Duration::from_millis(33); // ~30 fps cap
const Q_MIN: u8 = 30;
const Q_MAX: u8 = 80;
const Q_REFINE: u8 = 92;
const REFINE_AFTER: Duration = Duration::from_millis(350);
const REFINE_TILES_PER_FRAME: usize = 96;
const MAX_RUN: usize = 8;

pub enum VideoCmd {
    Add(u32),
    Remove(u32),
    Ack(u32, u32),
    Refresh(u32),
    SelectMonitor(u32),
}

/// State shared with the input thread / network task.
#[derive(Default)]
pub struct Shared {
    /// Top-left of the captured monitor in virtual-desktop coordinates.
    pub origin: Mutex<(i32, i32)>,
    pub size: Mutex<(u32, u32)>,
    /// Captured pixels per input-coordinate unit (2.0 on a Retina Mac, 1.0 elsewhere).
    pub scale: Mutex<f32>,
    pub monitors: Mutex<Vec<MonitorInfo>>,
    pub active_monitor: Mutex<u32>,
}

pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
    pub origin: (i32, i32),
    /// Image pixels per OS input coordinate (macOS reports monitors in points).
    pub scale: f32,
}

/// Not required to be `Send`: on Windows the capture handles (HMONITOR) are thread-bound,
/// so sources are constructed inside the video thread via a [`SourceFactory`].
pub trait FrameSource {
    fn monitors(&mut self) -> Vec<MonitorInfo>;
    fn capture(&mut self, index: u32) -> anyhow::Result<Frame>;
}

// ------------------------------------------------------------------ sources

pub struct XcapSource {
    cache: Vec<xcap::Monitor>,
}

impl XcapSource {
    pub fn new() -> Self {
        Self { cache: vec![] }
    }
    fn refresh(&mut self) {
        let mut list = xcap::Monitor::all().unwrap_or_default();
        // Primary first so index 0 is always the main screen.
        list.sort_by_key(|m| !m.is_primary().unwrap_or(false));
        self.cache = list;
    }
}

impl Default for XcapSource {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameSource for XcapSource {
    fn monitors(&mut self) -> Vec<MonitorInfo> {
        self.refresh();
        self.cache
            .iter()
            .enumerate()
            .map(|(i, m)| MonitorInfo {
                index: i as u32,
                name: m.friendly_name().or_else(|_| m.name()).unwrap_or_else(|_| format!("Display {}", i + 1)),
                width: m.width().unwrap_or(0),
                height: m.height().unwrap_or(0),
                primary: m.is_primary().unwrap_or(false),
            })
            .collect()
    }

    fn capture(&mut self, index: u32) -> anyhow::Result<Frame> {
        if self.cache.is_empty() {
            self.refresh();
        }
        let m = self
            .cache
            .get(index as usize)
            .or_else(|| self.cache.first())
            .ok_or_else(|| anyhow::anyhow!("no monitors found"))?;
        let origin = (m.x().unwrap_or(0), m.y().unwrap_or(0));
        let img = match m.capture_image() {
            Ok(i) => i,
            Err(e) => {
                self.cache.clear(); // monitor layout may have changed
                return Err(e.into());
            }
        };
        let (w, h) = (img.width() as usize, img.height() as usize);
        let logical_w = m.width().unwrap_or(w as u32).max(1);
        let scale = if cfg!(target_os = "macos") { w as f32 / logical_w as f32 } else { 1.0 };
        Ok(Frame { width: w, height: h, rgba: img.into_raw(), origin, scale })
    }
}

/// Animated synthetic screen for headless testing (`VEYNDESK_TEST_PATTERN=1`).
pub struct TestPattern {
    t: u32,
}

impl TestPattern {
    pub fn new() -> Self {
        Self { t: 0 }
    }
}

impl Default for TestPattern {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameSource for TestPattern {
    fn monitors(&mut self) -> Vec<MonitorInfo> {
        vec![MonitorInfo { index: 0, name: "Test pattern".into(), width: 1280, height: 720, primary: true }]
    }
    fn capture(&mut self, _index: u32) -> anyhow::Result<Frame> {
        let (w, h) = (1280usize, 720usize);
        self.t += 1;
        let mut rgba = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                rgba[i] = (x * 255 / w) as u8;
                rgba[i + 1] = (y * 255 / h) as u8;
                rgba[i + 2] = 160;
                rgba[i + 3] = 255;
            }
        }
        // A moving white square so there is always something dirty.
        let bx = (self.t as usize * 8) % (w - 100);
        for y in 300..400 {
            for x in bx..bx + 100 {
                let i = (y * w + x) * 4;
                rgba[i..i + 3].copy_from_slice(&[255, 255, 255]);
            }
        }
        Ok(Frame { width: w, height: h, rgba, origin: (0, 0), scale: 1.0 })
    }
}

// ------------------------------------------------------------------ sessions

struct Sess {
    dirty: Vec<bool>,
    tile_q: Vec<u8>,
    inflight: VecDeque<(u32, Instant)>,
    seq: u32,
    q: u8,
    wanted: u32,
    blocked: u32,
    last_eval: Instant,
    last_change: Instant,
    rtt_ms: f32,
    bytes: usize,
    frames: u32,
    last_stats: Instant,
}

impl Sess {
    fn new(n: usize) -> Self {
        let now = Instant::now();
        Self {
            dirty: vec![true; n],
            tile_q: vec![0; n],
            inflight: VecDeque::new(),
            seq: 0,
            q: 65,
            wanted: 0,
            blocked: 0,
            last_eval: now,
            last_change: now,
            rtt_ms: 50.0,
            bytes: 0,
            frames: 0,
            last_stats: now,
        }
    }
    fn reset(&mut self, n: usize) {
        self.dirty = vec![true; n];
        self.tile_q = vec![0; n];
    }

    fn on_ack(&mut self, seq: u32) {
        while let Some(&(s, sent)) = self.inflight.front() {
            if s.wrapping_sub(seq) as i32 > 0 {
                break; // ack for an older frame than the front; keep the rest
            }
            self.inflight.pop_front();
            if s == seq {
                let rtt = sent.elapsed().as_secs_f32() * 1000.0;
                self.rtt_ms = self.rtt_ms * 0.8 + rtt * 0.2;
            }
        }
    }

    /// AIMD-style quality control driven by how often we were blocked on acks.
    fn adapt(&mut self) {
        if self.last_eval.elapsed() < Duration::from_millis(500) {
            return;
        }
        let ratio = if self.wanted > 0 { self.blocked as f32 / self.wanted as f32 } else { 0.0 };
        if ratio > 0.35 || self.rtt_ms > 400.0 {
            self.q = self.q.saturating_sub(10).max(Q_MIN);
        } else if ratio < 0.1 && self.rtt_ms < 150.0 {
            self.q = (self.q + 3).min(Q_MAX);
        }
        self.wanted = 0;
        self.blocked = 0;
        self.last_eval = Instant::now();
    }
}

struct Grid {
    w: usize,
    h: usize,
    cols: usize,
    rows: usize,
}

impl Grid {
    fn new(w: usize, h: usize) -> Self {
        Self { w, h, cols: w.div_ceil(TILE), rows: h.div_ceil(TILE) }
    }
    fn n(&self) -> usize {
        self.cols * self.rows
    }
    fn rect(&self, col: usize, row: usize, run: usize) -> (usize, usize, usize, usize) {
        let x = col * TILE;
        let y = row * TILE;
        let w = (run * TILE).min(self.w - x);
        let h = TILE.min(self.h - y);
        (x, y, w, h)
    }
}

/// Which tiles differ between two frames of the same size.
fn changed_tiles(g: &Grid, prev: &[u8], cur: &[u8]) -> Vec<bool> {
    let stride = g.w * 4;
    (0..g.n())
        .into_par_iter()
        .map(|i| {
            let (col, row) = (i % g.cols, i / g.cols);
            let (x, y, w, h) = g.rect(col, row, 1);
            (y..y + h).any(|yy| {
                let s = yy * stride + x * 4;
                prev[s..s + w * 4] != cur[s..s + w * 4]
            })
        })
        .collect()
}

/// Horizontal runs of selected tiles → rects (col, row, run_len).
fn runs(g: &Grid, sel: &[bool]) -> Vec<(usize, usize, usize)> {
    let mut out = vec![];
    for row in 0..g.rows {
        let mut col = 0;
        while col < g.cols {
            if sel[row * g.cols + col] {
                let start = col;
                while col < g.cols && sel[row * g.cols + col] && col - start < MAX_RUN {
                    col += 1;
                }
                out.push((start, row, col - start));
            } else {
                col += 1;
            }
        }
    }
    out
}

fn encode_rect(frame: &Frame, x: usize, y: usize, w: usize, h: usize, q: u8) -> Vec<u8> {
    let stride = frame.width * 4;
    let mut buf = Vec::with_capacity(w * h * 4);
    for yy in y..y + h {
        let s = yy * stride + x * 4;
        buf.extend_from_slice(&frame.rgba[s..s + w * 4]);
    }
    let mut out = Vec::with_capacity(w * h / 4);
    let mut enc = Encoder::new(&mut out, q);
    enc.set_sampling_factor(if q >= Q_REFINE { SamplingFactor::F_1_1 } else { SamplingFactor::F_2_2 });
    if let Err(e) = enc.encode(&buf, w as u16, h as u16, ColorType::Rgba) {
        warn!("jpeg encode failed: {e}");
    }
    out
}

/// Encode selected tiles of `frame` and return the binary message (with sid prefix).
fn build_frame(frame: &Frame, g: &Grid, sid: u32, seq: u32, sel: &[bool], q: u8) -> (Vec<u8>, usize) {
    let rects = runs(g, sel);
    let encoded: Vec<(usize, usize, usize, usize, Vec<u8>)> = rects
        .par_iter()
        .map(|&(col, row, run)| {
            let (x, y, w, h) = g.rect(col, row, run);
            (x, y, w, h, encode_rect(frame, x, y, w, h, q))
        })
        .collect();
    let tiles: Vec<Tile> = encoded
        .iter()
        .map(|(x, y, w, h, j)| Tile { x: *x as u16, y: *y as u16, w: *w as u16, h: *h as u16, jpeg: j })
        .collect();
    let bytes = encoded.iter().map(|e| e.4.len()).sum();
    (encode_frame(Some(sid), seq, g.w as u16, g.h as u16, &tiles), bytes)
}

fn send_event(out: &UnboundedSender<Outgoing>, sid: u32, msg: HostEvent) {
    let m = HostToServer::ToViewer { sid, msg };
    let _ = out.send(Outgoing::Text(serde_json::to_string(&m).unwrap()));
}

// ------------------------------------------------------------------ main loop

pub type SourceFactory = Box<dyn FnOnce() -> Box<dyn FrameSource> + Send>;

pub fn spawn_video(make_source: SourceFactory, out: UnboundedSender<Outgoing>, shared: Arc<Shared>) -> mpsc::Sender<VideoCmd> {
    let (tx, rx) = mpsc::channel::<VideoCmd>();
    std::thread::Builder::new()
        .name("veyndesk-video".into())
        .spawn(move || {
            let mut source = make_source();
            let mons = source.monitors();
            if let Some(m) = mons.first() {
                *shared.size.lock().unwrap() = (m.width, m.height);
            }
            *shared.monitors.lock().unwrap() = mons;
            video_loop(source.as_mut(), rx, out, shared)
        })
        .expect("spawn video thread");
    tx
}

fn video_loop(source: &mut dyn FrameSource, rx: mpsc::Receiver<VideoCmd>, out: UnboundedSender<Outgoing>, shared: Arc<Shared>) {
    let mut sessions: HashMap<u32, Sess> = HashMap::new();
    let mut prev: Option<Frame> = None;
    let mut grid = Grid::new(0, 0);
    let mut monitor: u32 = 0;
    let mut cmds: Vec<VideoCmd> = vec![];

    loop {
        // Idle: block until somebody connects (no capture, ~0% CPU).
        if sessions.is_empty() {
            prev = None;
            match rx.recv() {
                Ok(c) => cmds.push(c),
                Err(_) => return,
            }
        }
        while let Ok(c) = rx.try_recv() {
            cmds.push(c);
        }
        for c in cmds.drain(..) {
            match c {
                VideoCmd::Add(sid) => {
                    let mons = source.monitors();
                    send_event(&out, sid, HostEvent::Monitors { list: mons.clone(), active: monitor });
                    *shared.monitors.lock().unwrap() = mons;
                    sessions.insert(sid, Sess::new(grid.n()));
                }
                VideoCmd::Remove(sid) => {
                    sessions.remove(&sid);
                }
                VideoCmd::Ack(sid, seq) => {
                    if let Some(s) = sessions.get_mut(&sid) {
                        s.on_ack(seq);
                    }
                }
                VideoCmd::Refresh(sid) => {
                    if let Some(s) = sessions.get_mut(&sid) {
                        s.reset(grid.n());
                    }
                }
                VideoCmd::SelectMonitor(idx) => {
                    monitor = idx;
                    *shared.active_monitor.lock().unwrap() = idx;
                    prev = None;
                }
            }
        }
        if sessions.is_empty() {
            continue;
        }

        let started = Instant::now();
        let frame = match source.capture(monitor) {
            Ok(f) => f,
            Err(e) => {
                // e.g. Windows secure desktop (UAC / lock screen) — keep sessions, retry.
                debug!("capture failed: {e}");
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
        };

        if frame.width != grid.w || frame.height != grid.h {
            grid = Grid::new(frame.width, frame.height);
            prev = None;
            for s in sessions.values_mut() {
                s.reset(grid.n());
            }
            *shared.size.lock().unwrap() = (frame.width as u32, frame.height as u32);
        }
        *shared.origin.lock().unwrap() = frame.origin;
        *shared.scale.lock().unwrap() = if frame.scale > 0.0 { frame.scale } else { 1.0 };

        let changed = match &prev {
            Some(p) => changed_tiles(&grid, &p.rgba, &frame.rgba),
            None => vec![true; grid.n()],
        };
        let any_changed = changed.iter().any(|&c| c);
        for s in sessions.values_mut() {
            if any_changed {
                s.last_change = Instant::now();
            }
            for (d, c) in s.dirty.iter_mut().zip(&changed) {
                *d |= *c;
            }
        }

        for (&sid, s) in sessions.iter_mut() {
            // Recover from a stuck viewer (e.g. a tab in the background).
            if let Some(&(_, t)) = s.inflight.front() {
                if t.elapsed() > Duration::from_secs(3) {
                    s.inflight.clear();
                }
            }
            let has_dirty = s.dirty.iter().any(|&d| d);
            if has_dirty {
                s.wanted += 1;
            }
            if s.inflight.len() >= MAX_INFLIGHT {
                if has_dirty {
                    s.blocked += 1;
                }
                s.adapt();
                continue;
            }

            let (sel, q) = if has_dirty {
                (std::mem::replace(&mut s.dirty, vec![false; grid.n()]), s.q)
            } else if s.last_change.elapsed() > REFINE_AFTER {
                let mut sel = vec![false; grid.n()];
                let mut n = 0;
                for (i, &tq) in s.tile_q.iter().enumerate() {
                    if tq < Q_REFINE && n < REFINE_TILES_PER_FRAME {
                        sel[i] = true;
                        n += 1;
                    }
                }
                if n == 0 {
                    s.adapt();
                    continue;
                }
                (sel, Q_REFINE)
            } else {
                s.adapt();
                continue;
            };

            for (tq, &on) in s.tile_q.iter_mut().zip(&sel) {
                if on {
                    *tq = q;
                }
            }
            s.seq = s.seq.wrapping_add(1);
            let (msg, bytes) = build_frame(&frame, &grid, sid, s.seq, &sel, q);
            s.inflight.push_back((s.seq, Instant::now()));
            s.bytes += bytes;
            s.frames += 1;
            let _ = out.send(Outgoing::Binary(msg));
            s.adapt();

            let el = s.last_stats.elapsed().as_secs_f32();
            if el >= 2.0 {
                send_event(
                    &out,
                    sid,
                    HostEvent::Stats {
                        fps: s.frames as f32 / el,
                        quality: s.q,
                        kbps: (s.bytes as f32 * 8.0 / 1000.0 / el) as u32,
                    },
                );
                s.frames = 0;
                s.bytes = 0;
                s.last_stats = Instant::now();
            }
        }

        prev = Some(frame);
        let spent = started.elapsed();
        if spent < FRAME_INTERVAL {
            std::thread::sleep(FRAME_INTERVAL - spent);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_merge_and_cap() {
        let g = Grid::new(64 * 10, 64 * 2);
        let mut sel = vec![false; g.n()];
        for s in sel.iter_mut().take(10) {
            *s = true; // full first row
        }
        sel[13] = true;
        let r = runs(&g, &sel);
        assert_eq!(r, vec![(0, 0, 8), (8, 0, 2), (3, 1, 1)]);
    }

    #[test]
    fn partial_edge_tiles() {
        let g = Grid::new(100, 70);
        assert_eq!((g.cols, g.rows), (2, 2));
        assert_eq!(g.rect(1, 1, 1), (64, 64, 36, 6));
    }

    #[test]
    fn detects_change_and_encodes() {
        let mut src = TestPattern::new();
        let a = src.capture(0).unwrap();
        let b = src.capture(0).unwrap();
        let g = Grid::new(a.width, a.height);
        let ch = changed_tiles(&g, &a.rgba, &b.rgba);
        let n = ch.iter().filter(|&&c| c).count();
        assert!(n > 0 && n < g.n() / 4, "only the moving square should be dirty, got {n}");
        let (msg, bytes) = build_frame(&b, &g, 9, 1, &ch, 60);
        assert!(bytes > 0);
        let f = veyndesk_protocol::decode_frame(&msg[4..]).unwrap();
        assert_eq!(f.width as usize, a.width);
        assert!(f.tiles.iter().all(|t| t.4.starts_with(&[0xFF, 0xD8])));
    }
}
