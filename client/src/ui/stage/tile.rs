//! What one stage tile shows: a watched stream's video with its hover overlay, a stream you can still
//! watch (dimmed preview + Watch), an audio-only sender (Listen), or your own share (live preview).
//!
//! The overlay of a watched tile is the old tile's (vignette, statbar, round buttons, audio pill) with a
//! fullscreen button added; the stream menu, stats toggle and PIP are a later rebuild (task I3 / I8).
//! Decoder stats (and the decoder lock for the bitrate) are only read while the overlay is visible.

use egui::epaint::{Mesh, RectShape, Vertex, WHITE_UV};
use egui::{Align, Color32, CornerRadius, Id, Layout, Rect, Response, Sense, Shape, Stroke, StrokeKind, Ui, UiBuilder, pos2, vec2};
use simple_screensharing_client::capture::destination::DestinationState;
use simple_screensharing_client::decode::ViewState;
use simple_screensharing_client::decode::render::Fit;
use simple_screensharing_client::protocol::{StreamInfo, StreamKind};
use simple_screensharing_client::session::{CaptureSourceId, DestinationId, RoomHandle};

use super::VolDrag;
use crate::theme::{self, Weight, font};
use crate::ui::{Cx, Modal, StreamKey, UiAction, share_label, tr, tr_with};
use crate::widgets::{self, Btn, FILL, HBox, fixed, paint_text, text_width};

/// Where and how a tile is drawn this frame.
pub(super) struct Spot {
    pub rect: Rect,
    /// Fill the rect (Crop) instead of fitting the picture whole.
    pub crop: bool,
    /// Corner radius; 0 in Floating, where windows overlap (the corner masks would paint over the one underneath).
    pub round: u8,
    /// The pointer is over this tile (and no tile above it).
    pub hovered: bool,
    pub always_names: bool,
    /// Label tiles with the room too ("Watched").
    pub cross_room: bool,
}

/// Rounded corners over a wgpu video. `Why` masks: the video is a paint callback that cannot be clipped to a
/// rounded rect, so the stage colour is painted over its corners.
fn corner_masks(ui: &Ui, rect: Rect, radius: u8) {
    if radius == 0 {
        return;
    }
    let r = radius as f32;
    let mut mesh = Mesh::default();
    for (corner, sx, sy) in [(rect.left_top(), 1.0, 1.0), (rect.right_top(), -1.0, 1.0), (rect.left_bottom(), 1.0, -1.0), (rect.right_bottom(), -1.0, -1.0)] {
        // Triangle fan from the corner point over the quarter arc.
        let c = pos2(corner.x + sx * r, corner.y + sy * r);
        let base = mesh.vertices.len() as u32;
        mesh.vertices.push(Vertex { pos: corner, uv: WHITE_UV, color: theme::STAGE_BG });
        const STEPS: u32 = 6;
        for s in 0..=STEPS {
            let a = std::f32::consts::FRAC_PI_2 * s as f32 / STEPS as f32;
            mesh.vertices.push(Vertex { pos: pos2(c.x - sx * r * a.cos(), c.y - sy * r * a.sin()), uv: WHITE_UV, color: theme::STAGE_BG });
        }
        for s in 0..STEPS {
            mesh.indices.extend_from_slice(&[base, base + 1 + s, base + 2 + s]);
        }
    }
    ui.painter().add(Shape::mesh(mesh));
}

/// `.tag`: a dark 26 px pill at the tile's top left, `[icon][name][· label]`, truncated to the tile.
pub(super) fn name_tag(ui: &Ui, rect: Rect, icon: &str, icon_col: Color32, name: &str, label: &str) {
    let (nf, lf) = (font(Weight::Bold, theme::FS_ROW), font(Weight::Regular, theme::FS_ROW));
    let label = format!("· {label}");
    let (nw, lw) = (text_width(ui, name, nf.clone(), 0.0).ceil(), text_width(ui, &label, lf.clone(), 0.0).ceil());
    let w = (7.0 + 15.0 + 6.0 + nw + 6.0 + lw + 9.0).min(rect.width() - 16.0);
    if w < 40.0 {
        return;
    }
    let bar = Rect::from_min_size(rect.min + vec2(theme::S2, theme::S2), vec2(w, 26.0));
    ui.painter().rect_filled(bar, CornerRadius::same(7), theme::STAGE_TAG);
    widgets::paint_icon(ui, Rect::from_min_size(bar.min + vec2(7.0, 0.0), vec2(15.0, 26.0)), icon, 15.0, icon_col);
    let tx = bar.left() + 7.0 + 15.0 + 6.0;
    let nr = paint_text(ui, Rect::from_x_y_ranges(tx..=bar.right() - 9.0, bar.y_range()), name, nf, theme::TEXT, Align::Min);
    if nr.right() + 6.0 < bar.right() - 9.0 {
        paint_text(ui, Rect::from_x_y_ranges(nr.right() + 6.0..=bar.right() - 9.0, bar.y_range()), &label, lf, theme::TEXT_DIM, Align::Min);
    }
}

