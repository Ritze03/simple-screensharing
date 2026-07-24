//! TEST-ONLY: a real, whole-system end-to-end integration test proving the
//! pipeline works together for real — network, crypto, fragmentation,
//! relay routing/subscription, decode — using two real
//! [`crate::net::ScreenshareClient`] connections talking to a real relay
//! server child process, with `videotestsrc` standing in for the
//! interactive portal/`pipewiresrc` capture source (see
//! [`crate::capture::start_sharing_from_test_source`] for why that seam
//! exists and why it's needed here instead of the real portal flow).
//!
//! # Why this lives in `src/`, not a `tests/` directory
//!
//! This crate is bin-only (`src/main.rs`, no `src/lib.rs`), so there is no
//! library target for an external `tests/*.rs` integration test to link
//! against — such a test would have no way to reach `capture`, `net`,
//! `decode`, etc. (all private modules of the binary crate). Adding a
//! `lib.rs` purely to support one test would be a bigger structural change
//! than this task's scope. Instead this follows the same convention every
//! other module in this crate already uses (`#[cfg(test)] mod tests { ... }`
//! colocated in the source file), just split into its own file for size and
//! included from `main.rs` as `#[cfg(test)] mod end_to_end_test;`. It has
//! full access to every internal API it needs as a normal side effect of
//! being compiled into the same crate for `cargo test`.
//!
//! # Process model: real server child process, in-process client tasks
//!
//! - The **relay server** runs as a real, separate OS child process (the
//!   actual `simple-screensharing-server` binary from the sibling repo) —
//!   this is the piece whose real network behavior (accepting two distinct
//!   QUIC connections, routing datagrams between them) is most important to
//!   prove for real, so it gets real process isolation.
//! - **Client A ("sharer")** and **Client B ("viewer")** run as two
//!   concurrent `tokio` tasks within this one test's async runtime, each
//!   with its own real `ScreenshareClient::connect()` (its own QUIC
//!   connection, its own `quinn::Endpoint`, its own nonce sequence) talking
//!   to the real server process above. Per this task's own guidance: the
//!   thing being proven is that the network/crypto/relay/decode path works
//!   correctly end to end, not that two client *processes* can coexist on
//!   one OS (nothing in `net.rs`/`capture.rs`/`decode.rs` is
//!   process-global state — no shared statics, no singleton sockets — so
//!   two in-process clients exercise exactly the same code paths two
//!   separate `simple-screensharing-client` processes would). Running them
//!   as two OS processes instead would mean a lot of extra process
//!   plumbing (spawn, stdio/log capture, IPC to hand back decoded frames
//!   for assertion) for no additional coverage.
//!
//! # Why the server is launched via `setsid script -qc ...`, not a plain spawn
//!
//! The server's `main.rs` always starts an interactive `ratatui` admin TUI
//! on its own thread via `ratatui::init()`, which calls crossterm's
//! `enable_raw_mode()` against the process's controlling terminal. Spawned
//! as a normal child process (stdio piped/redirected/null, no controlling
//! tty), that fails with `ENOTTY` ("No such device or address"), the TUI
//! thread panics, and `main()`'s `tokio::select!` treats the panicked TUI
//! thread join as a shutdown signal — `relay_task.abort()` runs and the
//! whole process exits within a couple hundred milliseconds, before the
//! relay ever gets a chance to serve anything. This was confirmed
//! empirically against the built release binary while writing this test
//! (`ratatui::init()` panics with
//! `Os { code: 6, kind: Uncategorized, message: "No such device or address" }`).
//! There is no headless/no-TUI flag on the server today, and adding one is
//! out of this task's (client-repo) scope. Wrapping the launch in
//! `script -qc` allocates a real pseudo-terminal for the child, which
//! satisfies crossterm's raw-mode requirement without any human ever near
//! it (no interactive input is ever sent), and `setsid` gives the whole
//! thing its own session so it's a clean unit to signal. Also confirmed
//! empirically: killing the `setsid`/`script` wrapper reliably takes the
//! actual relay server down with it too (closing the pty master on the
//! wrapper's exit delivers `SIGHUP` to the server, which isn't handled and
//! terminates it by default) — [`ServerGuard`]'s `pkill` fallback is belt
//! and suspenders for the rare case that doesn't happen fast enough.
//!
//! # Running this test
//!
//! Requires: the server repo built at
//! `~/Documents/Programming/simple-screensharing-server` (release or debug
//! profile), a working GStreamer install (same requirement every other
//! `capture.rs`/`decode.rs` test already has), and the `script`/`setsid`
//! utilities (`util-linux`; present on essentially every Linux desktop).
//! Not runnable in a typical unattended CI container without those, and
//! it's slower/heavier than a unit test, so it's `#[ignore]`d like this
//! crate's other live/hardware-dependent tests
//! (`capture::tests::live_portal_probe`,
//! `capture::tests::vaapi_pipeline_links_and_produces_encoded_output`):
//!
//! ```sh
//! cargo test end_to_end_test:: -- --ignored --nocapture
//! ```

