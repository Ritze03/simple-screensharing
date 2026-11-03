//! Pop-out viewer windows (owner: I16). One deferred viewport per stream
//! with `popped` set in `cx.videos` (re-declared every frame it should
//! exist — egui's viewport contract). The viewport paints the shared
//! `Arc<Mutex<VideoView>>` with `paint(ui, ui.max_rect(), Fit::Contain)`;
//! repaint is driven by the view (never `request_repaint()` for video). A
//! window closed by the OS (or its × button) sets `close_requested`, which
//! this module turns into [`UiAction::BringBack`] on the next main-window
//! frame.
//!
//! The pop-out viewport's feature set (see `superdoc/features/viewer-window.md`
//! for the behaviour spec and its history), drawn like the mockup's tile
//! overlay:
//! - **Hover-gated chrome**: while the pointer is anywhere in the window
//!   (no idle timeout, except in fullscreen: `OVERLAY_IDLE`, #124) a vignette, a top
//!   row `[fill statbar][29 ×]` and a bottom audio pill. The statbar is
//!   `[dot][name · room][WxH][dec/pres fps][bitrate]`, mono values in fixed
//!   tabular-width fields so they don't jitter.
//! - **Audio pill** `[25 mute][110 slider 0–200 %][34 %]` (or `[mute]Muted`),
//!   only when the sender shares audio; otherwise a "No audio shared" chip.
//!   Pushes [`UiAction::SetVolume`] through the main window.
//! - **Waiting / recovering / stalled**: a centred chip before the first frame, while the
//!   viewer recovers a lost keyframe ("Recovering…") and while the sharer flags its capture
//!   as stalled (always shown, not hover-gated; same wording as the in-app tile).
//! - **F11** or a **double-click** toggles fullscreen, **Escape** only leaves fullscreen (never
//!   closes).
//! - Title "sharer · room"; first size from the stream's aspect ratio.
//!
//! Performance: the viewport closure only locks the view and paints it when
//! not hovered — no allocation, no `request_repaint`. Data the chrome needs
//! (names, volume, bitrate, stall) is refreshed by the main window at most 4×
//! a second into [`Shared`]; the viewport sends volume changes back through
//! `Shared::pending_volume` and wakes the main window once per change.
//! `Why` the round trip: the viewport closure must be `Send + Sync + 'static`
//! and can't hold `Cx`, and screens never mutate `AppState` themselves.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use egui::{
    Align, Color32, CornerRadius, Layout, Rect, Sense, Stroke, StrokeKind, Ui, UiBuilder, Vec2,
    pos2, vec2,
};
use simple_screensharing_client::decode::ViewState;
use simple_screensharing_client::decode::render::{Fit, VideoView};

use super::{Cx, StreamKey, UiAction, tr};
use crate::theme::{self, Weight, font};
use crate::widgets::{self, FILL, HBox, fixed, paint_icon, paint_text, text_width};

/// How often the main window refreshes a pop-out's chrome data, seconds.
const INFO_INTERVAL: f64 = 0.25;
/// Height of the statbar, round buttons and audio pill (mockup 29 px).
const CHROME_H: f32 = 29.0;
/// Fullscreen overlay auto-hide: seconds without pointer movement, click, scroll
/// or key press before the overlay (and the mouse cursor) goes away (#124).
const OVERLAY_IDLE: f64 = 3.0;

/// Idle bookkeeping of one fullscreen overlay, kept in egui's temp memory per viewport
/// (the pop-out viewport closure cannot hold `Cx`; both callers share this).
#[derive(Clone, Copy, Default)]
struct Idle {
    /// `ctx` time of the last pointer move / press / scroll / key; None before the first frame.
    last_active: Option<f64>,
    pos: Option<egui::Pos2>,
    /// The pointer was over a control (statbar, button, audio pill) last frame.
    over: bool,
}

/// The auto-hide rule (pure): shown while the pointer rests on a control or the last
/// activity is younger than [`OVERLAY_IDLE`].
fn overlay_shown(now: f64, last_active: f64, over_control: bool) -> bool {
    over_control || now - last_active < OVERLAY_IDLE
}