/// A watched stream: its video (Contain, or Cover in Crop), the green ring while its shared audio is audible,
/// the health chip, and the hover overlay.
pub(super) fn video_tile(ui: &mut Ui, cx: &Cx, drag: &mut VolDrag, key: StreamKey, s: &Spot, acts: &mut Vec<UiAction>) {
    cx.videos.paint(key, ui, s.rect, if s.crop { Fit::Cover } else { Fit::Contain });
    corner_masks(ui, s.rect, s.round);
    let audible = cx.session(key.0).is_some_and(|x| x.sibling_audio_stream(key.1).is_some() && !x.volume(key.1, key.2).muted);
    if audible {
        ui.painter().rect_stroke(s.rect, CornerRadius::same(s.round), Stroke::new(2.0_f32, theme::SUCCESS), StrokeKind::Inside);
    }
    health_chip(ui, s.rect, stream_health(cx, key));
    if s.always_names && !s.hovered {
        let name = cx.member_name(key.0, key.1);
        let label = cx.session(key.0).and_then(|x| x.streams.iter().find(|i| i.owner == key.1 && i.stream_id == key.2)).map(|i| share_label(&i.label)).unwrap_or_default();
        let icon = if audible { "volume_up" } else { "volume_off" };
        name_tag(ui, s.rect, icon, theme::TEXT_DIM, &name, &label);
    }
    tile_overlay(ui, cx, drag, key, s, acts);
}

/// A stream in the room you do not watch yet (or an audio-only sender): a dark tile with the last preview
/// dimmed behind `[avatar][X is sharing Y][1920×1080][Watch stream][Pop out]` (audio only: `[Listen]`).
pub(super) fn idle_tile(ui: &mut Ui, cx: &Cx, key: StreamKey, info: &StreamInfo, s: &Spot, drag: &mut VolDrag, acts: &mut Vec<UiAction>) {
    let audio_only = info.kind == StreamKind::Audio;
    let rect = s.rect;
    let radius = CornerRadius::same(s.round);
    ui.painter().rect_filled(rect, radius, Color32::BLACK);
    if let Some(tex) = cx.session(key.0).and_then(|x| x.preview_textures.get(&(key.1, key.2))).filter(|_| !audio_only) {
        paint_cover(ui.painter(), rect, tex, radius, Color32::from_gray(90));
    }
    let name = cx.member_name(key.0, key.1);
    let label = share_label(&info.label);
    let listening = audio_only && cx.is_watching(key);
    let btns_h = theme::CTL_H;
    // Short tiles keep only the button, like the replica's container query.
    let full = rect.height() >= 200.0;
    let block_h = if full { 52.0 + theme::S2 + 18.0 + 4.0 + 16.0 + theme::S3 + btns_h } else { btns_h };
    let top = rect.center().y - block_h / 2.0;
    let mut y = top;
    if full {
        let disc = Rect::from_center_size(pos2(rect.center().x, y + 26.0), vec2(52.0, 52.0));
        widgets::paint_avatar(ui, disc, &initial(&name), theme::avatar_colors(&name), false);
        y += 52.0 + theme::S2;
        let text = if audio_only {
            tr_with("ui.stage.sharing_audio", "%{name} is sharing audio", &[("name", &name)])
        } else {
            tr_with("ui.stage.sharing", "%{name} is sharing %{label}", &[("name", &name), ("label", &label)])
        };
        let line = |y: f32, h: f32| Rect::from_x_y_ranges(rect.x_range().shrink(theme::S3), y..=y + h);
        paint_text(ui, line(y, 18.0), &text, font(Weight::Bold, 14.0), theme::TEXT, Align::Center);
        y += 18.0 + 4.0;
        let specs = if audio_only {
            tr("ui.room.audio_only", "Audio only")
        } else if info.width > 0 && info.height > 0 {
            format!("{}×{}", info.width, info.height)
        } else {
            String::new()
        };
        paint_text(ui, line(y, 16.0), &specs, font(Weight::Mono, theme::FS_SMALL), theme::TEXT_DIM, Align::Center);
        y += 16.0 + theme::S3;
    }
    let (watch, pop, listen, unlisten) =
        (tr("ui.stage.watch_stream", "Watch stream"), tr("ui.room.pop_out", "Pop out"), tr("ui.room.listen", "Listen"), tr("ui.room.stop_listening", "Stop listening"));
    let pop_tip = tr("ui.room.pop_out_card_tip", "Watch in its own window");
    let two = !audio_only && rect.width() >= 340.0;
    let bw = if two { 320.0 } else { 160.0_f32 }.min(rect.width() - 2.0 * theme::S3).max(0.0);
    let br = Rect::from_min_size(pos2(rect.center().x - bw / 2.0, y), vec2(bw, btns_h));
    let mut child = ui.new_child(UiBuilder::new().max_rect(br).layout(Layout::left_to_right(Align::Center)));
    let btns: Vec<Btn> = if audio_only {
        vec![if listening { Btn::secondary(&unlisten).icon("volume_off") } else { Btn::primary(&listen).icon("volume_up") }]
    } else if two {
        vec![Btn::primary(&watch).icon("play_arrow"), Btn::secondary(&pop).icon("open_in_new").tip(&pop_tip)]
    } else {
        vec![Btn::primary(&watch).icon("play_arrow")]
    };
    match (widgets::btn_row(&mut child, &btns), audio_only) {
        (Some(_), true) => acts.push(UiAction::ToggleListen(key)),
        (Some(0), false) => acts.push(UiAction::Watch(key)),
        (Some(1), false) => acts.push(UiAction::WatchPoppedOut(key)),
        _ => {}
    }
    if listening && s.hovered {
        let inner = rect.shrink(theme::S2);
        audio_pill(ui, cx, drag, key, Rect::from_min_size(pos2(inner.left(), inner.bottom() - OVL_H), vec2(inner.width(), OVL_H)), acts);
    }
}

