//! Webcam in picture (task I8, the replica's `.pip`): a person's webcam drawn inside their screen share instead of
//! as a tile of its own. The maths is `geom` (pure, tested: [`geom::layout_pip`], [`geom::move_pip`],
//! [`geom::resize_pip`], [`geom::snap_pip`]); this file draws one PIP and reads its pointer input.
//!
//! - **Drag** moves it (kept inside the visible picture, in Crop too), **release** snaps it to the nearest of eight
//!   spots (corners and side middles); **the handle** on the corner facing the middle of the picture resizes it with
//!   its aspect locked and the opposite corner fixed. A click changes nothing. Right click opens the webcam's own
//!   stream menu, a double click is the webcam's fullscreen (as on any stream).
//! - **It never moves what is under it:** the PIP's widgets are registered after its parent tile's, so a press on
//!   it is the PIP's, never a tile drag or a window move.
//! - **Square corners**, unlike the replica's 6 px radius: a video is a wgpu paint callback that cannot be clipped to
//!   a rounded rect (the tiles mask their corners with the stage colour, which would show here as black corners
//!   over the screen share). A thin outline and a shadow set it off instead.
//! - **The snap glides** (the replica's `.pip.snap`, .16 s `ease`, task I12): the spot is the state, the glide only
//!   what is drawn. A drag is 1:1 under the pointer (applied before the layout, [`apply_drag`]); on release the
//!   PIP glides from where it was let go to its spot ([`PipGlide`]: the start, tile-relative, so the glide also
//!   follows a tile that is gliding itself). A resize does not glide. *Why:* the user, "Moving the webcam around
//!   when in webcam PIP mode isnt as smooth as the artifact".

use egui::{Color32, CornerRadius, CursorIcon, Id, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};
use simple_screensharing_client::decode::render::Fit;

use super::geom::{self, Corner, PipGeo, PipPos};
use super::{Item, ItemId, Kind, tile};
use crate::theme;
use crate::ui::{Cx, Popover, PopoverKind, UiAction, drive_dump, tr, tr_with};
use crate::widgets;

/// Side of the resize handle's hit area and glyph box (`.prz`: 20 px).
const HANDLE: f32 = 20.0;

/// What a PIP drag does.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum PipDrag {
    Move,
    /// The handle on this corner.
    Resize(Corner),
}

/// A PIP drag in progress. `from` is the PIP as it was when the press started (tile-relative), `origin` where the
/// pointer was; everything the drag does is computed from those two, so it does not drift.
#[derive(Clone, Debug)]
pub(super) struct PipDragState {
    pub cam: ItemId,
    pub kind: PipDrag,
    pub origin: Pos2,
    pub from: PipGeo,
}

/// A PIP gliding to its spot after a drag: where it was let go (the rect's top left, tile-relative) and when. The
/// end is the PIP's spot as the layout has it now, so a tile that moves meanwhile carries it along.
#[derive(Clone, Copy, Debug)]
pub(super) struct PipGlide {
    pub from: egui::Vec2,
    pub start: f64,
}

/// One PIP to draw.
pub(super) struct Input<'a> {
    /// Name stem for the rig's dump (`pip.<name>`, `pip.<name>.grip`).
    pub name: String,
    /// The parent tile's rect on screen.
    pub tile: Rect,
    /// The parent stream's aspect ratio (what its picture is).
    pub parent_ar: f32,
    /// The parent fills its tile and is cropped around the centre (Crop).
    pub crop: bool,
    /// The webcam's item (`Kind::Video` or `Kind::Mine`).
    pub cam: &'a Item,
    /// The pointer is over the parent tile: the handle shows.
    pub hovered: bool,
    /// This PIP is being dragged.
    pub dragged: bool,
    /// The glide to its spot, if one runs, and the time (egui input time, seconds).
    pub glide: Option<PipGlide>,
    pub now: f64,
}

