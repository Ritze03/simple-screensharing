//! What one stage tile shows: a watched stream's video with its hover overlay, a stream you can still
//! watch (dimmed preview + Watch), an audio-only sender (Listen), or your own share (live preview).
//!
//! **Hover overlay (task I3, the replica's):** a watched tile shows nothing over the picture until the pointer
//! is on it. Then the name tag (`[icon][name][· label]`, the icon says whether its audio is audible) at the top
//! left, with the numbers of the Layout menu's "Show stats" (one switch for every stream) appended in reserved, tabular slots, and three round buttons at the
//! top right: Pop out, Fullscreen and `⋯` (the stream menu, [`tile_menu`]). "Always show names" keeps the tag
//! without hovering; the numbers and the buttons never stay. A right click on any tile opens the same menu.
//! Volume and Stop watching live in that menu (level loudness is the control-bar button), not in a pill over the picture.
//! *Why:* the user's call from the replica (the stage should be a clean picture until asked), and "a stream's
//! buttons and stats show only while it is hovered" (D1).
//! Decoder stats (and the decoder lock for the bitrate) are only read while the numbers are visible.

use egui::epaint::{Mesh, RectShape, Vertex, WHITE_UV};
use egui::{Align, Color32, CornerRadius, Id, Layout, Rect, Response, Sense, Shape, Stroke, StrokeKind, Ui, UiBuilder, pos2, vec2};
use simple_screensharing_client::capture::destination::DestinationState;
use simple_screensharing_client::decode::ViewState;
use simple_screensharing_client::decode::render::Fit;
use simple_screensharing_client::protocol::{ConnId, StreamInfo, StreamKind};
use simple_screensharing_client::session::{CaptureSourceId, DestinationId, RoomHandle};

use super::{ItemId, VolDrag};
use crate::theme::{self, Weight, font};
use crate::ui::{Cx, Modal, Popover, PopoverKind, StreamKey, UiAction, drive_dump, share_label, tr, tr_with};
use crate::widgets::{self, Btn, FILL, HBox, fixed, paint_text, text_width};

/// Where and how a tile is drawn this frame.
pub(super) struct Spot {
    /// The tile's index on the stage (`drive_dump` names).
    pub n: usize,
    pub rect: Rect,
    /// Fill the rect (Crop) instead of fitting the picture whole.
    pub crop: bool,
    /// Crop pan: how far the picture is shifted from the centre, in points ([`super::geom::pan_shift`]).
    pub pan: egui::Vec2,
    /// Corner radius; 0 in Floating, where windows overlap (the corner masks would paint over the one underneath).
    pub round: u8,
    /// The pointer is over this tile (and no tile above it).
    pub hovered: bool,
    pub always_names: bool,
    /// "Show stats" is on for this stream.
    pub stats: bool,
    /// Label tiles with the room too ("Watched").
    pub cross_room: bool,
    pub id: ItemId,
}

/// What a tile draws between its picture and its overlay: the webcam in picture ([`super::pip`]). `Why` a hook: a
/// video is a wgpu paint callback, so what covers what is the order the shapes and callbacks are added in; the name
/// tag and stats must come after the PIP or it covers the name (user: "The PIP webcam is drawn above the name in a
/// screenshare (Should be below it, so its visible)"). A tile calls it once, after its picture and before its tag and
/// buttons; the caller runs it afterwards if the tile returned early ([`run_between`]).
pub(super) type Between<'a> = Option<Box<dyn FnOnce(&mut Ui, &mut Vec<UiAction>) + 'a>>;

