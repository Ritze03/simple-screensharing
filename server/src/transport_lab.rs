//! Transport lab (test-only, every test `#[ignore]`d): measures what a
//! congestion-controller / datagram-buffer choice does to a video stream.
//! This produced the table in `superdoc/features/relay-transport.md`.
//!
//! One cell = one real relay (in-process, real `serve`, real `TransportTuning`
//! config) + a synthetic sharer + a synthetic viewer, both real quinn clients,
//! each reaching the relay through a UDP impairment proxy on loopback (loss,
//! one-way delay, a bottleneck rate with a tail-drop queue, blackouts). The
//! sharer sends frames shaped like the native client's (fragments of at most
//! 1000 B with the 19-byte header, one 2 s GOP, an IDR ~12x a delta); the
//! viewer reassembles them and tracks decodability like a decoder would.
//!
//! ```text
//! cargo test --release -p simple-screensharing-server transport_lab -- --ignored --nocapture
//! env: LAB_FILTER=substring of the cell name, LAB_SECS=seconds per cell (default 25),
//!      LAB_PAR=cells in parallel (default 6)
//! ```
//!
//! Reported per cell (measured after a 4 s warm-up):
//! - `arr` complete frames/s (every fragment arrived), `dec` decodable frames/s (a delta frame
//!   decodes only if it directly follows a decodable frame; a keyframe always does),
//! - `idr` complete keyframes / keyframes sent,
//! - `dead` seconds with no decodable frame, `maxgap` longest gap between decodable frames,
//! - `p50`/`p95` capture-to-complete latency of decodable frames,
//! - `ev%` share of forwarded datagrams that evicted older queued ones at the relay.
//!
//! `rec` cells let the viewer re-subscribe (rate-limited to 1/s) when it has had
//! no decodable frame for 400 ms, which makes the relay ask the sharer for a
//! keyframe: the recovery the viewer gets from task #101.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::net::UdpSocket;

use crate::cert::{CertManager, CertSource};
use crate::protocol::{
    ClientToRelay, MEDIA_FLAG_KEYFRAME, MEDIA_HEADER_LEN, MAX_MEDIA_DATAGRAM_LEN, MediaHeader,
    PROTOCOL_VERSION, RelayToClient, StreamKind,
};
use crate::relay::tests::{insecure_client_endpoint_with, recv_ctrl, send_ctrl, try_recv_ctrl};
use crate::relay::{build_endpoint_with, serve};
use crate::room::Registry;
use crate::transport::{CongestionControl, TransportTuning};

// ---------------------------------------------------------------------
// Impairment proxy
// ---------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct Impair {
    /// Random loss probability per packet.
    pub loss: f64,
    /// One-way delay.
    pub delay_ms: u64,
    /// Bottleneck rate (0 = none) with a tail-drop queue of `queue_ms`.
    pub rate_kbps: u64,
    pub queue_ms: u64,
}

struct Link {
    imp: Mutex<Impair>,
    blackout: AtomicBool,
    free_at: Mutex<Instant>,
}

impl Link {
    fn new(imp: Impair) -> Arc<Self> {
        Arc::new(Self { imp: Mutex::new(imp), blackout: AtomicBool::new(false), free_at: Mutex::new(Instant::now()) })
    }

    /// When the packet leaves the link, or `None` if it is dropped.
    fn schedule(&self, len: usize) -> Option<Instant> {
        if self.blackout.load(Ordering::Relaxed) {
            return None;
        }
        let imp = self.imp.lock().unwrap().clone();
        if imp.loss > 0.0 && rand::random::<f64>() < imp.loss {
            return None;
        }
        let now = Instant::now();
        let mut at = now;
        if imp.rate_kbps > 0 {
            let mut free = self.free_at.lock().unwrap();
            let start = (*free).max(now);
            if start - now > Duration::from_millis(imp.queue_ms) {
                return None;
            }
            *free = start + Duration::from_secs_f64(len as f64 * 8.0 / (imp.rate_kbps as f64 * 1000.0));
            at = *free;
        }
        Some(at + Duration::from_millis(imp.delay_ms))
    }
}

