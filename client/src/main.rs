//! Simple Screensharing — Rust desktop client.
//!
//! QUIC networking (`net.rs`), wire protocol (`protocol.rs`), fragmentation
//! (`fragment.rs`), capture (`capture.rs`/`audio.rs`), decode (`decode.rs`),
//! saved rooms (`rooms.rs`), the shared egui theme (`theme.rs`), and the
//! egui UI (`ui.rs`) make up the app.
//!
//! # Front-end
//!
//! This launches the eframe/egui desktop window (`ui.rs`) — the only
//! front-end the client has.

#[macro_use]
extern crate rust_i18n;

i18n!("locales", fallback = "en");

mod audio;
mod bandwidth;
mod capture;
mod decode;
#[cfg(test)]
mod end_to_end_test;
mod fragment;
mod net;
mod preview;
mod protocol;
mod rooms;
mod theme;
mod ui;

const USAGE: &str = "\
Simple Screensharing client

Usage: simple-screensharing-client [OPTIONS]

Opens the desktop (egui) window.

Options:
  -h, --help             Print this help and exit.
";

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Option<()>, String> {
    match args.next() {
        None => Ok(Some(())),
        Some(arg) if arg == "-h" || arg == "--help" => Ok(None),
        Some(other) => Err(format!("unrecognized argument {other:?}")),
    }
}

fn main() -> anyhow::Result<()> {
    match parse_args(std::env::args().skip(1)) {
        Ok(Some(())) => {}
        Ok(None) => {
            print!("{USAGE}");
            return Ok(());
        }
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            std::process::exit(2);
        }
    }

    // eframe/winit owns the main-thread event loop; several dependencies
    // (e.g. zbus, used by accesskit's AT-SPI integration and later by
    // ashpd) expect an active Tokio reactor even before we've wired up
    // real async networking. Build a multi-thread runtime and keep it
    // entered for the lifetime of the app. Actual capture/network work
    // should be spawned onto this runtime from a background thread rather
    // than blocking the UI thread.
    let tokio_rt = tokio::runtime::Runtime::new().expect("failed to start tokio runtime");
    let _tokio_guard = tokio_rt.enter();

    tracing_subscriber::fmt::init();

    // Presentation mode is only settable here, at surface-creation time: the
    // pinned eframe/egui-wgpu (0.33.3) doesn't expose the render surface for
    // reconfiguration once the window exists, so a change made in the
    // settings UI (`ui.rs`'s presentation-mode dropdown) is picked up on the
    // *next* launch, not live. See `superdoc/features/presentation-modes.md`.
    let presentation_mode = rooms::AppSettings::load().presentation_mode;
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([800.0, 600.0])
            .with_title("Simple Screensharing"),
        renderer: eframe::Renderer::Wgpu,
        wgpu_options: eframe::egui_wgpu::WgpuConfiguration {
            present_mode: presentation_mode.to_wgpu(),
            ..Default::default()
        },
        ..Default::default()
    };

    eframe::run_native(
        "Simple Screensharing",
        native_options,
        Box::new(|_cc| Ok(Box::new(ui::App::default()))),
    )?;
    Ok(())
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Option<()>, String> {
        parse_args(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn no_arguments_launches_the_gui() {
        assert_eq!(parse(&[]), Ok(Some(())));
    }

    #[test]
    fn help_and_errors_are_distinguishable() {
        assert!(parse(&["--help"]).unwrap().is_none());
        assert!(parse(&["--nope"]).is_err());
    }
}