use std::net::UdpSocket;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};

use crate::capture;
use crate::decode::StreamDecoder;
use crate::net::{ClientEvent, ScreenshareClient};
use crate::protocol::{PresenceEvent, RoomId};

/// Owns the spawned relay server child process (really, the `setsid
/// script -qc ...` wrapper around it — see module docs). Killed on drop —
/// success, assertion failure, or panic — so a failing test run never
/// leaves an orphaned server listening on the test port.
struct ServerGuard {
    child: Child,
    port: u16,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Belt-and-suspenders backstop in case the pty-close SIGHUP path
        // (see module docs) doesn't reach the actual relay process quickly
        // enough: directly hunt down anything still listening with this
        // exact invocation. Best-effort; failures here are not fatal to the
        // test (they'd just mean `pkill` isn't installed or found nothing).
        let _ = Command::new("pkill")
            .args(["-f", &format!("simple-screensharing-server {}", self.port)])
            .status();
    }
}

/// Binds an ephemeral UDP port, reads back the OS-assigned port, and drops
/// the socket — giving the real server process a (best-effort, not
/// airtight against a concurrent racer) free port to bind to. This project
/// has no cross-process way to do an atomic "bind and hand off the fd", so
/// this is the same pragmatic "ask the OS for a free port, then reuse the
/// number" pattern used to pick ephemeral ports for external processes in
/// general.
fn pick_free_udp_port() -> Result<u16> {
    let socket = UdpSocket::bind("127.0.0.1:0").context("failed to bind ephemeral probe socket")?;
    Ok(socket.local_addr()?.port())
}

/// Locates the built server binary in the sibling checkout this project's
/// repos are fixed to live in (see this task's own instructions / the
/// client repo's README). Prefers a release build (faster to run) but
/// falls back to debug.
fn server_binary_path() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("could not determine home directory"))?;
    let repo = home.join("Documents/Programming/simple-screensharing-server");
    let release = repo.join("target/release/simple-screensharing-server");
    let debug = repo.join("target/debug/simple-screensharing-server");
    if release.is_file() {
        Ok(release)
    } else if debug.is_file() {
        Ok(debug)
    } else {
        Err(anyhow!(
            "server binary not found at {release:?} or {debug:?} — build the server repo first \
             (`cd {repo:?} && cargo build --release`)"
        ))
    }
}

