//! The stage (task I1, `superdoc/ui/replica/index.html` "Stage"): the central area of a joined room and of
//! "Watched streams" is a black stage holding the room's streams in one of three layouts.
//!
//! - **Split** shows every picture whole in an even-ish grid (the row count that shows the most picture
//!   area), **Crop** fills the same cells (the video is cropped around its centre, `Fit::Cover`), **Floating**
//!   gives each stream a window you move and resize; windows keep the stream's aspect ratio. Split and Crop
//!   resize with the bars between streams; a double-click on a bar evens the grid out again. Dragging a tile
//!   onto another swaps them (Split, Crop). The maths is in [`geom`] (pure, tested), the tile contents in `tile`.
//! - **What is on it:** the room's streams of other people (watched ones as video, the others as a dimmed
//!   "X is sharing Y, Watch stream" tile, audio-only senders with a Listen button), then your own live share.
//!   "Watched" shows every watched stream of every room and nothing else. A popped-out stream is absent.
//! - **Per-view state** (`StageState`, UI-private, per session, not persisted): each room and "Watched" keep
//!   their own order, grid, window rects and z-order. Streams are keyed by [`ItemId`] (room handle + owner
//!   name + label), not by `ConnId`/`StreamId`, so a reconnect keeps the layout. The layout choice and the
//!   two stage options live in [`StageSettings`] (`Shell::stage`), because the Layout menu (a different
//!   screen) reads and sets them through [`UiAction::SetStageLayout`], [`UiAction::SetHideOwnStreams`] and
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

mod geom;
mod tile;

use std::collections::HashMap;

use egui::{Align, CursorIcon, Id, Pos2, Rect, Sense, Stroke, Ui, pos2, vec2};
use simple_screensharing_client::protocol::{StreamInfo, StreamKind};
use simple_screensharing_client::session::{CaptureSourceId, DestinationId, RoomHandle};

use geom::{Bar, FRect, Grid};
#[cfg(test)]
pub(super) use tile::tile_volume_slider;

use super::{Cx, ShareState, StreamKey, UiAction, View, tr};
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

    /// The layout of the stage shown now (Split when there is none); for the Layout menu.
    #[allow(dead_code)]
    pub fn stage_layout(&self) -> StageLayout {
        self.stage_view().map_or_else(StageLayout::default, |v| self.shell.stage.layout(&v))
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
            if let Some(room) = cx.selected_room()
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

/// The stage: fills `ui`'s whole rect.
pub fn show(ui: &mut Ui, cx: &mut Cx, st: &mut StageState) {
    let Some(view) = cx.stage_view() else { return };
    let full = ui.max_rect();
    ui.painter().rect_filled(full, 0.0, theme::STAGE_BG);
    let arena = full.shrink(theme::S4);
    let (hide_own, always_names) = (cx.shell.stage.hide_own, cx.shell.stage.always_names);
    let layout = cx.shell.stage.layout(&view);
    let cxr: &Cx = cx;
    let items = collect(cxr, &view);
    let StageState { views, drag, vol_drag } = st;
    let geom = views.entry(view.clone()).or_default();
    for it in &items {
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
        let resp = ui.interact(rect, Id::new(("stage_tile", &it.id)), Sense::click_and_drag());
        let spot = tile::Spot {
            rect,
            crop,
            round: if float { 0 } else { 8 },
            hovered: hover == Some(i) && !dragging,
            always_names,
            cross_room: matches!(view, StageView::Watched),
        };
        match &it.kind {
            Kind::Video(key) => tile::video_tile(ui, cxr, vol_drag, *key, &spot, &mut acts),
            Kind::Idle(key, info) | Kind::Audio(key, info) => tile::idle_tile(ui, cxr, *key, info, &spot, vol_drag, &mut acts),
            Kind::Mine(capture, destination) => tile::mine_tile(ui, cxr, it.id.room, *capture, *destination, &spot, &mut acts),
        }
        if float {
            ui.painter().rect_stroke(rect, 0.0, Stroke::new(1.0_f32, theme::BORDER), egui::StrokeKind::Outside);
            let handle = Rect::from_min_size(rect.max - vec2(20.0, 20.0), vec2(20.0, 20.0));
            let rz = ui.interact(handle, Id::new(("stage_resize", &it.id)), Sense::drag());
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
        let resp = ui.interact(*rect, Id::new(("stage_bar", bar)), Sense::click_and_drag());
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
