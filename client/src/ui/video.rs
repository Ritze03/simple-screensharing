//! Per-stream GPU video views, kept in step with the session's watches.
//!
//! One [`VideoEntry`] per watched *video* stream, keyed by [`StreamKey`]
//! (room, owner, stream id: a `StreamId` is only unique within one
//! connection). [`VideoViews::sync`] runs once per frame in the shell and
//! creates a `VideoView` for every new watch (from its decoder's
//! `FrameSource`) and drops the ones whose watch is gone — dropping a view
//! stops its repaint waker.
//!
//! The view sits behind `Arc<Mutex<_>>` so a pop-out's
//! `show_viewport_deferred` closure (which must be `Send + Sync + 'static`)
//! can paint the same view the tile would. Only one of the two draws at a
//! time: while `popped` the tile shows a placeholder instead.
//!
//! **A pop-out survives its watch being re-created** (task #132). A reconnect, a relay
//! restart, an L3 recovery or the sharer stopping and re-sharing all end the watch and later
//! start a new one under a new `(ConnId, StreamId)` (`session::WantedStream`). The `popped`
//! flag lives on the entry and the entry is keyed by that stream key, so before #132 the
//! pop-out window closed by itself and the stream came back inline in its tile. Now `sync`
//! remembers a vanished popped entry by owner name + label ([`PendingPop`]) and gives the flag
//! to the entry of a *resumed* watch (`WatchEntry::resumed`) that matches, for
//! [`REWATCH_GRACE`].

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use egui::{Response, Ui, Vec2};
use simple_screensharing_client::decode::render::{Fit, VideoView};
use simple_screensharing_client::protocol::{ConnId, StreamId};
use simple_screensharing_client::decode::FrameSource;
use simple_screensharing_client::session::{AppState, REWATCH_GRACE, RoomHandle};

/// Identity of a remote stream across rooms.
pub type StreamKey = (RoomHandle, ConnId, StreamId);

/// One watched video stream's display state.
pub struct VideoEntry {
    pub view: Arc<Mutex<VideoView>>,
    /// Shown in its own window (`popout.rs`) instead of its tile.
    pub popped: bool,
    /// Set from inside the pop-out viewport when its window is closed by the
    /// OS (close button, Alt+F4); `popout.rs` turns it into `BringBack`.
    pub close_requested: Arc<AtomicBool>,
    /// (owner display name, stream label) while popped: what survives a re-created watch,
    /// refreshed by `sync` while the roster still has the stream.
    identity: Option<(String, String)>,
}

/// One watch as `sync` sees it, read off the session.
pub struct LiveWatch {
    pub key: StreamKey,
    pub source: FrameSource,
    /// (owner display name, stream label); `None` when the roster lacks them.
    pub identity: Option<(String, String)>,
    /// `WatchEntry::resumed`.
    pub resumed: bool,
}

/// A popped-out stream whose watch ended and may be resumed (see the module doc).
struct PendingPop {
    room: RoomHandle,
    identity: (String, String),
    until: Instant,
}

/// All video views, in watch order.
#[derive(Default)]
pub struct VideoViews {
    entries: HashMap<StreamKey, VideoEntry>,
    /// Insertion order, so tiles don't reshuffle (HashMap order is random).
    order: Vec<StreamKey>,
    pending: Vec<PendingPop>,
}

impl VideoEntry {
    fn new(source: simple_screensharing_client::decode::FrameSource) -> Self {
        Self {
            view: Arc::new(Mutex::new(VideoView::new(source))),
            popped: false,
            close_requested: Arc::new(AtomicBool::new(false)),
            identity: None,
        }
    }
}

impl VideoViews {
    /// Create views for new watches, drop views of ended ones.
    pub fn sync(&mut self, state: &AppState) {
        let mut live = Vec::new();
        for (handle, room) in &state.rooms {
            let watches = room.watches.lock().unwrap();
            for ((owner, stream_id), entry) in watches.iter() {
                let key = (*handle, *owner, *stream_id);
                // Only a popped entry (or a resumed watch that may inherit one) needs it.
                let needed = self.entries.get(&key).map_or(entry.resumed, |e| e.popped);
                let identity = needed.then(|| {
                    let name = room.members.iter().find(|m| m.conn_id == *owner)?.display_name.clone();
                    let label = room.streams.iter().find(|s| s.owner == *owner && s.stream_id == *stream_id)?.label.clone();
                    Some((name, label))
                });
                live.push(LiveWatch {
                    key,
                    source: entry.decoder.lock().unwrap().frames(),
                    identity: identity.flatten(),
                    resumed: entry.resumed,
                });
            }
        }
        // A suspended room keeps its handle, so its popped streams may still come back.
        let rooms: Vec<RoomHandle> = state.rooms.keys().chain(state.reconnects.keys()).copied().collect();
        self.sync_with(live, &rooms, Instant::now());
    }