/// Update the idle state from this frame's input and say whether the overlay shows.
/// While idle-hidden it also sets the cursor to none; while shown it schedules the
/// repaint at the deadline (no repaint loop otherwise).
fn overlay_visible(ctx: &egui::Context) -> bool {
    let id = egui::Id::new("overlay_idle").with(ctx.viewport_id());
    let mut st: Idle = ctx.data(|d| d.get_temp(id)).unwrap_or_default();
    let (now, pos, active) = ctx.input(|i| {
        let key = i.events.iter().any(|e| matches!(e, egui::Event::Key { .. } | egui::Event::Text(_)));
        let active = i.pointer.any_down()
            || i.raw_scroll_delta != Vec2::ZERO
            || key
            || i.pointer.latest_pos() != st.pos;
        (i.time, i.pointer.latest_pos(), active)
    });
    st.pos = pos;
    let last = match st.last_active {
        Some(t) if !(active || st.over) => t,
        _ => now,
    };
    st.last_active = Some(last);
    ctx.data_mut(|d| d.insert_temp(id, st));
    let shown = overlay_shown(now, last, st.over);
    if shown {
        if !st.over {
            ctx.request_repaint_after(std::time::Duration::from_secs_f64((OVERLAY_IDLE - (now - last)).max(0.0) + 0.02));
        }
    } else {
        ctx.set_cursor_icon(egui::CursorIcon::None);
    }
    shown
}

/// Remember whether the pointer is over a control, so the next frame keeps the overlay.
fn overlay_note_over(ctx: &egui::Context, over: bool) {
    let id = egui::Id::new("overlay_idle").with(ctx.viewport_id());
    ctx.data_mut(|d| {
        let mut st: Idle = d.get_temp(id).unwrap_or_default();
        st.over = over;
        d.insert_temp(id, st);
    });
}

/// Pop-out UI state: one entry per open window.
#[derive(Default)]
pub struct PopoutState {
    wins: HashMap<StreamKey, Win>,
}

struct Win {
    shared: Arc<Shared>,
    /// First inner size, fixed for the window's life (a changing builder
    /// size would resize the window every frame).
    size: Vec2,
    /// `ctx` time of the next chrome-data refresh.
    next_info: f64,
}

/// What the main window and one pop-out viewport share.
struct Shared {
    view: Arc<Mutex<VideoView>>,
    close: Arc<AtomicBool>,
    /// [`ViewState`] of the stream as a number (0 healthy, 1 recovering, 2 sharer stalled).
    health: AtomicU8,
    info: Mutex<Info>,
    /// Volume edit from the viewport, applied by the main window.
    pending_volume: Mutex<Option<(u32, bool)>>,
    /// Level-loudness toggle from the viewport, applied by the main window.
    pending_level: Mutex<Option<bool>>,
}

/// Chrome data, refreshed by the main window.
#[derive(Default)]
struct Info {
    /// "sharer · room".
    label: String,
    /// "1920x1080", empty if unknown.
    res: String,
    has_audio: bool,
    volume: u32,
    muted: bool,
    /// The global loudness-levelling switch (`AppState::level_loudness`).
    level: bool,
    kbps: f64,
}

/// Declare every pop-out viewport for this frame.
pub fn show(ctx: &egui::Context, cx: &mut Cx, st: &mut PopoutState) {
    let popped = cx.videos.popped();
    st.wins.retain(|k, _| popped.contains(k));
    let now = ctx.input(|i| i.time);
    for key in popped {
        let Some(entry) = cx.videos.get(key) else {
            continue;
        };
        if entry.close_requested.load(Ordering::Relaxed) {
            st.wins.remove(&key);
            cx.act(UiAction::BringBack(key));
            continue;
        }
        let id = viewport_id(key);
        let win = st.wins.entry(key).or_insert_with(|| Win {
            shared: Arc::new(Shared {
                view: entry.view.clone(),
                close: entry.close_requested.clone(),
                health: AtomicU8::new(0),
                info: Mutex::new(Info::default()),
                pending_volume: Mutex::new(None),
                pending_level: Mutex::new(None),
            }),
            size: initial_size(cx.videos.frame_size(key).or_else(|| stream_size(cx, key))),
            next_info: 0.0,
        });
        if let Some((volume_percent, muted)) = win.shared.pending_volume.lock().unwrap().take() {
            cx.act(UiAction::SetVolume {
                stream: key,
                volume_percent,
                muted,
            });
        }
        if let Some(on) = win.shared.pending_level.lock().unwrap().take() {
            cx.act(UiAction::SetLevelLoudness(on));
        }
        if now >= win.next_info {
            win.next_info = now + INFO_INTERVAL;
            if refresh_info(cx, key, &win.shared) {
                ctx.request_repaint_of(id);
            }
        }
        let title = format!("{} · {}", cx.member_name(key.0, key.1), cx.room_name(key.0));
        let shared = win.shared.clone();
        let mut builder = egui::ViewportBuilder::default()
            .with_title(title)
            .with_inner_size(win.size)
            .with_min_inner_size([320.0, 180.0])
            .with_app_id("simple-screensharing-popout");
        if let Some(icon) = super::app_icon() {
            builder = builder.with_icon(icon);
        }
        ctx.show_viewport_deferred(
            id,
            builder,
            move |ctx, _class| viewport(ctx, &shared),
        );
    }
}

