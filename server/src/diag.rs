//! Field diagnostics (task #109): one compact, greppable summary line per active
//! stream and per busy connection every [`DEFAULT_WINDOW`], plus a final line when a
//! stream or connection ends.
//!
//! Why: friends hit stalls and the owner used to have to ask them for a client log. The
//! relay sees both legs of every stream (sharer uplink = what arrives, viewer downlink =
//! what it can forward), so these lines are usually enough to tell "sharer sent nothing /
//! huge keyframes" from "relay -> viewer path lost packets or evicts" from "viewer keeps
//! re-subscribing". Format and fields: `superdoc/features/relay-transport.md`
//! "Diagnostics log". Lines are `diag stream ...` / `diag conn ...` followed by
//! `key=value` fields, INFO level, in the normal relay log.
//!
//! Hot-path cost: the media forwarder calls [`StreamDiag::on_fragment`] once per datagram,
//! which is one uncontended `std::sync::Mutex` lock, a header parse and a few integer
//! adds (no allocation). Everything else (the 1 s tick, the room walk, formatting) runs in
//! one background task ([`run`]); it takes each room mutex briefly once per second and
//! does nothing when there are no rooms.
//!
//! Why a 1 s tick and not a per-datagram clock: "max bytes per second" needs a second
//! bucket; sampling the running byte counter once per second keeps `Instant::now()` and
//! bucket logic out of the per-datagram path.
//!
//! Why the sampler keeps its own `Arc` of every stream route / connection: a stream that is
//! stopped (or a connection that leaves) vanishes from the room in the same instant, and
//! with it its counters. Holding the `Arc`s lets the next tick notice the disappearance and
//! print the final, partial window, with no hook in the stop/leave code paths.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use crate::protocol::{ConnId, MEDIA_FLAG_STALLED, MediaHeader, RoomId, StreamId, StreamKind};
use crate::room::{Registry, RoomState, TransportKind};

/// Cadence of the periodic summary lines.
pub const DEFAULT_WINDOW: Duration = Duration::from_secs(60);

/// An unsubscribe followed by a subscribe of the same viewer within this long counts as a
/// re-subscribe, which is how the native viewer asks for a keyframe (Unsubscribe+Subscribe,
/// `superdoc/features/video/viewer-recovery.md`).
const RESUB_WITHIN: Duration = Duration::from_secs(5);

/// A connection with less UDP traffic than this per window (keep-alive pings are ~1 KB per
/// minute) counts as idle and gets no line.
const CONN_IDLE_BYTES: u64 = 16 * 1024;

// ---------------------------------------------------------------------
// Per-stream counters
// ---------------------------------------------------------------------

/// Counters of one stream for the current window; see the module docs.
#[derive(Default, Debug, Clone)]
pub struct StreamWindow {
    /// Media bytes (header included) of datagrams that arrived from the sharer.
    pub bytes: u64,
    pub dgrams: u64,
    /// Distinct frame ids seen (a new frame starts when `frame_id` changes).
    pub frames: u32,
    pub keyframes: u32,
    /// Frames flagged `MEDIA_FLAG_STALLED` (the sharer re-sending its cached IDR while its
    /// capture source is down).
    pub stalled_frames: u32,
    /// Largest keyframe, as the fragment count its header declares.
    pub kf_max_dgrams: u16,
    /// Largest keyframe, in bytes that actually arrived.
    pub kf_max_bytes: u64,
    /// Highest bytes-per-second over any tick interval of the window.
    pub bytes_s_max: u64,
    pub subscribes: u32,
    pub unsubscribes: u32,
    /// Unsub -> sub of the same viewer within [`RESUB_WITHIN`].
    pub resubs: u32,
    /// Datagrams forwarded to a viewer whose send queue was full (older ones got evicted).
    pub evicted: u64,

    // Carried across windows, never reported:
    last_frame: Option<u32>,
    cur_is_kf: bool,
    cur_frame_bytes: u64,
    last_tick_bytes: u64,
    last_unsub: HashMap<ConnId, Instant>,
}

impl StreamWindow {
    fn on_fragment(&mut self, data: &[u8]) {
        let Ok((h, _)) = MediaHeader::unpack(data) else {
            return;
        };
        let len = data.len() as u64;
        self.bytes += len;
        self.dgrams += 1;
        if self.last_frame != Some(h.frame_id) {
            self.last_frame = Some(h.frame_id);
            self.frames += 1;
            self.cur_frame_bytes = 0;
            self.cur_is_kf = h.is_keyframe();
            if self.cur_is_kf {
                self.keyframes += 1;
                self.kf_max_dgrams = self.kf_max_dgrams.max(h.fragment_count);
            }
            if h.flags & MEDIA_FLAG_STALLED != 0 {
                self.stalled_frames += 1;
            }
        }
        self.cur_frame_bytes += len;
        if self.cur_is_kf {
            self.kf_max_bytes = self.kf_max_bytes.max(self.cur_frame_bytes);
        }
    }

