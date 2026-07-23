//! Simple Screensharing — Rust/egui desktop client.
//!
//! QUIC networking (`net.rs`), wire protocol (`protocol.rs`), fragmentation
//! (`fragment.rs`), saved rooms (`rooms.rs`), and the join/roster UI
//! (`ui.rs`) are implemented. Capture and decode/render are still follow-up
//! work — see their module doc-comments.

#[macro_use]
extern crate rust_i18n;

i18n!("locales", fallback = "en");

mod audio;
mod capture;
mod decode;
mod fragment;
mod net;
mod preview;
mod protocol;
mod rooms;
mod ui;

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt::init();

    // eframe/winit owns the main-thread event loop; several dependencies
    // (e.g. zbus, used by accesskit's AT-SPI integration and later by
    // ashpd) expect an active Tokio reactor even before we've wired up
    // real async networking. Build a multi-thread runtime and keep it
    // entered for the lifetime of the app. Actual capture/network work
    // should be spawned onto this runtime from a background thread rather
    // than blocking the UI thread.
    let tokio_rt = tokio::runtime::Runtime::new().expect("failed to start tokio runtime");
    let _tokio_guard = tokio_rt.enter();

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([800.0, 600.0])
            .with_title("Simple Screensharing"),
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };

    eframe::run_native(
        "Simple Screensharing",
        native_options,
        Box::new(|_cc| Ok(Box::new(ui::App::default()))),
    )
}