/// Your own share in this room: a live preview of what you capture while the window is focused (task #78), a
/// screen-hatch placeholder otherwise, the tag `You · label`, and on hover `[viewers][PREVIEW]` and a Stream
/// settings button.
pub(super) fn mine_tile(ui: &mut Ui, cx: &Cx, h: RoomHandle, capture: CaptureSourceId, destination: DestinationId, s: &Spot, acts: &mut Vec<UiAction>) {
    let Some(c) = cx.capture(capture) else { return };
    let Some(d) = c.destinations.get(&destination) else { return };
    let rect = s.rect;
    let radius = CornerRadius::same(s.round);
    // Asking for the preview is what keeps it running: only a focused window paints this (`App::poll_live_previews`).
    if c.kind == StreamKind::Video && ui.ctx().input(|i| i.viewport().focused.unwrap_or(true) && !i.viewport().minimized.unwrap_or(false)) {
        acts.push(UiAction::LivePreviewSeen(capture));
    }
    let state = cx.destination_state(c, d);
    let recovering = cx.state.recovery(capture).is_some() && matches!(state, DestinationState::Live | DestinationState::Stalled { .. } | DestinationState::Failed(_));
    let (chip, on_air) = if recovering {
        (tr("ui.room.chip_recovering", "RECOVERING"), false)
    } else {
        match state {
            DestinationState::Live | DestinationState::Rebuilding { .. } => ("LIVE".to_owned(), true),
            DestinationState::Starting => (tr("ui.room.chip_starting", "STARTING"), false),
            DestinationState::Stalled { .. } => (tr("ui.room.chip_stalled", "STALLED"), false),
            DestinationState::Failed(_) => (tr("ui.room.chip_failed", "FAILED"), false),
        }
    };
    let live_tex = cx.shell.live_previews.get(&capture).and_then(|t| t.tex.as_ref());
    if let Some(tex) = live_tex {
        ui.painter().rect_filled(rect, radius, theme::TILE_BG);
        paint_cover(ui.painter(), rect, tex, radius, Color32::WHITE);
    } else {
        widgets::paint_hatch(ui.painter(), rect, radius, 8.0, theme::ELEV_19, theme::ELEV_14);
        thumb_placeholder(ui, rect, "cast", theme::SUCCESS, &format!("{} · {chip}", tr("ui.room.my_screen", "Your screen")));
    }
    if state == DestinationState::Starting && cx.state.capture_awaiting_first_frame(capture) {
        ui.interact(rect, Id::new(("stage_mine_hint", capture, destination)), Sense::hover())
            .on_hover_text(tr("ui.waiting_frame_hint", "An idle screen sends no picture until something on it changes. Move the mouse over the shared screen and the stream starts."));
    }
    let label = if on_air { share_label(&c.label) } else { format!("{} · {chip}", share_label(&c.label)) };
    name_tag(ui, rect, "cast", if on_air { theme::SUCCESS } else { theme::WARNING }, &tr("ui.room.you", "You"), &label);
    if !s.hovered {
        return;
    }
    // Top right: [PREVIEW][viewers][Stream settings], added after the tile's own widgets so they sit above.
    let inner = rect.shrink(theme::S2);
    let btn = Rect::from_min_size(pos2(inner.right() - OVL_H, inner.top()), vec2(OVL_H, OVL_H));
    let row = Rect::from_min_max(pos2(inner.left(), inner.top()), pos2(btn.left() - theme::S1, inner.top() + OVL_H));
    let mut r = ui.new_child(UiBuilder::new().max_rect(row).layout(Layout::right_to_left(Align::Center)));
    if let Some(names) = viewer_names(cx, h, d.stream_id) {
        let chip = widgets::chip_dark(&mut r, Some("visibility"), false, &names.len().to_string());
        viewers_dropdown(ui, &chip, &names);
    }
    if live_tex.is_some() {
        // Says the picture is a local preview, not what viewers get.
        widgets::chip_dark(&mut r, None, false, &tr("ui.room.preview_chip", "PREVIEW"))
            .on_hover_text(tr("ui.room.preview_tip", "A live preview of your screen, shown while this window is focused. Viewers get the full quality stream."));
    }
    if round_btn(ui, btn, Id::new(("stage_mine_settings", capture)), "tune", &tr("ui.room.stream_settings", "Stream settings"), false).clicked() {
        acts.push(UiAction::OpenModal(Modal::StreamSettings { capture, room: h }));
    }
}

fn initial(name: &str) -> String {
    name.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_default()
}

fn fmt_kbps(kbps: f64) -> String {
    if kbps >= 1000.0 { format!("{:.1} Mbps", kbps / 1000.0) } else { format!("{kbps:.0} kbps") }
}

