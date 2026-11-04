//! The stage (tasks I1 and I3, `superdoc/ui/replica/index.html` "Stage"): the central area of a joined room and of
//! "Watched streams" is a black stage with the room's streams, the people strip under them and the control bar.
//!
//! - **Split** shows every picture whole in an even-ish grid (the row count that shows the most picture
//!   area), **Crop** fills the same cells (the video is cropped around its centre, `Fit::Cover`), **Floating**
//!   gives each stream a window you move and resize; windows keep the stream's aspect ratio. Split and Crop
//!   resize with the bars between streams; a double-click on a bar evens the grid out again. Dragging a tile
//!   onto another swaps them (Split, Crop). The maths is in [`geom`] (pure, tested), the tile contents in `tile`.
//! - **What is on it:** the room's streams of other people (watched ones as video, the others as a dimmed
//!   "X is sharing Y, Watch stream" tile, audio-only senders with a Listen button), then your own live share.
//!   "Watched" shows every watched stream of every room and nothing else. A popped-out stream is absent from
//!   the stage; its owner's avatar in the strip carries a badge that brings it back.
//! - **Under the streams:** the people strip (`strip.rs`: avatars, ring, LIVE badge, hover card) and the control
//!   bar (`bar.rs`: split buttons, layout, room, people, leave, pop out, fullscreen), their menus in `menus.rs`.
//!   Outside fullscreen they always show and the streams get what is left; in **fullscreen** they lie over the
//!   stage so the streams never jump, and fade after 3 s without movement (`chrome.rs`, the pure rule).
//! - **The whole stage can pop out** into its own window ([`show_windows`]): an immediate viewport that draws
//!   this same stage ([`Host::Window`]), while the main window shows a note and "Bring it back". *Why an
//!   immediate viewport:* the stage needs `Cx` and the video views, which a deferred viewport's closure
//!   (`Send + Sync + 'static`, `viewer-window.md`) cannot hold; immediate runs inside our own frame. During a
//!   reconnect the window is gone and comes back with the room (a suspended room keeps its handle). What is
//!   left out: modals opened from it appear in the main window.
//! - **Per-view state** (`StageState`, UI-private, per session; only the layout, webcam in picture and the Layout switches are saved, see [`StageSettings`]): each room and "Watched" keep
//!   their own order, grid, window rects and z-order. Streams are keyed by [`ItemId`] (room handle + owner
//!   name + label), not by `ConnId`/`StreamId`, so a reconnect keeps the layout. **A person is a connection**
//!   (room + `ConnId`, [`Item::conn`]): whose a stream is never goes by the name, see [`ItemId`]. The layout choice and the
//!   stage options live in [`StageSettings`] (`Shell::stage`), because the Layout menu and the tiles read
//!   them through [`UiAction::SetStageLayout`], [`UiAction::SetHideOwnStreams`] and
//!   [`UiAction::SetAlwaysShowNames`] and [`UiAction::SetShowStats`].
//!
//! # Input and bars
//! Every tile is a `Sense::click_and_drag` widget registered in draw order, so with overlapping floating
//! windows the topmost one gets the pointer; the hover overlay belongs to the topmost tile under the pointer.
//! With the pointer between streams (also in the black space around a picture) all bars show in dark grey;
//! over a stream they hide; the bar under the pointer is blue, and stays blue for the whole drag, while the
//! others hide. A drag is applied **before** the frame is laid out, from the pointer's distance to where the
//! press started, so the bar under the pointer does not lag a frame.
//!
//! # Motion (task I12, the replica's `.st-tile` and `.pip.snap` transitions)
//! A tile whose layout rect changes (a swap, a layout switch, a stream added or removed, "Reset sizes", a bar
//! double-click) **glides** to it over 0.2 s with the CSS `ease` curve ([`geom::Glide`], per tile and per view,
//! arena-relative). It does not glide while a bar, a floating window or its resize handle is being dragged (the
//! replica's `nofx`: the tile must stay under the pointer), when the arena itself changed size (window resize,
//! fullscreen, the people strip hidden: nothing wanders), or when it appears. The tile being reordered follows the
//! pointer 1:1 and, let go, glides from where it was dropped to its cell (the swapped one to its new cell). A webcam in
//! picture glides to its spot for 0.16 s after a drag ([`pip`]). *Why:* the user: "Moving streams around (swapping
//! them) is not as smooth as on the artifact" and "Moving the webcam around when in webcam PIP mode isnt as smooth as
//! the artifact". Repaints are requested only while a glide runs.
//!
//! # Performance
//! No repaint loop: video repaints are frame-driven by `VideoView`, a drag repaints on input, a glide for its
//! 0.2 s.

mod bar;
mod chrome;
mod geom;
mod menus;
mod pip;
mod strip;
mod tile;

use std::collections::{HashMap, HashSet};

use egui::{Align, CursorIcon, Id, Pos2, Rect, Sense, Stroke, Ui, UiBuilder, pos2, vec2};
use simple_screensharing_client::capture::webcam;
use simple_screensharing_client::protocol::{ConnId, StreamInfo, StreamKind};
use simple_screensharing_client::rooms::{AppSettings, StagePrefs, ViewPrefs, WATCHED_KEY, room_identity};
use simple_screensharing_client::session::{CaptureSourceId, DestinationId, RoomHandle};

use geom::{Bar, FRect, Grid, PipGeo, PipPos};
#[cfg(test)]
pub(super) use tile::tile_volume_slider;

pub use bar::BarMenu;
use super::{Cx, Popover, PopoverKind, RoomEntry, ShareState, StreamKey, UiAction, View, drive_dump, tr};
use crate::theme::{self, Weight, font};
use crate::widgets::{self, paint_text};

/// What a stage shows: one room, or every watched stream of every room.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum StageView {
    Room(RoomHandle),
    Watched,
}

pub use simple_screensharing_client::rooms::StageLayout;

/// The stage choices the Layout menu sets (`Shell::stage`; changed only through `UiAction`s). Saved in
/// `settings.toml` (`AppSettings::stage`, #131): per stage the layout and webcam in picture, keyed by the room's
/// `room_identity` (a [`RoomHandle`] does not survive a restart) or `"watched"`; the three switches are global.
/// A room joined by a link that is not saved is saved here too, by its identity.
#[derive(Default)]
pub struct StageSettings {
    /// This session's choices per stage; a stage without an entry has the defaults ([`ViewPrefs::default`]).
    views: HashMap<StageView, ViewPrefs>,
    /// "Hide my own streams": your own share gets no tile.
    pub hide_own: bool,
    /// "Always show names": the name tag of a watched stream shows without hovering.
    pub always_names: bool,
    /// "Show stats": every watched stream's name tag carries its numbers while it is hovered (D1). Off until switched on.
    pub show_stats: bool,
    /// What the file holds (what [`save`](Self::save) writes back, `views` kept in step by `update`).
    saved: StagePrefs,
    /// Each joined room's saved key, learnt by [`sync_rooms`](Self::sync_rooms); never pruned (a suspended room keeps its handle).
    keys: HashMap<RoomHandle, String>,
}

impl StageSettings {
    /// The settings a start begins with: the saved ones (Watched is seeded now, a room when it joins).
    pub fn from_saved(saved: StagePrefs) -> Self {
        let mut views = HashMap::new();
        if let Some(p) = saved.views.get(WATCHED_KEY) {
            views.insert(StageView::Watched, *p);
        }
        Self { views, hide_own: saved.hide_own, always_names: saved.always_names, show_stats: saved.show_stats, saved, keys: HashMap::new() }
    }

