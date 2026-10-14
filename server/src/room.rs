//! Room / connection / stream state tracking (spec §10).
//!
//! # Concurrency design
//!
//! Two tiers, split by how often they are touched.
//!
//! **Control plane — `RwLock` registry + per-room `tokio::sync::Mutex`.**
//! The registry (`HashMap<RoomId, RoomState>`) is guarded by a
//! `tokio::sync::RwLock`: room creation/deletion is rare, lookups are not,
//! and a `Mutex` there would serialize unrelated rooms against each other
//! for no reason. Inside a room, everything that mutates shared room state
//! — presence, stream announcements, subscriber sets, the preview cache,
//! the admin log — happens under that room's own `tokio::sync::Mutex`.
//! Those operations are per *user action* (join, announce, subscribe,
//! leave), i.e. a handful per minute, so a single room-wide mutex is the
//! simplest thing that is obviously correct at friends scale (<=8
//! members/room).
//!
//! **Media data plane — lock-free [`StreamRoute`] snapshots, off the room
//! mutex entirely.** Media forwarding is *not* per user action: it runs
//! once per **fragment**. A frame is AEAD-encrypted once and then split
//! into ~1200-byte datagrams (spec §6.1/§6.3), so a single 60 fps sharer
//! emits hundreds of fragments per second, and a keyframe/IDR alone can be
//! hundreds of fragments. Each fragment previously re-acquired the
//! room-wide async mutex and held it across the whole per-subscriber
//! fan-out.
//!
//! **Why this changed:** that made lock-acquisition *volume* the
//! bottleneck. One sharer at a high capture framerate could saturate its
//! room's mutex, and because the lock is room-wide, it delayed every other
//! member's control messages *and* every other sharer's media forwarding —
//! the observed "one high-refresh-rate sharer slows down everyone else in
//! the room" symptom. The fan-out work itself was never the problem
//! (`quinn::Connection::send_datagram` is non-blocking); the serialization
//! point was.
//!
//! So the fan-out target list for each stream is now published as an
//! immutable snapshot in [`StreamRoute`], read without ever touching the
//! room mutex. [`StreamState::subscribers`] remains the **authority**; the
//! route is a derived cache, republished by [`RoomState::refresh_route`]
//! while the room mutex is still held, on every mutation of a subscriber
//! set. Reader/writer split:
//!
//! - **Readers** (per fragment, hot): clone the current `Arc<[RouteTarget]>`
//!   and release immediately.
//! - **Writers** (Subscribe/Unsubscribe/StopStream/leave, rare): rebuild
//!   the whole `Vec` and swap it in, under the room mutex.
//!
//! **Lock ordering (why this cannot deadlock):** a route's `RwLock` is a
//! strict *leaf*. Writers acquire it only while already holding the room
//! mutex, readers acquire it while holding nothing at all, and no code
//! path ever takes the room mutex (or awaits) while holding a route lock.
//! With no cycle possible, the only ordering rule to preserve is: never
//! reach for the room mutex from inside `StreamRoute`.
//!
//! **Why `std::sync::RwLock<Arc<[..]>>` and not `arc-swap` or
//! `tokio::sync::RwLock`:** the critical section is a single `Arc` clone —
//! it contains no `.await` and cannot block, so an async lock would buy
//! nothing and cost a scheduler round-trip. `arc-swap` would make the read
//! side genuinely lock-free rather than one uncontended atomic, but at a
//! few hundred reads/sec against writers measured in per-minute user
//! actions, that difference is unmeasurable here and not worth a new
//! dependency for a hobby-scale server. If a future profile ever shows
//! this read as hot, `ArcSwap<Vec<RouteTarget>>` is a drop-in replacement
//! for the two methods on `StreamRoute`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use web_transport_quinn::Session;
use tokio::sync::{Mutex, RwLock, mpsc};

/// `std`'s blocking `RwLock`, used *only* for the media-route snapshot
/// swap. Aliased to keep it visually distinct from the async
/// `tokio::sync::RwLock` that guards the registry — mixing the two up
/// would be a real bug, holding a blocking lock across an `.await`.
use std::sync::RwLock as SyncRwLock;