struct Proxy {
    addr: SocketAddr,
    /// client -> relay
    up: Arc<Link>,
    /// relay -> client
    down: Arc<Link>,
}

async fn start_proxy(relay: SocketAddr, up: Impair, down: Impair) -> Proxy {
    let front = Arc::new(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap());
    let addr = front.local_addr().unwrap();
    let (up_link, down_link) = (Link::new(up), Link::new(down));
    let proxy = Proxy { addr, up: up_link.clone(), down: down_link.clone() };
    // One ordered delay queue per direction and client: a task per packet
    // would let the multi-threaded runtime reorder packets, which no real
    // path does at this rate and which breaks the viewer's decode chain.
    type Queue = tokio::sync::mpsc::UnboundedSender<(Instant, Vec<u8>)>;
    fn pipe<F, Fut>(send: F) -> Queue
    where
        F: Fn(Vec<u8>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(Instant, Vec<u8>)>();
        tokio::spawn(async move {
            while let Some((at, d)) = rx.recv().await {
                tokio::time::sleep_until(at.into()).await;
                send(d).await;
            }
        });
        tx
    }
    tokio::spawn(async move {
        let mut upstream: HashMap<SocketAddr, (Arc<UdpSocket>, Queue)> = HashMap::new();
        let mut buf = vec![0u8; 2048];
        loop {
            let Ok((n, from)) = front.recv_from(&mut buf).await else { return };
            if !upstream.contains_key(&from) {
                let s = Arc::new(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap());
                s.connect(relay).await.unwrap();
                let (s_up, front_dn) = (s.clone(), front.clone());
                let q_up = pipe(move |d| {
                    let s = s_up.clone();
                    async move {
                        let _ = s.send(&d).await;
                    }
                });
                let q_dn = pipe(move |d| {
                    let f = front_dn.clone();
                    async move {
                        let _ = f.send_to(&d, from).await;
                    }
                });
                // relay -> client direction
                let (s2, down2) = (s.clone(), down_link.clone());
                tokio::spawn(async move {
                    let mut b = vec![0u8; 2048];
                    while let Ok(n) = s2.recv(&mut b).await {
                        if let Some(at) = down2.schedule(n) {
                            let _ = q_dn.send((at, b[..n].to_vec()));
                        }
                    }
                });
                upstream.insert(from, (s, q_up));
            }
            if let Some(at) = up_link.schedule(n) {
                let _ = upstream[&from].1.send((at, buf[..n].to_vec()));
            }
        }
    });
    proxy
}

// ---------------------------------------------------------------------
// Cell
// ---------------------------------------------------------------------

#[derive(Clone)]
pub struct Cell {
    pub name: String,
    pub relay: TransportTuning,
    /// Both the sharer's and the viewer's transport (the native client's `net.rs`).
    pub client: TransportTuning,
    pub fps: u32,
    pub kbps: u32,
    pub secs: u64,
    /// Sharer leg (sharer -> relay is `.0`, relay -> sharer `.1`).
    pub sharer_leg: (Impair, Impair),
    /// Viewer leg (viewer -> relay is `.0`, relay -> viewer `.1`; media travels on `.1`).
    pub viewer_leg: (Impair, Impair),
    pub recover: bool,
    /// Optional scripted blackout of the viewer leg: (from_s, to_s) after warm-up start.
    pub blackout: Option<(u64, u64)>,
    /// Sharer spreads a keyframe's fragments over this long instead of one burst
    /// (what a paced sender would do; 0 = burst like the native client today).
    pub pace_idr_ms: u64,
}

impl Cell {
    fn new(name: &str) -> Self {
        Self {
            name: name.into(),
            relay: TransportTuning::default(),
            client: TransportTuning::default(),
            fps: 60,
            kbps: 4000,
            secs: lab_secs(),
            sharer_leg: Default::default(),
            viewer_leg: Default::default(),
            recover: false,
            blackout: None,
            pace_idr_ms: 0,
        }
    }
}