/// Run the hook if it has not run yet.
pub(super) fn run_between(b: &mut Between, ui: &mut Ui, acts: &mut Vec<UiAction>) {
    if let Some(f) = b.take() {
        f(ui, acts);
    }
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

/// The three numbers of "Show stats" as text: resolution, frame rate, bitrate.
pub(super) struct Nums {
    pub res: String,
    pub fps: String,
    pub rate: String,
}

/// What the stats slots reserve room for: the biggest resolution, a 3-digit frame rate, 10+ Mbps. The pill is
/// as wide as these, not as the current values, so it never jitters as a number gains a digit (D1).
const RESERVE: (&str, &str, &str) = ("9999×9999", "999 fps", "99.9 Mbps");

/// Which numbers of the stats fit `avail` points: `(resolution, bitrate)`; the frame rate stays while anything
/// does, `None` when not even it fits. Order of loss: resolution (changes least), then bitrate (fps is the
/// health signal). `widths` are the reserved slot widths; every slot sits after a gap of [`theme::S2`].
fn stats_fields(avail: f32, widths: (f32, f32, f32)) -> Option<(bool, bool)> {
    let (res, fps, rate) = widths;
    let g = theme::S2;
    if (g + res) + (g + fps) + (g + rate) <= avail {
        Some((true, true))
    } else if (g + fps) + (g + rate) <= avail {
        Some((false, true))
    } else if g + fps <= avail {
        Some((false, false))
    } else {
        None
    }
}

/// `.tag`: a dark 26 px pill at the tile's top left, `[icon][name][· label]` and, with `nums`, the stats slots.
/// At most `max_w` wide (name and label truncate, then the numbers drop, see [`stats_fields`]). Returns the
/// pill's rect and whether numbers were left out for lack of room.
pub(super) fn name_tag(ui: &Ui, rect: Rect, max_w: f32, icon: &str, icon_col: Color32, name: &str, label: &str, nums: Option<&Nums>) -> Rect {
    let (nf, lf, mf) = (font(Weight::Bold, theme::FS_ROW), font(Weight::Regular, theme::FS_ROW), font(Weight::Mono, theme::FS_SMALL));
    let label = if label.is_empty() { String::new() } else { format!("· {label}") };
    let (nw, lw) = (text_width(ui, name, nf.clone(), 0.0).ceil(), text_width(ui, &label, lf.clone(), 0.0).ceil());
    let lead = 7.0 + 15.0 + 6.0;
    let base = lead + nw + if lw > 0.0 { 6.0 + lw } else { 0.0 } + 9.0;
    let widths = (text_width(ui, RESERVE.0, mf.clone(), 0.0).ceil(), text_width(ui, RESERVE.1, mf.clone(), 0.0).ceil(), text_width(ui, RESERVE.2, mf.clone(), 0.0).ceil());
    let fields = nums.and_then(|_| stats_fields(max_w - base, widths));
    let num_w = fields.map_or(0.0, |(res, rate)| {
        theme::S2 * (1.0 + f32::from(res) + f32::from(rate)) + widths.1 + if res { widths.0 } else { 0.0 } + if rate { widths.2 } else { 0.0 }
    });
    let w = (base + num_w).min(max_w);
    let bar = Rect::from_min_size(rect.min + vec2(theme::S2, theme::S2), vec2(w.max(0.0), 26.0));
    if w < 40.0 {
        return bar;
    }
    ui.painter().rect_filled(bar, CornerRadius::same(7), theme::STAGE_TAG);
    widgets::paint_icon(ui, Rect::from_min_size(bar.min + vec2(7.0, 0.0), vec2(15.0, 26.0)), icon, 15.0, icon_col);
    let right = bar.right() - 9.0 - num_w;
    let tx = bar.left() + lead;
    let nr = paint_text(ui, Rect::from_x_y_ranges(tx..=right.max(tx), bar.y_range()), name, nf, theme::TEXT, Align::Min);
    if lw > 0.0 && nr.right() + 6.0 < right {
        paint_text(ui, Rect::from_x_y_ranges(nr.right() + 6.0..=right, bar.y_range()), &label, lf, theme::TEXT_DIM, Align::Min);
    }
    if let (Some(n), Some((res, rate))) = (nums, fields) {
        let num = theme::OVL_NUM;
        let mut x = right.max(tx);
        let mut slot = |text: &str, w: f32| {
            x += theme::S2;
            paint_text(ui, Rect::from_x_y_ranges(x..=x + w, bar.y_range()), text, mf.clone(), num, Align::Max);
            x += w;
        };
        if res {
            slot(&n.res, widths.0);
        }
        slot(&n.fps, widths.1);
        if rate {
            slot(&n.rate, widths.2);
        }
    }
    bar
}

/// A watched stream: its video (Contain, or Cover in Crop), the green ring while its shared audio is audible,
/// the health chip, the name tag and the hover buttons.
pub(super) fn video_tile(ui: &mut Ui, cx: &Cx, key: StreamKey, s: &Spot, between: &mut Between, acts: &mut Vec<UiAction>) {
    cx.videos.paint_panned(key, ui, s.rect, if s.crop { Fit::Cover } else { Fit::Contain }, s.pan);
    corner_masks(ui, s.rect, s.round);
    let has_audio = cx.stream_has_audio(key);
    let audible = has_audio && cx.session(key.0).is_some_and(|x| !x.volume(key.1, key.2).muted);
    if audible {
        ui.painter().rect_stroke(s.rect, CornerRadius::same(s.round), Stroke::new(2.0_f32, theme::SUCCESS), StrokeKind::Inside);
    }
    health_chip(ui, s.rect, stream_health(cx, key));
    run_between(between, ui, acts);
    drive_dump::line(|| format!("stats.{} {}", s.n, u8::from(s.stats)));
    if s.always_names || s.hovered {
        let owner = cx.member_name(key.0, key.1);
        let name = if s.cross_room { format!("{owner} · {}", cx.room_name(key.0)) } else { owner };
        let label = cx.session(key.0).and_then(|x| x.streams.iter().find(|i| i.owner == key.1 && i.stream_id == key.2)).map(|i| share_label(&i.label)).unwrap_or_default();
        // A webcam has no audio of its own: its icon says what it is instead.
        let (icon, col) = match (cx.is_webcam_stream(key), has_audio, audible) {
            (true, ..) => ("videocam", theme::TEXT_DIM),
            (_, false, _) => ("volume_off", theme::TEXT_DIM),
            (_, true, false) => ("volume_off", theme::DANGER),
            (_, true, true) => ("volume_up", theme::TEXT_DIM),
        };
        let nums = (s.stats && s.hovered).then(|| stream_nums(cx, key));
        let max_w = s.rect.width() - 2.0 * theme::S2 - if s.hovered { TOOLS_RESERVE } else { 0.0 };
        let tag = name_tag(ui, s.rect, max_w, icon, col, &name, &label, nums.as_ref().map(|n| &n.0));
        drive_dump::rect(|| format!("tag.{}", s.n), tag);
        if let Some((_, tip)) = nums {
            ui.interact(tag, Id::new(("tag_tip", key)), Sense::hover()).on_hover_text(tip);
        }
    }
    tools(ui, s, Some(key), acts);
}

/// The numbers of a watched stream and the tooltip that adds what the pill has no room for.
fn stream_nums(cx: &Cx, key: StreamKey) -> (Nums, String) {
    let Some(entry) = cx.videos.get(key) else { return (Nums { res: String::new(), fps: String::new(), rate: String::new() }, String::new()) };
    let stats = entry.view.lock().unwrap().source().stats();
    let kbps = cx.session(key.0).and_then(|s| s.watches.lock().unwrap().get(&(key.1, key.2)).map(|w| w.decoder.lock().unwrap().received_bitrate_kbps())).unwrap_or(0.0);
    let res = cx.videos.frame_size(key).map(|(w, h)| format!("{w}×{h}")).unwrap_or_default();
    let nums = Nums { res: res.clone(), fps: format!("{:.0} fps", stats.decoded_fps), rate: fmt_kbps(kbps) };
    let codec = stats.info.codec.map(|c| c.name()).unwrap_or("—");
    let dec = stats.info.decoder_name.as_deref().unwrap_or("—");
    let tip = tr_with(
        "ui.room.stats_tip",
        "Decoded %{decoded} fps · shown %{presented} fps\n%{codec} via %{decoder} · %{dropped} dropped",
        &[
            ("decoded", &format!("{:.0}", stats.decoded_fps)),
            ("presented", &format!("{:.0}", stats.presented_fps)),
            ("codec", codec),
            ("decoder", dec),
            ("dropped", &stats.access_units_dropped.to_string()),
        ],
    );
    (nums, format!("{res} · {}\n{tip}", fmt_kbps(kbps)))
}

/// Room the three round buttons and their gaps take at the top right (3 x 29 + 2 x 4 + the tile's own gap).
const TOOLS_RESERVE: f32 = 3.0 * OVL_H + 2.0 * theme::S1 + theme::S2;

/// The hover buttons at the tile's top right, fading in over 0.12 s: `[Pop out][Fullscreen][⋯]` for a watched
/// stream (`key`), only `[⋯]` for the others. `⋯` opens the stream menu at the button.
fn tools(ui: &mut Ui, s: &Spot, key: Option<StreamKey>, acts: &mut Vec<UiAction>) {
    let id = Id::new(("tile_tools", &s.id));
    let alpha = ui.ctx().animate_bool_with_time(id, s.hovered, 0.12);
    if alpha <= 0.0 {
        return;
    }
    let mut ov = ui.new_child(UiBuilder::new().id_salt(id).max_rect(s.rect).layout(Layout::left_to_right(Align::Center)));
    ov.set_clip_rect(s.rect.intersect(ui.clip_rect()));
    ov.multiply_opacity(alpha);
    let inner = s.rect.shrink(theme::S2);
    let btn = |i: f32| Rect::from_min_size(pos2(inner.right() - OVL_H - i * (OVL_H + theme::S1), inner.top()), vec2(OVL_H, OVL_H));
    let more = btn(0.0);
    drive_dump::rect(|| format!("tool.{}.more", s.n), more);
    if key.is_some() {
        drive_dump::rect(|| format!("tool.{}.fs", s.n), btn(1.0));
        drive_dump::rect(|| format!("tool.{}.pop", s.n), btn(2.0));
    }
    if round_btn(&mut ov, more, id.with("more"), "more_horiz", &tr("ui.stage.stream_menu", "Volume and more"), false).clicked() {
        acts.push(UiAction::OpenPopover(Popover { kind: PopoverKind::Tile(s.id.clone()), anchor: more }));
    }
    if let Some(key) = key {
        if round_btn(&mut ov, btn(1.0), id.with("fs"), "fullscreen", &tr("ui.stage.fullscreen", "Fullscreen"), false).clicked() {
            acts.push(UiAction::EnterStreamFullscreen(key));
        }
        if round_btn(&mut ov, btn(2.0), id.with("pop"), "open_in_new", &tr("ui.room.pop_out_tip", "Pop out to its own window"), false).clicked() {
            acts.push(UiAction::PopOut(key));
        }
    }
}

/// A stream in the room you do not watch yet (or an audio-only sender): a dark tile with the last preview
/// dimmed behind `[avatar][X is sharing Y][1920×1080][Watch stream][Pop out]` (audio only: `[Listen]`).
pub(super) fn idle_tile(ui: &mut Ui, cx: &Cx, key: StreamKey, info: &StreamInfo, s: &Spot, acts: &mut Vec<UiAction>) {
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
        crate::ui::paint_person(ui, disc, &name, &cx.emoji_of(key.0, key.1), false);
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
    drive_dump::rect(|| format!("watch.{}", s.n), br);
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
    tools(ui, s, None, acts);
}

/// Your own share in this room: a live preview of what you capture while the window is focused (task #78), a
/// screen-hatch placeholder otherwise, the tag `You · label`, and on hover `[viewers][PREVIEW]`, a Stream
/// settings button and `⋯`.
pub(super) fn mine_tile(ui: &mut Ui, cx: &Cx, h: RoomHandle, capture: CaptureSourceId, destination: DestinationId, s: &Spot, between: &mut Between, acts: &mut Vec<UiAction>) {
    let Some(c) = cx.capture(capture) else { return };
    let Some(d) = c.destinations.get(&destination) else { return };
    let rect = s.rect;
    let radius = CornerRadius::same(s.round);
    let webcam = c.is_webcam();
    ask_live_preview(ui, c, capture, cx.shell.stage.preview_unfocused, acts);
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
        paint_cover_panned(ui.painter(), rect, tex, radius, Color32::WHITE, s.pan);
    } else {
        widgets::paint_hatch(ui.painter(), rect, radius, 8.0, theme::ELEV_19, theme::ELEV_14);
        let (icon, what) = if webcam { ("videocam", tr("ui.room.my_webcam", "Your webcam")) } else { ("cast", tr("ui.room.my_screen", "Your screen")) };
        thumb_placeholder(ui, rect, icon, theme::SUCCESS, &format!("{what} · {}", chip.to_lowercase()));
    }
    // A camera has frames at once; the hint is about a screen that sends nothing while it does not change.
    if !webcam && state == DestinationState::Starting && cx.state.capture_awaiting_first_frame(capture) {
        ui.interact(rect, Id::new(("stage_mine_hint", capture, destination)), Sense::hover())
            .on_hover_text(tr("ui.waiting_frame_hint", "An idle screen sends no picture until something on it changes. Move the mouse over the shared screen and the stream starts."));
    }
    run_between(between, ui, acts);
    let label = if on_air { share_label(&c.label) } else { format!("{} · {chip}", share_label(&c.label)) };
    // Like every tag: while hovered or with "Always show names", and always while the share is not on air (a
    // starting, stalled or failed share must say so, whatever the placeholder under it shows).
    if s.hovered || s.always_names || !on_air {
        let max_w = rect.width() - 2.0 * theme::S2 - if s.hovered { 2.0 * OVL_H + 3.0 * theme::S1 + 120.0 } else { 0.0 };
        let tag = name_tag(ui, rect, max_w, if webcam { "videocam" } else { "cast" }, if on_air { theme::SUCCESS } else { theme::WARNING }, &tr("ui.room.you", "You"), &label, None);
        drive_dump::rect(|| format!("tag.{}", s.n), tag);
    }
    if !s.hovered {
        return;
    }
    // Top right: [PREVIEW][viewers][Stream settings][⋯], added after the tile's own widgets so they sit above.
    let inner = rect.shrink(theme::S2);
    let more = Rect::from_min_size(pos2(inner.right() - OVL_H, inner.top()), vec2(OVL_H, OVL_H));
    let btn = more.translate(vec2(-(OVL_H + theme::S1), 0.0));
    drive_dump::rect(|| format!("tool.{}.more", s.n), more);
    drive_dump::rect(|| format!("tool.{}.settings", s.n), btn);
    let row = Rect::from_min_max(pos2(inner.left(), inner.top()), pos2(btn.left() - theme::S1, inner.top() + OVL_H));
    let mut r = ui.new_child(UiBuilder::new().max_rect(row).layout(Layout::right_to_left(Align::Center)));
    if let Some(viewers) = viewer_list(cx, h, d.stream_id) {
        let chip = widgets::chip_dark(&mut r, Some("visibility"), false, &viewers.len().to_string());
        viewers_dropdown(ui, cx, h, &chip, &viewers);
    }
    if live_tex.is_some() {
        // Says the picture is a local preview, not what viewers get.
        widgets::chip_dark(&mut r, None, false, &tr("ui.room.preview_chip", "PREVIEW"))
            .on_hover_text(tr("ui.room.preview_tip", "A live preview of your screen, shown while this window is focused. Viewers get the full quality stream."));
    }
    let (tip, modal) = if webcam {
        (tr("ui.room.webcam_settings", "Webcam settings"), Modal::WebcamSettings { room: h })
    } else {
        (tr("ui.room.stream_settings", "Stream settings"), Modal::StreamSettings { capture, room: h })
    };
    if round_btn(ui, btn, Id::new(("stage_mine_settings", capture)), "tune", &tip, false).clicked() {
        acts.push(UiAction::OpenModal(modal));
    }
    if round_btn(ui, more, Id::new(("stage_mine_more", capture)), "more_horiz", &tr("ui.stage.stream_menu", "Volume and more"), false).clicked() {
        acts.push(UiAction::OpenPopover(Popover { kind: PopoverKind::Tile(s.id.clone()), anchor: more }));
    }
}