fn viewport_id(key: StreamKey) -> egui::ViewportId {
    egui::ViewportId::from_hash_of(("popout", key.0.0, key.1, key.2))
}

/// The stream's announced size (before the first decoded frame).
fn stream_size(cx: &Cx, key: StreamKey) -> Option<(u32, u32)> {
    let s = cx.session(key.0)?;
    s.streams
        .iter()
        .find(|i| i.owner == key.1 && i.stream_id == key.2)
        .map(|i| (i.width as u32, i.height as u32))
}

/// 960 points wide at the stream's aspect ratio (16:9 when unknown), height
/// clamped to 270..=960.
fn initial_size(frame: Option<(u32, u32)>) -> Vec2 {
    match frame {
        Some((w, h)) if w > 0 && h > 0 => vec2(
            960.0,
            (960.0 * h as f32 / w as f32).clamp(270.0, 960.0).round(),
        ),
        _ => vec2(960.0, 540.0),
    }
}

/// The chrome data of one stream, plus its health number (see [`Shared::health`]).
fn collect_info(cx: &Cx, key: StreamKey) -> (Info, u8) {
    let (room, owner, stream) = key;
    let label = format!("{} · {}", cx.member_name(room, owner), cx.room_name(room));
    let res = stream_size(cx, key)
        .filter(|(w, h)| *w > 0 && *h > 0)
        .map(|(w, h)| format!("{w}x{h}"))
        .unwrap_or_default();
    let vol = cx.state.volume(room, owner, stream);
    let (mut has_audio, mut kbps, mut health) = (false, 0.0, 0u8);
    if let Some(s) = cx.session(room)
        && let Some(w) = s.watches.lock().unwrap().get(&(owner, stream))
    {
        has_audio = w.audio_stream_id.is_some();
        let mut d = w.decoder.lock().unwrap();
        kbps = d.received_bitrate_kbps();
        health = match d.view_state(std::time::Instant::now()) {
            ViewState::Healthy => 0,
            ViewState::Recovering => 1,
            ViewState::SharerStalled => 2,
        };
    }
    let info = Info { label, res, has_audio, volume: vol.volume_percent, muted: vol.muted, level: cx.state.level_loudness, kbps };
    (info, health)
}

/// Refresh `shared`'s chrome data. Returns true when the stall state flipped
/// (the viewport then needs a repaint even with no new frames).
fn refresh_info(cx: &Cx, key: StreamKey, shared: &Shared) -> bool {
    let (new, health) = collect_info(cx, key);
    {
        let mut info = shared.info.lock().unwrap();
        let (volume, muted, level) = (info.volume, info.muted, info.level);
        *info = new;
        if shared.pending_level.lock().unwrap().is_some() {
            info.level = level;
        }
        // Keep the viewport's own just-made edit until it has been applied.
        if shared.pending_volume.lock().unwrap().is_some() {
            info.volume = volume;
            info.muted = muted;
        }
    }
    shared.health.swap(health, Ordering::Relaxed) != health
}