    /// Learn the key of every joined room and seed a new one from the file. Cheap; called every frame and
    /// before a change is stored.
    pub fn sync_rooms<'a>(&mut self, rooms: impl Iterator<Item = (RoomHandle, &'a str)>) {
        for (h, join_string) in rooms {
            if self.keys.contains_key(&h) {
                continue;
            }
            let key = room_identity(join_string);
            if let Some(p) = self.saved.views.get(&key) {
                self.views.entry(StageView::Room(h)).or_insert(*p);
            }
            self.keys.insert(h, key);
        }
    }

    fn prefs(&self, view: &StageView) -> ViewPrefs {
        self.views.get(view).copied().unwrap_or_default()
    }

    fn key(&self, view: &StageView) -> Option<String> {
        match view {
            StageView::Watched => Some(WATCHED_KEY.to_string()),
            StageView::Room(h) => self.keys.get(h).cloned(),
        }
    }

    fn update(&mut self, view: StageView, f: impl FnOnce(&mut ViewPrefs)) {
        let mut p = self.prefs(&view);
        f(&mut p);
        if let Some(key) = self.key(&view) {
            if p == ViewPrefs::default() {
                self.saved.views.remove(&key);
            } else {
                self.saved.views.insert(key, p);
            }
        }
        self.views.insert(view, p);
    }

    /// Write the settings to `settings.toml`.
    pub(super) fn save(&mut self) {
        self.saved.hide_own = self.hide_own;
        self.saved.always_names = self.always_names;
        self.saved.show_stats = self.show_stats;
        let mut settings = AppSettings::load();
        settings.stage = self.saved.clone();
        if let Err(e) = settings.save() {
            tracing::warn!("failed to save the layout settings: {e:#}");
        }
    }

    /// The layout of `view` (Split until it is changed).
    pub fn layout(&self, view: &StageView) -> StageLayout {
        self.prefs(view).layout
    }

    pub(super) fn set_layout(&mut self, view: StageView, layout: StageLayout) {
        self.update(view, |p| p.layout = layout);
    }

    /// Whether `view` puts webcams in their owner's picture (on until switched off).
    pub fn pip(&self, view: &StageView) -> bool {
        self.prefs(view).pip
    }

    pub(super) fn set_pip(&mut self, view: StageView, on: bool) {
        self.update(view, |p| p.pip = on);
    }
}

impl Cx<'_> {
    /// The stage the central area shows now (`None` for the welcome view and rooms that are not joined).
    pub fn stage_view(&self) -> Option<StageView> {
        match &self.nav.view {
            View::Watching => Some(StageView::Watched),
            View::Room(_) => self.selected_room().and_then(|r| r.handle).map(StageView::Room),
            View::Welcome => None,
        }
    }

    /// Whether `key` is a webcam stream (its label starts with `Webcam`, plan D7).
    fn is_webcam_stream(&self, key: StreamKey) -> bool {
        self.session(key.0).is_some_and(|s| s.streams.iter().any(|i| i.owner == key.1 && i.stream_id == key.2 && webcam::is_webcam_label(&i.label)))
    }

    /// Whether the sender of `key` shares audio with it. A webcam never does: the sound belongs to the screen
    /// (`RoomSession::sync_audio_watches` does not follow a webcam either).
    fn stream_has_audio(&self, key: StreamKey) -> bool {
        !self.is_webcam_stream(key) && (drive_dump::fake_audio() || self.session(key.0).is_some_and(|s| s.sibling_audio_stream(key.1).is_some()))
    }

    /// The joined room a stage shows (the open room, or the one a pop-out window was made for).
    fn stage_room(&self, h: RoomHandle) -> Option<RoomEntry> {
        self.rooms().into_iter().find(|r| r.handle == Some(h))
    }
}

/// A tile's identity across reconnects: room + owner name + label (+ audio flag, + the n-th of identical
/// ones, [`number_identical`]). Your own share is `who == ""` (one per room). `Why` not `StreamKey`: `ConnId` and
/// `StreamId` change with every reconnect (`WantedStream`). It names a place in the layout, nothing else: whose a
/// stream is goes by [`Item::conn`], because two people may have one name.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ItemId {
    room: RoomHandle,
    who: String,
    label: String,
    audio: bool,
    nth: u8,
}

/// A volume slider drag in progress: the value shown until the pushed `SetVolume` has landed.
pub(super) type VolDrag = Option<(StreamKey, u32)>;

/// Stage UI state, UI-private (see the module doc).
#[derive(Default)]
pub struct StageState {
    views: HashMap<StageView, ViewGeom>,
    drag: Option<Drag>,
    vol_drag: VolDrag,
    /// The people button hid the strip (it shows until then).
    pub(super) strip_hidden: bool,
    /// Stages shown in their own window ([`show_windows`]); the main window shows a note for these.
    pub(super) popped: HashSet<StageView>,
    /// Fullscreen auto-hide state of the chrome ([`chrome`]); `None` outside fullscreen.
    chrome: Option<chrome::Chrome>,
    /// Level meters of the audio dropdown ([`super::share::sync_level_taps`]).
    pub(super) taps: Option<Vec<u32>>,
}

impl StageState {
    /// "Reset sizes and positions": forget `view`'s grid, floating windows and their order.
    pub(super) fn reset_view(&mut self, view: &StageView) {
        if let Some(g) = self.views.get_mut(view) {
            g.grid = None;
            g.float.clear();
            g.z.clear();
            g.pip.clear();
            g.pip_glide.clear();
        }
    }
}

/// Which window a stage is drawn in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Host {
    /// The main window's central area.
    Main,
    /// Its own window ([`show_windows`]): fullscreen is that window's, there is no sidebar button.
    Window,
}

/// One view's own layout state. Never pruned: a stream that comes back (reconnect, re-share) keeps its place.
#[derive(Default)]
struct ViewGeom {
    order: Vec<ItemId>,
    /// Floating: back to front.
    z: Vec<ItemId>,
    float: HashMap<ItemId, FRect>,
    grid: Option<Grid>,
    /// Webcam in picture: each webcam's size and spot in its parent's picture, per view ([`pip_pairs`]).
    pip: HashMap<ItemId, PipPos>,
    /// Where each tile is drawn while it glides to its layout rect (arena-relative, [`geom::Glide`]).
    glide: HashMap<ItemId, geom::Glide>,
    /// The arena size of the last frame: a different one (window resize, fullscreen) moves tiles without a glide.
    arena: Option<(f32, f32)>,
    /// Webcams gliding to their spot after a drag ([`pip`]).
    pip_glide: HashMap<ItemId, pip::PipGlide>,
}

/// A pointer drag in progress; `origin` is where the press started.
enum Drag {
    Bar { bar: Bar, origin: Pos2, start_center: f32, start: Grid },
    Move { id: ItemId, origin: Pos2, from: FRect },
    Resize { id: ItemId, origin: Pos2, from: FRect },
    Reorder { id: ItemId, origin: Pos2 },
    /// A webcam in picture moved or resized ([`pip`]).
    Pip(pip::PipDragState),
}

enum Kind {
    Video(StreamKey),
    /// A video stream of someone else that is not watched.
    Idle(StreamKey, StreamInfo),
    /// An audio-only sender.
    Audio(StreamKey, StreamInfo),
    Mine(CaptureSourceId, DestinationId),
}