fn lab_secs() -> u64 {
    std::env::var("LAB_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(25)
}

const WARMUP: Duration = Duration::from_secs(4);
const GOP_SECS: u32 = 2;
const IDR_FACTOR: u32 = 12;
const CONN_SHARER: u64 = 0xA000_0000_0000_0001;
const CONN_VIEWER: u64 = 0xB000_0000_0000_0002;

#[derive(Default, Debug)]
#[allow(dead_code)]
pub struct Report {
    pub frames_sent: u64,
    pub frames_complete: u64,
    pub frames_decodable: u64,
    pub idr_sent: u64,
    pub idr_complete: u64,
    pub dead_secs: u64,
    pub max_gap_ms: u64,
    pub p50_ms: u64,
    pub p95_ms: u64,
    pub evict_pct: f64,
    pub rx_kbps: u64,
    pub secs: f64,
}

struct SentFrame {
    at: Instant,
    key: bool,
}

#[derive(Default)]
struct Shared {
    sent: Mutex<HashMap<u32, SentFrame>>,
    want_key: AtomicBool,
    stop: AtomicBool,
    measuring: AtomicBool,
}

fn frame_delta_bytes(fps: u32, kbps: u32) -> usize {
    // One GOP carries one IDR (IDR_FACTOR x delta) and gop_frames-1 deltas.
    let gop_frames = (fps * GOP_SECS) as f64;
    let gop_bytes = kbps as f64 * 1000.0 / 8.0 * GOP_SECS as f64;
    (gop_bytes / (gop_frames - 1.0 + IDR_FACTOR as f64)) as usize
}

async fn sharer(ep: quinn::Endpoint, addr: SocketAddr, cell: Cell, sh: Arc<Shared>) {
    let conn = ep.connect(addr, "localhost").unwrap().await.expect("sharer handshake");
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    send_ctrl(&mut send, &ClientToRelay::Join { conn_id: CONN_SHARER, room_id: [7; 16], display_name: "s".into() }).await;
    let _ = recv_ctrl(&mut recv).await;
    send_ctrl(
        &mut send,
        &ClientToRelay::AnnounceStream { stream_id: 0, kind: StreamKind::Video, label: "lab".into(), width: 1920, height: 1080 },
    )
    .await;
    let sh2 = sh.clone();
    tokio::spawn(async move {
        while let Some(m) = try_recv_ctrl(&mut recv).await {
            if matches!(m, RelayToClient::KeyframeRequest { .. }) {
                sh2.want_key.store(true, Ordering::Relaxed);
            }
        }
    });
    let delta = frame_delta_bytes(cell.fps, cell.kbps);
    let payload_len = MAX_MEDIA_DATAGRAM_LEN - MEDIA_HEADER_LEN;
    let mut tick = tokio::time::interval(Duration::from_secs_f64(1.0 / cell.fps as f64));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let gop = cell.fps * GOP_SECS;
    let mut frame_id: u32 = 0;
    while !sh.stop.load(Ordering::Relaxed) {
        tick.tick().await;
        let key = frame_id % gop == 0 || sh.want_key.swap(false, Ordering::Relaxed);
        if key && frame_id % gop != 0 {
            // an on-demand IDR restarts the GOP clock only in the viewer's eyes; fine
        }
        let size = if key { delta * IDR_FACTOR as usize } else { delta };
        let n = size.div_ceil(payload_len).max(1);
        sh.sent.lock().unwrap().insert(frame_id, SentFrame { at: Instant::now(), key });
        for i in 0..n {
            if key && cell.pace_idr_ms > 0 && i > 0 && i % 8 == 0 {
                let groups = n.div_ceil(8) as u64;
                tokio::time::sleep(Duration::from_micros(cell.pace_idr_ms * 1000 / groups)).await;
            }
            let h = MediaHeader {
                version: PROTOCOL_VERSION,
                flags: if key { MEDIA_FLAG_KEYFRAME } else { 0 },
                conn_id: CONN_SHARER,
                stream_id: 0,
                frame_id,
                fragment_index: i as u16,
                fragment_count: n as u16,
            };
            let mut d = h.pack().to_vec();
            d.resize(MAX_MEDIA_DATAGRAM_LEN, 0x5a);
            let _ = conn.send_datagram(Bytes::from(d));
        }
        frame_id += 1;
    }
    conn.close(0u32.into(), b"done");
}

#[derive(Default)]
struct ViewerOut {
    complete: u64,
    decodable: u64,
    idr_complete: u64,
    decodable_times: Vec<Instant>,
    latencies_ms: Vec<u64>,
    rx_bytes: u64,
}

async fn viewer(ep: quinn::Endpoint, addr: SocketAddr, cell: Cell, sh: Arc<Shared>) -> ViewerOut {
    let conn = ep.connect(addr, "localhost").unwrap().await.expect("viewer handshake");
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    send_ctrl(&mut send, &ClientToRelay::Join { conn_id: CONN_VIEWER, room_id: [7; 16], display_name: "v".into() }).await;
    let _ = recv_ctrl(&mut recv).await;
    // wait for the stream to exist
    tokio::time::sleep(Duration::from_millis(500)).await;
    let sub = ClientToRelay::Subscribe { target: CONN_SHARER, stream_id: 0 };
    send_ctrl(&mut send, &sub).await;
    tokio::spawn(async move { while try_recv_ctrl(&mut recv).await.is_some() {} });

    struct Asm {
        count: u16,
        got: HashSet<u16>,
        key: bool,
    }
    let mut asm: HashMap<u32, Asm> = HashMap::new();
    let mut out = ViewerOut::default();
    let mut chain_ok = false;
    let mut last_dec: Option<u32> = None;
    let mut last_progress = Instant::now();
    let mut last_resub = Instant::now();
    let mut ticker = tokio::time::interval(Duration::from_millis(100));
    loop {
        tokio::select! {
            d = conn.read_datagram() => {
                let Ok(d) = d else { break };
                let Ok((h, _)) = MediaHeader::unpack(&d) else { continue };
                let measuring = sh.measuring.load(Ordering::Relaxed);
                if measuring {
                    out.rx_bytes += d.len() as u64;
                }
                let a = asm.entry(h.frame_id).or_insert_with(|| Asm { count: h.fragment_count, got: HashSet::new(), key: h.is_keyframe() });
                a.got.insert(h.fragment_index);
                if a.got.len() as u16 == a.count {
                    let key = a.key;
                    asm.remove(&h.frame_id);
                    asm.retain(|id, _| h.frame_id.saturating_sub(*id) < 600);
                    let now = Instant::now();
                    let decodable = if key {
                        chain_ok = true;
                        true
                    } else if chain_ok && last_dec.is_some_and(|l| h.frame_id == l + 1) {
                        true
                    } else {
                        chain_ok = false;
                        false
                    };
                    if decodable {
                        last_dec = Some(h.frame_id);
                        last_progress = now;
                    }
                    if measuring {
                        out.complete += 1;
                        if key {
                            out.idr_complete += 1;
                        }
                        if decodable {
                            out.decodable += 1;
                            out.decodable_times.push(now);
                            if let Some(s) = sh.sent.lock().unwrap().get(&h.frame_id) {
                                out.latencies_ms.push(now.duration_since(s.at).as_millis() as u64);
                            }
                        }
                    }
                }
            }
            _ = ticker.tick() => {
                if sh.stop.load(Ordering::Relaxed) { break; }
                if cell.recover && last_progress.elapsed() > Duration::from_millis(400) && last_resub.elapsed() > Duration::from_secs(1) {
                    last_resub = Instant::now();
                    let unsub = ClientToRelay::Unsubscribe { target: CONN_SHARER, stream_id: 0 };
                    send_ctrl(&mut send, &unsub).await;
                    send_ctrl(&mut send, &sub).await;
                }
            }
        }
    }
    out
}

pub async fn run_cell(cell: Cell) -> Report {
    let registry = Arc::new(Registry::new((Ipv4Addr::LOCALHOST, 0).into()));
    let certs = CertManager::new(CertSource::ephemeral()).unwrap();
    certs.set_tuning(cell.relay.clone());
    let endpoint = build_endpoint_with((IpAddr::V4(Ipv4Addr::LOCALHOST), 0).into(), &certs).unwrap();
    let relay_addr = endpoint.local_addr().unwrap();
    registry.set_cert_info(certs.info());
    tokio::spawn(serve(registry.clone(), endpoint.clone()));

    let p_sharer = start_proxy(relay_addr, cell.sharer_leg.0.clone(), cell.sharer_leg.1.clone()).await;
    let p_viewer = start_proxy(relay_addr, cell.viewer_leg.0.clone(), cell.viewer_leg.1.clone()).await;

    let sh = Arc::new(Shared::default());
    let ep_s = insecure_client_endpoint_with(Some(cell.client.build()));
    let ep_v = insecure_client_endpoint_with(Some(cell.client.build()));
    let sharer_task = tokio::spawn(sharer(ep_s, p_sharer.addr, cell.clone(), sh.clone()));
    let viewer_task = tokio::spawn(viewer(ep_v, p_viewer.addr, cell.clone(), sh.clone()));

    tokio::time::sleep(WARMUP).await;
    let (fwd0, ev0) = (
        registry.datagrams_forwarded.load(Ordering::Relaxed),
        registry.datagrams_evicting.load(Ordering::Relaxed),
    );
    let sent_before = sh.sent.lock().unwrap().len() as u64;
    let idr_before = sh.sent.lock().unwrap().values().filter(|f| f.key).count() as u64;
    let t0 = Instant::now();
    sh.measuring.store(true, Ordering::Relaxed);
    if let Some((from, to)) = cell.blackout {
        let link = p_viewer.down.clone();
        let link_up = p_viewer.up.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(from)).await;
            link.blackout.store(true, Ordering::Relaxed);
            link_up.blackout.store(true, Ordering::Relaxed);
            tokio::time::sleep(Duration::from_secs(to - from)).await;
            link.blackout.store(false, Ordering::Relaxed);
            link_up.blackout.store(false, Ordering::Relaxed);
        });
    }
    tokio::time::sleep(Duration::from_secs(cell.secs)).await;
    sh.measuring.store(false, Ordering::Relaxed);
    let secs = t0.elapsed().as_secs_f64();
    let (fwd1, ev1) = (
        registry.datagrams_forwarded.load(Ordering::Relaxed),
        registry.datagrams_evicting.load(Ordering::Relaxed),
    );
    let (sent_after, idr_after) = {
        let s = sh.sent.lock().unwrap();
        (s.len() as u64, s.values().filter(|f| f.key).count() as u64)
    };
    sh.stop.store(true, Ordering::Relaxed);
    let out = viewer_task.await.unwrap();
    let _ = sharer_task.await;
    endpoint.close(0u32.into(), b"done");

    // dead seconds / max gap from decodable frame times
    let mut dead = 0;
    for s in 0..cell.secs {
        let (a, b) = (t0 + Duration::from_secs(s), t0 + Duration::from_secs(s + 1));
        if !out.decodable_times.iter().any(|t| *t >= a && *t < b) {
            dead += 1;
        }
    }
    let mut marks = vec![t0];
    marks.extend(out.decodable_times.iter().cloned());
    marks.push(t0 + Duration::from_secs_f64(secs));
    let max_gap = marks.windows(2).map(|w| w[1].duration_since(w[0]).as_millis() as u64).max().unwrap_or(0);
    let mut lat = out.latencies_ms.clone();
    lat.sort();
    let pct = |p: f64| lat.get(((lat.len() as f64 - 1.0) * p) as usize).copied().unwrap_or(0);
    Report {
        frames_sent: sent_after - sent_before,
        frames_complete: out.complete,
        frames_decodable: out.decodable,
        idr_sent: idr_after - idr_before,
        idr_complete: out.idr_complete,
        dead_secs: dead,
        max_gap_ms: max_gap,
        p50_ms: pct(0.5),
        p95_ms: pct(0.95),
        evict_pct: if fwd1 > fwd0 { 100.0 * (ev1 - ev0) as f64 / (fwd1 - fwd0) as f64 } else { 0.0 },
        rx_kbps: (out.rx_bytes as f64 * 8.0 / 1000.0 / secs) as u64,
        secs,
    }
}