    /// [`Self::sync`] on explicit input (the part tests drive).
    pub(super) fn sync_with(&mut self, live: Vec<LiveWatch>, rooms: &[RoomHandle], now: Instant) {
        for w in &live {
            let key = w.key;
            match self.entries.entry(key) {
                std::collections::hash_map::Entry::Vacant(slot) => {
                    let mut entry = VideoEntry::new(w.source.clone());
                    entry.popped = w.resumed && Self::take_pending(&mut self.pending, key.0, &w.identity, now);
                    entry.identity = w.identity.clone();
                    slot.insert(entry);
                    self.order.push(key);
                }
                // Same key, different decoder: the watch ended and a new
                // one took the key between two frames (the room's stream
                // id counters make this unlikely, not impossible). Keeping
                // the old view would show the dead stream's last frame.
                std::collections::hash_map::Entry::Occupied(mut slot) => {
                    if !slot.get().view.lock().unwrap().source().same_source(&w.source) {
                        let popped = slot.get().popped && w.resumed;
                        let mut entry = VideoEntry::new(w.source.clone());
                        entry.popped = popped;
                        entry.identity = w.identity.clone();
                        slot.insert(entry);
                    } else if slot.get().popped && w.identity.is_some() {
                        slot.get_mut().identity = w.identity.clone();
                    }
                }
            }
        }
        // Ended watches: a popped one is remembered, to be resumed with its window.
        let gone: Vec<StreamKey> = self.entries.keys().filter(|k| !live.iter().any(|w| w.key == **k)).copied().collect();
        for key in gone {
            if let Some(entry) = self.entries.remove(&key)
                && entry.popped
                && let Some(identity) = entry.identity
            {
                self.pending.push(PendingPop { room: key.0, identity, until: now + REWATCH_GRACE });
            }
        }
        self.order.retain(|k| self.entries.contains_key(k));
        self.pending.retain(|p| p.until > now && rooms.contains(&p.room));
    }

    /// Consume the pending pop-out of `room` whose identity is `identity`.
    fn take_pending(pending: &mut Vec<PendingPop>, room: RoomHandle, identity: &Option<(String, String)>, now: Instant) -> bool {
        let Some(identity) = identity.as_ref() else { return false };
        match pending.iter().position(|p| p.room == room && &p.identity == identity && p.until > now) {
            Some(i) => {
                pending.remove(i);
                true
            }
            None => false,
        }
    }

    pub fn get(&self, key: StreamKey) -> Option<&VideoEntry> {
        self.entries.get(&key)
    }

    pub(super) fn get_mut(&mut self, key: StreamKey) -> Option<&mut VideoEntry> {
        self.entries.get_mut(&key)
    }

    /// Every watched video stream, oldest watch first.
    pub fn keys(&self) -> Vec<StreamKey> {
        self.order.clone()
    }

    /// Popped-out streams, oldest first.
    pub fn popped(&self) -> Vec<StreamKey> {
        self.order.iter().copied().filter(|k| self.entries.get(k).is_some_and(|e| e.popped)).collect()
    }

    /// Allocate `size` in `ui` and draw `key`'s video letterboxed into it
    /// (black before the first frame). `None` if `key` isn't watched.
    pub fn show(&self, key: StreamKey, ui: &mut Ui, size: Vec2, fit: Fit) -> Option<Response> {
        let entry = self.entries.get(&key)?;
        Some(entry.view.lock().unwrap().show(ui, size, fit))
    }


    /// Draw `key`'s video letterboxed into `rect` without allocating layout
    /// space (black before the first frame). `None` if `key` isn't watched.
    pub fn paint(&self, key: StreamKey, ui: &Ui, rect: egui::Rect, fit: Fit) -> Option<egui::Rect> {
        let entry = self.entries.get(&key)?;
        Some(entry.view.lock().unwrap().paint(ui, rect, fit))
    }

    /// Newest frame size in pixels (`None` before the first frame).
    pub fn frame_size(&self, key: StreamKey) -> Option<(u32, u32)> {
        self.entries.get(&key)?.view.lock().unwrap().frame_size()
    }
}