/// Statbar / round button / audio pill height.
const OVL_H: f32 = 29.0;

/// Cover-fit a preview texture into `rect` (crop, never letterbox), tinted by `tint`.
fn paint_cover(p: &egui::Painter, rect: Rect, tex: &egui::TextureHandle, radius: CornerRadius, tint: Color32) {
    let [tw, th] = tex.size();
    let (ta, ra) = (tw as f32 / th.max(1) as f32, rect.width() / rect.height());
    let uv = if ta > ra {
        let k = ra / ta;
        Rect::from_min_max(pos2(0.5 - k / 2.0, 0.0), pos2(0.5 + k / 2.0, 1.0))
    } else {
        let k = ta / ra;
        Rect::from_min_max(pos2(0.0, 0.5 - k / 2.0), pos2(1.0, 0.5 + k / 2.0))
    };
    p.add(RectShape::filled(rect, radius, tint).with_texture(tex.id(), uv));
}

/// What the viewer's decoder says about a watched stream (recovering from loss,
/// or the sharer's capture is stalled). `Healthy` when unknown.
fn stream_health(cx: &Cx, key: StreamKey) -> ViewState {
    cx.session(key.0)
        .and_then(|s| s.watches.lock().unwrap().get(&(key.1, key.2)).map(|w| w.decoder.lock().unwrap().view_state(std::time::Instant::now())))
        .unwrap_or(ViewState::Healthy)
}

/// A centred chip over a tile whose stream is not healthy, so a frozen picture is never
/// mistaken for a live one: amber dot + "Recovering…" (a keyframe was lost or the link
/// is down; the viewer is already asking for a new one) or "Sharer's capture stalled"
/// (the sender flagged it; advisory, nothing acts on it). Nothing is drawn when healthy.
fn health_chip(ui: &Ui, rect: Rect, state: ViewState) {
    let text = match state {
        ViewState::Healthy => return,
        ViewState::Recovering => tr("video.recovering", "Recovering…"),
        ViewState::SharerStalled => tr("video.sharer_stalled", "Sharer's capture stalled"),
    };
    let f = font(Weight::Bold, theme::FS_SMALL);
    let dot = 7.0;
    let w = (text_width(ui, &text, f.clone(), 0.0) + dot + theme::S2 + 2.0 * theme::S3).min(rect.width() - 2.0 * theme::S2);
    let r = Rect::from_center_size(rect.center(), vec2(w, OVL_H));
    ui.painter().rect_filled(r, CornerRadius::same(15), theme::CHIP_DARK);
    ui.painter().circle_filled(pos2(r.left() + theme::S3 + dot / 2.0, r.center().y), dot / 2.0, theme::WARNING);
    let tr_ = Rect::from_x_y_ranges(r.left() + theme::S3 + dot + theme::S2..=r.right() - theme::S3, r.y_range());
    paint_text(ui, tr_, &text, f, theme::TEXT, Align::Min);
}


/// The hover overlay of a tile: vignette, `[hbox·top: fill statbar, Pop out,
/// Fullscreen, Stop watching]`, `[hbox·bottom: audio pill]`, inset 8. Fades in over
/// 0.12 s while the pointer is over the tile; nothing is built at alpha 0.
fn tile_overlay(ui: &mut Ui, cx: &Cx, drag: &mut VolDrag, key: StreamKey, s: &Spot, acts: &mut Vec<UiAction>) {
    let (rect, cross_room) = (s.rect, s.cross_room);
    let id = Id::new(("tile_overlay", key));
    let alpha = ui.ctx().animate_bool_with_time(id, s.hovered, 0.12);
    if alpha <= 0.0 {
        return;
    }
    let mut ov = ui.new_child(UiBuilder::new().id_salt(id).max_rect(rect).layout(Layout::left_to_right(Align::Center)));
    ov.set_clip_rect(rect.intersect(ui.clip_rect()));
    ov.multiply_opacity(alpha);

    // Vignette: .55 → .1 over the top 40 %, → .82 at the bottom.
    let v = |a: f32| Color32::from_rgba_unmultiplied(10, 11, 13, (a * 255.0) as u8);
    let split = rect.top() + rect.height() * 0.4;
    let tr_ = s.round;
    let top_r = CornerRadius { nw: tr_, ne: tr_, sw: 0, se: 0 };
    let bot_r = CornerRadius { nw: 0, ne: 0, sw: tr_, se: tr_ };
    widgets::paint_gradient(ov.painter(), Rect::from_min_max(rect.min, pos2(rect.right(), split)), top_r, 180.0, v(0.55), v(0.10));
    widgets::paint_gradient(ov.painter(), Rect::from_min_max(pos2(rect.left(), split), rect.max), bot_r, 180.0, v(0.10), v(0.82));

    let inner = rect.shrink(theme::S2);
    // --- top: [fill statbar][29 pop out][29 fullscreen][29 stop], gaps 8 / 4 -------------
    let top = Rect::from_min_size(inner.min, vec2(inner.width(), OVL_H));
    let stop_r = Rect::from_min_size(pos2(top.right() - OVL_H, top.top()), vec2(OVL_H, OVL_H));
    let fs_r = stop_r.translate(vec2(-(OVL_H + theme::S1), 0.0));
    let pop_r = fs_r.translate(vec2(-(OVL_H + theme::S1), 0.0));
    let bar_cell = Rect::from_min_max(top.min, pos2(pop_r.left() - theme::S2, top.bottom()));
    statbar(&mut ov, cx, key, bar_cell, cross_room);
    if round_btn(&mut ov, pop_r, id.with("pop"), "open_in_new", &tr("ui.room.pop_out_tip", "Pop out to its own window"), false).clicked() {
        acts.push(UiAction::PopOut(key));
    }
    if round_btn(&mut ov, fs_r, id.with("fs"), "fullscreen", &tr("ui.stage.fullscreen", "Fullscreen"), false).clicked() {
        acts.push(UiAction::EnterStreamFullscreen(key));
    }
    if round_btn(&mut ov, stop_r, id.with("stop"), "close", &tr("ui.room.stop_watching", "Stop watching"), true).clicked() {
        acts.push(UiAction::Unwatch(key));
    }

    // --- bottom: audio pill, or "No audio shared" --------------------------
    let bottom = Rect::from_min_size(pos2(inner.left(), inner.bottom() - OVL_H), vec2(inner.width(), OVL_H));
    let has_audio = cx.session(key.0).is_some_and(|s| s.sibling_audio_stream(key.1).is_some());
    if has_audio {
        audio_pill(&mut ov, cx, drag, key, bottom, acts);
    } else {
        let mut c = ov.new_child(UiBuilder::new().max_rect(bottom).layout(Layout::left_to_right(Align::Center)));
        widgets::chip_dark(&mut c, Some("volume_off"), false, &tr("ui.room.no_audio", "No audio shared"));
    }
}