/// The pop-out's overlay (centre chip, hover chrome) over the main window's own
/// fullscreen stream (`Nav::fullscreen`), same code as the pop-out viewport.
/// Its top-right button leaves fullscreen instead of closing a window.
/// Returns true when that button was clicked.
pub fn fullscreen_overlay(ui: &mut Ui, cx: &mut Cx, key: StreamKey, rect: Rect, first_frame: bool) -> bool {
    let (mut info, health) = collect_info(cx, key);
    if !first_frame || health != 0 {
        centre_chip(ui, rect, &chip_text(first_frame, health));
    }
    if !ui.ctx().input(|i| i.pointer.has_pointer()) || !overlay_visible(ui.ctx()) {
        return false;
    }
    let Some(entry) = cx.videos.get(key) else {
        return false;
    };
    let stats = entry.view.lock().unwrap().source().stats();
    let tip = tr("ui.popout.exit_fullscreen_tip", "Exit fullscreen");
    let out = chrome(ui, rect, &mut info, (stats.decoded_fps, stats.presented_fps), ("fullscreen_exit", &tip));
    overlay_note_over(ui.ctx(), out.over);
    if let Some((volume_percent, muted)) = out.volume {
        cx.act(UiAction::SetVolume { stream: key, volume_percent, muted });
    }
    if let Some(on) = out.level {
        cx.act(UiAction::SetLevelLoudness(on));
    }
    out.button
}

fn chip_text(first_frame: bool, health: u8) -> String {
    match (first_frame, health) {
        (false, _) => tr("video.waiting_frame", "Waiting for first decoded frame…"),
        (true, 2) => tr("video.sharer_stalled", "Sharer's capture stalled"),
        (true, _) => tr("video.recovering", "Recovering…"),
    }
}

// ---------------------------------------------------------------------------
// The viewport (runs in the pop-out window's own pass)
// ---------------------------------------------------------------------------

fn viewport(ctx: &egui::Context, shared: &Shared) {
    let fullscreen_now = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
    // Windowed pop-outs keep the chrome for as long as the pointer is inside; only
    // fullscreen auto-hides it (#124).
    let hovered = ctx.input(|i| i.pointer.has_pointer()) && (!fullscreen_now || overlay_visible(ctx));
    let mut double_clicked = false;
    egui::CentralPanel::default()
        .frame(egui::Frame::NONE.fill(Color32::BLACK))
        .show(ctx, |ui| {
            let rect = ui.max_rect();
            let pic = shared.view.lock().unwrap().paint(ui, rect, Fit::Contain);
            let first_frame = pic.width() > 0.0;
            // Double-click toggles fullscreen. Registered before the chrome,
            // whose buttons then take their own clicks.
            double_clicked = ui.interact(rect, ui.id().with("popout_dbl"), Sense::click()).double_clicked();
            let health = shared.health.load(Ordering::Relaxed);
            if !first_frame || health != 0 {
                centre_chip(ui, rect, &chip_text(first_frame, health));
            }
            if hovered {
                let stats = shared.view.lock().unwrap().source().stats();
                let tip = tr("ui.popout.close_tip", "Close and bring it back to its tile");
                let out = chrome(
                    ui,
                    rect,
                    &mut shared.info.lock().unwrap(),
                    (stats.decoded_fps, stats.presented_fps),
                    ("close", &tip),
                );
                if fullscreen_now {
                    overlay_note_over(ui.ctx(), out.over);
                }
                if out.button {
                    shared.close.store(true, Ordering::Relaxed);
                    ui.ctx().request_repaint_of(egui::ViewportId::ROOT);
                }
                if out.volume.is_some() {
                    *shared.pending_volume.lock().unwrap() = out.volume;
                    ui.ctx().request_repaint_of(egui::ViewportId::ROOT);
                }
                if out.level.is_some() {
                    *shared.pending_level.lock().unwrap() = out.level;
                    ui.ctx().request_repaint_of(egui::ViewportId::ROOT);
                }
            }
        });
    let (f11, esc, fullscreen) = ctx.input(|i| {
        (
            i.key_pressed(egui::Key::F11),
            i.key_pressed(egui::Key::Escape),
            i.viewport().fullscreen.unwrap_or(false),
        )
    });
    if f11 || double_clicked {
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fullscreen));
    } else if esc && fullscreen {
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
    }
    if ctx.input(|i| i.viewport().close_requested()) {
        shared.close.store(true, Ordering::Relaxed);
        ctx.request_repaint_of(egui::ViewportId::ROOT);
    }
}