struct Item {
    id: ItemId,
    /// Whose it is: a person is a connection, so this and the room say who, never `id.who` (the name, for showing).
    /// Yours is your own `ConnId`.
    conn: ConnId,
    kind: Kind,
    /// Aspect ratio (w/h) of the picture.
    ar: f32,
    mine: bool,
    popped: bool,
    /// This webcam is drawn inside its owner's screen share ([`pip_pairs`]), so it has no tile. Set by [`show`].
    in_pip: bool,
}

impl Item {
    /// A webcam (its label starts with `Webcam`, plan D7): yours or someone else's.
    fn is_cam(&self) -> bool {
        webcam::is_webcam_label(&self.id.label)
    }

    /// The stream id of someone else's stream.
    fn stream_id(&self) -> Option<u8> {
        match &self.kind {
            Kind::Video(key) | Kind::Idle(key, _) | Kind::Audio(key, _) => Some(key.2),
            Kind::Mine(..) => None,
        }
    }
}

/// Give items of one identity (two same-named owners sharing one label, or one owner's two "Screen"s) their `nth`, in
/// `(ConnId, StreamId)` order, so a tile never shares its layout with another. `Why` that order and not the list's: the
/// same stream keeps its number from frame to frame, whatever order the announcements came in. A `ConnId` is random
/// and new after a reconnect, so two same-named owners may swap numbers then and with them their places: nothing is lost,
/// and nothing that survives a reconnect can tell the two apart (`session::match_wanted` has the same limit).
fn number_identical(items: &mut [Item]) {
    let mut by_conn: Vec<usize> = (0..items.len()).collect();
    by_conn.sort_by_key(|&k| (items[k].conn, items[k].stream_id()));
    let mut seen: HashMap<ItemId, u8> = HashMap::new();
    for k in by_conn {
        let n = seen.entry(items[k].id.clone()).or_default();
        items[k].id.nth = *n;
        *n = n.saturating_add(1);
    }
}

/// Webcam in picture: parent item id -> index of the webcam item that sits inside it (task I8, the replica's
/// `pipPairs`). The parent is the webcam owner's first screen share, in this view's order, that is **on the
/// stage and playing**: yours, or one you watch, and not popped out. So with no screen share running, with one you
/// do not watch (its tile holds the Watch button), or with the share popped out, the webcam stays a tile of its
/// own. A webcam you do not watch yet stays a tile too, for the same reason: it is not playing, so it is
/// not in `list`. One parent holds one webcam (a second one keeps its tile).
fn pip_pairs(items: &[Item], order: &[ItemId]) -> HashMap<ItemId, usize> {
    let list: Vec<usize> = order
        .iter()
        .filter_map(|id| items.iter().position(|i| &i.id == id))
        .filter(|&k| !items[k].popped && matches!(items[k].kind, Kind::Video(_) | Kind::Mine(..)))
        .collect();
    let mut pairs = HashMap::new();
    for &c in list.iter().filter(|&&k| items[k].is_cam()) {
        let owner = (items[c].id.room, items[c].conn);
        if let Some(&p) = list.iter().find(|&&k| !items[k].is_cam() && (items[k].id.room, items[k].conn) == owner) {
            pairs.insert(items[p].id.clone(), c);
        }
    }
    pairs
}

fn aspect(w: f32, h: f32) -> f32 {
    if w > 0.0 && h > 0.0 { w / h } else { 16.0 / 9.0 }
}

/// What `view` shows, in the session's order.
fn collect(cx: &Cx, view: &StageView) -> Vec<Item> {
    let mut out: Vec<Item> = Vec::new();
    let mut add = |room: RoomHandle, conn: ConnId, who: String, label: String, audio: bool, kind: Kind, ar: f32| {
        let popped = matches!(&kind, Kind::Video(k) if cx.is_popped(*k));
        let mine = matches!(kind, Kind::Mine(..));
        out.push(Item { id: ItemId { room, who, label, audio, nth: 0 }, conn, kind, ar, mine, popped, in_pip: false });
    };
    let mine_ar = |capture: CaptureSourceId, destination: DestinationId| {
        let p = cx.capture(capture).and_then(|c| c.destinations.get(&destination)).map(|d| match &d.runtime {
            Some(rt) => rt.lock().unwrap().params(),
            None => d.params,
        });
        p.map_or(aspect(0.0, 0.0), |p| aspect(p.width as f32, p.height as f32))
    };
    let video_ar = |key: StreamKey, info: Option<&StreamInfo>| match cx.videos.frame_size(key) {
        Some((w, h)) => aspect(w as f32, h as f32),
        None => info.map_or(aspect(0.0, 0.0), |i| aspect(i.width as f32, i.height as f32)),
    };
    match view {
        StageView::Room(h) => {
            for info in cx.shares_in(*h) {
                let key = (*h, info.owner, info.stream_id);
                let who = cx.member_name(*h, info.owner);
                if info.kind == StreamKind::Audio {
                    add(*h, info.owner, who, info.label.clone(), true, Kind::Audio(key, info.clone()), aspect(0.0, 0.0));
                } else if cx.videos.get(key).is_some() {
                    add(*h, info.owner, who, info.label.clone(), false, Kind::Video(key), video_ar(key, Some(info)));
                } else {
                    add(*h, info.owner, who, info.label.clone(), false, Kind::Idle(key, info.clone()), aspect(info.width as f32, info.height as f32));
                }
            }
            let me = cx.session(*h).map_or(0, |s| s.client.conn_id());
            if let Some(room) = cx.stage_room(*h)
                && let ShareState::Live { capture, destination } = cx.share_state(&room)
            {
                add(*h, me, String::new(), String::new(), false, Kind::Mine(capture, destination), mine_ar(capture, destination));
            }
            // Your webcam is a share of its own (task I8), a tile like your screen's.
            if let Some((capture, destination, _)) = cx.webcam_share(*h) {
                add(*h, me, String::new(), webcam::LABEL.to_string(), false, Kind::Mine(capture, destination), mine_ar(capture, destination));
            }
        }
        StageView::Watched => {
            for key in cx.watched() {
                let info = cx.session(key.0).and_then(|s| s.streams.iter().find(|i| i.owner == key.1 && i.stream_id == key.2));
                let label = info.map(|i| i.label.clone()).unwrap_or_default();
                add(key.0, key.1, cx.member_name(key.0, key.1), label, false, Kind::Video(key), video_ar(key, info));
            }
        }
    }
    number_identical(&mut out);
    out
}

/// A centred `[icon][title][text]` on the stage (nothing to show).
fn note(ui: &Ui, rect: Rect, icon: &str, title: &str, text: Option<&str>) {
    let c = rect.center();
    let line = |y: f32, h: f32| Rect::from_x_y_ranges(rect.x_range().shrink(theme::S4), y..=y + h);
    widgets::paint_icon(ui, Rect::from_center_size(pos2(c.x, c.y - 30.0), vec2(40.0, 40.0)), icon, 34.0, theme::TEXT_FAINT);
    paint_text(ui, line(c.y - 4.0, 20.0), title, font(Weight::Bold, 15.0), theme::TEXT, Align::Center);
    if let Some(t) = text {
        paint_text(ui, line(c.y + 20.0, 18.0), t, font(Weight::Regular, theme::FS_LABEL), theme::TEXT_DIM, Align::Center);
    }
}