/// `hbox·statbar` (29 px pill, pad 10, gap 8): `[7 dot][name][res][fps 6ch][rate 8ch]`,
/// content-sized up to `cell`'s width (the name truncates). The numbers sit in
/// fixed tabular-width slots so the bar doesn't jitter as they change. Hover
/// shows decoded vs presented fps (presentation must keep up with decode).
/// In a narrow tile ([`statbar_fields`]) the resolution and then the bitrate
/// leave the bar for the tooltip instead of being cut off mid-number.
fn statbar(ui: &mut Ui, cx: &Cx, key: StreamKey, cell: Rect, cross_room: bool) {
    let Some(entry) = cx.videos.get(key) else { return };
    let stats = entry.view.lock().unwrap().source().stats();
    let kbps = cx
        .session(key.0)
        .and_then(|s| s.watches.lock().unwrap().get(&(key.1, key.2)).map(|w| w.decoder.lock().unwrap().received_bitrate_kbps()))
        .unwrap_or(0.0);
    let name = cx.member_name(key.0, key.1);
    let name = if cross_room { format!("{name} · {}", cx.room_name(key.0)) } else { name };
    let res = cx.videos.frame_size(key).map(|(w, h)| format!("{w}x{h}")).unwrap_or_default();
    let fps = format!("{:.0}fps", stats.decoded_fps);
    let rate = fmt_kbps(kbps);

    let nf = font(Weight::Bold, theme::FS_ROW);
    let mf = font(Weight::Mono, theme::FS_SMALL);
    let ch = text_width(ui, "0", mf.clone(), 0.0);
    let (res_w, fps_w, rate_w) = (text_width(ui, &res, mf.clone(), 0.0).ceil(), (6.0 * ch).ceil(), (8.0 * ch).ceil());
    let (show_res, show_rate) = statbar_fields(cell.width(), res_w, fps_w, rate_w);
    let (res_w, rate_w) = (if show_res { res_w } else { 0.0 }, if show_rate { rate_w } else { 0.0 });
    let fixed_w = statbar_fixed_w(res_w, fps_w, rate_w);
    let name_w = text_width(ui, &name, nf.clone(), 0.0).ceil().min((cell.width() - fixed_w).max(0.0));
    let w = (fixed_w + name_w).min(cell.width());
    let bar = Rect::from_min_size(cell.min, vec2(w, OVL_H));
    let p = ui.painter();
    p.rect_filled(bar, CornerRadius::same(15), Color32::from_black_alpha(158));
    let mut child = ui.new_child(UiBuilder::new().max_rect(bar).layout(Layout::left_to_right(Align::Center)));
    child.shrink_clip_rect(bar);
    let slots = [fixed(7.0), fixed(name_w), fixed(res_w), fixed(fps_w), fixed(rate_w)];
    let row = HBox::new("hbox·statbar", OVL_H).pad(10.0, 10.0).slots(&slots).show(&mut child);
    let c = row.cell(0).center();
    // Amber instead of green while the stream is recovering or the sharer's capture is stalled.
    let (dot, glow) = if stream_health(cx, key) == ViewState::Healthy {
        (theme::SUCCESS, theme::SUCCESS_GLOW)
    } else {
        (theme::WARNING, theme::WARNING)
    };
    child.painter().circle_filled(c, 5.0, glow.gamma_multiply(0.25));
    child.painter().circle_filled(c, 3.5, dot);
    let num = Color32::from_rgba_unmultiplied(232, 233, 236, 217);
    row.text(&child, 1, &name, nf, theme::TEXT, Align::Min);
    if show_res {
        row.text(&child, 2, &res, mf.clone(), num, Align::Min);
    }
    row.text(&child, 3, &fps, mf.clone(), num, Align::Max);
    if show_rate {
        row.text(&child, 4, &rate, mf, num, Align::Max);
    }
    row.response.on_hover_ui(|ui| {
        // What a narrow bar left out, so it is still one hover away.
        let hidden: Vec<&str> = [(!show_res).then_some(res.as_str()), (!show_rate).then_some(rate.as_str())]
            .into_iter()
            .flatten()
            .filter(|t| !t.is_empty())
            .collect();
        if !hidden.is_empty() {
            ui.label(hidden.join(" · "));
        }
        let codec = stats.info.codec.map(|c| c.name()).unwrap_or("—");
        let dec = stats.info.decoder_name.as_deref().unwrap_or("—");
        ui.label(tr_with(
            "ui.room.stats_tip",
            "Decoded %{decoded} fps · shown %{presented} fps\n%{codec} via %{decoder} · %{dropped} dropped",
            &[
                ("decoded", &format!("{:.0}", stats.decoded_fps)),
                ("presented", &format!("{:.0}", stats.presented_fps)),
                ("codec", codec),
                ("decoder", dec),
                ("dropped", &stats.access_units_dropped.to_string()),
            ],
        ));
    });
}