/// Spawns the real relay server binary as a child process listening on
/// `port`. See this module's doc comment ("Why the server is launched via
/// `setsid script -qc ...`") for why this isn't a plain `Command::spawn`.
fn spawn_relay_server(port: u16) -> Result<ServerGuard> {
    let binary = server_binary_path()?;
    // The server writes its `logs/` directory relative to its current
    // working directory (see its `main.rs`'s `LOG_DIR`). Without an
    // explicit `current_dir`, it would inherit *this test process's* cwd
    // (the client repo root) and litter a `logs/` directory there instead
    // of alongside the server binary where it belongs.
    let server_repo_dir = binary
        .parent() // .../target/release or .../target/debug
        .and_then(|p| p.parent()) // .../target
        .and_then(|p| p.parent()) // server repo root
        .ok_or_else(|| anyhow!("could not determine server repo directory from {binary:?}"))?
        .to_path_buf();
    let cmd_string = format!("{} {port}", binary.display());
    let child = Command::new("setsid")
        .args(["script", "-qc", &cmd_string, "/dev/null"])
        .current_dir(&server_repo_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context(
            "failed to spawn relay server child process \
             (requires `setsid` and `script` from util-linux)",
        )?;
    Ok(ServerGuard { child, port })
}

/// Retries [`ScreenshareClient::connect`] with a short backoff, giving a
/// freshly spawned server process time to finish self-signed cert
/// generation and bind its QUIC endpoint before the first real connection
/// attempt lands.
async fn connect_with_retry(
    join_string: &str,
    display_name: &str,
) -> Result<(
    ScreenshareClient,
    crate::net::PresenceSnapshotState,
    tokio::sync::mpsc::UnboundedReceiver<ClientEvent>,
)> {
    let mut last_err = None;
    for _ in 0..40 {
        match ScreenshareClient::connect(join_string, display_name).await {
            Ok(v) => return Ok(v),
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("connect_with_retry: no attempts were made")))
}

/// Simple population-variance-of-luma proxy: mean of (R+G+B)/3 per pixel,
/// then variance around that mean. A flat/all-one-color or all-zero frame
/// (e.g. a corrupt decode) has variance ~0; `videotestsrc`'s `smpte` bar
/// pattern has strong, sustained block-to-block color contrast, so a
/// correctly decoded frame's variance should be large and unambiguous.
fn color_variance(img: &egui::ColorImage) -> f64 {
    let n = img.pixels.len();
    if n == 0 {
        return 0.0;
    }
    let luma = |p: &egui::Color32| (p.r() as f64 + p.g() as f64 + p.b() as f64) / 3.0;
    let mean = img.pixels.iter().map(luma).sum::<f64>() / n as f64;
    img.pixels.iter().map(|p| (luma(p) - mean).powi(2)).sum::<f64>() / n as f64
}

const VIDEO_STREAM_ID: u8 = 1;
const WIDTH: u32 = 320;
const HEIGHT: u32 = 240;

#[tokio::test]
#[ignore = "spawns a real relay server child process and real GStreamer capture/decode \
            pipelines; run explicitly with `cargo test end_to_end_test:: -- --ignored --nocapture`"]
async fn real_capture_encrypt_fragment_relay_decode_round_trip() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();

    let port = pick_free_udp_port().expect("failed to pick a free UDP port for the test relay");
    eprintln!("[e2e] starting relay server on 127.0.0.1:{port}");
    let _server = spawn_relay_server(port).expect("failed to spawn relay server child process");

    let server_addr = format!("127.0.0.1:{port}");
    let room_id: RoomId = rand::random();
    let room_key: [u8; 32] = rand::random();
    let join_string = crate::net::format_join_string(&server_addr, &room_id, &room_key);

    eprintln!("[e2e] connecting client A (sharer)");
    let (client_a, _snapshot_a, events_a) = connect_with_retry(&join_string, "sharer")
        .await
        .expect("client A failed to connect to the relay after retries");
    eprintln!("[e2e] connecting client B (viewer)");
    let (client_b, _snapshot_b, mut events_b) = connect_with_retry(&join_string, "viewer")
        .await
        .expect("client B failed to connect to the relay after retries");

    // Client A never needs to read its own event channel for this test, but
    // it must be kept alive (not dropped) — dropping it would make net.rs's
    // background reader loops treat every `tx.send(...)` as a hard error
    // and shut themselves down, tearing down client A's connection.
    let _events_a_keepalive = events_a;

    let source_fragment = format!(
        "videotestsrc is-live=true pattern=smpte \
         ! video/x-raw,width={WIDTH},height={HEIGHT},framerate=15/1"
    );

    eprintln!("[e2e] starting real capture pipeline on client A (videotestsrc, real encode/encrypt/fragment/send)");
    let capture_handle = capture::start_sharing_from_test_source(&client_a, VIDEO_STREAM_ID, 2000, &source_fragment)
        .await
        .expect("start_sharing_from_test_source failed");

    let owner_conn_id = client_a.conn_id();

    // The relay silently ignores a Subscribe for a stream it doesn't know
    // about yet (see the server's relay.rs), so wait for client B to
    // actually observe the StreamStarted presence delta for A's stream
    // first -- this also proves presence propagation works for real.
    eprintln!("[e2e] waiting for client B to see StreamStarted presence delta");
    let saw_stream_started = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match events_b.recv().await {
                Some(ClientEvent::PresenceDelta(PresenceEvent::StreamStarted(info)))
                    if info.owner == owner_conn_id && info.stream_id == VIDEO_STREAM_ID =>
                {
                    return true;
                }
                Some(_) => continue,
                None => return false,
            }
        }
    })
    .await;
    assert_eq!(
        saw_stream_started,
        Ok(true),
        "client B never observed client A's StreamStarted presence delta over the real relay"
    );

    eprintln!("[e2e] client B subscribing to client A's stream");
    client_b
        .subscribe(owner_conn_id, VIDEO_STREAM_ID)
        .await
        .expect("client B: subscribe() failed");

    let mut decoder = StreamDecoder::new(VIDEO_STREAM_ID).expect("failed to build StreamDecoder");

    eprintln!("[e2e] waiting for decoded frames to arrive on client B");
    let mut frames: Vec<egui::ColorImage> = Vec::new();
    let mut media_frames_received: u64 = 0;
    let collect_result = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            tokio::select! {
                event = events_b.recv() => {
                    match event {
                        Some(ClientEvent::MediaFrame(frame))
                            if frame.conn_id == owner_conn_id && frame.stream_id == VIDEO_STREAM_ID =>
                        {
                            media_frames_received += 1;
                            decoder.feed(&frame);
                        }
                        Some(_) => {}
                        None => break,
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(30)) => {}
            }
            if let Some(img) = decoder.latest_frame() {
                frames.push(img);
                if frames.len() >= 5 {
                    break;
                }
            }
        }
    })
    .await;

    eprintln!(
        "[e2e] stopping: {media_frames_received} decrypted media datagram-frames received on client B, \
         {} decoded ColorImage frame(s) collected",
        frames.len()
    );

    // Teardown first (best-effort, logged not asserted) so a failing
    // assertion below still leaves the pipeline/connections/server cleanly
    // stopped -- `_server`'s Drop handles the server process regardless.
    if let Err(e) = capture_handle.stop().await {
        tracing::warn!("[e2e] capture_handle.stop() reported an error (non-fatal to the test): {e:#}");
    }
    let _ = client_a.leave().await;
    let _ = client_b.leave().await;

    assert!(
        collect_result.is_ok(),
        "timed out waiting for 5 decoded frames on client B (got {} media frames, {} decoded)",
        media_frames_received,
        frames.len()
    );
    assert!(
        media_frames_received > 0,
        "client B never received a single decrypted media frame over the real relay"
    );
    assert!(!frames.is_empty(), "no decoded ColorImage frames were produced");

    for (i, img) in frames.iter().enumerate() {
        assert_eq!(
            img.size,
            [WIDTH as usize, HEIGHT as usize],
            "decoded frame {i} has unexpected dimensions"
        );
    }

    let last = frames.last().expect("frames is non-empty, checked above");
    let variance = color_variance(last);
    eprintln!("[e2e] last decoded frame color variance: {variance:.1}");
    assert!(
        variance > 100.0,
        "decoded frame looks flat/corrupt (luma variance {variance:.1}, expected the smpte \
         color-bar pattern to show strong contrast)"
    );

    eprintln!(
        "[e2e] PASS: {} frames decoded, {}x{}, last-frame variance {:.1}",
        frames.len(),
        WIDTH,
        HEIGHT,
        variance
    );
}