/// The streams of one stage inside `arena` (or the note when there is nothing to show). `hover_ok` is false
/// while fullscreen has hidden the chrome: the tile overlays go with it.
fn draw_arena(ui: &mut Ui, cx: &mut Cx, st: &mut StageState, view: &StageView, items: &[Item], arena: Rect, hover_ok: bool, chrome_top: f32) {
    // What the strip and bar cover in fullscreen takes no input from the streams under it: a press on a bar
    // button must never grab a resize bar or a tile behind it (found in the rig: the Layout button sat on a bar).
    let live = |r: Rect| {
        let r = r.intersect(Rect::from_min_max(pos2(f32::MIN, f32::MIN), pos2(f32::MAX, chrome_top)));
        if r.is_positive() { r } else { Rect::from_min_size(r.min, vec2(0.0, 0.0)) }
    };
    let (hide_own, always_names, show_stats) = (cx.shell.stage.hide_own, cx.shell.stage.always_names, cx.shell.stage.show_stats);
    let layout = cx.shell.stage.layout(view);
    let pip_on = cx.shell.stage.pip(view);
    let cxr: &Cx = cx;
    let StageState { views, drag, .. } = st;
    let geom = views.entry(view.clone()).or_default();
    for it in items {
        if !geom.order.contains(&it.id) {
            geom.order.push(it.id.clone());
        }
    }
    // Webcam in picture: a webcam inside its owner's screen share has no tile of its own.
    let pairs = if pip_on { pip_pairs(items, &geom.order) } else { HashMap::new() };
    let in_pip: HashSet<ItemId> = pairs.values().map(|&c| items[c].id.clone()).collect();
    let vis: Vec<&Item> = geom
        .order
        .iter()
        .filter_map(|id| items.iter().find(|i| &i.id == id))
        .filter(|i| !i.popped && !(hide_own && i.mine) && !in_pip.contains(&i.id))
        .collect();
    if vis.is_empty() {
        *drag = None;
        let (icon, title, text) = if items.is_empty() {
            match view {
                StageView::Room(_) => ("tv_off", tr("ui.room.nobody_title", "Nobody is sharing yet"), Some(tr("ui.room.nobody_text", "Start your own share with the bar below."))),
                StageView::Watched => ("visibility_off", tr("ui.room.watching_empty_title", "You're not watching anything"), Some(tr("ui.room.watching_empty_text", "Open a room and click Watch on a share."))),
            }
        } else if items.iter().all(|i| i.mine || i.popped) && items.iter().any(|i| i.mine) && hide_own && !items.iter().any(|i| i.popped && !i.mine) {
            ("visibility_off", tr("ui.stage.own_hidden_title", "Your own streams are hidden"), Some(tr("ui.stage.own_hidden_text", "Others still see them.")))
        } else {
            ("open_in_new", tr("ui.stage.all_popped_title", "Every stream is in its own window"), Some(tr("ui.stage.all_popped_text", "Close a window to bring its stream back.")))
        };
        note(ui, arena, icon, &title, text.as_deref());
        return;
    }

    if arena.width() < 8.0 || arena.height() < 8.0 {
        // A window too small for any picture (the bar and strip take what there is).
        *drag = None;
        return;
    }
    let (w, h) = (arena.width(), arena.height());
    let ars: Vec<f32> = vis.iter().map(|i| i.ar).collect();
    let (crop, float) = (layout == StageLayout::Crop, layout == StageLayout::Floating);
    let counts = geom::best_rows(&ars, w, h);
    if geom.grid.as_ref().is_none_or(|g| g.counts != counts) {
        geom.grid = Some(Grid::even(counts));
    }
    // An active drag, applied before the layout so nothing lags a frame.
    let (pointer, down) = ui.input(|i| (i.pointer.latest_pos(), i.pointer.primary_down()));
    if let (Some(d), Some(p), true) = (drag.as_ref(), pointer, down) {
        let idx = |id: &ItemId| vis.iter().position(|i| &i.id == id);
        match d {
            Drag::Bar { bar, origin, start_center, start } if geom.grid.as_ref().is_some_and(|g| g.counts == start.counts) => {
                let want = start_center + if matches!(bar, Bar::V { .. }) { p.x - origin.x } else { p.y - origin.y };
                geom.grid = Some(geom::drag_bar(start, *bar, want, &ars, w, h, crop));
            }
            Drag::Move { id, origin, from } => {
                geom.float.insert(id.clone(), geom::move_float(*from, p.x - origin.x, p.y - origin.y, w, h));
            }
            Drag::Resize { id, origin, from } => {
                if let Some(i) = idx(id) {
                    geom.float.insert(id.clone(), geom::resize_float(*from, p.x - origin.x, p.y - origin.y, ars[i], w, h));
                }
            }
            Drag::Pip(d) => pip::apply_drag(d, p, &mut geom.pip),
            _ => {}
        }
    }
    let grid = geom.grid.clone().unwrap_or_else(|| Grid::even(Vec::new()));
    let placed = geom::place(&grid, &ars, w, h, crop);
    let off = arena.min.to_vec2();
    let rects: Vec<Rect> = if float {
        let split = if crop { geom::place(&grid, &ars, w, h, false).pics } else { placed.pics.clone() };
        vis.iter()
            .enumerate()
            .map(|(i, it)| {
                let f = geom.float.entry(it.id.clone()).or_insert_with(|| FRect::from_rect(split[i], w, h));
                // A window always has its stream's aspect ratio, whatever layout or frame size it came from.
                *f = geom::fit_float(*f, it.ar, w, h);
                f.to_rect(w, h).translate(off)
            })
            .collect()
    } else {
        placed.pics.iter().map(|r| r.translate(off)).collect()
    };
    drive_dump::line(|| format!("layout {layout:?}\ntiles {}", vis.len()));
    // Glide: where each tile is drawn this frame (`rects` stay the layout's, the drop targets).
    let now = ui.input(|i| i.time);
    let arena_moved = geom.arena.is_none_or(|(pw, ph)| (pw - w).abs() > 0.5 || (ph - h).abs() > 0.5);
    geom.arena = Some((w, h));
    // A bar, a window or its handle under the pointer, or a changed arena: nothing glides (`nofx`).
    let snap = arena_moved || matches!(drag, Some(Drag::Bar { .. } | Drag::Move { .. } | Drag::Resize { .. }));
    let reorder_id = match drag {
        Some(Drag::Reorder { id, .. }) => Some(id.clone()),
        _ => None,
    };
    geom.glide.retain(|id, _| vis.iter().any(|i| &i.id == id));
    let mut animating = false;
    let cur: Vec<Rect> = vis
        .iter()
        .zip(&rects)
        .map(|(it, r)| {
            let target = r.translate(-off);
            let dragged = reorder_id.as_ref() == Some(&it.id);
            let g = geom::Glide::step(geom.glide.get(&it.id).copied(), target, now, snap || dragged, geom::TILE_GLIDE);
            geom.glide.insert(it.id.clone(), g);
            let (drawn, done) = g.at(now, geom::TILE_GLIDE);
            animating |= !done;
            drawn.translate(off)
        })
        .collect();
    if animating {
        ui.ctx().request_repaint();
    }
    let cells = grid.cells(w, h);
    for (i, it) in vis.iter().enumerate() {
        drive_dump::rect(|| format!("tile.{i}"), cur[i]);
        drive_dump::rect(|| format!("tile_goal.{i}"), rects[i]);
        if let Some(c) = cells.get(i) {
            drive_dump::rect(|| format!("cell.{i}"), c.translate(off));
        }
        drive_dump::line(|| {
            let kind = match it.kind {
                Kind::Video(_) => "video",
                Kind::Idle(..) => "idle",
                Kind::Audio(..) => "audio",
                Kind::Mine(..) => "mine",
            };
            format!("tileinfo {i} {kind} ar {:.3} who={} label={}", it.ar, it.id.who, it.id.label)
        });
    }
    let mut paint_order: Vec<usize> = (0..vis.len()).collect();
    if float {
        paint_order.sort_by_key(|&i| geom.z.iter().position(|z| *z == vis[i].id).map_or(-1, |p| p as i32));
    }
    let reordering = match drag {
        Some(Drag::Reorder { id, origin }) => vis.iter().position(|i| &i.id == id).zip(pointer.map(|p| p - *origin)),
        _ => None,
    };
    let mut shown = cur.clone();
    if let Some((i, delta)) = reordering {
        shown[i] = cur[i].translate(delta);
        paint_order.retain(|&x| x != i);
        paint_order.push(i);
    }
    // The topmost tile under the pointer, and the bar under it (a bar wins over a tile edge).
    let bars: Vec<(Bar, Rect)> = if float { Vec::new() } else { placed.bars.iter().map(|(b, r)| (*b, r.translate(off))).collect() };
    let over_bar = bars.iter().find(|(_, r)| ui.rect_contains_pointer(*r)).map(|(b, _)| *b);
    let hover = if over_bar.is_some() { None } else { paint_order.iter().rev().copied().find(|&i| ui.rect_contains_pointer(shown[i])) };
    let dragging = drag.is_some();

    let mut acts: Vec<UiAction> = Vec::new();
    let mut started: Option<Drag> = None;
    let press = ui.input(|i| i.pointer.press_origin());
    let mut raise: Option<ItemId> = None;
    let mut pip_geos: HashMap<ItemId, PipGeo> = HashMap::new();
    for &i in &paint_order {
        let (it, rect) = (vis[i], shown[i]);
        let resp = ui.interact(live(rect), Id::new(("stage_tile", &it.id)), Sense::click_and_drag());
        let spot = tile::Spot {
            n: i,
            rect,
            crop,
            round: if float { 0 } else { 8 },
            hovered: hover == Some(i) && !dragging && hover_ok && menu_ok(cxr, &it.id),
            always_names,
            stats: show_stats,
            cross_room: matches!(view, StageView::Watched),
            id: it.id.clone(),
        };
        // The webcam in this stream's picture goes between the tile's picture and its overlay, so the name tag and the
        // stats are above it (and below a floating window's own handle, drawn next).
        let mut pip_out: Option<pip::Out> = None;
        let mut between: tile::Between = None;
        if let Some(&ci) = pairs.get(&it.id) {
            let cam = &items[ci];
            let pos = *geom.pip.entry(cam.id.clone()).or_default();
            let input = pip::Input {
                name: i.to_string(),
                tile: rect,
                parent_ar: it.ar,
                crop,
                cam,
                hovered: hover == Some(i) && hover_ok,
                dragged: matches!(drag, Some(Drag::Pip(d)) if d.cam == cam.id),
                glide: geom.pip_glide.get(&cam.id).copied(),
                now,
            };
            let (live, press, slot) = (&live, press, &mut pip_out);
            between = Some(Box::new(move |ui: &mut Ui, acts: &mut Vec<UiAction>| *slot = Some(pip::show(ui, cxr, &input, pos, press, live, acts))));
        }
        match &it.kind {
            Kind::Video(key) => tile::video_tile(ui, cxr, *key, &spot, &mut between, &mut acts),
            Kind::Idle(key, info) | Kind::Audio(key, info) => tile::idle_tile(ui, cxr, *key, info, &spot, &mut acts),
            Kind::Mine(capture, destination) => tile::mine_tile(ui, cxr, it.id.room, *capture, *destination, &spot, &mut between, &mut acts),
        }
        tile::run_between(&mut between, ui, &mut acts);
        drop(between);
        if let (Some(&ci), Some(out)) = (pairs.get(&it.id), pip_out) {
            let cam = &items[ci];
            pip_geos.insert(cam.id.clone(), out.geo);
            if out.glide_done {
                geom.pip_glide.remove(&cam.id);
            }
            if out.raise && float {
                raise = Some(it.id.clone());
            }
            if started.is_none() {
                started = out.started.map(Drag::Pip);
            }
        }
        if float {
            ui.painter().rect_stroke(rect, 0.0, Stroke::new(1.0_f32, theme::BORDER), egui::StrokeKind::Outside);
            let handle = Rect::from_min_size(rect.max - vec2(20.0, 20.0), vec2(20.0, 20.0));
            drive_dump::rect(|| format!("win.{i}"), rect);
            drive_dump::rect(|| format!("grip.{i}"), handle);
            let rz = ui.interact(live(handle), Id::new(("stage_resize", &it.id)), Sense::drag());
            let grip = Stroke::new(1.7_f32, theme::TEXT_DIM);
            // The replica's `.rz`: two stripes across the corner, 10 and 16.8 px from it.
            for k in [10.0, 16.8] {
                ui.painter().line_segment([pos2(handle.right(), handle.bottom() - k), pos2(handle.right() - k, handle.bottom())], grip);
            }
            if rz.hovered() || rz.dragged() {
                ui.ctx().set_cursor_icon(CursorIcon::ResizeNwSe);
            }
            if rz.is_pointer_button_down_on() {
                raise = Some(it.id.clone());
            }
            if rz.drag_started()
                && let (Some(origin), Some(from)) = (press, geom.float.get(&it.id))
            {
                started = Some(Drag::Resize { id: it.id.clone(), origin, from: *from });
            }
        }
        if resp.is_pointer_button_down_on() && float {
            raise = Some(it.id.clone());
        }
        if resp.double_clicked()
            && let Kind::Video(key) = &it.kind
        {
            acts.push(UiAction::EnterStreamFullscreen(*key));
        }
        if resp.secondary_clicked()
            && let Some(p) = resp.interact_pointer_pos()
        {
            acts.push(UiAction::OpenPopover(Popover { kind: PopoverKind::Tile(it.id.clone()), anchor: Rect::from_min_size(p, vec2(1.0, 1.0)) }));
        }
        if resp.drag_started()
            && started.is_none()
            && let Some(origin) = press
        {
            started = Some(match geom.float.get(&it.id) {
                Some(from) if float => Drag::Move { id: it.id.clone(), origin, from: *from },
                _ => Drag::Reorder { id: it.id.clone(), origin },
            });
        }
    }

    // Resize bars (Split, Crop), on top of the tiles.
    let bar_drag = match drag {
        Some(Drag::Bar { bar, .. }) => Some(*bar),
        _ => None,
    };
    let grey = !dragging && (over_bar.is_some() || (ui.rect_contains_pointer(arena) && hover.is_none()));
    for (bar, rect) in &bars {
        let resp = ui.interact(live(*rect), Id::new(("stage_bar", bar)), Sense::click_and_drag());
        let vertical = matches!(bar, Bar::V { .. });
        if resp.hovered() || bar_drag == Some(*bar) {
            ui.ctx().set_cursor_icon(if vertical { CursorIcon::ResizeHorizontal } else { CursorIcon::ResizeVertical });
        }
        let color = if bar_drag == Some(*bar) || (!dragging && over_bar == Some(*bar)) {
            Some(theme::ACCENT)
        } else if grey {
            Some(theme::BORDER)
        } else {
            None
        };
        let name = match bar {
            Bar::V { row, col } => format!("v{row}.{col}"),
            Bar::H { row } => format!("h{row}"),
        };
        drive_dump::rect(|| format!("bar.{name}"), *rect);
        drive_dump::line(|| format!("barstate {name} {}", if color == Some(theme::ACCENT) { "blue" } else if color.is_some() { "grey" } else { "hidden" }));
        if let Some(c) = color {
            let line = if vertical {
                Rect::from_center_size(rect.center(), vec2(3.0, rect.height() * 0.6))
            } else {
                Rect::from_center_size(rect.center(), vec2(rect.width() * 0.6, 3.0))
            };
            ui.painter().rect_filled(line, 2.0, c);
        }
        if resp.double_clicked() {
            geom.grid = Some(Grid::even(grid.counts.clone()));
        }
        if resp.drag_started()
            && let Some(origin) = press
        {
            // The maths is arena-relative.
            started = Some(Drag::Bar { bar: *bar, origin, start_center: geom::bar_center(*bar, rect.translate(-off)), start: grid.clone() });
        }
    }

    if let Some(id) = raise {
        geom.z.retain(|z| *z != id);
        geom.z.push(id);
    }
    // Release: a reorder drag swaps the two tiles.
    if drag.is_some() && !down {
        if let (Some(Drag::Reorder { id, .. }), Some(p)) = (drag.as_ref(), pointer)
            && let Some(i) = vis.iter().position(|it| &it.id == id)
        {
            // Both tiles glide to their cells; the dragged one starts where it was dropped (the replica lands it
            // there first), the other from where it is drawn (its glide carries on, retargeted next frame).
            let dropped = shown[i].translate(-off);
            let target = vis.iter().zip(&rects).find(|(it, r)| &it.id != id && r.contains(p)).map(|(it, _)| it.id.clone());
            let swap = geom.order.iter().position(|o| o == id).zip(target.and_then(|t| geom.order.iter().position(|o| *o == t)));
            if let Some((from, to)) = swap {
                geom.order.swap(from, to);
                geom.glide.insert(id.clone(), geom::Glide::at_rest(dropped, now));
            } else {
                geom.glide.insert(id.clone(), geom::Glide { from: dropped, to: rects[i].translate(-off), start: now });
            }
            ui.ctx().request_repaint();
        }
        // A webcam in picture that was moved goes to the nearest spot, gliding there.
        if let Some(Drag::Pip(d)) = drag.as_ref() {
            pip::release(d, &pip_geos, &mut geom.pip, &mut geom.pip_glide, now);
            ui.ctx().request_repaint();
        }
        *drag = None;
    }
    if started.is_some() && drag.is_none() {
        *drag = started;
    }
    if let Some(Drag::Reorder { id, .. }) = drag.as_ref()
        && let Some(p) = pointer
        && let Some(target) = vis.iter().zip(&rects).find(|(it, r)| &it.id != id && r.contains(p))
    {
        // The drop target: an accent ring and a faint wash.
        let r = target.1;
        ui.painter().rect_filled(*r, 8.0, theme::ACCENT_GHOST_BG_HOVER);
        ui.painter().rect_stroke(*r, 8.0, Stroke::new(2.0_f32, theme::ACCENT), egui::StrokeKind::Inside);
    }
    for a in acts {
        cx.act(a);
    }
}