pub(super) struct Out {
    /// Where it is, tile-relative (what a release snaps).
    pub geo: PipGeo,
    /// A press on it: a floating parent comes to the front.
    pub raise: bool,
    pub started: Option<PipDragState>,
    /// The glide is over (or was never there): the caller drops it.
    pub glide_done: bool,
}

/// Draw one PIP in its parent tile and read its input. `live` limits input to what the fullscreen chrome does
/// not cover (see `draw_arena`); `press` is the press origin of the pointer, for a drag that starts now.
pub(super) fn show(ui: &mut Ui, cx: &Cx, i: &Input, pos: PipPos, press: Option<Pos2>, live: &dyn Fn(Rect) -> Rect, acts: &mut Vec<UiAction>) -> Out {
    let geo = geom::layout_pip(i.tile.width(), i.tile.height(), i.parent_ar, i.cam.ar, i.crop, &pos);
    let off = i.tile.min.to_vec2();
    let mut rect = geo.rect.translate(off);
    // The glide to the spot after a drag; the spot itself (`geo`) is already where it ends.
    let mut glide_done = true;
    if let Some(g) = i.glide.filter(|_| !i.dragged) {
        let t = ((i.now - g.start) / geom::PIP_GLIDE).clamp(0.0, 1.0) as f32;
        if t < 1.0 {
            glide_done = false;
            let min = (g.from + (geo.rect.min.to_vec2() - g.from) * geom::ease(t)).to_pos2() + off;
            rect = Rect::from_min_size(min, rect.size());
            ui.ctx().request_repaint();
        }
    }
    let painter = ui.painter().clone();
    painter.add(egui::Shadow { offset: [0, 6], blur: 18, spread: 0, color: Color32::from_black_alpha(140) }.as_shape(rect, CornerRadius::ZERO));
    match &i.cam.kind {
        Kind::Video(key) => {
            cx.videos.paint(*key, ui, rect, Fit::Contain);
        }
        Kind::Mine(capture, _) => {
            if let Some(c) = cx.capture(*capture) {
                // The PIP stands in for the webcam's own tile, which would have asked for the preview.
                tile::ask_live_preview(ui, c, *capture, acts);
            }
            painter.rect_filled(rect, CornerRadius::ZERO, Color32::BLACK);
            match cx.shell.live_previews.get(capture).and_then(|t| t.tex.as_ref()) {
                Some(tex) => tile::paint_cover(&painter, rect, tex, CornerRadius::ZERO, Color32::WHITE),
                None => widgets::paint_icon(ui, rect, "videocam", 22.0, theme::TEXT_FAINT),
            }
        }
        _ => {}
    }
    painter.rect_stroke(rect, CornerRadius::ZERO, Stroke::new(1.0_f32, Color32::from_white_alpha(56)), StrokeKind::Outside);

    let id = Id::new(("stage_pip", &i.cam.id));
    let resp = ui.interact(live(rect), id, Sense::click_and_drag());
    // The handle sits inside the PIP on its corner (`corner` faces the middle of the picture).
    let c = geo.corner;
    let grip = Rect::from_min_size(
        pos2(if c.right { rect.right() - HANDLE } else { rect.left() }, if c.bottom { rect.bottom() - HANDLE } else { rect.top() }),
        vec2(HANDLE, HANDLE),
    );
    let hr = ui.interact(live(grip), id.with("grip"), Sense::drag());
    drive_dump::rect(|| format!("pip.{}", i.name), rect);
    drive_dump::rect(|| format!("pip.{}.grip", i.name), grip);
    drive_dump::line(|| {
        let spot = pos.spot.map_or("free".to_string(), |(a, b)| format!("{a},{b}"));
        format!("pipinfo {} spot={spot} w={:.3} handle={}{} who={}", i.name, pos.w, if c.bottom { 'b' } else { 't' }, if c.right { 'r' } else { 'l' }, i.cam.id.who)
    });
    if i.hovered || resp.hovered() || hr.hovered() || i.dragged {
        let ink = Color32::from_white_alpha(230);
        let (sx, sy) = (if c.right { 1.0 } else { -1.0 }, if c.bottom { 1.0 } else { -1.0 });
        // The corner glyph of `.prz::after`: an L 4 px in from the box, 2 px strokes, with a dark under-stroke.
        let at = pos2(if c.right { grip.right() } else { grip.left() }, if c.bottom { grip.bottom() } else { grip.top() });
        let a = pos2(at.x - sx * 4.0, at.y - sy * 14.0);
        let b = pos2(at.x - sx * 4.0, at.y - sy * 4.0);
        let d = pos2(at.x - sx * 14.0, at.y - sy * 4.0);
        painter.add(egui::Shape::line(vec![a, b, d], Stroke::new(4.0_f32, Color32::from_black_alpha(120))));
        painter.add(egui::Shape::line(vec![a, b, d], Stroke::new(2.0_f32, ink)));
    }
    if hr.hovered() || hr.dragged() {
        ui.ctx().set_cursor_icon(if c.right == c.bottom { CursorIcon::ResizeNwSe } else { CursorIcon::ResizeNeSw });
    } else if resp.dragged() {
        ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
    } else if resp.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::Grab);
    }
    let who = if i.cam.mine { tr("ui.room.you", "You") } else { i.cam.id.who.clone() };
    let tip = if i.cam.mine {
        tr("ui.stage.pip_tip_mine", "Your webcam: drag to move, right-click for its menu")
    } else {
        tr_with("ui.stage.pip_tip", "%{who}'s webcam: drag to move, right-click for its menu", &[("who", &who)])
    };
    let resp = resp.on_hover_text(tip);

    let mut out = Out { geo, raise: hr.is_pointer_button_down_on() || resp.is_pointer_button_down_on(), started: None, glide_done };
    if let Some(origin) = press {
        let begin = |kind| PipDragState { cam: i.cam.id.clone(), kind, origin, from: geo };
        if hr.drag_started() {
            out.started = Some(begin(PipDrag::Resize(c)));
        } else if resp.drag_started() {
            out.started = Some(begin(PipDrag::Move));
        }
    }
    if resp.double_clicked()
        && let Kind::Video(key) = &i.cam.kind
    {
        acts.push(UiAction::EnterStreamFullscreen(*key));
    }
    if resp.secondary_clicked()
        && let Some(p) = resp.interact_pointer_pos()
    {
        acts.push(UiAction::OpenPopover(Popover { kind: PopoverKind::Tile(i.cam.id.clone()), anchor: Rect::from_min_size(p, vec2(1.0, 1.0)) }));
    }
    out
}

