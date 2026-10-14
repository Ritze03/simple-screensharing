//! Headless test-pattern sender: joins a room and shares a `videotestsrc`
//! through the production capture -> encode -> encrypt -> fragment -> send
//! path. No display, no portal, no audio: the source for cross-platform
//! checks (a Linux sender for a Windows viewer, see `features/windows.md`)
//! and for any run that must not touch a real desktop.
//!
//! ```text
//! cargo run --release -p simple-screensharing-client --features test-source \
//!     --example test_sender -- <join-link> \
//!     [--name sender] [--size 1920x1080] [--fps 60] [--kbps 8000] \
//!     [--pattern ball|smpte|zone-plate|snow] [--seconds 0]
//! ```
//!
//! `--seconds 0` (the default) runs until the process is killed. The encoder
//! is whatever `capture.rs` picks for this machine (VA hardware, else
//! `x264enc`), exactly as in the app. Behind the `test-source` cargo feature
//! because it uses `capture::start_sharing_from_test_source`, a seam that does
//! not exist in a normal build.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use simple_screensharing_client::{capture, net};

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let mut args = std::env::args().skip(1);
    let join = args.next().context("usage: test_sender <join-link> [--name N] [--size WxH] [--fps N] [--kbps N] [--pattern P] [--seconds N]")?;
    let (mut name, mut size, mut fps, mut kbps, mut pattern, mut seconds) =
        ("test-sender".to_string(), (1920u32, 1080u32), 60u32, 8000u32, "ball".to_string(), 0u64);
    while let Some(flag) = args.next() {
        let mut value = || args.next().with_context(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--name" => name = value()?,
            "--size" => {
                let v = value()?;
                let (w, h) = v.split_once('x').context("--size is WxH")?;
                size = (w.parse()?, h.parse()?);
            }
            "--fps" => fps = value()?.parse()?,
            "--kbps" => kbps = value()?.parse()?,
            "--pattern" => pattern = value()?,
            "--seconds" => seconds = value()?.parse()?,
            other => bail!("unknown argument {other}"),
        }
    }

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let endpoint = net::build_client_endpoint()?;
        let (client, _snapshot, mut events) = net::ScreenshareClient::connect(&endpoint, &join, &name).await.context("connect to the relay")?;
        tracing::info!("connected as conn {:?}", client.conn_id());
        // The client's reader loops stop if nobody drains the event channel.
        tokio::spawn(async move { while events.recv().await.is_some() {} });

        let (w, h) = size;
        let source = format!("videotestsrc is-live=true pattern={pattern} ! video/x-raw,width={w},height={h},framerate={fps}/1");
        let handle = capture::start_sharing_from_test_source(&client, 1, kbps, fps, &source).await.context("start sharing")?;
        tracing::info!("sharing {w}x{h}@{fps} {pattern} at {kbps} kbps; waiting for viewers");

        let started = Instant::now();
        loop {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(1)) => {
                    if seconds > 0 && started.elapsed() >= Duration::from_secs(seconds) { break; }
                }
                _ = tokio::signal::ctrl_c() => break,
            }
        }
        handle.stop().await?;
        client.leave_and_close().await;
        Ok(())
    })
}