/// Width of the statbar without its name: pads 10+10, the 7 px dot and a gap
/// 8 before every following slot (a 0-wide slot keeps its gap; harmless).
fn statbar_fixed_w(res_w: f32, fps_w: f32, rate_w: f32) -> f32 {
    10.0 + 7.0 + theme::S2 + theme::S2 + res_w + theme::S2 + fps_w + theme::S2 + rate_w + 10.0
}

/// Which optional statbar numbers fit `cell_w`, as `(resolution, bitrate)`;
/// fps always stays. `Why` this order: at the 800 px minimum window (PR #4)
/// a room tile is ~264 px and its statbar cell ~178 px, too narrow for
/// `1920x1080 30fps 7.6 Mbps`, which was cut mid-number. The resolution
/// changes least (the share card shows it too), the received bitrate is
/// next, fps is the health signal. Hidden values move into the tooltip.
fn statbar_fields(cell_w: f32, res_w: f32, fps_w: f32, rate_w: f32) -> (bool, bool) {
    if statbar_fixed_w(res_w, fps_w, rate_w) <= cell_w {
        (true, true)
    } else if statbar_fixed_w(0.0, fps_w, rate_w) <= cell_w {
        (false, true)
    } else {
        (false, false)
    }
}

/// `.round-btn`: 29 px circle, translucent dark fill, faint white ring,
/// 16 px icon; `stop` turns the icon red on hover.
fn round_btn(ui: &mut Ui, rect: Rect, id: Id, icon: &str, tip: &str, stop: bool) -> Response {
    let resp = ui.interact(rect, id, Sense::click());
    let hover = resp.hovered();
    let p = ui.painter();
    let fill = Color32::from_rgba_unmultiplied(16, 17, 20, if hover { 204 } else { 140 });
    p.circle(rect.center(), rect.width() / 2.0 - 0.5, fill, Stroke::new(1.0_f32, Color32::from_white_alpha(41)));
    let col = if hover && stop { theme::DANGER } else { theme::TEXT };
    widgets::paint_icon(ui, rect, icon, 16.0, col);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tip)
}

/// The tile's volume slider. Pushes `SetVolume` on **every** change, mid-drag included (#135, user:
/// "make sure that when the volume slider is moved it instantly sets the volume even while moving and
/// not only when the user actually lifts his mouse button"). `Why:` the user wants to hear the level
/// while choosing it. `SetVolume` only sets the decoder's `volume` element and the in-memory
/// session value (nothing is written to disk), so one per changed frame is cheap. `drag` keeps
/// the shown value one frame ahead of the session until the pushed value has landed.
pub(in crate::ui) fn tile_volume_slider(ui: &mut Ui, drag: &mut VolDrag, key: StreamKey, shown: u32, applied: u32, acts: &mut Vec<UiAction>) -> Response {
    let mut v = shown;
    let r = crate::ui::volume_slider(ui, &mut v);
    if r.dragged() || r.changed() {
        *drag = Some((key, v));
    }
    if r.changed() {
        acts.push(UiAction::SetVolume { stream: key, volume_percent: v, muted: false });
    } else if !r.dragged() && (*drag).is_some_and(|(k, d)| k == key && d == applied) {
        // The pushed value has landed: stop overriding it.
        *drag = None;
    }
    r
}