use crate::protocol::{ConnId, RelayToClient, RoomId, StreamId, StreamKind};

/// A recent event string for a room's admin-log ring buffer (TUI Room
/// Detail tab).
#[derive(Debug, Clone)]
pub struct LogEvent {
    pub text: String,
}

/// Bounded ring buffer of recent per-room events, for the TUI.
#[derive(Debug, Default)]
pub struct EventLog {
    buf: std::collections::VecDeque<LogEvent>,
    cap: usize,
}

impl EventLog {
    pub fn new(cap: usize) -> Self {
        Self {
            buf: std::collections::VecDeque::with_capacity(cap),
            cap,
        }
    }

    pub fn push(&mut self, text: impl Into<String>) {
        if self.buf.len() >= self.cap {
            self.buf.pop_front();
        }
        self.buf.push_back(LogEvent {
            text: text.into(),
        });
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &LogEvent> {
        self.buf.iter()
    }
}

/// Per-connection bandwidth counters (bytes forwarded by the relay),
/// sampled by the TUI once/sec to compute a rate. These count only
/// relayed *media datagram* bytes (the dominant traffic); control/preview
/// traffic is comparatively negligible for a bandwidth readout.
#[derive(Debug, Default)]
pub struct ByteCounters {
    pub rx: AtomicU64,
    pub tx: AtomicU64,
}

impl ByteCounters {
    pub fn add_rx(&self, n: u64) {
        self.rx.fetch_add(n, Ordering::Relaxed);
    }
    pub fn add_tx(&self, n: u64) {
        self.tx.fetch_add(n, Ordering::Relaxed);
    }
}

/// A handle to a live connection actor, held by `RoomState` and by the
/// relay's own dispatch loop.
pub struct ConnState {
    pub conn_id: ConnId,
    pub display_name: String,
    /// Channel into this connection's control-write task; sending here
    /// writes a `RelayToClient` frame on that connection's control bidi
    /// stream.
    pub control_tx: mpsc::UnboundedSender<RelayToClient>,
    /// The session handle, used to open uni streams (preview fan-out) and
    /// send datagrams (media forwarding) to this member. A native member is a
    /// raw-QUIC [`Session`], a browser a WebTransport one: same type, so
    /// everything downstream of the accept path is transport-agnostic (see
    /// `relay::handle_connection`).
    pub connection: Session,
    /// How this member connected (for the TUI only).
    pub transport: TransportKind,
    /// Streams (owned by *other* connections) that this connection is
    /// currently subscribed to.
    pub subscriptions: HashSet<(ConnId, StreamId)>,
    /// Number of streams this connection currently owns (mirrors
    /// `Member.stream_count` sent to clients).
    pub stream_count: u8,
}

/// One resolved fan-out destination for a stream's media datagrams: the
/// subscriber's id (for logging) plus a clone of its [`Session`] handle (cheap
/// — internally `Arc`s).
#[derive(Clone)]
pub struct RouteTarget {
    pub conn_id: ConnId,
    pub connection: Session,
}

/// Which transport a member arrived on. Only used for display; nothing in the
/// relay's logic branches on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    /// Raw QUIC, ALPN `ssshare/1` (the desktop client).
    Native,
    /// HTTP/3 WebTransport, ALPN `h3` (the browser viewer).
    Browser,
}

/// The media data plane for one stream: an immutable snapshot of the
/// current fan-out targets plus the stream's rx byte counter.
///
/// Shared as `Arc<StreamRoute>` between the room (which republishes the
/// snapshot under the room mutex) and the owning connection's media
/// forwarder task (which reads it per fragment, holding no room lock).
/// See the module docs for why the hot path may not touch the room mutex.
pub struct StreamRoute {
    /// Current fan-out targets, replaced wholesale (copy-on-write) by
    /// [`StreamRoute::publish`]. Derived from
    /// [`StreamState::subscribers`], which stays authoritative.
    targets: SyncRwLock<Arc<[RouteTarget]>>,
    /// Bytes of *incoming* media datagrams accepted for this stream
    /// (source bitrate, before subscriber fan-out). Sampled by the TUI
    /// once/sec for the Room Detail per-stream bandwidth readout.
    ///
    /// Counted by the forwarder, not by the receiving actor, so it
    /// excludes fragments shed by a saturated media queue — i.e. it
    /// reads as *forwarded* source bitrate. Under overload it therefore
    /// undercounts what the sender actually put on the wire; the
    /// process-wide `Registry::bandwidth.rx` counter is the one that
    /// still sees every arriving byte. That difference is deliberate:
    /// the per-stream readout is meant to reflect what the relay is
    /// really moving.
    pub bytes_received: AtomicU64,
}

