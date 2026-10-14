//! Pop-out viewer windows (owner: I16). One deferred viewport per stream
//! with `popped` set in `cx.videos` (re-declared every frame it should
//! exist — egui's viewport contract). The viewport paints the shared
//! `Arc<Mutex<VideoView>>` with `paint(ui, ui.max_rect(), Fit::Contain)`;
//! repaint is driven by the view (never `request_repaint()` for video). A
//! window closed by the OS sets `close_requested`, which this module turns
//! into [`UiAction::BringBack`] on the next main-window frame.
//!
//! **STUB**: borderless window with the video, F11 fullscreen, no overlay.

use std::sync::atomic::Ordering;

use simple_screensharing_client::decode::render::Fit;

use super::{Cx, UiAction};

/// Pop-out UI state (I16: add fields freely).
#[derive(Default)]
pub struct PopoutState {}

/// Declare every pop-out viewport for this frame.
pub fn show(ctx: &egui::Context, cx: &mut Cx, _st: &mut PopoutState) {
    for key in cx.videos.popped() {
        let Some(entry) = cx.videos.get(key) else { continue };
        if entry.close_requested.load(Ordering::Relaxed) {
            cx.act(UiAction::BringBack(key));
            continue;
        }
        let view = entry.view.clone();
        let close = entry.close_requested.clone();
        let title = format!("{} · {}", cx.member_name(key.0, key.1), cx.room_name(key.0));
        let id = egui::ViewportId::from_hash_of(("popout", key.0.0, key.1, key.2));
        ctx.show_viewport_deferred(
            id,
            egui::ViewportBuilder::default().with_title(title).with_inner_size([960.0, 540.0]).with_app_id("simple-screensharing-popout"),
            move |ctx, _class| {
                egui::CentralPanel::default().frame(egui::Frame::NONE.fill(egui::Color32::BLACK)).show(ctx, |ui| {
                    view.lock().unwrap().paint(ui, ui.max_rect(), Fit::Contain);
                });
                if ctx.input(|i| i.key_pressed(egui::Key::F11)) {
                    let fs = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fs));
                }
                if ctx.input(|i| i.viewport().close_requested()) {
                    close.store(true, Ordering::Relaxed);
                }
            },
        );
    }
}
