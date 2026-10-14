//! Pop-out viewer windows (owner: I16). One deferred viewport per stream
//! with `popped` set in `cx.videos` (re-declared every frame it should
//! exist — egui's viewport contract). The viewport paints the shared
//! `Arc<Mutex<VideoView>>` with `paint(ui, ui.max_rect(), Fit::Contain)`;
//! repaint is driven by the view (never `request_repaint()` for video). A
//! window closed by the OS (or its × button) sets `close_requested`, which
//! this module turns into [`UiAction::BringBack`] on the next main-window
//! frame.
//!
//! Feature set of the Electron viewer window (`frontend/viewer.html`,
//! `superdoc/features/viewer-window.md`), drawn like the mockup's tile
//! overlay:
//! - **Hover-gated chrome**: while the pointer is anywhere in the window (the
//!   Electron `body:hover`; no idle timeout there either) a vignette, a top
//!   row `[fill statbar][29 ×]` and a bottom audio pill. The statbar is
//!   `[dot][name · room][WxH][dec/pres fps][bitrate]`, mono values in fixed
//!   tabular-width fields so they don't jitter.
//! - **Audio pill** `[25 mute][110 slider 0–200 %][34 %]` (or `[mute]Muted`),
//!   only when the sender shares audio; otherwise a "No audio shared" chip.
//!   Pushes [`UiAction::SetVolume`] through the main window.
//! - **Waiting / stalled**: a centred chip before the first frame and while
//!   the decoder reports a stall (always shown, not hover-gated).
//! - **F11** toggles fullscreen, **Escape** only leaves fullscreen (never
//!   closes — the Electron rule).
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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use egui::{
    Align, Color32, CornerRadius, Layout, Rect, Sense, Stroke, StrokeKind, Ui, UiBuilder, Vec2,
    pos2, vec2,
};
use simple_screensharing_client::decode::render::{Fit, VideoView};

use super::modals::tr;
use super::{Cx, StreamKey, UiAction};
use crate::theme::{self, Weight, font};
use crate::widgets::{self, FILL, HBox, fixed, paint_icon, paint_text, text_width};