impl StreamRoute {
    pub fn new() -> Self {
        Self {
            targets: SyncRwLock::new(Arc::from(Vec::new())),
            bytes_received: AtomicU64::new(0),
        }
    }

    /// Take a cheap handle on the current target set. Hot path: called
    /// once per media fragment. The lock is held only for the `Arc`
    /// clone, never across an `.await`.
    pub fn targets(&self) -> Arc<[RouteTarget]> {
        self.targets
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    /// Publish a new target set, replacing the previous snapshot.
    /// Callers must hold the room mutex so this stays consistent with
    /// the authoritative `subscribers` set.
    ///
    /// A fragment already past its `targets()` read may still be
    /// delivered to a target removed by this call. That is unavoidable
    /// (and harmless): even the old room-mutex design could not recall a
    /// datagram already handed to quinn, and a stale send just returns
    /// `Err` on a closed connection, which the forwarder ignores.
    pub fn publish(&self, targets: Vec<RouteTarget>) {
        *self
            .targets
            .write()
            .unwrap_or_else(|poison| poison.into_inner()) = Arc::from(targets);
    }
}

impl Default for StreamRoute {
    fn default() -> Self {
        Self::new()
    }
}

pub struct StreamState {
    pub owner: ConnId,
    pub stream_id: StreamId,
    pub kind: StreamKind,
    pub label: String,
    pub width: u16,
    pub height: u16,
    /// Authoritative subscriber set. **Every** mutation of this set must
    /// be followed by [`RoomState::refresh_route`] for this stream while
    /// the room mutex is still held, or the data plane will keep using a
    /// stale fan-out snapshot.
    pub subscribers: HashSet<ConnId>,
    /// Data-plane handle for this stream (fan-out snapshot + rx counter),
    /// shared with the owner's media forwarder task.
    pub route: Arc<StreamRoute>,
    /// Opaque cached ciphertext of the most recent preview blob for this
    /// stream (§7.2), including the 15-byte cleartext header — replayed
    /// verbatim to newcomers and never decrypted.
    pub last_preview: Option<Bytes>,
    /// Timestamp of the last `KeyframeRequest` sent to the owner for
    /// this stream, used purely for admin/debug visibility (the actual
    /// ~100ms coalescing policy is the *sender's* job per spec §9; the
    /// relay's own coalescing here is a light debounce so a burst of
    /// SUBSCRIBEs doesn't spam the owner with duplicate requests within
    /// the same tick).
    pub last_keyframe_request: Option<std::time::Instant>,
}

pub struct RoomState {
    pub connections: HashMap<ConnId, ConnState>,
    pub streams: HashMap<(ConnId, StreamId), StreamState>,
    pub log: EventLog,
}

impl RoomState {
    pub fn new() -> Self {
        Self {
            connections: HashMap::new(),
            streams: HashMap::new(),
            log: EventLog::new(200),
        }
    }

    /// Broadcast a `RelayToClient` message to every connection in the room
    /// except `except`, if given.
    pub fn broadcast(&self, msg: &RelayToClient, except: Option<ConnId>) {
        for (id, conn) in &self.connections {
            if Some(*id) == except {
                continue;
            }
            let _ = conn.control_tx.send(msg.clone());
        }
    }