/// The Room settings menu for a room with no stage (see `room::show_header`).
pub fn room_menu_popover(ctx: &egui::Context, cx: &mut Cx, room: &RoomEntry) {
    menus::room_popover(ctx, cx, room);
}

/// Stream fullscreen in the main window (the one watched stream filling the window, `Nav::fullscreen`) draws the
/// webcam in picture that belongs to it, if the open stage has one: same spot, same size as on the stage, and the
/// same drag and handle (task I8, D1: it "stays on its spot across layouts, tile resizes and fullscreen"). The stage's
/// own fullscreen and a popped-out stage draw the PIP with the rest of the stage.
pub fn fullscreen_pip(ui: &mut Ui, cx: &mut Cx, st: &mut StageState, parent: StreamKey, rect: Rect) {
    let Some(view) = cx.stage_view() else { return };
    if !cx.shell.stage.pip(&view) {
        return;
    }
    let items = collect(cx, &view);
    let StageState { views, drag, .. } = st;
    let geom = views.entry(view).or_default();
    for it in &items {
        if !geom.order.contains(&it.id) {
            geom.order.push(it.id.clone());
        }
    }
    let pairs = pip_pairs(&items, &geom.order);
    let Some(parent_item) = items.iter().find(|i| matches!(i.kind, Kind::Video(k) if k == parent)) else { return };
    let Some(&ci) = pairs.get(&parent_item.id) else { return };
    let cam = &items[ci];
    let (pointer, down, press, now) = ui.input(|i| (i.pointer.latest_pos(), i.pointer.primary_down(), i.pointer.press_origin(), i.time));
    if let (Some(Drag::Pip(d)), Some(p), true) = (drag.as_ref(), pointer, down) {
        pip::apply_drag(d, p, &mut geom.pip);
    }
    let pos = *geom.pip.entry(cam.id.clone()).or_default();
    let input = pip::Input {
        name: "fs".into(),
        tile: rect,
        parent_ar: parent_item.ar,
        crop: false,
        cam,
        hovered: false,
        dragged: matches!(drag, Some(Drag::Pip(d)) if d.cam == cam.id),
        glide: geom.pip_glide.get(&cam.id).copied(),
        now,
    };
    let mut acts: Vec<UiAction> = Vec::new();
    let out = pip::show(ui, cx, &input, pos, press, &|r| r, &mut acts);
    if out.glide_done {
        geom.pip_glide.remove(&cam.id);
    }
    if drag.is_none() {
        *drag = out.started.map(Drag::Pip);
    }
    if !down && let Some(Drag::Pip(d)) = drag.as_ref() {
        pip::release(d, &HashMap::from([(cam.id.clone(), out.geo)]), &mut geom.pip, &mut geom.pip_glide, now);
        ui.ctx().request_repaint();
        *drag = None;
    }
    for a in acts {
        cx.act(a);
    }
}

