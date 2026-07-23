//! Entry point for the simple-screensharing relay server.
//!
//! This binary sets up logging (to a file, since the TUI will eventually
//! own the terminal), starts the tokio runtime, and will go on to start
//! the QUIC listener and the ratatui admin interface.

mod protocol;
mod relay;
mod room;
mod tui;

use anyhow::Result;
use tracing_subscriber::EnvFilter;

/// Directory (relative to the working directory) where log files are written.
const LOG_DIR: &str = "logs";
/// Base filename for the daily-rolling log file.
const LOG_FILE_PREFIX: &str = "relay.log";

fn init_logging() -> tracing_appender::non_blocking::WorkerGuard {
    let file_appender = tracing_appender::rolling::daily(LOG_DIR, LOG_FILE_PREFIX);
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::fmt()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    guard
}

#[tokio::main]
async fn main() -> Result<()> {
    // Keep the guard alive for the lifetime of main() so buffered log lines
    // are flushed on exit. Logging goes to a file (not stdout/stderr) so it
    // never collides with the ratatui TUI, which owns the terminal.
    let _log_guard = init_logging();

    tracing::info!("simple-screensharing-server starting up (v{})", env!("CARGO_PKG_VERSION"));

    // TODO: start QUIC listener (relay::), start TUI (tui::)

    tracing::info!("startup complete, nothing to do yet — exiting cleanly");

    Ok(())
}
