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
//!   (`Send + Sync + 'static`, `viewer-window.md`) cannot hold; immediate runs inside our own frame. What is
//!   left out: the window is keyed by the room's `RoomHandle`, so it closes when the room reconnects (the stage
//!   comes back in the main window), and modals opened from it appear in the main window.
//! - **Per-view state** (`StageState`, UI-private, per session, not persisted): each room and "Watched" keep
//!   their own order, grid, window rects and z-order. Streams are keyed by [`ItemId`] (room handle + owner
//!   name + label), not by `ConnId`/`StreamId`, so a reconnect keeps the layout. The layout choice and the
//!   two stage options live in [`StageSettings`] (`Shell::stage`), because the Layout menu and the tiles read
//!   them through [`UiAction::SetStageLayout`], [`UiAction::SetHideOwnStreams`] and
//!   [`UiAction::SetAlwaysShowNames`].
//!
//! # Input and bars
//! Every tile is a `Sense::click_and_drag` widget registered in draw order, so with overlapping floating
//! windows the topmost one gets the pointer; the hover overlay belongs to the topmost tile under the pointer.
//! With the pointer between streams (also in the black space around a picture) all bars show in dark grey;
//! over a stream they hide; the bar under the pointer is blue, and stays blue for the whole drag, while the
//! others hide. A drag is applied **before** the frame is laid out, from the pointer's distance to where the
//! press started, so the bar under the pointer does not lag a frame.
//!
//! # Performance
//! No repaint loop: video repaints are frame-driven by `VideoView`, a drag repaints on input.

mod bar;
mod chrome;
mod geom;
mod menus;
mod strip;
mod tile;

use std::collections::{HashMap, HashSet};

use egui::{Align, CursorIcon, Id, Pos2, Rect, Sense, Stroke, Ui, UiBuilder, pos2, vec2};
use simple_screensharing_client::protocol::{StreamInfo, StreamKind};
use simple_screensharing_client::session::{CaptureSourceId, DestinationId, RoomHandle};

use geom::{Bar, FRect, Grid};
#[cfg(test)]
pub(super) use tile::tile_volume_slider;

pub use bar::BarMenu;
use super::{Cx, Popover, PopoverKind, RoomEntry, ShareState, StreamKey, UiAction, View, tr};
use crate::theme::{self, Weight, font};
use crate::widgets::{self, paint_text};

/// What a stage shows: one room, or every watched stream of every room.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum StageView {
    Room(RoomHandle),
    Watched,
}

/// A stage layout (the Layout menu's three choices).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StageLayout {
    #[default]
    Split,
    Crop,
    Floating,
}

/// The stage choices the Layout menu sets (`Shell::stage`; changed only through `UiAction`s).
#[derive(Default)]
pub struct StageSettings {
    layouts: HashMap<StageView, StageLayout>,
    /// "Hide my own streams": your own share gets no tile.
    pub hide_own: bool,
    /// "Always show names": the name tag of a watched stream shows without hovering.
    pub always_names: bool,
}

impl StageSettings {
    /// The layout of `view` (Split until it is changed).
    pub fn layout(&self, view: &StageView) -> StageLayout {
        self.layouts.get(view).copied().unwrap_or_default()
    }

    pub(super) fn set_layout(&mut self, view: StageView, layout: StageLayout) {
        self.layouts.insert(view, layout);
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

    /// The joined room a stage shows (the open room, or the one a pop-out window was made for).
    fn stage_room(&self, h: RoomHandle) -> Option<RoomEntry> {
        self.rooms().into_iter().find(|r| r.handle == Some(h))
    }
}

/// A tile's identity across reconnects: room + owner name + label (+ audio flag, + the n-th of identical
/// ones). Your own share is `who == ""` (one per room). `Why` not `StreamKey`: `ConnId` and `StreamId` change
/// with every reconnect (`WantedStream`).
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
    /// "Show stats on the stream" is on for these watched streams.
    pub(super) stats: HashSet<StreamKey>,
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
}

/// A pointer drag in progress; `origin` is where the press started.
enum Drag {
    Bar { bar: Bar, origin: Pos2, start_center: f32, start: Grid },
    Move { id: ItemId, origin: Pos2, from: FRect },
    Resize { id: ItemId, origin: Pos2, from: FRect },
    Reorder { id: ItemId, origin: Pos2 },
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
    kind: Kind,
    /// Aspect ratio (w/h) of the picture.
    ar: f32,
    mine: bool,
    popped: bool,
}

fn aspect(w: f32, h: f32) -> f32 {
    if w > 0.0 && h > 0.0 { w / h } else { 16.0 / 9.0 }
}

