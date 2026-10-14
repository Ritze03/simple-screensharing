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

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use egui::{Rect, Response, Ui, Vec2};
use simple_screensharing_client::decode::render::{Fit, VideoView};
use simple_screensharing_client::protocol::{ConnId, StreamId};
use simple_screensharing_client::session::{AppState, RoomHandle};

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
}

/// All video views, in watch order.
#[derive(Default)]
pub struct VideoViews {
    entries: HashMap<StreamKey, VideoEntry>,
    /// Insertion order, so tiles don't reshuffle (HashMap order is random).
    order: Vec<StreamKey>,
}

impl VideoViews {
    /// Create views for new watches, drop views of ended ones.
    pub fn sync(&mut self, state: &AppState) {
        let mut live: Vec<StreamKey> = Vec::new();
        for (handle, room) in &state.rooms {
            let watches = room.watches.lock().unwrap();
            for ((owner, stream_id), entry) in watches.iter() {
                let key = (*handle, *owner, *stream_id);
                live.push(key);
                if let std::collections::hash_map::Entry::Vacant(slot) = self.entries.entry(key) {
                    let source = entry.decoder.lock().unwrap().frames();
                    slot.insert(VideoEntry {
                        view: Arc::new(Mutex::new(VideoView::new(source))),
                        popped: false,
                        close_requested: Arc::new(AtomicBool::new(false)),
                    });
                    self.order.push(key);
                }
            }
        }
        self.entries.retain(|k, _| live.contains(k));
        self.order.retain(|k| live.contains(k));
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

    /// Draw into `rect` without allocating; returns the picture rect.
    pub fn paint(&self, key: StreamKey, ui: &Ui, rect: Rect, fit: Fit) -> Option<Rect> {
        let entry = self.entries.get(&key)?;
        Some(entry.view.lock().unwrap().paint(ui, rect, fit))
    }

    /// Newest frame size in pixels (`None` before the first frame).
    pub fn frame_size(&self, key: StreamKey) -> Option<(u32, u32)> {
        self.entries.get(&key)?.view.lock().unwrap().frame_size()
    }
}
