//! Simple Screensharing — Rust desktop client.
//!
//! QUIC networking (`net.rs`), wire protocol (`protocol.rs`), fragmentation
//! (`fragment.rs`), capture (`capture.rs`/`audio.rs`), decode (`decode.rs`),
//! saved rooms (`rooms.rs`), the shared egui theme (`theme.rs`), and the
//! egui UI (`ui/`, shell + contract in `ui/mod.rs`) make up the app.
//!
//! # Front-end
//!
//! This launches the eframe/egui desktop window (`ui/`), the client's only
//! front-end. Only the UI modules (`ui`, `theme`, `widgets`) live in this
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

use std::sync::atomic::{AtomicBool, Ordering};

use simple_screensharing_client::rooms::PresentationMode;

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

/// Windows package: point GStreamer at the plugins shipped next to the exe.
///
/// `package.ps1` (client/packaging/windows) lays out `gst-plugins\` and
/// `gst-plugin-scanner.exe` beside `simple-screensharing-client.exe`. When that
/// folder exists the exe uses it and ignores any GStreamer installed on the
/// machine (a second, different install on PATH would otherwise be mixed in);
/// without it (a developer build) nothing is touched and the system install
/// (`GSTREAMER_1_0_ROOT_MSVC_X86_64` / PATH) is used. Must run before the
/// first GStreamer call, hence first thing in `main`, while still single-threaded.
#[cfg(windows)]
fn use_bundled_gstreamer() {
    let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())) else { return };
    let plugins = dir.join("gst-plugins");
    if !plugins.is_dir() {
        return;
    }
    // SAFETY: called first in `main`, before any other thread exists.
    unsafe {
        std::env::set_var("GST_PLUGIN_SYSTEM_PATH_1_0", &plugins);
        std::env::set_var("GST_PLUGIN_PATH_1_0", &plugins);
        std::env::set_var("GST_PLUGIN_SCANNER_1_0", dir.join("gst-plugin-scanner.exe"));
        // Keep the plugin registry per user, not next to the (possibly read-only) exe.
        if std::env::var_os("GST_REGISTRY_1_0").is_none() {
            if let Some(cache) = dirs::cache_dir() {
                let _ = std::fs::create_dir_all(cache.join("simple-screensharing"));
                std::env::set_var("GST_REGISTRY_1_0", cache.join("simple-screensharing").join("gst-registry.bin"));
            }
        }
    }
}

fn main() -> anyhow::Result<()> {
    #[cfg(windows)]
    use_bundled_gstreamer();
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
    // settings modal is picked up on the *next* launch, not live. See
    // `superdoc/features/presentation-modes.md`.
    let presentation_mode = if std::env::var_os(PRESENT_FALLBACK_ENV).is_some() {
        PresentationMode::AutoVsync
    } else {
        simple_screensharing_client::rooms::AppSettings::load().presentation_mode
    };
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([960.0, 600.0])
            .with_title("Simple Screensharing")
            .with_app_id("simple-screensharing"),
        renderer: eframe::Renderer::Wgpu,
        wgpu_options: wgpu_configuration(presentation_mode),
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

    let result = eframe::run_native("Simple Screensharing", native_options, Box::new(|cc| Ok(Box::new(ui::App::new(cc)))));
    if result.is_err() && PRESENT_MODE_UNSUPPORTED.load(Ordering::Relaxed) {
        // The surface lacks the chosen mode (see `wgpu_configuration`).
        // winit can't build a second event loop in this process, so relaunch
        // with the override that forces `AutoVsync` for this run (the stored
        // setting is left alone).
        tracing::warn!("presentation mode {presentation_mode:?} is not supported by this surface; relaunching with Vsync");
        let mut relaunch = std::process::Command::new(std::env::current_exe()?);
        relaunch.args(std::env::args_os().skip(1)).env(PRESENT_FALLBACK_ENV, "1");
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            return Err(relaunch.exec().into());
        }
        // Windows has no `exec`: run the second instance to completion as a
        // child and exit with its status (the first window is already gone).
        #[cfg(not(unix))]
        {
            let status = relaunch.status()?;
            std::process::exit(status.code().unwrap_or(1));
        }
    }
    result?;
    Ok(())
}

/// Set for a relaunch after the configured presentation mode turned out to
/// be unsupported: this run uses `AutoVsync`.
const PRESENT_FALLBACK_ENV: &str = "SSS_PRESENT_MODE_FALLBACK";

/// Set by the adapter selector when the surface doesn't list the requested
/// present mode.
static PRESENT_MODE_UNSUPPORTED: AtomicBool = AtomicBool::new(false);

/// eframe's wgpu setup for `mode`. For anything but `AutoVsync` (the only
/// mode wgpu falls back from by itself) the adapter selector also checks
/// that the window's surface supports the mode, and fails the setup if not —
/// an explicit unsupported mode is otherwise a fatal `UnsupportedPresentMode`
/// at surface configure. `main` turns that failure into a relaunch with
/// `AutoVsync`. The adapter choice mirrors wgpu's own for the power
/// preference (discrete, then integrated, … ; first listed wins a tie, so
/// Vulkan before GL).
fn wgpu_configuration(mode: PresentationMode) -> eframe::egui_wgpu::WgpuConfiguration {
    use eframe::egui_wgpu::{WgpuConfiguration, WgpuSetup, WgpuSetupCreateNew, wgpu};
    let requested = mode.to_wgpu();
    let mut config = WgpuConfiguration { present_mode: requested, ..Default::default() };
    if mode == PresentationMode::AutoVsync {
        return config;
    }
    let mut setup = WgpuSetupCreateNew::default();
    let power = setup.power_preference;
    setup.native_adapter_selector = Some(std::sync::Arc::new(move |adapters: &[wgpu::Adapter], surface: Option<&wgpu::Surface<'_>>| {
        let rank = |t: wgpu::DeviceType| {
            use wgpu::DeviceType::*;
            match (power, t) {
                (wgpu::PowerPreference::LowPower, IntegratedGpu) => 0,
                (wgpu::PowerPreference::LowPower, DiscreteGpu) => 1,
                (_, DiscreteGpu) => 0,
                (_, IntegratedGpu) => 1,
                (_, Other) => 2,
                (_, VirtualGpu) => 3,
                (_, Cpu) => 4,
            }
        };
        let adapter = adapters
            .iter()
            .filter(|a| surface.is_none_or(|s| a.is_surface_supported(s)))
            .min_by_key(|a| rank(a.get_info().device_type))
            .ok_or_else(|| "no wgpu adapter can present to this window".to_string())?;
        if let Some(surface) = surface {
            let modes = surface.get_capabilities(adapter).present_modes;
            if !modes.contains(&requested) {
                PRESENT_MODE_UNSUPPORTED.store(true, Ordering::Relaxed);
                return Err(format!("present mode {requested:?} unsupported (surface offers {modes:?})"));
            }
        }
        Ok(adapter.clone())
    }));
    config.wgpu_setup = WgpuSetup::CreateNew(setup);
    config
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