fn centre_chip(ui: &Ui, rect: Rect, text: &str) {
    let f = font(Weight::Bold, theme::FS_SMALL);
    let w = text_width(ui, text, f.clone(), 0.0) + 2.0 * theme::S3;
    let r = Rect::from_center_size(rect.center(), vec2(w, CHROME_H));
    ui.painter()
        .rect_filled(r, CornerRadius::same(15), theme::CHIP_DARK);
    paint_text(ui, r, text, f, theme::TEXT, Align::Center);
}

/// What the user did in [`chrome`] this frame.
struct ChromeOut {
    /// The top-right round button was clicked.
    button: bool,
    /// New (volume, muted) when the audio pill was edited.
    volume: Option<(u32, bool)>,
    /// New level-loudness switch when its button in the audio pill was clicked.
    level: Option<bool>,
    /// The pointer is over the statbar, the button or the audio pill / chip.
    over: bool,
}

/// Vignette + top row (statbar, round button `(icon, tip)`) + bottom row
/// (audio pill). `fps` is (decoded, presented).
fn chrome(ui: &mut Ui, rect: Rect, info: &mut Info, fps: (f64, f64), button: (&str, &str)) -> ChromeOut {
    let pointer = ui.input(|i| i.pointer.latest_pos()).unwrap_or(pos2(f32::NAN, f32::NAN));
    let p = ui.painter();
    let shade = |a: f32| Color32::from_rgba_unmultiplied(10, 11, 13, (a * 255.0) as u8);
    let split = rect.top() + rect.height() * 0.4;
    widgets::paint_gradient(
        p,
        Rect::from_x_y_ranges(rect.x_range(), rect.top()..=split),
        CornerRadius::ZERO,
        180.0,
        shade(0.55),
        shade(0.1),
    );
    widgets::paint_gradient(
        p,
        Rect::from_x_y_ranges(rect.x_range(), split..=rect.bottom()),
        CornerRadius::ZERO,
        180.0,
        shade(0.1),
        shade(0.82),
    );

    let inner = rect.shrink(theme::S2);
    let mut ov = ui.new_child(
        UiBuilder::new()
            .max_rect(inner)
            .layout(Layout::top_down(Align::Min)),
    );
    ov.spacing_mut().item_spacing = Vec2::ZERO;

    // Top: [fill statbar][29 ×].
    let top = HBox::new("popout_top", CHROME_H)
        .gap(theme::S2)
        .slots(&[FILL, fixed(CHROME_H)])
        .show(&mut ov);
    let bar = top.show(&mut ov, 0, Align::Min, |ui| {
        statbar(ui, info, fps.0, fps.1);
        ui.min_rect()
    });
    let button = top.show(&mut ov, 1, Align::Min, |ui| round_btn(ui, button.0, button.1));
    let mut over = bar.contains(pointer) || button.rect.contains(pointer);
    let button = button.clicked();

    // Bottom: [audio pill | no-audio chip].
    let mut bottom = ov.new_child(
        UiBuilder::new()
            .max_rect(Rect::from_min_max(
                pos2(inner.left(), inner.bottom() - CHROME_H),
                inner.max,
            ))
            .layout(Layout::left_to_right(Align::Center)),
    );
    let (mut volume, mut level) = (None, None);
    if info.has_audio {
        (volume, level) = audio_pill(&mut bottom, info);
    } else {
        widgets::chip_dark(
            &mut bottom,
            Some("volume_off"),
            false,
            &tr("ui.popout.no_audio", "No audio shared"),
        );
    }
    over |= bottom.min_rect().contains(pointer);
    ChromeOut { button, volume, level, over }
}