/// `hbox·audio-pill` (29 px, pad 2/2, gap 4): `[25 mute][110 volume 0–500 %][34 pct][25 level]`,
/// or `[25 mute][Muted]` (pad 2/10) while muted. Both end buttons sit concentric in the pill's
/// half-circle ends, 2 px in (#130). `level` is the global loudness-levelling toggle (#126). The slider pushes `SetVolume` on every change, mid-drag too (#135).
fn audio_pill(ui: &mut Ui, cx: &Cx, drag: &mut VolDrag, key: StreamKey, bottom: Rect, acts: &mut Vec<UiAction>) {
    let Some(s) = cx.session(key.0) else { return };
    let vol = s.volume(key.1, key.2);
    let shown = (*drag).filter(|(k, _)| *k == key).map_or(vol.volume_percent, |(_, v)| v);
    let muted_l = tr("ui.room.muted", "Muted");
    let pct = format!("{shown}%");
    let tf = font(Weight::Mono, theme::FS_TINY);
    let tail = if vol.muted { text_width(ui, &muted_l, tf.clone(), 0.0).ceil() } else { 110.0 + theme::S1 + 34.0 + theme::S1 + 25.0 };
    // Right pad 2 like the left one, so the level button fills the right end cap (#130); 10 after the "Muted" text.
    let pad_r = if vol.muted { 10.0 } else { 2.0 };
    let w = 2.0 + 25.0 + theme::S1 + tail + pad_r;
    let pill = Rect::from_min_size(bottom.min, vec2(w.min(bottom.width()), OVL_H));
    ui.painter().rect(pill, CornerRadius::same(15), Color32::from_black_alpha(184), Stroke::new(1.0_f32, Color32::from_white_alpha(46)), StrokeKind::Inside);

    let mut child = ui.new_child(UiBuilder::new().max_rect(pill).layout(Layout::left_to_right(Align::Center)));
    child.shrink_clip_rect(pill);
    let slots_muted = [fixed(25.0), fixed(tail)];
    let slots = [fixed(25.0), fixed(110.0), fixed(34.0), fixed(25.0)];
    let row = HBox::new("hbox·audio-pill", OVL_H).gap(theme::S1).pad(2.0, pad_r).slots(if vol.muted { &slots_muted } else { &slots }).show(&mut child);

    // Mute toggle: 25 px round, red icon while muted.
    let mr = Rect::from_center_size(row.cell(0).center(), vec2(25.0, 25.0));
    let mresp = child.interact(mr, Id::new(("tile_mute", key)), Sense::click());
    if mresp.hovered() {
        child.painter().circle_filled(mr.center(), 12.5, Color32::from_white_alpha(20));
    }
    let (icon, col, tip) = if vol.muted {
        ("volume_off", theme::DANGER, tr("ui.room.unmute", "Listen"))
    } else {
        ("volume_up", theme::TEXT, tr("ui.room.mute", "Mute this stream"))
    };
    widgets::paint_icon(&child, mr, icon, 16.0, col);
    if mresp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(&tip).clicked() {
        acts.push(UiAction::SetVolume { stream: key, volume_percent: vol.volume_percent, muted: !vol.muted });
    }
    let num = Color32::from_rgba_unmultiplied(232, 233, 236, 217);
    if vol.muted {
        row.text(&child, 1, &muted_l, tf, num, Align::Min);
        return;
    }
    // The slider is 32 px tall; centre it on the 29 px pill.
    let sc = row.cell(1);
    let sr = Rect::from_center_size(sc.center(), vec2(sc.width(), theme::CTL_H));
    let mut sl = child.new_child(UiBuilder::new().id_salt(("tile_vol", key)).max_rect(sr).layout(Layout::left_to_right(Align::Center)));
    tile_volume_slider(&mut sl, drag, key, shown, vol.volume_percent, acts).on_hover_text(tr("ui.room.volume", "Volume"));
    row.text(&child, 2, &pct, tf, num, Align::Max);
    let lr = Rect::from_center_size(row.cell(3).center(), vec2(25.0, 25.0));
    if crate::ui::level_toggle(&child, lr, Id::new(("tile_level", key)), cx.state.level_loudness) {
        acts.push(UiAction::SetLevelLoudness(!cx.state.level_loudness));
    }
}

// ---------------------------------------------------------------------------
// Share cards
// ---------------------------------------------------------------------------


/// A centred `[icon 24][label 11]` on a thumb.
fn thumb_placeholder(ui: &Ui, thumb: Rect, icon: &str, icon_col: Color32, label: &str) {
    let cy = thumb.center().y;
    let line = |y: f32, h: f32| Rect::from_x_y_ranges(thumb.x_range().shrink(theme::S2), y..=y + h);
    widgets::paint_icon(ui, line(cy - 22.0, 24.0), icon, 24.0, icon_col);
    paint_text(ui, line(cy + 6.0, 16.0), label, font(Weight::Regular, theme::FS_TINY), theme::TEXT_FAINT, Align::Center);
}

/// Display names of the people watching our stream `stream_id` in room `h` (the relay's list,
/// spec 9.1, resolved through the roster, sorted). `None` when the relay does not report viewers
/// (an older relay): the card then shows no count at all, since "0" would be a lie.
fn viewer_names(cx: &Cx, h: RoomHandle, stream_id: u8) -> Option<Vec<String>> {
    let s = cx.session(h)?;
    if !s.viewers_supported {
        return None;
    }
    let ids = s.viewers.get(&stream_id).map(|(v, _)| v.as_slice()).unwrap_or(&[]);
    let mut names: Vec<String> = ids.iter().filter_map(|c| s.members.iter().find(|m| m.conn_id == *c)).map(|m| m.display_name.clone()).collect();
    names.sort_by_key(|n| n.to_lowercase());
    Some(names)
}

