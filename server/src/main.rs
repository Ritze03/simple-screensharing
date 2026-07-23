//! Entry point for the simple-screensharing relay server.
//!
//! Sets up file logging (the TUI owns the terminal, so nothing goes to
//! stdout/stderr), starts the QUIC listener as a tokio task, runs the
//! ratatui admin TUI on its own OS thread, and shuts down cleanly when
//! the TUI quits ('q') or the process receives Ctrl-C.

mod protocol;
mod relay;
mod room;
mod tui;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;

use crate::room::Registry;

/// Directory (relative to the working directory) where log files are written.
const LOG_DIR: &str = "logs";
/// Base filename for the daily-rolling log file.
const LOG_FILE_PREFIX: &str = "relay.log";
/// Default UDP port the relay listens on if not overridden.
const DEFAULT_PORT: u16 = 4433;

fn init_logging() -> tracing_appender::non_blocking::WorkerGuard {
    let file_appender = tracing_appender::rolling::daily(LOG_DIR, LOG_FILE_PREFIX);
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::fmt()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    guard
}

/// Resolve the listen port: first CLI arg, else `SSSHARE_PORT` env var,
/// else [`DEFAULT_PORT`].
fn resolve_port() -> Result<u16> {
    if let Some(arg) = std::env::args().nth(1) {
        return arg
            .parse()
            .with_context(|| format!("invalid port argument: {arg:?}"));
    }
    if let Ok(env) = std::env::var("SSSHARE_PORT") {
        return env
            .parse()
            .with_context(|| format!("invalid SSSHARE_PORT: {env:?}"));
    }
    Ok(DEFAULT_PORT)
}

#[tokio::main]
async fn main() -> Result<()> {
    // Keep the guard alive for the lifetime of main() so buffered log lines
    // are flushed on exit. Logging goes to a file (not stdout/stderr) so it
    // never collides with the ratatui TUI, which owns the terminal.
    let _log_guard = init_logging();

    tracing::info!(
        "simple-screensharing-server starting up (v{})",
        env!("CARGO_PKG_VERSION")
    );

    let port = resolve_port()?;
    let listen_addr: SocketAddr = (std::net::Ipv6Addr::UNSPECIFIED, port).into();

    let registry = Arc::new(Registry::new(listen_addr));

    let relay_registry = registry.clone();
    let relay_task = tokio::spawn(async move {
        if let Err(e) = relay::run(relay_registry, listen_addr).await {
            tracing::error!("relay listener exited with error: {e:#}");
        }
    });

    // The TUI owns crossterm/terminal state and does blocking I/O, so it
    // runs on a plain OS thread rather than a tokio task; it talks to the
    // shared registry via `blocking_read`/`blocking_lock`, which is only
    // safe outside a tokio async execution context (true for a bare
    // std::thread).
    let tui_registry = registry.clone();
    let tui_thread = std::thread::spawn(move || tui::run(tui_registry));

    // Wait for whichever comes first: the TUI quitting, Ctrl-C, or the
    // relay listener dying unexpectedly.
    tokio::select! {
        join = tokio::task::spawn_blocking(move || tui_thread.join()) => {
            match join {
                Ok(Ok(Ok(()))) => tracing::info!("TUI exited normally, shutting down"),
                Ok(Ok(Err(e))) => tracing::error!("TUI exited with error: {e:#}"),
                Ok(Err(_)) => tracing::error!("TUI thread panicked"),
                Err(e) => tracing::error!("failed to join TUI thread: {e:#}"),
            }
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("received Ctrl-C, shutting down");
        }
    }

    relay_task.abort();

    tracing::info!("shutdown complete");
    Ok(())
}