/// `.statbar`: dark pill, `[dot][fill name][res][fps][bitrate]`, content-sized
/// up to the cell width (the name truncates first).
fn statbar(ui: &mut Ui, info: &Info, decoded: f64, presented: f64) {
    let mono = font(Weight::Mono, theme::FS_SMALL);
    let ch = text_width(ui, "0", mono.clone(), 0.0);
    let name_f = font(Weight::Bold, theme::FS_ROW);
    let res_w = if info.res.is_empty() {
        0.0
    } else {
        ch * info.res.len() as f32
    };
    let (fps_w, kbps_w) = (ch * 10.0, ch * 9.0);
    let name_w = text_width(ui, &info.label, name_f.clone(), 0.0).ceil();
    let gap = theme::S2;
    let content = 7.0 + gap + name_w + gap + res_w + gap + fps_w + gap + kbps_w;
    let w = (content + 20.0).min(ui.available_width());
    let slots = [fixed(7.0), FILL, fixed(res_w), fixed(fps_w), fixed(kbps_w)];
    let row = HBox::new("popout_statbar", CHROME_H)
        .width(w)
        .pad(10.0, 10.0)
        .gap(gap)
        .slots(&slots)
        .sense(Sense::hover())
        .show(ui);
    let p = ui.painter();
    p.rect_filled(
        row.rect,
        CornerRadius::same(15),
        Color32::from_black_alpha(158),
    );
    let dot = row.cell(0).center();
    p.circle_filled(dot, 5.0, theme::SUCCESS_GLOW.gamma_multiply(0.25));
    p.circle_filled(dot, 3.5, theme::SUCCESS);
    row.text(ui, 1, &info.label, name_f, theme::TEXT, Align::Min);
    let val = Color32::from_rgba_unmultiplied(232, 233, 236, 217);
    if !info.res.is_empty() {
        row.text(ui, 2, &info.res, mono.clone(), val, Align::Max);
    }
    let fps = format!("{:.0}/{:.0}fps", decoded, presented);
    row.text(ui, 3, &fps, mono.clone(), val, Align::Max);
    row.text(ui, 4, &fmt_kbps(info.kbps), mono, val, Align::Max);
    row.response.on_hover_text(tr(
        "ui.popout.stats_tip",
        "Resolution · decoded / shown frames per second · received bitrate",
    ));
}

fn fmt_kbps(kbps: f64) -> String {
    if kbps >= 1000.0 {
        format!("{:.1} Mbps", kbps / 1000.0)
    } else {
        format!("{kbps:.0} kbps")
    }
}