/// Most viewer rows the dropdown lists; the rest collapse into one "+N" row.
const VIEWER_ROWS_MAX: usize = 8;
/// Width of the viewers dropdown.
const VIEWERS_POP_W: f32 = 200.0;

/// The hover dropdown under the viewer chip (user request, task #116: "hovers something like a
/// viewer number ... a drop-down thingy ... the profile picture and the username"): one row per
/// viewer, avatar then name, the same avatar the People list uses. Open while the pointer is on the
/// chip or, once open, anywhere on the chip, the dropdown or the gap between them, so the pointer
/// can travel into it. Nobody watching: no dropdown, a plain tooltip says so.
fn viewers_dropdown(ui: &Ui, chip: &Response, names: &[String]) {
    if names.is_empty() {
        chip.clone().on_hover_text(tr("ui.room.viewers_none", "Nobody is watching this yet."));
        return;
    }
    let ctx = ui.ctx();
    let id = chip.id.with("viewers_dropdown");
    // The dropdown's rect from the previous frame, kept only while it is open.
    let shown: Option<Rect> = ctx.data(|d| d.get_temp(id));
    let hovering = |r: Rect| ctx.pointer_hover_pos().is_some_and(|p| r.expand(3.0).contains(p));
    if !(chip.hovered() || shown.is_some_and(|r| hovering(r.union(chip.rect)))) {
        ctx.data_mut(|d| d.remove::<Rect>(id));
        return;
    }
    let rows = names.len().min(VIEWER_ROWS_MAX);
    let extra = names.len() - rows;
    let height = 24.0 + (rows + usize::from(extra > 0)) as f32 * 28.0 + 12.0;
    let screen = ctx.content_rect();
    // Hangs off the chip's right edge, 4 px below (above if the window ends first).
    let x = (chip.rect.right() - VIEWERS_POP_W).max(screen.left() + 8.0);
    let below = chip.rect.bottom() + 4.0;
    let y = if below + height > screen.bottom() - 8.0 { chip.rect.top() - 4.0 - height } else { below };
    let frame = egui::Frame::new()
        .fill(theme::PANEL_RAISED)
        .stroke(Stroke::new(1.0_f32, theme::BORDER))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(egui::Margin::same(6))
        .shadow(egui::Shadow { offset: [0, 14], blur: 40, spread: 0, color: Color32::from_black_alpha(153) });
    let area = egui::Area::new(id).order(egui::Order::Foreground).fixed_pos(pos2(x, y)).show(ctx, |ui| {
        frame.show(ui, |ui| {
            ui.set_width(VIEWERS_POP_W - 12.0);
            ui.spacing_mut().item_spacing.y = 0.0;
            let (hr, _) = ui.allocate_exact_size(vec2(ui.available_width(), 24.0), Sense::hover());
            let title = format!("{} · {}", tr("ui.room.viewers_title", "Watching now").to_uppercase(), names.len());
            paint_text(ui, hr.shrink2(vec2(theme::S2, 0.0)), &title, font(Weight::Bold, theme::FS_SECTION), theme::ACCENT, Align::Min);
            for name in &names[..rows] {
                let row = HBox::new("viewer_row", 28.0).pad(theme::S2, theme::S2).slots(&[fixed(theme::AVATAR_SM), FILL]).show(ui);
                // The slot is as tall as the row; the avatar is a 22 px disc in its middle.
                let disc = Rect::from_center_size(row.cell(0).center(), vec2(theme::AVATAR_SM, theme::AVATAR_SM));
                widgets::paint_avatar(ui, disc, &initial(name), theme::avatar_colors(name), false);
                row.text(ui, 1, name, font(Weight::Regular, theme::FS_LABEL), theme::TEXT, Align::Min);
            }
            if extra > 0 {
                let row = HBox::new("viewer_more", 28.0).pad(theme::S2, theme::S2).slots(&[FILL]).show(ui);
                row.text(ui, 0, &format!("+{extra}"), font(Weight::Regular, theme::FS_LABEL), theme::TEXT_DIM, Align::Min);
            }
        });
    });
    ctx.data_mut(|d| d.insert_temp(id, area.response.rect));
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statbar_drops_resolution_then_bitrate_when_narrow() {
        // Roughly the real widths: res "1920x1080" 80, fps 6ch 42, rate 8ch 56.
        let full = statbar_fixed_w(80.0, 42.0, 56.0);
        assert_eq!(statbar_fields(full + 40.0, 80.0, 42.0, 56.0), (true, true));
        assert_eq!(statbar_fields(full - 1.0, 80.0, 42.0, 56.0), (false, true));
        // A narrow tile (~178 px statbar cell) keeps fps + bitrate.
        assert_eq!(statbar_fields(178.0, 80.0, 42.0, 56.0), (false, true));
        assert_eq!(statbar_fields(120.0, 80.0, 42.0, 56.0), (false, false));
    }

    #[test]
    fn kbps_formatting() {
        assert_eq!(fmt_kbps(5500.0), "5.5 Mbps");
        assert_eq!(fmt_kbps(640.4), "640 kbps");
    }
}