    /// Sample the byte counter; `dt` is the seconds since the previous tick.
    fn tick(&mut self, dt: f64) {
        let delta = self.bytes.saturating_sub(self.last_tick_bytes);
        self.last_tick_bytes = self.bytes;
        if dt > 0.05 {
            self.bytes_s_max = self.bytes_s_max.max((delta as f64 / dt) as u64);
        }
    }

    fn on_subscribe(&mut self, viewer: ConnId, now: Instant) {
        self.subscribes += 1;
        if let Some(t) = self.last_unsub.remove(&viewer)
            && now.saturating_duration_since(t) <= RESUB_WITHIN
        {
            self.resubs += 1;
        }
    }

    fn on_unsubscribe(&mut self, viewer: ConnId, now: Instant) {
        self.unsubscribes += 1;
        self.last_unsub.insert(viewer, now);
    }

    /// Hand out the window's counters and start a new one. Per-frame carry-over state
    /// stays (a frame can straddle the boundary); old unsubscribe stamps are pruned so the
    /// map stays at most one entry per recent viewer.
    fn take(&mut self, now: Instant) -> StreamWindow {
        self.last_unsub
            .retain(|_, t| now.saturating_duration_since(*t) <= RESUB_WITHIN);
        let mut out = std::mem::take(self);
        *self = StreamWindow {
            last_frame: out.last_frame,
            cur_is_kf: out.cur_is_kf,
            cur_frame_bytes: out.cur_frame_bytes,
            last_unsub: std::mem::take(&mut out.last_unsub),
            ..Default::default()
        };
        out
    }
}

/// The shared handle on a stream's [`StreamWindow`]: the owner's media forwarder writes it
/// per datagram, the control plane per subscribe/unsubscribe, the sampler reads it.
#[derive(Default)]
pub struct StreamDiag(Mutex<StreamWindow>);

impl StreamDiag {
    fn lock(&self) -> std::sync::MutexGuard<'_, StreamWindow> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }
    /// One media datagram arrived from the sharer (the whole datagram, header included).
    pub fn on_fragment(&self, data: &[u8]) {
        self.lock().on_fragment(data);
    }
    /// One forward toward a viewer evicted older queued datagrams.
    pub fn on_eviction(&self) {
        self.lock().evicted += 1;
    }
    pub fn on_subscribe(&self, viewer: ConnId) {
        self.lock().on_subscribe(viewer, Instant::now());
    }
    pub fn on_unsubscribe(&self, viewer: ConnId) {
        self.lock().on_unsubscribe(viewer, Instant::now());
    }
}

// ---------------------------------------------------------------------
// Per-connection counters
// ---------------------------------------------------------------------

/// What the relay itself counts per connection (the rest comes from quinn's path stats):
/// datagrams forwarded *to* this connection into a full send queue.
#[derive(Default)]
pub struct ConnDiag {
    pub evicted: AtomicU64,
}

/// The quinn counters a connection line reports as deltas (plus the gauges).
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
struct PathSnap {
    sent: u64,
    lost: u64,
    congestion: u64,
    tx_bytes: u64,
    rx_bytes: u64,
    rtt_ms: u64,
    cwnd: u64,
    mtu: u16,
}

impl PathSnap {
    fn of(session: &web_transport_quinn::Session) -> Self {
        let quic: &quinn::Connection = session;
        let s = quic.stats();
        Self {
            sent: s.path.sent_packets,
            lost: s.path.lost_packets,
            congestion: s.path.congestion_events,
            tx_bytes: s.udp_tx.bytes,
            rx_bytes: s.udp_rx.bytes,
            rtt_ms: quic.rtt().as_millis() as u64,
            cwnd: s.path.cwnd,
            mtu: s.path.current_mtu,
        }
    }
}

// ---------------------------------------------------------------------
// Line formatting (pure)
// ---------------------------------------------------------------------