/// `.round-btn`: 29 px circle, translucent dark, thin light ring.
fn round_btn(ui: &mut Ui, icon: &str, tip: &str) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(CHROME_H), Sense::click());
    let fill = Color32::from_rgba_unmultiplied(16, 17, 20, if resp.hovered() { 204 } else { 140 });
    let p = ui.painter();
    p.circle_filled(rect.center(), CHROME_H / 2.0, fill);
    p.circle_stroke(
        rect.center(),
        CHROME_H / 2.0 - 0.5,
        Stroke::new(1.0_f32, Color32::from_white_alpha(41)),
    );
    paint_icon(ui, rect, icon, 16.0, theme::TEXT);
    resp.on_hover_text(tip)
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// `.audio-pill`: `[25 mute][110 slider][34 %][25 level]`, or `[25 mute][Muted]`.
/// Returns the new (volume, muted) when the user changed it, and the new level-loudness
/// switch when its button was clicked (#126).
fn audio_pill(ui: &mut Ui, info: &mut Info) -> (Option<(u32, bool)>, Option<bool>) {
    let pct_f = font(Weight::Mono, theme::FS_TINY);
    let muted_txt = tr("ui.popout.muted", "Muted");
    let slots_on = [fixed(25.0), fixed(110.0), fixed(34.0), fixed(25.0)];
    let muted_w = text_width(ui, &muted_txt, pct_f.clone(), 0.0).ceil();
    let slots_off = [fixed(25.0), fixed(muted_w)];
    // The row shape is fixed by the state it was laid out for; a click on
    // mute this pass only changes it next pass.
    let was_muted = info.muted;
    let (slots, w): (&[_], f32) = if was_muted {
        (&slots_off, 2.0 + 25.0 + 4.0 + muted_w + 10.0)
    } else {
        (&slots_on, 2.0 + 25.0 + 4.0 + 110.0 + 4.0 + 34.0 + 4.0 + 25.0 + 2.0)
    };
    // Right pad 2 like the left one: the level button fills the right end cap (#130).
    let pad_r = if was_muted { 10.0 } else { 2.0 };
    let row = HBox::new("popout_audio", CHROME_H)
        .width(w)
        .pad(2.0, pad_r)
        .gap(theme::S1)
        .slots(slots)
        .show(ui);
    ui.painter().rect(
        row.rect,
        CornerRadius::same(15),
        Color32::from_black_alpha(184),
        Stroke::new(1.0_f32, Color32::from_white_alpha(46)),
        StrokeKind::Inside,
    );
    let (mut out, mut level) = (None, None::<bool>);
    let tip = if info.muted {
        tr("ui.popout.unmute", "Unmute")
    } else {
        tr("ui.popout.mute", "Mute this stream")
    };
    let mute = row.show(ui, 0, Align::Center, |ui| {
        let (rect, resp) = ui.allocate_exact_size(Vec2::splat(25.0), Sense::click());
        if resp.hovered() {
            ui.painter()
                .circle_filled(rect.center(), 12.5, Color32::from_white_alpha(20));
        }
        let (icon, color) = if info.muted {
            ("volume_off", theme::DANGER)
        } else {
            ("volume_up", theme::TEXT)
        };
        paint_icon(ui, rect, icon, 16.0, color);
        resp.on_hover_text(tip)
            .on_hover_cursor(egui::CursorIcon::PointingHand)
    });
    if mute.clicked() {
        info.muted = !info.muted;
        out = Some((info.volume, info.muted));
        ui.ctx().request_repaint();
    }
    if was_muted {
        row.text(ui, 1, &muted_txt, pct_f, theme::DANGER, Align::Min);
    } else {
        let mut v = info.volume;
        if row
            .show(ui, 1, Align::Min, |ui| {
                super::volume_slider(ui, &mut v)
            })
            .changed()
        {
            info.volume = v;
            out = Some((v, info.muted));
        }
        let pct = format!("{}%", info.volume);
        row.text(
            ui,
            2,
            &pct,
            pct_f,
            Color32::from_rgba_unmultiplied(232, 233, 236, 217),
            Align::Max,
        );
        let lr = Rect::from_center_size(row.cell(3).center(), Vec2::splat(25.0));
        if super::level_toggle(ui, lr, ui.id().with("popout_level"), info.level) {
            info.level = !info.level;
            level = Some(info.level);
            ui.ctx().request_repaint();
        }
    }
    (out, level)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_size_follows_the_stream_aspect_ratio() {
        assert_eq!(initial_size(Some((1920, 1080))), vec2(960.0, 540.0));
        assert_eq!(initial_size(Some((2560, 1080))), vec2(960.0, 405.0));
        assert_eq!(
            initial_size(Some((1080, 1920))),
            vec2(960.0, 960.0),
            "portrait is clamped"
        );
        assert_eq!(initial_size(None), vec2(960.0, 540.0));
        assert_eq!(initial_size(Some((0, 0))), vec2(960.0, 540.0));
    }

    #[test]
    fn overlay_hides_after_the_idle_time_unless_a_control_is_hovered() {
        assert!(overlay_shown(10.0, 10.0, false));
        assert!(overlay_shown(10.0 + OVERLAY_IDLE - 0.01, 10.0, false));
        assert!(!overlay_shown(10.0 + OVERLAY_IDLE, 10.0, false));
        assert!(overlay_shown(100.0, 10.0, true), "resting on a control keeps it");
    }

    #[test]
    fn bitrate_formats_like_the_tile() {
        assert_eq!(fmt_kbps(950.4), "950 kbps");
        assert_eq!(fmt_kbps(8120.0), "8.1 Mbps");
    }

    /// #130, user: "The dynamic volume button should be all the way in the right half-circle":
    /// the level button's disc is concentric with the pill's right end cap (and the mute
    /// button's hover disc with the left one).
    #[test]
    fn level_button_sits_concentric_in_the_right_end_cap() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut info = Info { label: String::new(), res: String::new(), has_audio: true, volume: 100, muted: false, level: true, kbps: 0.0 };
        let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(600.0, 100.0))), ..Default::default() };
        let out = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                audio_pill(ui, &mut info);
            });
        });
        let mut pill = None;
        let mut disc = None;
        for cs in &out.shapes {
            match &cs.shape {
                egui::Shape::Rect(r) if r.corner_radius == CornerRadius::same(15) => pill = Some(r.rect),
                egui::Shape::Circle(c) if c.fill == theme::ACCENT_CHIP_BG => disc = Some(*c),
                _ => {}
            }
        }
        let (pill, disc) = (pill.expect("pill painted"), disc.expect("level disc painted"));
        let cap = egui::pos2(pill.right() - pill.height() / 2.0, pill.center().y);
        assert!((disc.center - cap).length() < 0.01, "disc {:?} vs end-cap centre {cap:?} (pill {pill:?})", disc.center);
    }
}