/// A drag in progress, applied to the webcam's stored position (before the frame is laid out, like the stage's
/// other drags, so the PIP under the pointer does not lag a frame).
pub(super) fn apply_drag(d: &PipDragState, pointer: Pos2, pip: &mut std::collections::HashMap<ItemId, PipPos>) {
    let Some(pos) = pip.get_mut(&d.cam) else { return };
    let delta = pointer - d.origin;
    match d.kind {
        PipDrag::Move => geom::move_pip(&d.from, delta, pos),
        PipDrag::Resize(c) => geom::resize_pip(&d.from, c, delta, pos),
    }
}

/// The pointer was released: a moved PIP goes to the nearest spot, from where it was drawn this frame, and glides
/// there from where it was let go.
pub(super) fn release(
    d: &PipDragState,
    geos: &std::collections::HashMap<ItemId, PipGeo>,
    pip: &mut std::collections::HashMap<ItemId, PipPos>,
    glides: &mut std::collections::HashMap<ItemId, PipGlide>,
    now: f64,
) {
    if d.kind == PipDrag::Move
        && let (Some(pos), Some(geo)) = (pip.get_mut(&d.cam), geos.get(&d.cam))
    {
        geom::snap_pip(geo, pos);
        glides.insert(d.cam.clone(), PipGlide { from: geo.rect.min.to_vec2(), start: now });
    }
}