/// Asking for the live preview is what keeps it running: only a focused window paints this
/// (`App::poll_live_previews`). The own tile of a share does it every pass it is drawn, and so does a webcam's
/// picture-in-picture, which stands in for its tile.
pub(super) fn ask_live_preview(ui: &Ui, c: &simple_screensharing_client::session::LocalCapture, capture: CaptureSourceId, keep_unfocused: bool, acts: &mut Vec<UiAction>) {
    if c.kind == StreamKind::Video && ui.ctx().input(|i| crate::ui::live_preview_allowed(i.viewport().focused.unwrap_or(true), i.viewport().minimized.unwrap_or(false), c.is_webcam(), keep_unfocused)) {
        acts.push(UiAction::LivePreviewSeen(capture));
    }
}

pub(super) fn fmt_kbps(kbps: f64) -> String {
    if kbps >= 1000.0 { format!("{:.1} Mbps", kbps / 1000.0) } else { format!("{kbps:.0} kbps") }
}

/// Statbar / round button height.
const OVL_H: f32 = 29.0;

/// Cover-fit a preview texture into `rect` (crop, never letterbox), tinted by `tint`.
pub(super) fn paint_cover(p: &egui::Painter, rect: Rect, tex: &egui::TextureHandle, radius: CornerRadius, tint: Color32) {
    paint_cover_panned(p, rect, tex, radius, tint, egui::Vec2::ZERO);
}