fn fmt_report(name: &str, r: &Report) -> String {
    format!(
        "{name:<46} arr {:>5.1}  dec {:>5.1}  idr {:>2}/{:<2}  dead {:>2}s  maxgap {:>5}ms  p50 {:>4}  p95 {:>5}  ev {:>5.1}%  rx {:>5}kbps",
        r.frames_complete as f64 / r.secs,
        r.frames_decodable as f64 / r.secs,
        r.idr_complete,
        r.idr_sent,
        r.dead_secs,
        r.max_gap_ms,
        r.p50_ms,
        r.p95_ms,
        r.evict_pct,
        r.rx_kbps,
    )
}

// ---------------------------------------------------------------------
// Matrix
// ---------------------------------------------------------------------

fn tuning(cc: CongestionControl, buf_kb: usize) -> TransportTuning {
    TransportTuning { cc, datagram_send_buffer: buf_kb * 1024, ..Default::default() }
}

/// (label, relay tuning, client tuning). The client column is the sharer's uplink
/// controller (the viewer sends no media), so it follows the relay column.
fn configs() -> Vec<(&'static str, TransportTuning, TransportTuning)> {
    use CongestionControl::*;
    vec![
        ("cubic/1M", tuning(Cubic, 1024), tuning(Cubic, 1024)),
        ("newreno/1M", tuning(NewReno, 1024), tuning(NewReno, 1024)),
        ("bbr/1M", tuning(Bbr, 1024), tuning(Bbr, 1024)),
        ("bbr/256K", tuning(Bbr, 256), tuning(Bbr, 256)),
        ("bbr/512K", tuning(Bbr, 512), tuning(Bbr, 512)),
        ("bbr/128K", tuning(Bbr, 128), tuning(Bbr, 128)),
        ("cubic/128K", tuning(Cubic, 128), tuning(Cubic, 128)),
    ]
}