/// What `view` shows, in the session's order.
fn collect(cx: &Cx, view: &StageView) -> Vec<Item> {
    let mut out: Vec<Item> = Vec::new();
    let mut add = |room: RoomHandle, who: String, label: String, audio: bool, kind: Kind, ar: f32| {
        let nth = out.iter().filter(|i| i.id.room == room && i.id.who == who && i.id.label == label && i.id.audio == audio).count() as u8;
        let popped = matches!(&kind, Kind::Video(k) if cx.is_popped(*k));
        let mine = matches!(kind, Kind::Mine(..));
        out.push(Item { id: ItemId { room, who, label, audio, nth }, kind, ar, mine, popped });
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
                    add(*h, who, info.label.clone(), true, Kind::Audio(key, info.clone()), aspect(0.0, 0.0));
                } else if cx.videos.get(key).is_some() {
                    add(*h, who, info.label.clone(), false, Kind::Video(key), video_ar(key, Some(info)));
                } else {
                    add(*h, who, info.label.clone(), false, Kind::Idle(key, info.clone()), aspect(info.width as f32, info.height as f32));
                }
            }
            if let Some(room) = cx.stage_room(*h)
                && let ShareState::Live { capture, destination } = cx.share_state(&room)
                && let Some(d) = cx.capture(capture).and_then(|c| c.destinations.get(&destination))
            {
                let p = match &d.runtime {
                    Some(rt) => rt.lock().unwrap().params(),
                    None => d.params,
                };
                add(*h, String::new(), String::new(), false, Kind::Mine(capture, destination), aspect(p.width as f32, p.height as f32));
            }
        }
        StageView::Watched => {
            for key in cx.watched() {
                let info = cx.session(key.0).and_then(|s| s.streams.iter().find(|i| i.owner == key.1 && i.stream_id == key.2));
                let label = info.map(|i| i.label.clone()).unwrap_or_default();
                add(key.0, cx.member_name(key.0, key.1), label, false, Kind::Video(key), video_ar(key, info));
            }
        }
    }
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
    let (hide_own, always_names) = (cx.shell.stage.hide_own, cx.shell.stage.always_names);
    let layout = cx.shell.stage.layout(view);
    let cxr: &Cx = cx;
    let StageState { views, drag, stats, .. } = st;
    let geom = views.entry(view.clone()).or_default();
    for it in items {
        if !geom.order.contains(&it.id) {
            geom.order.push(it.id.clone());
        }
    }
    let vis: Vec<&Item> =
        geom.order.iter().filter_map(|id| items.iter().find(|i| &i.id == id)).filter(|i| !i.popped && !(hide_own && i.mine)).collect();
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
    let mut paint_order: Vec<usize> = (0..vis.len()).collect();
    if float {
        paint_order.sort_by_key(|&i| geom.z.iter().position(|z| *z == vis[i].id).map_or(-1, |p| p as i32));
    }
    let reordering = match drag {
        Some(Drag::Reorder { id, origin }) => vis.iter().position(|i| &i.id == id).zip(pointer.map(|p| p - *origin)),
        _ => None,
    };
    let mut shown = rects.clone();
    if let Some((i, delta)) = reordering {
        shown[i] = rects[i].translate(delta);
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
    for &i in &paint_order {
        let (it, rect) = (vis[i], shown[i]);
        let resp = ui.interact(live(rect), Id::new(("stage_tile", &it.id)), Sense::click_and_drag());
        let spot = tile::Spot {
            rect,
            crop,
            round: if float { 0 } else { 8 },
            hovered: hover == Some(i) && !dragging && hover_ok && menu_ok(cxr, &it.id),
            always_names,
            stats: matches!(&it.kind, Kind::Video(k) if stats.contains(k)),
            cross_room: matches!(view, StageView::Watched),
            id: it.id.clone(),
        };
        match &it.kind {
            Kind::Video(key) => tile::video_tile(ui, cxr, *key, &spot, &mut acts),
            Kind::Idle(key, info) | Kind::Audio(key, info) => tile::idle_tile(ui, cxr, *key, info, &spot, &mut acts),
            Kind::Mine(capture, destination) => tile::mine_tile(ui, cxr, it.id.room, *capture, *destination, &spot, &mut acts),
        }
        if float {
            ui.painter().rect_stroke(rect, 0.0, Stroke::new(1.0_f32, theme::BORDER), egui::StrokeKind::Outside);
            let handle = Rect::from_min_size(rect.max - vec2(20.0, 20.0), vec2(20.0, 20.0));
            let rz = ui.interact(live(handle), Id::new(("stage_resize", &it.id)), Sense::drag());
            let grip = Stroke::new(1.5_f32, theme::TEXT_DIM);
            for k in [4.0, 10.0] {
                ui.painter().line_segment([pos2(handle.right() - 2.0, handle.bottom() - k), pos2(handle.right() - k, handle.bottom() - 2.0)], grip);
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
            && let Some(from) = geom.order.iter().position(|o| o == id)
            && let Some(target) = vis.iter().zip(&rects).find(|(it, r)| &it.id != id && r.contains(p)).map(|(it, _)| it.id.clone())
            && let Some(to) = geom.order.iter().position(|o| *o == target)
        {
            geom.order.swap(from, to);
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
    let mut child = ui.new_child(UiBuilder::new().max_rect(br));
    let label = tr("ui.stage.bring_back", "Bring it back");
    if widgets::Btn::secondary(&label).icon("close_fullscreen").show_sized(&mut child, br.width()).clicked() {
        st.popped.remove(&view);
    }
}

/// The popped-out stages: one immediate viewport each, drawing the same stage ([`Host::Window`]). Closing the
/// window brings the stage back; so does a room that is gone.
pub fn show_windows(ctx: &egui::Context, cx: &mut Cx, st: &mut StageState) {
    let views: Vec<StageView> = st.popped.iter().cloned().collect();
    for view in views {
        let title = match &view {
            StageView::Room(h) => cx.room_name(*h),
            StageView::Watched => tr("ui.room.watched_title", "Watched streams"),
        };
        if matches!(&view, StageView::Room(h) if cx.stage_room(*h).is_none()) {
            st.popped.remove(&view);
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
            egui::CentralPanel::default().frame(egui::Frame::NONE.fill(theme::STAGE_BG)).show(ctx, |ui| show(ui, cx, st, Host::Window, &view));
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
    let items = collect(cx, view);
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
