//! Simple Screensharing — Rust desktop client.
//!
//! QUIC networking (`net.rs`), wire protocol (`protocol.rs`), fragmentation
//! (`fragment.rs`), capture (`capture.rs`/`audio.rs`), decode (`decode.rs`),
//! saved rooms (`rooms.rs`), the shared egui theme (`theme.rs`), and the
//! egui UI (`ui.rs`) make up the app.
//!
//! # Front-end
//!
//! This launches the eframe/egui desktop window (`ui.rs`). egui is becoming
//! the client's only front-end; the Electron shell is still present until it
//! is removed later. Only the UI modules (`ui`, `theme`, `widgets`) live in this
//! bin; the core (net, capture, decode, rooms, session, ...) is the
//! `simple_screensharing_client` lib crate (`lib.rs`).
//!
//! `--gallery` opens the widget gallery (`widgets::gallery`) instead of the app:
//! every theme/widget helper with fake data, for visual QC of the new design.

#[macro_use]
extern crate rust_i18n;

i18n!("locales", fallback = "en");

mod theme;
mod ui;
mod widgets;

const USAGE: &str = "\
Simple Screensharing client

Usage: simple-screensharing-client [OPTIONS]

Opens the desktop (egui) window.

Options:
      --gallery          Open the widget gallery (design QC) instead of the app.
  -h, --help             Print this help and exit.
";

/// What `main` should launch.
#[derive(Debug, PartialEq, Eq)]
enum Mode {
    /// The app window.
    App,
    /// The widget gallery (`--gallery`).
    Gallery,
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Option<Mode>, String> {
    let mode = match args.next() {
        None => Mode::App,
        Some(arg) if arg == "-h" || arg == "--help" => return Ok(None),
        Some(arg) if arg == "--gallery" => Mode::Gallery,
        Some(other) => return Err(format!("unrecognized argument {other:?}")),
    };
    match args.next() {
        None => Ok(Some(mode)),
        Some(other) => Err(format!("unrecognized argument {other:?}")),
    }
}

fn main() -> anyhow::Result<()> {
    let mode = match parse_args(std::env::args().skip(1)) {
        Ok(Some(mode)) => mode,
        Ok(None) => {
            print!("{USAGE}");
            return Ok(());
        }
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            std::process::exit(2);
        }
    };

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
    let presentation_mode = simple_screensharing_client::rooms::AppSettings::load().presentation_mode;
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

    if mode == Mode::Gallery {
        let options = eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([1600.0, 1000.0])
                .with_title("Simple Screensharing — widget gallery")
                .with_app_id("simple-screensharing-gallery"),
            persist_window: false,
            ..native_options
        };
        widgets::gallery::run(options)?;
        return Ok(());
    }

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

    fn parse(args: &[&str]) -> Result<Option<Mode>, String> {
        parse_args(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn no_arguments_launches_the_gui() {
        assert_eq!(parse(&[]), Ok(Some(Mode::App)));
    }

    #[test]
    fn gallery_flag_opens_the_gallery() {
        assert_eq!(parse(&["--gallery"]), Ok(Some(Mode::Gallery)));
        assert!(parse(&["--gallery", "extra"]).is_err());
    }

    #[test]
    fn help_and_errors_are_distinguishable() {
        assert!(parse(&["--help"]).unwrap().is_none());
        assert!(parse(&["--nope"]).is_err());
    }
}