fn short_room(room: &RoomId) -> String {
    room[..4].iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Slow-changing identity of a stream, for the line.
struct StreamMeta {
    room: RoomId,
    owner: ConnId,
    owner_name: String,
    stream: StreamId,
    kind: StreamKind,
    label: String,
    viewers: u32,
}

/// `None` when the window is idle: nothing arrived, nobody watches, nothing happened.
fn stream_line(m: &StreamMeta, w: &StreamWindow, secs: f64, ended: bool) -> Option<String> {
    if w.bytes == 0 && m.viewers == 0 && w.subscribes == 0 && w.unsubscribes == 0 && w.evicted == 0
    {
        return None;
    }
    let secs = secs.max(0.001);
    Some(format!(
        "diag stream room={} owner={:016x} owner_name={:?} stream={} kind={:?} label={:?} \
         win_s={:.0} end={} viewers={} bytes_s_avg={} bytes_s_max={} dgrams={} frames={} \
         keyframes={} stalled_frames={} kf_max_dgrams={} kf_max_bytes={} \
         subs={} unsubs={} resubs={} evicted={}",
        short_room(&m.room),
        m.owner,
        m.owner_name,
        m.stream,
        m.kind,
        m.label,
        secs,
        ended as u8,
        m.viewers,
        (w.bytes as f64 / secs) as u64,
        w.bytes_s_max,
        w.dgrams,
        w.frames,
        w.keyframes,
        w.stalled_frames,
        w.kf_max_dgrams,
        w.kf_max_bytes,
        w.subscribes,
        w.unsubscribes,
        w.resubs,
        w.evicted,
    ))
}

struct ConnMeta {
    room: RoomId,
    conn: ConnId,
    name: String,
    transport: TransportKind,
    shares: u8,
    watching: usize,
}

/// `None` for an idle connection (see [`CONN_IDLE_BYTES`]) that evicted nothing.
fn conn_line(
    m: &ConnMeta,
    prev: &PathSnap,
    cur: &PathSnap,
    evicted: u64,
    secs: f64,
    ended: bool,
) -> Option<String> {
    let tx = cur.tx_bytes.saturating_sub(prev.tx_bytes);
    let rx = cur.rx_bytes.saturating_sub(prev.rx_bytes);
    if tx + rx < CONN_IDLE_BYTES && evicted == 0 {
        return None;
    }
    let secs = secs.max(0.001);
    Some(format!(
        "diag conn room={} conn={:016x} name={:?} transport={} win_s={:.0} end={} shares={} \
         watching={} tx_bytes_s={} rx_bytes_s={} sent={} lost={} congestion={} rtt_ms={} \
         cwnd={} mtu={} evicted={}",
        short_room(&m.room),
        m.conn,
        m.name,
        match m.transport {
            TransportKind::Native => "native",
            TransportKind::Browser => "browser",
        },
        secs,
        ended as u8,
        m.shares,
        m.watching,
        (tx as f64 / secs) as u64,
        (rx as f64 / secs) as u64,
        cur.sent.saturating_sub(prev.sent),
        cur.lost.saturating_sub(prev.lost),
        cur.congestion.saturating_sub(prev.congestion),
        cur.rtt_ms,
        cur.cwnd,
        cur.mtu,
        evicted,
    ))
}

// ---------------------------------------------------------------------
// Sampler
// ---------------------------------------------------------------------

type StreamKey = (RoomId, ConnId, StreamId);
type ConnKey = (RoomId, ConnId);

struct StreamSnap {
    key: StreamKey,
    route: Arc<crate::room::StreamRoute>,
    kind: StreamKind,
    label: String,
    owner_name: String,
    viewers: u32,
}

struct ConnSnap {
    key: ConnKey,
    name: String,
    transport: TransportKind,
    diag: Arc<ConnDiag>,
    session: web_transport_quinn::Session,
    shares: u8,
    watching: usize,
}

struct TrackedStream {
    route: Arc<crate::room::StreamRoute>,
    meta: StreamMeta,
    window_start: Instant,
}

impl TrackedStream {
    fn emit(&mut self, now: Instant, ended: bool) -> Option<String> {
        let w = self.route.diag.lock().take(now);
        let secs = now.saturating_duration_since(self.window_start).as_secs_f64();
        self.window_start = now;
        stream_line(&self.meta, &w, secs, ended)
    }
}

struct TrackedConn {
    meta: ConnMeta,
    diag: Arc<ConnDiag>,
    session: web_transport_quinn::Session,
    prev: PathSnap,
    window_start: Instant,
}

impl TrackedConn {
    fn emit(&mut self, now: Instant, ended: bool) -> Option<String> {
        let cur = PathSnap::of(&self.session);
        let evicted = self.diag.evicted.swap(0, Ordering::Relaxed);
        let secs = now.saturating_duration_since(self.window_start).as_secs_f64();
        let line = conn_line(&self.meta, &self.prev, &cur, evicted, secs, ended);
        self.prev = cur;
        self.window_start = now;
        line
    }
}

/// Owns the sampler state: which streams/connections are tracked and when each window
/// started. Driven by [`run`] once per second; tests call [`Sampler::sample`] directly.
pub struct Sampler {
    window: Duration,
    last_tick: Instant,
    streams: HashMap<StreamKey, TrackedStream>,
    conns: HashMap<ConnKey, TrackedConn>,
}

impl Sampler {
    pub fn new(window: Duration, now: Instant) -> Self {
        Self {
            window,
            last_tick: now,
            streams: HashMap::new(),
            conns: HashMap::new(),
        }
    }

    /// Walk the registry, advance every window and return the lines that are due.
    pub async fn sample(&mut self, registry: &Registry, now: Instant) -> Vec<String> {
        let (streams, conns) = snapshot(registry).await;
        self.observe(streams, conns, now)
    }

    fn observe(
        &mut self,
        streams: Vec<StreamSnap>,
        conns: Vec<ConnSnap>,
        now: Instant,
    ) -> Vec<String> {
        let dt = now.saturating_duration_since(self.last_tick).as_secs_f64();
        self.last_tick = now;
        let mut lines = Vec::new();

        let mut seen: HashSet<StreamKey> = HashSet::new();
        for s in streams {
            seen.insert(s.key);
            // A re-announced stream_id gets a fresh route: end the old one's window.
            if self
                .streams
                .get(&s.key)
                .is_some_and(|t| !Arc::ptr_eq(&t.route, &s.route))
                && let Some(mut old) = self.streams.remove(&s.key)
            {
                lines.extend(old.emit(now, true));
            }
            let t = self.streams.entry(s.key).or_insert_with(|| TrackedStream {
                route: s.route.clone(),
                meta: StreamMeta {
                    room: s.key.0,
                    owner: s.key.1,
                    owner_name: String::new(),
                    stream: s.key.2,
                    kind: s.kind,
                    label: String::new(),
                    viewers: 0,
                },
                window_start: now,
            });
            t.meta.owner_name = s.owner_name;
            t.meta.label = s.label;
            t.meta.viewers = s.viewers;
            t.route.diag.lock().tick(dt);
            if now.saturating_duration_since(t.window_start) >= self.window {
                lines.extend(t.emit(now, false));
            }
        }
        let gone: Vec<StreamKey> = self
            .streams
            .keys()
            .filter(|k| !seen.contains(*k))
            .copied()
            .collect();
        for k in gone {
            if let Some(mut t) = self.streams.remove(&k) {
                lines.extend(t.emit(now, true));
            }
        }

        let mut seen: HashSet<ConnKey> = HashSet::new();
        for c in conns {
            seen.insert(c.key);
            if self
                .conns
                .get(&c.key)
                .is_some_and(|t| !Arc::ptr_eq(&t.diag, &c.diag))
                && let Some(mut old) = self.conns.remove(&c.key)
            {
                lines.extend(old.emit(now, true));
            }
            let t = self.conns.entry(c.key).or_insert_with(|| TrackedConn {
                meta: ConnMeta {
                    room: c.key.0,
                    conn: c.key.1,
                    name: String::new(),
                    transport: c.transport,
                    shares: 0,
                    watching: 0,
                },
                diag: c.diag.clone(),
                session: c.session.clone(),
                prev: PathSnap::default(),
                window_start: now,
            });
            t.meta.name = c.name;
            t.meta.shares = c.shares;
            t.meta.watching = c.watching;
            if now.saturating_duration_since(t.window_start) >= self.window {
                lines.extend(t.emit(now, false));
            }
        }
        let gone: Vec<ConnKey> = self
            .conns
            .keys()
            .filter(|k| !seen.contains(*k))
            .copied()
            .collect();
        for k in gone {
            if let Some(mut t) = self.conns.remove(&k) {
                lines.extend(t.emit(now, true));
            }
        }
        lines
    }
}

fn collect_room(room_id: RoomId, r: &RoomState, streams: &mut Vec<StreamSnap>, conns: &mut Vec<ConnSnap>) {
    for s in r.streams.values() {
        streams.push(StreamSnap {
            key: (room_id, s.owner, s.stream_id),
            route: s.route.clone(),
            kind: s.kind,
            label: s.label.clone(),
            owner_name: r
                .connections
                .get(&s.owner)
                .map(|c| c.display_name.clone())
                .unwrap_or_default(),
            viewers: s.subscribers.len() as u32,
        });
    }
    for c in r.connections.values() {
        conns.push(ConnSnap {
            key: (room_id, c.conn_id),
            name: c.display_name.clone(),
            transport: c.transport,
            diag: c.diag.clone(),
            session: c.connection.clone(),
            shares: c.stream_count,
            watching: c.subscriptions.len(),
        });
    }
}

async fn snapshot(registry: &Registry) -> (Vec<StreamSnap>, Vec<ConnSnap>) {
    let rooms: Vec<_> = registry
        .rooms
        .read()
        .await
        .iter()
        .map(|(id, r)| (*id, r.clone()))
        .collect();
    let (mut streams, mut conns) = (Vec::new(), Vec::new());
    for (room_id, room) in rooms {
        collect_room(room_id, &*room.lock().await, &mut streams, &mut conns);
    }
    (streams, conns)
}

/// The background task: tick once per second, log what is due. Ends when the registry is
/// dropped.
pub async fn run(registry: Weak<Registry>, window: Duration) {
    let mut sampler = Sampler::new(window, Instant::now());
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        let Some(registry) = registry.upgrade() else {
            return;
        };
        for line in sampler.sample(&registry, Instant::now()).await {
            tracing::info!("{line}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{MEDIA_FLAG_KEYFRAME, MEDIA_HEADER_LEN, WIRE_VERSION};

    fn dgram(flags: u8, frame_id: u32, idx: u16, count: u16, body: usize) -> Vec<u8> {
        let h = MediaHeader {
            version: WIRE_VERSION,
            flags,
            conn_id: 1,
            stream_id: 0,
            frame_id,
            fragment_index: idx,
            fragment_count: count,
        };
        let mut v = h.pack().to_vec();
        v.resize(MEDIA_HEADER_LEN + body, 0);
        v
    }

    #[test]
    fn counts_distinct_frames_keyframes_and_sizes() {
        let mut w = StreamWindow::default();
        // delta frame 1 (1 fragment), keyframe 2 (3 fragments), delta frame 3 (2 fragments).
        w.on_fragment(&dgram(0, 1, 0, 1, 100));
        for i in 0..3 {
            w.on_fragment(&dgram(MEDIA_FLAG_KEYFRAME, 2, i, 3, 1000));
        }
        w.on_fragment(&dgram(0, 3, 0, 2, 10));
        w.on_fragment(&dgram(0, 3, 1, 2, 10));
        w.on_fragment(&[1, 2, 3]); // garbage: ignored
        assert_eq!((w.frames, w.keyframes, w.dgrams), (3, 1, 6));
        assert_eq!(w.kf_max_dgrams, 3);
        assert_eq!(w.kf_max_bytes, 3 * (MEDIA_HEADER_LEN as u64 + 1000));
        let all = (MEDIA_HEADER_LEN as u64 + 100)
            + 3 * (MEDIA_HEADER_LEN as u64 + 1000)
            + 2 * (MEDIA_HEADER_LEN as u64 + 10);
        assert_eq!(w.bytes, all);
    }

    #[test]
    fn stalled_frames_counted_once_per_frame() {
        let mut w = StreamWindow::default();
        w.on_fragment(&dgram(MEDIA_FLAG_KEYFRAME | MEDIA_FLAG_STALLED, 5, 0, 2, 10));
        w.on_fragment(&dgram(MEDIA_FLAG_KEYFRAME | MEDIA_FLAG_STALLED, 5, 1, 2, 10));
        assert_eq!((w.frames, w.stalled_frames, w.keyframes), (1, 1, 1));
    }

    #[test]
    fn max_bytes_per_second_uses_tick_deltas() {
        let mut w = StreamWindow::default();
        w.on_fragment(&dgram(0, 1, 0, 1, 1000 - MEDIA_HEADER_LEN));
        w.tick(1.0);
        assert_eq!(w.bytes_s_max, 1000);
        w.on_fragment(&dgram(0, 2, 0, 1, 3000 - MEDIA_HEADER_LEN));
        w.tick(2.0); // 3000 bytes over 2 s
        assert_eq!(w.bytes_s_max, 1500);
        w.tick(1.0); // idle second never lowers the max
        assert_eq!(w.bytes_s_max, 1500);
    }

    #[test]
    fn take_resets_counters_but_not_the_frame_in_progress() {
        let mut w = StreamWindow::default();
        let now = Instant::now();
        w.on_fragment(&dgram(MEDIA_FLAG_KEYFRAME, 7, 0, 2, 10));
        w.evicted += 1;
        let out = w.take(now);
        assert_eq!((out.frames, out.keyframes, out.evicted, out.dgrams), (1, 1, 1, 1));
        assert_eq!((w.frames, w.bytes, w.evicted, w.keyframes), (0, 0, 0, 0));
        // The second fragment of frame 7 arrives in the next window: not a new frame.
        w.on_fragment(&dgram(MEDIA_FLAG_KEYFRAME, 7, 1, 2, 10));
        assert_eq!((w.frames, w.keyframes), (0, 0));
    }

    #[test]
    fn resubscribe_needs_same_viewer_within_window() {
        let mut w = StreamWindow::default();
        let t0 = Instant::now();
        w.on_unsubscribe(1, t0);
        w.on_subscribe(1, t0 + Duration::from_millis(20));
        assert_eq!(w.resubs, 1);
        // Another viewer subscribing fresh is not a re-subscribe.
        w.on_subscribe(2, t0 + Duration::from_millis(30));
        assert_eq!(w.resubs, 1);
        // Unsub, then a sub far later (a person closing and reopening the watch).
        w.on_unsubscribe(1, t0 + Duration::from_secs(10));
        w.on_subscribe(1, t0 + Duration::from_secs(20));
        assert_eq!((w.resubs, w.subscribes, w.unsubscribes), (1, 3, 2));
        // A stamp is consumed by its sub: a second sub does not count again.
        w.on_unsubscribe(3, t0);
        w.on_subscribe(3, t0 + Duration::from_secs(1));
        w.on_subscribe(3, t0 + Duration::from_secs(2));
        assert_eq!(w.resubs, 2);
    }

    #[test]
    fn take_prunes_old_unsubscribe_stamps() {
        let mut w = StreamWindow::default();
        let t0 = Instant::now();
        w.on_unsubscribe(1, t0);
        w.on_unsubscribe(2, t0 + Duration::from_secs(58));
        w.take(t0 + Duration::from_secs(60));
        assert_eq!(w.last_unsub.len(), 1);
        assert!(w.last_unsub.contains_key(&2));
    }

    fn meta(viewers: u32) -> StreamMeta {
        StreamMeta {
            room: [0xab; 16],
            owner: 0xdead_beef,
            owner_name: "Anna \"A\"".into(),
            stream: 2,
            kind: StreamKind::Video,
            label: "Monitor 1".into(),
            viewers,
        }
    }

    #[test]
    fn stream_line_is_greppable_and_idle_windows_are_quiet() {
        let mut w = StreamWindow::default();
        assert!(stream_line(&meta(0), &w, 60.0, false).is_none(), "idle, no viewers");
        assert!(stream_line(&meta(1), &w, 60.0, false).is_some(), "viewers but no data = stall evidence");
        w.on_fragment(&dgram(MEDIA_FLAG_KEYFRAME, 1, 0, 1, 6000 - MEDIA_HEADER_LEN));
        w.tick(1.0);
        let line = stream_line(&meta(1), &w, 2.0, true).unwrap();
        assert!(line.starts_with("diag stream room=abababab owner=00000000deadbeef "), "{line}");
        for f in [
            r#"owner_name="Anna \"A\"""#,
            "stream=2",
            "kind=Video",
            r#"label="Monitor 1""#,
            "win_s=2",
            "end=1",
            "viewers=1",
            "bytes_s_avg=3000",
            "bytes_s_max=6000",
            "frames=1",
            "keyframes=1",
            "kf_max_dgrams=1",
            "kf_max_bytes=6000",
            "resubs=0",
            "evicted=0",
        ] {
            assert!(line.contains(f), "missing {f} in {line}");
        }
        assert!(!line.contains('\n'));
    }

    #[test]
    fn conn_line_reports_deltas_and_skips_idle() {
        let m = ConnMeta {
            room: [1; 16],
            conn: 0xff,
            name: "Bob".into(),
            transport: TransportKind::Browser,
            shares: 0,
            watching: 1,
        };
        let prev = PathSnap { sent: 100, lost: 1, congestion: 0, tx_bytes: 1000, rx_bytes: 500, ..Default::default() };
        let idle = PathSnap { sent: 112, tx_bytes: 1900, rx_bytes: 900, rtt_ms: 20, ..prev };
        assert!(conn_line(&m, &prev, &idle, 0, 60.0, false).is_none());
        assert!(conn_line(&m, &prev, &idle, 3, 60.0, false).is_some(), "evictions always log");
        let cur = PathSnap {
            sent: 1100,
            lost: 11,
            congestion: 2,
            tx_bytes: 1000 + 600_000,
            rx_bytes: 500,
            rtt_ms: 33,
            cwnd: 70_000,
            mtu: 1452,
        };
        let line = conn_line(&m, &prev, &cur, 5, 60.0, false).unwrap();
        for f in [
            "diag conn room=01010101 conn=00000000000000ff name=\"Bob\" transport=browser",
            "win_s=60 end=0",
            "watching=1",
            "tx_bytes_s=10000",
            "rx_bytes_s=0",
            "sent=1000",
            "lost=10",
            "congestion=2",
            "rtt_ms=33",
            "cwnd=70000",
            "mtu=1452",
            "evicted=5",
        ] {
            assert!(line.contains(f), "missing {f} in {line}");
        }
    }

    fn snap(key: StreamKey, route: &Arc<crate::room::StreamRoute>, viewers: u32) -> StreamSnap {
        StreamSnap {
            key,
            route: route.clone(),
            kind: StreamKind::Video,
            label: "Monitor 1".into(),
            owner_name: "Anna".into(),
            viewers,
        }
    }

    #[test]
    fn sampler_emits_per_window_and_a_final_line_when_the_stream_disappears() {
        let key = ([7; 16], 9, 0);
        let route = Arc::new(crate::room::StreamRoute::new(true));
        let t0 = Instant::now();
        let mut s = Sampler::new(Duration::from_secs(60), t0);

        // Second 1..59: data flows, no line yet.
        route.diag.on_fragment(&dgram(MEDIA_FLAG_KEYFRAME, 1, 0, 1, 500));
        for i in 1..60 {
            let lines = s.observe(vec![snap(key, &route, 1)], vec![], t0 + Duration::from_secs(i));
            assert!(lines.is_empty(), "no line before the window ends");
        }
        let lines = s.observe(vec![snap(key, &route, 1)], vec![], t0 + Duration::from_secs(61));
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("end=0") && lines[0].contains("frames=1"), "{}", lines[0]);

        // Next window: nothing arrives, one viewer -> still a line (stall evidence).
        let lines = s.observe(vec![snap(key, &route, 1)], vec![], t0 + Duration::from_secs(121));
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("frames=0") && lines[0].contains("bytes_s_avg=0"), "{}", lines[0]);

        // The stream is stopped mid-window after some traffic: a final line, once.
        route.diag.on_fragment(&dgram(0, 2, 0, 1, 500));
        let lines = s.observe(vec![], vec![], t0 + Duration::from_secs(131));
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("end=1") && lines[0].contains("win_s=10"), "{}", lines[0]);
        assert!(s.observe(vec![], vec![], t0 + Duration::from_secs(132)).is_empty());
    }

    #[test]
    fn sampler_ends_the_old_window_when_a_stream_id_is_reannounced() {
        let key = ([7; 16], 9, 0);
        let old = Arc::new(crate::room::StreamRoute::new(true));
        let new = Arc::new(crate::room::StreamRoute::new(true));
        let t0 = Instant::now();
        let mut s = Sampler::new(Duration::from_secs(60), t0);
        old.diag.on_fragment(&dgram(0, 1, 0, 1, 100));
        assert!(s.observe(vec![snap(key, &old, 0)], vec![], t0 + Duration::from_secs(1)).is_empty());
        let lines = s.observe(vec![snap(key, &new, 0)], vec![], t0 + Duration::from_secs(2));
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("end=1"), "{}", lines[0]);
    }

    #[test]
    fn sampler_stays_silent_without_streams() {
        let mut s = Sampler::new(Duration::from_secs(60), Instant::now());
        for i in 0..200 {
            assert!(s.observe(vec![], vec![], Instant::now() + Duration::from_secs(i)).is_empty());
        }
    }

    /// The whole chain against a real relay: sharer announces, viewer subscribes, media
    /// flows, the viewer re-subscribes, then the sharer leaves.
    #[tokio::test]
    async fn real_relay_produces_stream_and_conn_lines() {
        use crate::protocol::{ClientToRelay, RelayToClient};
        use crate::relay::tests::{await_room_state, insecure_client_endpoint, recv_ctrl, send_ctrl};
        use std::net::{IpAddr, Ipv4Addr};

        const SHARER: ConnId = 0xA000_0000_0000_0001;
        const VIEWER: ConnId = 0xB000_0000_0000_0002;
        let room_id: RoomId = [0x5a; 16];

        let registry = Arc::new(Registry::new((Ipv4Addr::LOCALHOST, 0).into()));
        let server = crate::relay::build_endpoint((IpAddr::V4(Ipv4Addr::LOCALHOST), 0).into())
            .expect("bind");
        let addr = server.local_addr().unwrap();
        tokio::spawn(crate::relay::serve(registry.clone(), server));
        let client = insecure_client_endpoint();

        let join = async |id: ConnId, name: &str| {
            let c = client.connect(addr, "localhost").unwrap().await.unwrap();
            let (mut tx, mut rx) = c.open_bi().await.unwrap();
            send_ctrl(&mut tx, &ClientToRelay::Join { conn_id: id, room_id, display_name: name.into() }).await;
            assert!(matches!(recv_ctrl(&mut rx).await, RelayToClient::PresenceSnapshot { .. }));
            (c, tx, rx)
        };
        let (sharer, mut s_tx, _s_rx) = join(SHARER, "Anna").await;
        send_ctrl(&mut s_tx, &ClientToRelay::AnnounceStream {
            stream_id: 0, kind: StreamKind::Video, label: "Monitor 1".into(), width: 1920, height: 1080,
        }).await;
        let (viewer, mut v_tx, _v_rx) = join(VIEWER, "Bob").await;
        send_ctrl(&mut v_tx, &ClientToRelay::Subscribe { target: SHARER, stream_id: 0 }).await;
        await_room_state(&registry, room_id, "subscribed", |r| {
            r.streams.get(&(SHARER, 0)).is_some_and(|s| s.subscribers.contains(&VIEWER))
        }).await;

        let t0 = Instant::now();
        let mut sampler = Sampler::new(Duration::from_secs(60), t0);
        assert!(sampler.sample(&registry, t0).await.is_empty());

        // 40 frames of 1 KB, frame 0 a keyframe; wait until the viewer has them all.
        let mk = |flags, frame_id| {
            let h = MediaHeader { version: WIRE_VERSION, flags, conn_id: SHARER, stream_id: 0,
                frame_id, fragment_index: 0, fragment_count: 1 };
            let mut v = h.pack().to_vec();
            v.resize(1000, 0);
            bytes::Bytes::from(v)
        };
        for f in 0..40 {
            sharer.send_datagram(mk(if f == 0 { MEDIA_FLAG_KEYFRAME } else { 0 }, f)).unwrap();
        }
        let mut got = 0;
        while got < 40 {
            tokio::time::timeout(Duration::from_secs(2), viewer.read_datagram())
                .await.expect("forwarded datagram").unwrap();
            got += 1;
        }
        send_ctrl(&mut v_tx, &ClientToRelay::Unsubscribe { target: SHARER, stream_id: 0 }).await;
        send_ctrl(&mut v_tx, &ClientToRelay::Subscribe { target: SHARER, stream_id: 0 }).await;
        await_room_state(&registry, room_id, "resubscribed", |r| {
            r.streams.get(&(SHARER, 0)).is_some_and(|s| {
                s.route.diag.lock().resubs == 1 && s.subscribers.contains(&VIEWER)
            })
        }).await;

        let lines = sampler.sample(&registry, t0 + Duration::from_secs(61)).await;
        let stream = lines.iter().find(|l| l.starts_with("diag stream ")).expect("stream line");
        for f in ["room=5a5a5a5a", r#"owner_name="Anna""#, r#"label="Monitor 1""#, "end=0", "viewers=1",
                  "dgrams=40", "frames=40", "keyframes=1", "kf_max_dgrams=1", "kf_max_bytes=1000",
                  "subs=2", "unsubs=1", "resubs=1", "evicted=0"] {
            assert!(stream.contains(f), "missing {f} in {stream}");
        }
        let conn = lines.iter().find(|l| l.starts_with("diag conn ") && l.contains(r#"name="Bob""#))
            .expect("viewer conn line (it received ~40 KB)");
        assert!(conn.contains("transport=native") && conn.contains("watching=1"), "{conn}");

        // The sharer leaves: the stream's final line is flagged end=1 (the connection's
        // own partial window is idle by then and stays quiet).
        sharer.close(0u32.into(), b"bye");
        let mut ended = Vec::new();
        for i in 0..100 {
            ended = sampler.sample(&registry, t0 + Duration::from_secs(62 + i)).await;
            if !ended.is_empty() { break; }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(ended.iter().any(|l| l.starts_with("diag stream ") && l.contains("end=1")), "{ended:?}");
    }
}
