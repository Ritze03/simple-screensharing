//! Simple Screensharing — Rust desktop client.
//!
//! QUIC networking (`net.rs`), wire protocol (`protocol.rs`), fragmentation
//! (`fragment.rs`), capture (`capture.rs`/`audio.rs`), decode (`decode.rs`),
//! saved rooms (`rooms.rs`), the shared egui theme (`theme.rs`), and the
//! egui UI (`ui.rs`) make up the app.
//!
//! # Front-ends
//!
//! By default this launches the eframe/egui desktop window (`ui.rs`) — that
//! is the product, and passing no flags behaves exactly as it always has.
//! `--tui` instead runs the terminal front-end in `tui.rs`, a development and
//! testing convenience for driving the client from a tmux pane (see that
//! module's docs for the full rationale). Both share the same tokio runtime
//! setup below and the same networking/capture/decode modules; only the
//! presentation layer differs.

#[macro_use]
extern crate rust_i18n;

i18n!("locales", fallback = "en");

mod audio;
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
mod tui;
mod ui;

use std::path::PathBuf;

const USAGE: &str = "\
Simple Screensharing client

Usage: simple-screensharing-client [OPTIONS]

With no options, opens the normal desktop (egui) window.

Options:
      --tui              Run the terminal UI instead of the desktop window.
                         Intended for development/testing from a tmux pane;
                         see src/tui.rs.
      --join <LINK>      Pre-fill the join link (server|room-id|key). --tui only.
      --name <NAME>      Pre-fill the display name. --tui only.
      --log-file <PATH>  Where --tui writes tracing output, since the terminal
                         is occupied by the UI. Defaults to a file in the
                         system temp directory.
  -h, --help             Print this help and exit.
";

/// Hand-rolled rather than pulled from `clap`: four flags, no subcommands, no
/// completions. A dependency would cost more (compile time, another version to
/// keep current) than the ~30 lines it would save.
struct CliArgs {
    tui: bool,
    join: Option<String>,
    name: Option<String>,
    log_file: Option<PathBuf>,
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<Option<CliArgs>, String> {
    let mut parsed = CliArgs { tui: false, join: None, name: None, log_file: None };
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        let mut value = |flag: &str| args.next().ok_or_else(|| format!("{flag} requires a value"));
        match arg.as_str() {
            "--tui" => parsed.tui = true,
            "--join" => parsed.join = Some(value("--join")?),
            "--name" => parsed.name = Some(value("--name")?),
            "--log-file" => parsed.log_file = Some(PathBuf::from(value("--log-file")?)),
            "-h" | "--help" => return Ok(None),
            other => return Err(format!("unrecognized argument {other:?}")),
        }
    }
    Ok(Some(parsed))
}

fn main() -> anyhow::Result<()> {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(Some(args)) => args,
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
    //
    // `--tui` reuses this unchanged: `tui::run` also occupies the main
    // thread and also only ever `tokio::spawn`s (never `block_on`s), so the
    // same entered-runtime arrangement serves both front-ends.
    let tokio_rt = tokio::runtime::Runtime::new().expect("failed to start tokio runtime");
    let _tokio_guard = tokio_rt.enter();

    if args.tui {
        // Must happen *instead of* the stdout subscriber below: the TUI owns
        // the terminal, and log lines written to stdout would corrupt it.
        let log_path = args.log_file.unwrap_or_else(tui::default_log_path);
        tui::init_logging(&log_path)?;
        return tui::run(tui::TuiOptions {
            join_string: args.join,
            display_name: args.name,
            log_path,
        });
    }

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

    fn parse(args: &[&str]) -> Result<Option<CliArgs>, String> {
        parse_args(args.iter().map(|s| s.to_string()))
    }

    /// The load-bearing guarantee of this whole feature: no flags must still
    /// mean "the egui window, exactly as before".
    #[test]
    fn no_arguments_keeps_the_gui_default() {
        let args = parse(&[]).unwrap().unwrap();
        assert!(!args.tui);
        assert!(args.join.is_none() && args.name.is_none() && args.log_file.is_none());
    }

    #[test]
    fn tui_flag_and_values_parse() {
        let args = parse(&["--tui", "--join", "h:1|r|k", "--name", "agent", "--log-file", "/tmp/x.log"])
            .unwrap()
            .unwrap();
        assert!(args.tui);
        assert_eq!(args.join.as_deref(), Some("h:1|r|k"));
        assert_eq!(args.name.as_deref(), Some("agent"));
        assert_eq!(args.log_file, Some(PathBuf::from("/tmp/x.log")));
    }

    #[test]
    fn help_and_errors_are_distinguishable() {
        assert!(parse(&["--help"]).unwrap().is_none());
        assert!(parse(&["--join"]).is_err());
        assert!(parse(&["--nope"]).is_err());
    }
}