/// An open stream menu keeps its own tile's buttons up; any other open menu or modal takes the pointer, so
/// no hover chrome shows behind it (the replica's `pin-ovl`).
fn menu_ok(cx: &Cx, id: &ItemId) -> bool {
    match &cx.nav.popover {
        Some(Popover { kind: PopoverKind::Tile(open), .. }) => open == id,
        Some(_) => false,
        None => cx.nav.modal.is_none(),
    }
}

/// The stage of the main window's central area: the stage itself, or the note while it is popped out.
pub fn show_main(ui: &mut Ui, cx: &mut Cx, st: &mut StageState) {
    let Some(view) = cx.stage_view() else { return };
    if !st.popped.contains(&view) {
        show(ui, cx, st, Host::Main, &view);
        return;
    }
    let full = ui.max_rect();
    ui.painter().rect_filled(full, 0.0, theme::STAGE_BG);
    if cx.nav.stage_fullscreen.is_some() {
        cx.act(UiAction::ExitStageFullscreen);
    }
    note(ui, full, "open_in_new", &tr("ui.stage.popped_title", "The stage is open in its own window"), Some(&tr("ui.stage.popped_text", "Your streams keep playing there.")));
    let c = full.center();
    let br = Rect::from_center_size(pos2(c.x, c.y + 58.0), vec2(180.0, theme::CTL_H));
    drive_dump::rect(|| "stage.bring_back".into(), br);
    let mut child = ui.new_child(UiBuilder::new().max_rect(br));
    let label = tr("ui.stage.bring_back", "Bring it back");
    if widgets::Btn::secondary(&label).icon("close_fullscreen").show_sized(&mut child, br.width()).clicked() {
        st.popped.remove(&view);
    }
}

