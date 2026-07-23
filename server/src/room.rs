//! Room / connection / stream state tracking (spec §10).
//!
//! Concurrency design: the whole registry (`HashMap<RoomId, RoomState>`) is
//! guarded by a single `tokio::sync::RwLock`. Rooms are looked up/mutated on
//! every control message and every media datagram/preview stream, so reads
//! vastly outnumber writes for *finding* a room, but almost every operation
//! inside a room also mutates something (subscriber sets, presence, caches).
//! A single `Mutex` around the whole map would serialize unrelated rooms
//! against each other for no reason; per-room locking would be the "more
//! correct" design but adds real complexity for a friends-scale (<=8
//! members/room, few rooms concurrently) hobby server. We split the
//! difference: an outer `RwLock` for registry membership (room
//! creation/deletion, which is rare) plus each `RoomState` wrapped in its
//! own `tokio::sync::Mutex` for the mutations that happen on every message.
//! This means a hot path only ever contends with other connections in the
//! *same* room, while adding/removing whole rooms is cheap and rare.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use quinn::Connection;
use tokio::sync::{Mutex, RwLock, mpsc};

use crate::protocol::{ConnId, RelayToClient, RoomId, StreamId, StreamKind};

/// A recent event string for a room's admin-log ring buffer (TUI Room
/// Detail tab).
#[derive(Debug, Clone)]
pub struct LogEvent {
    pub at: std::time::SystemTime,
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
            at: std::time::SystemTime::now(),
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
    /// The raw quinn connection handle, used to open uni streams (preview
    /// fan-out) and send datagrams (media forwarding) to this member.
    pub connection: Connection,
    /// Streams (owned by *other* connections) that this connection is
    /// currently subscribed to.
    pub subscriptions: HashSet<(ConnId, StreamId)>,
    /// Number of streams this connection currently owns (mirrors
    /// `Member.stream_count` sent to clients).
    pub stream_count: u8,
}

pub struct StreamState {
    pub owner: ConnId,
    pub stream_id: StreamId,
    pub kind: StreamKind,
    pub label: String,
    pub width: u16,
    pub height: u16,
    pub subscribers: HashSet<ConnId>,
    /// Total bytes of *incoming* media datagrams received for this
    /// stream (source bitrate, before subscriber fan-out). Sampled by
    /// the TUI once/sec for the Room Detail per-stream bandwidth readout.
    pub bytes_received: AtomicU64,
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
    pub room_id: RoomId,
    pub connections: HashMap<ConnId, ConnState>,
    pub streams: HashMap<(ConnId, StreamId), StreamState>,
    pub log: EventLog,
}

impl RoomState {
    pub fn new(room_id: RoomId) -> Self {
        Self {
            room_id,
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
}

impl Registry {
    pub fn new(listen_addr: std::net::SocketAddr) -> Self {
        Self {
            rooms: RwLock::new(HashMap::new()),
            bandwidth: ByteCounters::default(),
            started_at: std::time::Instant::now(),
            listen_addr,
        }
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
            .or_insert_with(|| Arc::new(Mutex::new(RoomState::new(room_id))))
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