/// How often the main window refreshes a pop-out's chrome data, seconds.
const INFO_INTERVAL: f64 = 0.25;
/// Height of the statbar, round buttons and audio pill (mockup 29 px).
const CHROME_H: f32 = 29.0;

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
    stalled: AtomicBool,
    info: Mutex<Info>,
    /// Volume edit from the viewport, applied by the main window.
    pending_volume: Mutex<Option<(u32, bool)>>,
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
                stalled: AtomicBool::new(false),
                info: Mutex::new(Info::default()),
                pending_volume: Mutex::new(None),
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
        if now >= win.next_info {
            win.next_info = now + INFO_INTERVAL;
            if refresh_info(cx, key, &win.shared) {
                ctx.request_repaint_of(id);
            }
        }
        let title = format!("{} · {}", cx.member_name(key.0, key.1), cx.room_name(key.0));
        let shared = win.shared.clone();
        ctx.show_viewport_deferred(
            id,
            egui::ViewportBuilder::default()
                .with_title(title)
                .with_inner_size(win.size)
                .with_min_inner_size([320.0, 180.0])
                .with_app_id("simple-screensharing-popout"),
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

/// Refresh `shared`'s chrome data. Returns true when the stall state flipped
/// (the viewport then needs a repaint even with no new frames).
fn refresh_info(cx: &Cx, key: StreamKey, shared: &Shared) -> bool {
    let (room, owner, stream) = key;
    let label = format!("{} · {}", cx.member_name(room, owner), cx.room_name(room));
    let res = stream_size(cx, key)
        .filter(|(w, h)| *w > 0 && *h > 0)
        .map(|(w, h)| format!("{w}x{h}"))
        .unwrap_or_default();
    let vol = cx.state.volume(room, owner, stream);
    let (mut has_audio, mut kbps, mut stalled) = (false, 0.0, false);
    if let Some(s) = cx.session(room)
        && let Some(w) = s.watches.lock().unwrap().get(&(owner, stream))
    {
        has_audio = w.audio_stream_id.is_some();
        let mut d = w.decoder.lock().unwrap();
        kbps = d.received_bitrate_kbps();
        stalled = d.is_stalled();
    }
    {
        let mut info = shared.info.lock().unwrap();
        info.label = label;
        info.res = res;
        info.has_audio = has_audio;
        info.kbps = kbps;
        // Keep the viewport's own just-made edit until it has been applied.
        if shared.pending_volume.lock().unwrap().is_none() {
            info.volume = vol.volume_percent;
            info.muted = vol.muted;
        }
    }
    shared.stalled.swap(stalled, Ordering::Relaxed) != stalled
}

// ---------------------------------------------------------------------------
// The viewport (runs in the pop-out window's own pass)
// ---------------------------------------------------------------------------

fn viewport(ctx: &egui::Context, shared: &Shared) {
    let hovered = ctx.input(|i| i.pointer.has_pointer());
    egui::CentralPanel::default()
        .frame(egui::Frame::NONE.fill(Color32::BLACK))
        .show(ctx, |ui| {
            let rect = ui.max_rect();
            let pic = shared.view.lock().unwrap().paint(ui, rect, Fit::Contain);
            let first_frame = pic.width() > 0.0;
            if !first_frame || shared.stalled.load(Ordering::Relaxed) {
                let text = if first_frame {
                    t!("stream_control.state_stalled")
                } else {
                    t!("video.waiting_frame")
                };
                centre_chip(ui, rect, &text);
            }
            if hovered {
                chrome(ui, rect, shared);
            }
        });
    let (f11, esc, fullscreen) = ctx.input(|i| {
        (
            i.key_pressed(egui::Key::F11),
            i.key_pressed(egui::Key::Escape),
            i.viewport().fullscreen.unwrap_or(false),
        )
    });
    if f11 {
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

/// Vignette + top row (statbar, ×) + bottom row (audio pill).
fn chrome(ui: &mut Ui, rect: Rect, shared: &Shared) {
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
    let mut info = shared.info.lock().unwrap();

    // Top: [fill statbar][29 ×].
    let top = HBox::new("popout_top", CHROME_H)
        .gap(theme::S2)
        .slots(&[FILL, fixed(CHROME_H)])
        .show(&mut ov);
    let stats = shared.view.lock().unwrap().source().stats();
    top.show(&mut ov, 0, Align::Min, |ui| {
        statbar(ui, &info, stats.decoded_fps, stats.presented_fps)
    });
    if top
        .show(&mut ov, 1, Align::Min, |ui| {
            round_btn(
                ui,
                "close",
                &tr("ui.popout.close_tip", "Close and bring it back to its tile"),
            )
        })
        .clicked()
    {
        shared.close.store(true, Ordering::Relaxed);
        ui.ctx().request_repaint_of(egui::ViewportId::ROOT);
    }

    // Bottom: [audio pill | no-audio chip].
    let mut bottom = ov.new_child(
        UiBuilder::new()
            .max_rect(Rect::from_min_max(
                pos2(inner.left(), inner.bottom() - CHROME_H),
                inner.max,
            ))
            .layout(Layout::left_to_right(Align::Center)),
    );
    if info.has_audio {
        if let Some(v) = audio_pill(&mut bottom, &mut info) {
            *shared.pending_volume.lock().unwrap() = Some(v);
            ui.ctx().request_repaint_of(egui::ViewportId::ROOT);
        }
    } else {
        widgets::chip_dark(
            &mut bottom,
            Some("volume_off"),
            false,
            &tr("ui.popout.no_audio", "No audio shared"),
        );
    }
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

/// `.audio-pill`: `[25 mute][110 slider][34 %]`, or `[25 mute][Muted]`.
/// Returns the new (volume, muted) when the user changed it.
fn audio_pill(ui: &mut Ui, info: &mut Info) -> Option<(u32, bool)> {
    let pct_f = font(Weight::Mono, theme::FS_TINY);
    let muted_txt = tr("ui.popout.muted", "Muted");
    let slots_on = [fixed(25.0), fixed(110.0), fixed(34.0)];
    let muted_w = text_width(ui, &muted_txt, pct_f.clone(), 0.0).ceil();
    let slots_off = [fixed(25.0), fixed(muted_w)];
    let (slots, w): (&[_], f32) = if info.muted {
        (&slots_off, 2.0 + 25.0 + 4.0 + muted_w + 10.0)
    } else {
        (&slots_on, 2.0 + 25.0 + 4.0 + 110.0 + 4.0 + 34.0 + 10.0)
    };
    let row = HBox::new("popout_audio", CHROME_H)
        .width(w)
        .pad(2.0, 10.0)
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
    let mut out = None;
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
    }
    if info.muted {
        row.text(ui, 1, &muted_txt, pct_f, theme::DANGER, Align::Min);
    } else {
        let mut v = info.volume;
        if row
            .show(ui, 1, Align::Min, |ui| {
                widgets::slider(ui, &mut v, 0..=200, 5.0)
            })
            .changed()
        {
            info.volume = v;
            out = Some((v, false));
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
    }
    out
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
    fn bitrate_formats_like_the_tile() {
        assert_eq!(fmt_kbps(950.4), "950 kbps");
        assert_eq!(fmt_kbps(8120.0), "8.1 Mbps");
    }
}