/// The popped-out stages: one immediate viewport each, drawing the same stage ([`Host::Window`]). Closing the
/// window brings the stage back; so does a room that is gone. A suspended room is not gone: its window is away for
/// the outage and comes back with the room, like a stream's pop-out (D1).
pub fn show_windows(ctx: &egui::Context, cx: &mut Cx, st: &mut StageState) {
    let views: Vec<StageView> = st.popped.iter().cloned().collect();
    for view in views {
        let title = match &view {
            StageView::Room(h) => cx.room_name(*h),
            StageView::Watched => tr("ui.room.watched_title", "Watched streams"),
        };
        if let StageView::Room(h) = &view
            && cx.stage_room(*h).is_none()
        {
            if !cx.state.reconnects.contains_key(h) {
                st.popped.remove(&view);
            }
            continue;
        }
        let mut builder = egui::ViewportBuilder::default()
            .with_title(title)
            .with_inner_size([1280.0, 760.0])
            .with_min_inner_size([480.0, 320.0])
            .with_app_id("simple-screensharing-popout");
        if let Some(icon) = super::app_icon() {
            builder = builder.with_icon(icon);
        }
        let mut closed = false;
        ctx.show_viewport_immediate(egui::ViewportId::from_hash_of(("stage_window", &view)), builder, |ctx, _class| {
            closed = ctx.input(|i| i.viewport().close_requested());
            drive_dump::in_window(true);
            egui::CentralPanel::default().frame(egui::Frame::NONE.fill(theme::STAGE_BG)).show(ctx, |ui| show(ui, cx, st, Host::Window, &view));
            drive_dump::in_window(false);
        });
        if closed {
            st.popped.remove(&view);
        }
    }
}