fn matrix() -> Vec<Cell> {
    let mut cells = Vec::new();
    let only: Option<Vec<String>> = std::env::var("LAB_CONFIGS").ok().map(|s| s.split(',').map(String::from).collect());
    for (label, relay, client) in configs() {
        if only.as_ref().is_some_and(|o| !o.iter().any(|x| x == label)) {
            continue;
        }
        let mk = |name: String, f: &dyn Fn(&mut Cell)| {
            let mut c = Cell::new(&format!("{label} {name}"));
            c.relay = relay.clone();
            c.client = client.clone();
            f(&mut c);
            c
        };
        // Random loss on the viewer leg, both directions, 4 Mbit/s 60 fps.
        for rtt in [20u64, 80] {
            for loss in [0.0, 0.005, 0.01, 0.02] {
                for rec in [false, true] {
                    cells.push(mk(format!("loss{:.1}% rtt{rtt} {}", loss * 100.0, if rec { "rec" } else { "raw" }), &|c| {
                        let i = Impair { loss, delay_ms: rtt / 2, ..Default::default() };
                        c.viewer_leg = (i.clone(), i);
                        c.recover = rec;
                    }));
                }
            }
        }
        // Narrow / near-stream-rate bottleneck on the viewer leg, rtt 40 ms.
        for rate in [2500u64, 3500, 4200, 5000, 8000] {
            for q in [50u64, 150, 300] {
                cells.push(mk(format!("narrow{rate}k q{q}ms rec"), &|c| {
                    let down = Impair { delay_ms: 10, rate_kbps: rate, queue_ms: q, ..Default::default() };
                    let up = Impair { delay_ms: 10, ..Default::default() };
                    c.viewer_leg = (up, down);
                    c.recover = true;
                }));
            }
        }
        // Loss on the SHARER leg (uplink governed by the client's controller).
        for loss in [0.01, 0.02] {
            cells.push(mk(format!("uplink loss{:.0}% rtt40 rec", loss * 100.0), &|c| {
                let i = Impair { loss, delay_ms: 10, ..Default::default() };
                c.sharer_leg = (i.clone(), i);
                c.recover = true;
            }));
        }
        // Blackout of the viewer leg: 6 s outage.
        cells.push(mk("blackout6s rtt40 rec".into(), &|c| {
            let i = Impair { delay_ms: 10, ..Default::default() };
            c.viewer_leg = (i.clone(), i);
            c.recover = true;
            c.blackout = Some((6, 12));
        }));
        // Higher bitrate: 12 Mbit/s 60 fps (IDR ~290 KB), 120 fps/8 Mbit (IDR ~190 KB).
        for (fps, kbps) in [(60u32, 12000u32), (120, 8000)] {
            let tag = format!("{kbps}k@{fps}");
            cells.push(mk(format!("hi {tag} clean rtt40 rec"), &|c| {
                c.fps = fps;
                c.kbps = kbps;
                let i = Impair { delay_ms: 10, ..Default::default() };
                c.viewer_leg = (i.clone(), i);
                c.recover = true;
            }));
            cells.push(mk(format!("hi {tag} loss1% rtt40 rec"), &|c| {
                c.fps = fps;
                c.kbps = kbps;
                let i = Impair { loss: 0.01, delay_ms: 10, ..Default::default() };
                c.viewer_leg = (i.clone(), i);
                c.recover = true;
            }));
            for q in [50u64, 150] {
                cells.push(mk(format!("hi {tag} link{}k q{q}ms rec", kbps * 5 / 4), &|c| {
                    c.fps = fps;
                    c.kbps = kbps;
                    let down = Impair { delay_ms: 10, rate_kbps: kbps as u64 * 5 / 4, queue_ms: q, ..Default::default() };
                    c.viewer_leg = (Impair { delay_ms: 10, ..Default::default() }, down);
                    c.recover = true;
                }));
            }
        }
        // Does a paced sender (IDR spread over 60 ms) fix the shallow-queue burst loss?
        for rate in [5000u64, 8000] {
            cells.push(mk(format!("paced narrow{rate}k q50ms rec"), &|c| {
                let down = Impair { delay_ms: 10, rate_kbps: rate, queue_ms: 50, ..Default::default() };
                c.viewer_leg = (Impair { delay_ms: 10, ..Default::default() }, down);
                c.recover = true;
                c.pace_idr_ms = 60;
            }));
        }
        // Narrow SHARER uplink (governed by the client's controller).
        for (rate, q) in [(4200u64, 150u64), (5000, 100), (8000, 50)] {
            cells.push(mk(format!("uplink narrow{rate}k q{q}ms rec"), &|c| {
                let up = Impair { delay_ms: 10, rate_kbps: rate, queue_ms: q, ..Default::default() };
                c.sharer_leg = (up, Impair { delay_ms: 10, ..Default::default() });
                c.recover = true;
            }));
        }
    }
    let filter = std::env::var("LAB_FILTER").ok();
    // LAB_FILTER: comma-separated substrings, a cell runs if it matches any.
    cells.retain(|c| filter.as_ref().is_none_or(|f| f.split(',').any(|s| c.name.contains(s))));
    cells
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "lab: minutes of wall-clock, prints a table"]
async fn lab_matrix() {
    let cells = matrix();
    let par: usize = std::env::var("LAB_PAR").ok().and_then(|s| s.parse().ok()).unwrap_or(6);
    let sem = Arc::new(tokio::sync::Semaphore::new(par));
    let done = Arc::new(AtomicU64::new(0));
    let total = cells.len();
    let mut tasks = Vec::new();
    for cell in cells {
        let (sem, done) = (sem.clone(), done.clone());
        tasks.push(tokio::spawn(async move {
            let _p = sem.acquire().await.unwrap();
            let name = cell.name.clone();
            let r = run_cell(cell).await;
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            let line = fmt_report(&name, &r);
            eprintln!("[{n}/{total}] {line}");
            if let Ok(path) = std::env::var("LAB_OUT") {
                use std::io::Write;
                if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                    let _ = writeln!(f, "{line}");
                }
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
}

// ---------------------------------------------------------------------
// Idle timeout / keep-alive checks
// ---------------------------------------------------------------------

async fn join_only(ep: &quinn::Endpoint, addr: SocketAddr, id: u64) -> quinn::Connection {
    let conn = ep.connect(addr, "localhost").unwrap().await.expect("handshake");
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    send_ctrl(&mut send, &ClientToRelay::Join { conn_id: id, room_id: [9; 16], display_name: "q".into() }).await;
    let _ = recv_ctrl(&mut recv).await;
    // keep the control stream open in the background (as the real client does)
    tokio::spawn(async move {
        let _keep = send;
        while try_recv_ctrl(&mut recv).await.is_some() {}
    });
    conn
}

/// Seconds until a connection whose path goes dark (all packets dropped from
/// t=3 s) is declared dead by quinn, for each idle-timeout setting.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "lab: about a minute"]
async fn lab_idle_blackhole() {
    let mut tasks = Vec::new();
    for (label, idle, ka) in [("60s/15s (old)", 60u64, 15u64), ("20s/5s (new)", 20, 5)] {
        tasks.push(tokio::spawn(async move {
            let t = TransportTuning {
                max_idle: Duration::from_secs(idle),
                keep_alive: Duration::from_secs(ka),
                ..Default::default()
            };
            let certs = CertManager::new(CertSource::ephemeral()).unwrap();
            certs.set_tuning(t.clone());
            let registry = Arc::new(Registry::new((Ipv4Addr::LOCALHOST, 0).into()));
            let endpoint = build_endpoint_with((IpAddr::V4(Ipv4Addr::LOCALHOST), 0).into(), &certs).unwrap();
            tokio::spawn(serve(registry, endpoint.clone()));
            let p = start_proxy(endpoint.local_addr().unwrap(), Impair::default(), Impair::default()).await;
            let ep = insecure_client_endpoint_with(Some(t.build()));
            let conn = join_only(&ep, p.addr, 1).await;
            tokio::time::sleep(Duration::from_secs(3)).await;
            p.up.blackout.store(true, Ordering::Relaxed);
            p.down.blackout.store(true, Ordering::Relaxed);
            let t0 = Instant::now();
            let why = conn.closed().await;
            eprintln!(
                "idle {label}: black-holed connection declared dead after {:.1}s ({why})",
                t0.elapsed().as_secs_f64()
            );
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
}

/// Old and new settings on either end must interoperate: a quiet joined
/// connection (no media) stays up for 45 s in every combination.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "lab: 45 s"]
async fn lab_idle_compat() {
    let old = TransportTuning::legacy();
    let new = TransportTuning::default();
    let mut tasks = Vec::new();
    for (rl, relay) in [("old-relay", old.clone()), ("new-relay", new.clone())] {
        for (cl, client) in [("old-client", old.clone()), ("new-client", new.clone())] {
            let (relay, client) = (relay.clone(), client.clone());
            tasks.push(tokio::spawn(async move {
                let certs = CertManager::new(CertSource::ephemeral()).unwrap();
                certs.set_tuning(relay);
                let registry = Arc::new(Registry::new((Ipv4Addr::LOCALHOST, 0).into()));
                let endpoint = build_endpoint_with((IpAddr::V4(Ipv4Addr::LOCALHOST), 0).into(), &certs).unwrap();
                tokio::spawn(serve(registry, endpoint.clone()));
                let ep = insecure_client_endpoint_with(Some(client.build()));
                let conn = join_only(&ep, endpoint.local_addr().unwrap(), 1).await;
                let alive = tokio::time::timeout(Duration::from_secs(45), conn.closed()).await.is_err();
                eprintln!("compat {rl} + {cl}: quiet connection still open after 45s = {alive}");
                assert!(alive, "{rl} + {cl}: connection died while idle");
            }));
        }
    }
    for t in tasks {
        t.await.unwrap();
    }
}