/// [`paint_cover`] with the picture shifted by `pan` points from the centre (Crop's pan, already clamped by `geom::pan_shift`).
pub(super) fn paint_cover_panned(p: &egui::Painter, rect: Rect, tex: &egui::TextureHandle, radius: CornerRadius, tint: Color32, pan: egui::Vec2) {
    let [tw, th] = tex.size();
    let (ta, ra) = (tw as f32 / th.max(1) as f32, rect.width() / rect.height());
    let uv = if ta > ra {
        let k = ra / ta;
        Rect::from_min_max(pos2(0.5 - k / 2.0, 0.0), pos2(0.5 + k / 2.0, 1.0))
    } else {
        let k = ta / ra;
        Rect::from_min_max(pos2(0.0, 0.5 - k / 2.0), pos2(1.0, 0.5 + k / 2.0))
    };
    let uv = uv.translate(egui::vec2(-pan.x * uv.width() / rect.width(), -pan.y * uv.height() / rect.height()));
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

/// The volume slider of the stream menu. Pushes `SetVolume` on **every** change, mid-drag included (#135,
/// user: "make sure that when the volume slider is moved it instantly sets the volume even while moving and
/// not only when the user actually lifts his mouse button"). `Why:` the user wants to hear the level
/// while choosing it. `SetVolume` only sets the decoder's `volume` element and the in-memory
/// session value (nothing is written to disk), so one per changed frame is cheap. `drag` keeps
/// the shown value one frame ahead of the session until the pushed value has landed. The mouse wheel over it
/// steps it too (5 % up to 200 %, 10 % above, `ui::volume_slider`).
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

/// Where a stream menu's volume row writes: the stream's audio (a video stream's sibling audio, or the
/// audio-only stream itself, `RoomSession::volume` resolves both).
pub(super) fn menu_volume_row(ui: &mut Ui, cx: &Cx, drag: &mut VolDrag, key: StreamKey, acts: &mut Vec<UiAction>) {
    let Some(s) = cx.session(key.0) else { return };
    let vol = s.volume(key.1, key.2);
    let shown = (*drag).filter(|(k, _)| *k == key).map_or(vol.volume_percent, |(_, v)| v);
    let row = HBox::new("menu_volume", theme::CTL_H).gap(theme::S1).pad(theme::S2, theme::S2).slots(&[fixed(28.0), FILL, fixed(40.0)]).show(ui);
    let mr = Rect::from_center_size(row.cell(0).center(), vec2(25.0, 25.0));
    let mresp = ui.interact(mr, Id::new(("menu_mute", key)), Sense::click());
    if mresp.hovered() {
        ui.painter().circle_filled(mr.center(), 12.5, Color32::from_white_alpha(20));
    }
    let (icon, col, tip) = if vol.muted {
        ("volume_off", theme::DANGER, tr("ui.room.unmute", "Listen"))
    } else {
        ("volume_up", theme::TEXT, tr("ui.room.mute", "Mute this stream"))
    };
    widgets::paint_icon(ui, mr, icon, 16.0, col);
    if mresp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tip).clicked() {
        acts.push(UiAction::SetVolume { stream: key, volume_percent: vol.volume_percent, muted: !vol.muted });
    }
    let applied = vol.volume_percent;
    drive_dump::rect(|| "menu.volume".into(), row.cell(1));
    drive_dump::rect(|| "menu.mute".into(), mr);
    drive_dump::line(|| format!("volume {shown}"));
    row.show(ui, 1, Align::Min, |ui| {
        ui.add_enabled_ui(!vol.muted, |ui| tile_volume_slider(ui, drag, key, shown, applied, acts).on_hover_text(tr("ui.room.volume", "Volume")));
    });
    let text = if vol.muted { tr("ui.room.muted", "Muted") } else { format!("{shown}%") };
    row.text(ui, 2, &text, font(Weight::Mono, theme::FS_TINY), theme::TEXT_DIM, Align::Max);
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

/// The connections and display names of the people watching our stream `stream_id` in room `h` (the
/// relay's list, spec 9.1, resolved through the roster, sorted by name). `None` when the relay does not
/// report viewers (an older relay): the card then shows no count at all, since "0" would be a lie.
pub(super) fn viewer_list(cx: &Cx, h: RoomHandle, stream_id: u8) -> Option<Vec<(ConnId, String)>> {
    let s = cx.session(h)?;
    if !s.viewers_supported {
        return None;
    }
    let ids = s.viewers.get(&stream_id).map(|(v, _)| v.as_slice()).unwrap_or(&[]);
    let mut viewers: Vec<(ConnId, String)> = ids.iter().filter_map(|c| s.members.iter().find(|m| m.conn_id == *c)).map(|m| (m.conn_id, m.display_name.clone())).collect();
    viewers.sort_by_key(|(conn, name)| (name.to_lowercase(), *conn));
    Some(viewers)
}

/// Most viewer rows the dropdown lists; the rest collapse into one "+N" row.
const VIEWER_ROWS_MAX: usize = 8;
/// Width of the viewers dropdown.
const VIEWERS_POP_W: f32 = 200.0;

/// The hover dropdown under the viewer chip (user request, task #116: "hovers something like a
/// viewer number ... a drop-down thingy ... the profile picture and the username"): one row per
/// viewer, avatar then name, the same avatar the people strip uses. Open while the pointer is on the
/// chip or, once open, anywhere on the chip, the dropdown or the gap between them, so the pointer
/// can travel into it. Nobody watching: no dropdown, a plain tooltip says so.
fn viewers_dropdown(ui: &Ui, cx: &Cx, room: RoomHandle, chip: &Response, viewers: &[(ConnId, String)]) {
    if viewers.is_empty() {
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
    let rows = viewers.len().min(VIEWER_ROWS_MAX);
    let extra = viewers.len() - rows;
    let height = 24.0 + (rows + usize::from(extra > 0)) as f32 * 28.0 + 12.0;
    let screen = ctx.content_rect();
    // Hangs off the chip's right edge, 4 px below (above if the window ends first).
    let x = (chip.rect.right() - VIEWERS_POP_W).max(screen.left() + 8.0);
    let below = chip.rect.bottom() + 4.0;
    let y = if below + height > screen.bottom() - 8.0 { chip.rect.top() - 4.0 - height } else { below };
    let area = egui::Area::new(id).order(egui::Order::Foreground).fixed_pos(pos2(x, y)).show(ctx, |ui| {
        crate::ui::menu::frame().show(ui, |ui| {
            ui.set_width(VIEWERS_POP_W - 12.0);
            ui.spacing_mut().item_spacing.y = 0.0;
            let (hr, _) = ui.allocate_exact_size(vec2(ui.available_width(), 24.0), Sense::hover());
            let title = format!("{} · {}", tr("ui.room.viewers_title", "Watching now").to_uppercase(), viewers.len());
            paint_text(ui, hr.shrink2(vec2(theme::S2, 0.0)), &title, font(Weight::Bold, theme::FS_SECTION), theme::ACCENT, Align::Min);
            for (conn, name) in &viewers[..rows] {
                let row = HBox::new("viewer_row", 28.0).pad(theme::S2, theme::S2).slots(&[fixed(theme::AVATAR_SM), FILL]).show(ui);
                // The slot is as tall as the row; the avatar is a 22 px disc in its middle.
                let disc = Rect::from_center_size(row.cell(0).center(), vec2(theme::AVATAR_SM, theme::AVATAR_SM));
                crate::ui::paint_person(ui, disc, name, &cx.emoji_of(room, *conn), false);
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
    fn stats_drop_resolution_then_bitrate_when_narrow() {
        // Roughly the real mono widths: "9999×9999" 78, "999 fps" 60, "99.9 Mbps" 78.
        let w = (78.0, 60.0, 78.0);
        let g = theme::S2;
        let all = (g + 78.0) + (g + 60.0) + (g + 78.0);
        assert_eq!(stats_fields(all, w), Some((true, true)));
        assert_eq!(stats_fields(all - 1.0, w), Some((false, true)));
        assert_eq!(stats_fields(g + 60.0 + g + 78.0, w), Some((false, true)));
        assert_eq!(stats_fields(g + 60.0 + g + 78.0 - 1.0, w), Some((false, false)));
        assert_eq!(stats_fields(g + 60.0, w), Some((false, false)));
        assert_eq!(stats_fields(g + 59.0, w), None, "not even the frame rate fits");
    }

    /// D1: the stats pill is as wide as its reserved slots, whatever the numbers say, so it cannot jitter.
    #[test]
    fn the_stats_reserve_covers_the_biggest_numbers_and_never_the_current_ones() {
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let mf = font(Weight::Mono, theme::FS_SMALL);
                let reserve = |s: &str| text_width(ui, s, mf.clone(), 0.0);
                // Digits are tabular: any value of the same length measures the same, longer ones are bounded by the reserve.
                for (text, slot) in [("1920×1080", RESERVE.0), ("640×360", RESERVE.0), ("9999×9999", RESERVE.0), ("5 fps", RESERVE.1), ("144 fps", RESERVE.1), ("999 fps", RESERVE.1), ("640 kbps", RESERVE.2), ("5.5 Mbps", RESERVE.2), ("12.3 Mbps", RESERVE.2), ("99.9 Mbps", RESERVE.2)] {
                    assert!(reserve(text) <= reserve(slot) + 0.01, "{text} must fit the slot reserved for {slot}");
                }
                assert!((reserve("1920×1080") - reserve("9999×9999")).abs() < 0.01, "a tabular font: the resolution slot is the same width for any digits");
                // The pill's width does not depend on the numbers.
                let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(900.0, 300.0));
                let small = Nums { res: "640×360".into(), fps: "5 fps".into(), rate: "640 kbps".into() };
                let big = Nums { res: "9999×9999".into(), fps: "999 fps".into(), rate: "99.9 Mbps".into() };
                let a = name_tag(ui, rect, 800.0, "volume_up", theme::TEXT, "Ritze", "Screen", Some(&small));
                let b = name_tag(ui, rect, 800.0, "volume_up", theme::TEXT, "Ritze", "Screen", Some(&big));
                assert_eq!(a.width(), b.width(), "same pill for small and big numbers");
                let bare = name_tag(ui, rect, 800.0, "volume_up", theme::TEXT, "Ritze", "Screen", None);
                assert!(a.width() > bare.width(), "the numbers add their reserved room");
            });
        });
    }

    #[test]
    fn kbps_formatting() {
        assert_eq!(fmt_kbps(5500.0), "5.5 Mbps");
        assert_eq!(fmt_kbps(640.4), "640 kbps");
    }
}