/// The stage `view`, filling `ui`'s whole rect: the streams, then the people strip and the control bar (under
/// the streams, or over them in fullscreen).
fn show(ui: &mut Ui, cx: &mut Cx, st: &mut StageState, host: Host, view: &StageView) {
    let ctx = ui.ctx().clone();
    let full = ui.max_rect();
    ui.painter().rect_filled(full, 0.0, theme::STAGE_BG);
    let mut items = collect(cx, view);
    if cx.shell.stage.pip(view) {
        // The strip's cards say which webcams sit inside a picture.
        let order = st.views.get(view).map(|g| g.order.clone()).unwrap_or_default();
        for ci in pip_pairs(&items, &order).into_values() {
            items[ci].in_pip = true;
        }
    }
    // Menus first: a click outside one that lands on a bar button must close it before the button reacts.
    menus::show(&ctx, cx, st, view, &items);

    let fullscreen = match host {
        Host::Main => cx.nav.stage_fullscreen.is_some(),
        Host::Window => ctx.input(|i| i.viewport().fullscreen.unwrap_or(false)),
    };
    if host == Host::Window {
        let (f11, esc) = ctx.input(|i| (i.key_pressed(egui::Key::F11), i.key_pressed(egui::Key::Escape)));
        if f11 || (esc && fullscreen && cx.nav.popover.is_none()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fullscreen));
        }
    }
    // Fullscreen: the chrome hides itself (`chrome.rs`); an open menu, modal, dropdown or drag holds it up.
    let shown = if fullscreen {
        let now = ctx.input(|i| i.time);
        let held = cx.nav.popover.is_some() || cx.nav.modal.is_some() || st.drag.is_some() || super::menu::any_select_open(&ctx);
        let (inside, active) = ctx.input(|i| {
            let moved = i.events.iter().any(|e| {
                matches!(e, egui::Event::PointerMoved(_) | egui::Event::PointerButton { .. } | egui::Event::MouseWheel { .. } | egui::Event::Key { .. } | egui::Event::Text(_))
            });
            (i.pointer.latest_pos().is_some_and(|p| full.contains(p)), moved)
        });
        let (next, shown) = chrome::step(st.chrome.unwrap_or_else(|| chrome::Chrome::start(now)), now, inside, active, held);
        if let Some(rest) = next.remaining(now) {
            ctx.request_repaint_after(std::time::Duration::from_secs_f64(rest + 0.02));
        }
        st.chrome = Some(next);
        if !shown && inside {
            ctx.set_cursor_icon(CursorIcon::None);
        }
        shown
    } else {
        st.chrome = None;
        true
    };
    let alpha = ctx.animate_bool_with_time(Id::new(("stage_chrome", host)), shown, 0.25);
    drive_dump::line(|| format!("chrome {}\nchrome_alpha {alpha:.2}\nstage_full {:.0} {:.0}", u8::from(shown), full.width(), full.height()));

    let bar = bar::Bar::new(cx, st, view, host, fullscreen);
    let arr = bar.arrange(ui, full.width());
    let people = strip::people(cx, view, &items);
    let strip_h = if st.strip_hidden || people.is_empty() { 0.0 } else { theme::STRIP_H };
    let bar_h = arr.height();
    let arena = if fullscreen {
        full.shrink(theme::S2)
    } else {
        Rect::from_min_max(full.min + vec2(theme::S4, theme::S4), pos2(full.right() - theme::S4, (full.bottom() - strip_h - bar_h).max(full.top() + theme::S4)))
    };
    // The chrome that lies over the streams in fullscreen (strip and bar), from its top edge down.
    let chrome_top = if fullscreen && alpha > 0.0 { full.bottom() - bar_h - strip_h } else { f32::MAX };
    draw_arena(ui, cx, st, view, &items, arena, shown, chrome_top);

    // Shown but not yet faded in: the widgets are already there, so a click right after the pointer comes back works.
    if alpha <= 0.0 && !shown {
        return;
    }
    let mut ov = ui.new_child(UiBuilder::new().id_salt("stage_chrome").max_rect(full));
    ov.multiply_opacity(alpha);
    let bar_rect = Rect::from_min_max(pos2(full.left(), (full.bottom() - bar_h).max(full.top())), full.max);
    let strip_rect = Rect::from_min_max(pos2(full.left(), bar_rect.top() - strip_h), pos2(full.right(), bar_rect.top()));
    if fullscreen {
        ov.painter().rect_filled(bar_rect, 0.0, theme::STAGE_CHROME);
        if strip_h > 0.0 {
            widgets::paint_gradient(ov.painter(), strip_rect, egui::CornerRadius::ZERO, 180.0, egui::Color32::TRANSPARENT, theme::STAGE_CHROME);
        }
    }
    let mut acts: Vec<UiAction> = Vec::new();
    if strip_h > 0.0 {
        strip::show(&mut ov, cx, strip_rect, view, &people, &items, &mut acts);
    }
    for a in acts {
        cx.act(a);
    }
    bar.show(&mut ov, cx, st, bar_rect, &arr);
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOM: RoomHandle = RoomHandle(1);

    /// A connection per name, unless a test says otherwise ([`of`]).
    fn conn(who: &str) -> ConnId {
        who.bytes().map(u64::from).sum()
    }

    fn item(who: &str, label: &str, kind: Kind) -> Item {
        let mine = matches!(kind, Kind::Mine(..));
        let conn = match &kind {
            Kind::Video(key) | Kind::Idle(key, _) | Kind::Audio(key, _) => key.1,
            Kind::Mine(..) => conn(who),
        };
        Item { id: ItemId { room: ROOM, who: who.into(), label: label.into(), audio: false, nth: 0 }, conn, kind, ar: 16.0 / 9.0, mine, popped: false, in_pip: false }
    }

    fn video(who: &str, label: &str, stream: u8) -> Item {
        of(conn(who), who, label, stream)
    }

    /// A watched stream of the connection `owner`, whatever its name.
    fn of(owner: ConnId, who: &str, label: &str, stream: u8) -> Item {
        item(who, label, Kind::Video((ROOM, owner, stream)))
    }

    fn idle(who: &str, label: &str, stream: u8) -> Item {
        let owner = conn(who);
        let info = StreamInfo { owner, stream_id: stream, kind: StreamKind::Video, label: label.into(), width: 1280, height: 720 };
        item(who, label, Kind::Idle((ROOM, owner, stream), info))
    }

    fn order(items: &[Item]) -> Vec<ItemId> {
        items.iter().map(|i| i.id.clone()).collect()
    }

    /// Plan D1: "Webcam in picture moves a person's webcam into their first playing screen share".
    #[test]
    fn a_webcam_goes_into_its_owners_playing_screen_share() {
        let items = [video("Jonas", "Screen", 1), video("Jonas", "Webcam", 2), video("Ana", "Screen", 3)];
        let pairs = pip_pairs(&items, &order(&items));
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[&items[0].id], 1, "Jonas's webcam sits in Jonas's screen, not Ana's");
        // The webcam may come first in the stage's order: same result.
        let items = [video("Jonas", "Webcam", 2), video("Jonas", "Screen", 1)];
        assert_eq!(pip_pairs(&items, &order(&items))[&items[1].id], 0);
        // The first screen share when there are two.
        let items = [video("Jonas", "Screen", 1), video("Jonas", "Window · mpv", 4), video("Jonas", "Webcam", 2)];
        let pairs = pip_pairs(&items, &order(&items));
        assert_eq!(pairs[&items[0].id], 2);
        assert!(!pairs.contains_key(&items[1].id));
    }

    /// D1: with no screen share playing, or none that you watch, or it is popped out, or you do not watch the
    /// webcam itself (its tile holds the Watch button), the webcam stays a tile.
    #[test]
    fn a_webcam_stays_a_tile_when_there_is_no_playing_share_to_sit_in() {
        let none = |items: &[Item]| pip_pairs(items, &order(items)).is_empty();
        assert!(none(&[video("Jonas", "Webcam", 2)]), "no screen share at all");
        assert!(none(&[idle("Jonas", "Screen", 1), video("Jonas", "Webcam", 2)]), "the screen is not watched");
        assert!(none(&[video("Jonas", "Screen", 1), idle("Jonas", "Webcam", 2)]), "the webcam is not watched");
        let mut popped = [video("Jonas", "Screen", 1), video("Jonas", "Webcam", 2)];
        popped[0].popped = true;
        assert!(none(&popped), "the screen is in its own window");
        popped[0].popped = false;
        popped[1].popped = true;
        assert!(none(&popped), "so is the webcam");
        // An audio-only sender is no picture.
        let owner = conn("Jonas");
        let info = StreamInfo { owner, stream_id: 5, kind: StreamKind::Audio, label: "Desktop audio".into(), width: 0, height: 0 };
        let mut audio = [item("Jonas", "Desktop audio", Kind::Audio((ROOM, owner, 5), info)), video("Jonas", "Webcam", 2)];
        audio[0].id.audio = true;
        assert!(none(&audio));
    }

    /// Your own webcam goes into your own screen (both are `who == ""`), and one parent holds one webcam.
    #[test]
    fn your_own_webcam_sits_in_your_own_screen_and_one_parent_holds_one_webcam() {
        let items = [item("", "", Kind::Mine(CaptureSourceId(0), DestinationId(0))), item("", "Webcam", Kind::Mine(CaptureSourceId(1), DestinationId(1)))];
        assert_eq!(pip_pairs(&items, &order(&items))[&items[0].id], 1);
        let two = [video("Jonas", "Screen", 1), video("Jonas", "Webcam", 2), video("Jonas", "Webcam", 3)];
        let pairs = pip_pairs(&two, &order(&two));
        assert_eq!(pairs.len(), 1, "the second webcam keeps its tile");
        // Another room's person of the same name is another person.
        let mut other_room = [video("Jonas", "Screen", 1), video("Jonas", "Webcam", 2)];
        other_room[1].id.room = RoomHandle(2);
        assert!(pip_pairs(&other_room, &order(&other_room)).is_empty());
    }

    /// The user's report: two clients in one room under one name. A person is a connection, so a webcam goes into its
    /// own connection's screen, not the first screen of that name.
    #[test]
    fn a_webcam_stays_with_its_connection_when_two_people_have_one_name() {
        let mut items = [of(1, "Ritze", "Screen", 1), of(2, "Ritze", "Screen", 1), of(2, "Ritze", "Webcam", 2)];
        number_identical(&mut items);
        let pairs = pip_pairs(&items, &order(&items));
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[&items[1].id], 2, "in the second Ritze's screen");
        // The other Ritze's screen alone is no parent.
        let mut items = [of(1, "Ritze", "Screen", 1), of(2, "Ritze", "Webcam", 2)];
        number_identical(&mut items);
        assert!(pip_pairs(&items, &order(&items)).is_empty());
    }

    /// Two same-named owners sharing one label are two tiles, in `(ConnId, StreamId)` order whatever the list's order is.
    #[test]
    fn identical_items_get_their_own_id_in_a_stable_order() {
        let ids = |mut items: Vec<Item>| {
            number_identical(&mut items);
            let mut v: Vec<(ConnId, u8, u8)> = items.iter().map(|i| (i.conn, i.stream_id().unwrap(), i.id.nth)).collect();
            v.sort();
            v
        };
        let want = vec![(1, 4, 0), (1, 9, 1), (2, 0, 2)];
        assert_eq!(ids(vec![of(2, "Ritze", "Screen", 0), of(1, "Ritze", "Screen", 9), of(1, "Ritze", "Screen", 4)]), want);
        assert_eq!(ids(vec![of(1, "Ritze", "Screen", 4), of(2, "Ritze", "Screen", 0), of(1, "Ritze", "Screen", 9)]), want, "the announcement order does not matter");
        // Different labels, an audio flag or another name are not identical.
        let mut items = vec![of(1, "Ritze", "Screen", 0), of(1, "Ritze", "Webcam", 1), of(2, "Mo", "Screen", 0)];
        number_identical(&mut items);
        assert!(items.iter().all(|i| i.id.nth == 0));
    }

    /// A reconnect gives both owners new `ConnId`s: the two ids stay the same two, whoever gets which, so the layout
    /// keyed by them survives (they may swap places, nothing is lost).
    #[test]
    fn a_reconnect_keeps_both_ids_of_two_same_named_owners() {
        let mut before = [of(10, "Ritze", "Screen", 0), of(20, "Ritze", "Screen", 0)];
        number_identical(&mut before);
        let mut after = [of(99, "Ritze", "Screen", 3), of(5, "Ritze", "Screen", 1)];
        number_identical(&mut after);
        let set = |i: &[Item]| i.iter().map(|x| x.id.clone()).collect::<HashSet<_>>();
        assert_eq!(set(&before).len(), 2);
        assert_eq!(set(&before), set(&after));
    }
}