    /// Recompute and publish the media fan-out snapshot for one stream
    /// from its authoritative `subscribers` set.
    ///
    /// Call this — still holding the room mutex — after *any* mutation of
    /// that set (Subscribe, Unsubscribe, a member leaving). Resolving
    /// `ConnId -> Connection` here, under the lock, is what lets the
    /// forwarding path skip the room mutex entirely. Subscribers with no
    /// live `ConnState` are skipped, matching the old inline
    /// `connections.get(sub)` lookup.
    ///
    /// No-op for an unknown stream (already stopped/removed) — the caller
    /// that removed it is responsible for having emptied its route.
    pub fn refresh_route(&self, key: (ConnId, StreamId)) {
        let Some(stream) = self.streams.get(&key) else {
            return;
        };
        let targets: Vec<RouteTarget> = stream
            .subscribers
            .iter()
            .filter_map(|sub| self.connections.get(sub))
            .map(|c| RouteTarget {
                conn_id: c.conn_id,
                connection: c.connection.clone(),
            })
            .collect();
        stream.route.publish(targets);
    }

    /// Send a `RelayToClient` message to exactly one connection, if it's
    /// still present in the room.
    pub fn send_to(&self, target: ConnId, msg: RelayToClient) {
        if let Some(conn) = self.connections.get(&target) {
            let _ = conn.control_tx.send(msg);
        }
    }
}

/// The shared registry of all live rooms, plus process-wide bandwidth
/// counters and startup metadata used by the TUI.
pub struct Registry {
    pub rooms: RwLock<HashMap<RoomId, Arc<Mutex<RoomState>>>>,
    pub bandwidth: ByteCounters,
    pub started_at: std::time::Instant,
    pub listen_addr: std::net::SocketAddr,
    /// Currently open connections that finished the QUIC handshake, by
    /// transport (joined or not). Shown by the TUI.
    pub native_connections: std::sync::atomic::AtomicUsize,
    pub browser_connections: std::sync::atomic::AtomicUsize,
    /// The TLS cert currently being served, published by
    /// [`crate::cert::CertManager::maintain`]. `None` until the relay has one.
    pub cert: std::sync::RwLock<Option<crate::cert::CertInfo>>,
}

impl Registry {
    pub fn new(listen_addr: std::net::SocketAddr) -> Self {
        Self {
            rooms: RwLock::new(HashMap::new()),
            bandwidth: ByteCounters::default(),
            started_at: std::time::Instant::now(),
            listen_addr,
            native_connections: Default::default(),
            browser_connections: Default::default(),
            cert: std::sync::RwLock::new(None),
        }
    }

    pub fn set_cert_info(&self, info: crate::cert::CertInfo) {
        *self.cert.write().unwrap_or_else(|p| p.into_inner()) = Some(info);
    }

    pub fn cert_info(&self) -> Option<crate::cert::CertInfo> {
        self.cert.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Get the room for `room_id`, creating it if this is the first
    /// member to join (spec §10: "rooms created on first join").
    pub async fn get_or_create_room(&self, room_id: RoomId) -> Arc<Mutex<RoomState>> {
        if let Some(room) = self.rooms.read().await.get(&room_id) {
            return room.clone();
        }
        let mut rooms = self.rooms.write().await;
        rooms
            .entry(room_id)
            .or_insert_with(|| Arc::new(Mutex::new(RoomState::new())))
            .clone()
    }

    /// Drop a room from the registry if it's empty (spec §10: "dropped
    /// when last member leaves"). Cheap best-effort check; caller must
    /// hold no lock on the room's own mutex when calling this (it takes
    /// a fresh lock to check emptiness).
    pub async fn drop_room_if_empty(&self, room_id: RoomId, room: &Arc<Mutex<RoomState>>) {
        let empty = room.lock().await.connections.is_empty();
        if empty {
            let mut rooms = self.rooms.write().await;
            // Re-check under the write lock: another join could have
            // raced in between the check above and taking this lock.
            if let Some(current) = rooms.get(&room_id) {
                if Arc::ptr_eq(current, room)
                    && current
                        .try_lock()
                        .map(|r| r.connections.is_empty())
                        .unwrap_or(false)
                {
                    rooms.remove(&room_id);
                }
            }
        }
    }
}
