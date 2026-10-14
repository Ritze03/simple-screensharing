//! Entry point for the simple-screensharing relay server.
//!
//! Sets up file logging (the TUI owns the terminal, so nothing goes to
//! stdout/stderr), starts the QUIC listener as a tokio task (native and
//! browser/WebTransport peers on the same UDP port, see `relay.rs`), runs the
//! ratatui admin TUI on its own OS thread, and shuts down cleanly when
//! the TUI quits ('q') or the process receives Ctrl-C.

mod cert;
mod protocol;
mod relay;
mod room;
mod tui;

#[cfg(test)]
mod browser_smoke;

#[cfg(test)]
mod transport_tests;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tracing_subscriber::EnvFilter;

use crate::cert::CertSource;
use crate::room::Registry;

/// Directory (relative to the working directory) where log files are written.
const LOG_DIR: &str = "logs";
/// Base filename for the daily-rolling log file.
const LOG_FILE_PREFIX: &str = "relay.log";
/// Directory (relative to the working directory) holding the generated TLS
/// certificate and key, next to `logs/`.
const STATE_DIR: &str = "state";
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

const USAGE: &str = "\
usage: simple-screensharing-server [PORT] [OPTIONS]

  PORT                 UDP port to listen on (default: $SSSHARE_PORT or 4433)
  --cert FILE          PEM certificate chain from a real CA (e.g. Let's Encrypt).
                       With --key, browsers connect without a cert hash and the
                       relay never rotates the certificate.
  --key FILE           PEM private key for --cert
  --state-dir DIR      where the generated self-signed cert is kept so its hash
                       survives restarts (default: ./state)
  --san NAME           extra subject-alternative-name (host or IP) for the
                       generated cert; repeatable
  --cert-validity-secs N, --cert-rotate-before-secs N
                       test hooks: shorten the generated cert's lifetime and
                       the rotation lead time (defaults: 13 days, 2 days)
";

struct Options {
    port: u16,
    cert_source: CertSource,
}

fn parse_args() -> Result<Options> {
    let mut port: Option<u16> = None;
    let (mut cert, mut key) = (None::<PathBuf>, None::<PathBuf>);
    let mut state_dir = PathBuf::from(STATE_DIR);
    let mut sans = Vec::new();
    let mut validity = cert::DEFAULT_VALIDITY;
    let mut rotate_before = cert::DEFAULT_ROTATE_BEFORE;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| {
            args.next()
                .with_context(|| format!("{name} needs a value\n\n{USAGE}"))
        };
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            "--cert" => cert = Some(value("--cert")?.into()),
            "--key" => key = Some(value("--key")?.into()),
            "--state-dir" => state_dir = value("--state-dir")?.into(),
            "--san" => sans.push(value("--san")?),
            "--cert-validity-secs" => {
                validity = Duration::from_secs(value("--cert-validity-secs")?.parse()?)
            }
            "--cert-rotate-before-secs" => {
                rotate_before = Duration::from_secs(value("--cert-rotate-before-secs")?.parse()?)
            }
            flag if flag.starts_with("--") => bail!("unknown option {flag}\n\n{USAGE}"),
            positional => {
                port = Some(
                    positional
                        .parse()
                        .with_context(|| format!("invalid port argument: {positional:?}"))?,
                )
            }
        }
    }

    let port = match port {
        Some(p) => p,
        None => match std::env::var("SSSHARE_PORT") {
            Ok(env) => env
                .parse()
                .with_context(|| format!("invalid SSSHARE_PORT: {env:?}"))?,
            Err(_) => DEFAULT_PORT,
        },
    };
    let cert_source = match (cert, key) {
        (Some(cert), Some(key)) => CertSource::Provided { cert, key },
        (None, None) => CertSource::SelfSigned {
            state_dir: Some(state_dir),
            extra_sans: sans,
            validity,
            rotate_before,
        },
        _ => bail!("--cert and --key must be given together\n\n{USAGE}"),
    };
    Ok(Options { port, cert_source })
}

#[tokio::main]
async fn main() -> Result<()> {
    // Parse before the TUI/logging take over so usage errors reach the terminal.
    let options = parse_args()?;
    // Keep the guard alive for the lifetime of main() so buffered log lines
    // are flushed on exit. Logging goes to a file (not stdout/stderr) so it
    // never collides with the ratatui TUI, which owns the terminal.
    let _log_guard = init_logging();

    tracing::info!(
        "simple-screensharing-server starting up (v{})",
        env!("CARGO_PKG_VERSION")
    );

    let Options { port, cert_source } = options;
    let listen_addr: SocketAddr = (std::net::Ipv6Addr::UNSPECIFIED, port).into();

    let registry = Arc::new(Registry::new(listen_addr));

    let relay_registry = registry.clone();
    let relay_task = tokio::spawn(async move {
        if let Err(e) = relay::run(relay_registry, listen_addr, cert_source).await {
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
