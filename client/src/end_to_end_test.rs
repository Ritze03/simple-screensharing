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
//! `~/GitHubProjects/simple-screensharing/simple-screensharing-server`
//! (release or debug profile), a working GStreamer install (same requirement every other
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
pub(crate) struct ServerGuard {
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
pub(crate) fn pick_free_udp_port() -> Result<u16> {
    let socket = UdpSocket::bind("127.0.0.1:0").context("failed to bind ephemeral probe socket")?;
    Ok(socket.local_addr()?.port())
}

/// Locates the built server binary in the sibling checkout this project's
/// repos are fixed to live in (see this task's own instructions / the
/// client repo's README). Prefers a release build (faster to run) but
/// falls back to debug.
/// Candidate sibling-checkout roots for the relay server repo, in preference
/// order. Two, not one: the workspace originally nested each repo under a
/// `master/` directory and no longer does. Probing both keeps this harness
/// working on either layout instead of failing with a path that simply isn't
/// there any more.
pub(crate) fn server_repo_candidates() -> Result<Vec<PathBuf>> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("could not determine home directory"))?;
    Ok(vec![
        home.join("GitHubProjects/simple-screensharing/simple-screensharing-server"),
        home.join("GitHubProjects/simple-screensharing/master/simple-screensharing-server"),
    ])
}

fn server_binary_path() -> Result<PathBuf> {
    let candidates = server_repo_candidates()?;
    for repo in &candidates {
        for profile in ["release", "debug"] {
            let binary = repo.join(format!("target/{profile}/simple-screensharing-server"));
            if binary.is_file() {
                return Ok(binary);
            }
        }
    }
    Err(anyhow!(
        "server binary not found under any of {candidates:?} (target/release or target/debug) — \
         build the server repo first (`cargo build --release` in it)"
    ))
}

/// Spawns the real relay server binary as a child process listening on
/// `port`. See this module's doc comment ("Why the server is launched via
/// `setsid script -qc ...`") for why this isn't a plain `Command::spawn`.
pub(crate) fn spawn_relay_server(port: u16) -> Result<ServerGuard> {
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
///
/// Builds its own [`crate::net::build_client_endpoint`] and reuses it across
/// every retry, mirroring what the real front-ends do with their one
/// `App`-owned endpoint (see `net.rs`'s "One endpoint, N connections").
pub(crate) async fn connect_with_retry(
    join_string: &str,
    display_name: &str,
) -> Result<(
    ScreenshareClient,
    crate::net::PresenceSnapshotState,
    tokio::sync::mpsc::UnboundedReceiver<ClientEvent>,
)> {
    let endpoint = crate::net::build_client_endpoint()?;
    let mut last_err = None;
    for _ in 0..40 {
        match ScreenshareClient::connect(&endpoint, join_string, display_name).await {
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
    let capture_handle = capture::start_sharing_from_test_source(
        &client_a,
        VIDEO_STREAM_ID,
        2000,
        capture::DEFAULT_FRAMERATE,
        &source_fragment,
    )
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

/// Reproduces (and proves the fix for) the "resubscribe after the encoder
/// paused never resumes" bug: client B watches, stops watching (subscriber
/// count -> 0, encoder pauses via `encoder_valve`), then watches again
/// (fresh subscribe, subscriber count -> 1 again). This drives client A's
/// real `ClientEvent::SubscriberCount`/`ClientEvent::KeyframeRequest`
/// handling exactly the way `ui.rs`'s event loop does (see its
/// `ClientEvent::SubscriberCount`/`ClientEvent::KeyframeRequest` arms) —
/// unlike the round-trip test above, this one actually reads and forwards
/// client A's event channel, since that forwarding is exactly the wiring
/// under test.
#[tokio::test]
#[ignore = "spawns a real relay server child process and real GStreamer capture/decode \
            pipelines; run explicitly with `cargo test end_to_end_test:: -- --ignored --nocapture`"]
async fn resubscribe_after_pause_resumes_encoder_and_frames_flow() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();

    let port = pick_free_udp_port().expect("failed to pick a free UDP port for the test relay");
    eprintln!("[e2e-resub] starting relay server on 127.0.0.1:{port}");
    let _server = spawn_relay_server(port).expect("failed to spawn relay server child process");

    let server_addr = format!("127.0.0.1:{port}");
    let room_id: RoomId = rand::random();
    let room_key: [u8; 32] = rand::random();
    let join_string = crate::net::format_join_string(&server_addr, &room_id, &room_key);

    eprintln!("[e2e-resub] connecting client A (sharer)");
    let (client_a, _snapshot_a, mut events_a) = connect_with_retry(&join_string, "sharer")
        .await
        .expect("client A failed to connect to the relay after retries");
    eprintln!("[e2e-resub] connecting client B (viewer)");
    let (client_b, _snapshot_b, mut events_b) = connect_with_retry(&join_string, "viewer")
        .await
        .expect("client B failed to connect to the relay after retries");

    let source_fragment = format!(
        "videotestsrc is-live=true pattern=smpte \
         ! video/x-raw,width={WIDTH},height={HEIGHT},framerate=15/1"
    );

    eprintln!("[e2e-resub] starting real capture pipeline on client A");
    let capture_handle = capture::start_sharing_from_test_source(
        &client_a,
        VIDEO_STREAM_ID,
        2000,
        capture::DEFAULT_FRAMERATE,
        &source_fragment,
    )
    .await
    .expect("start_sharing_from_test_source failed");

    let owner_conn_id = client_a.conn_id();
    let start = std::time::Instant::now();

    eprintln!("[e2e-resub] waiting for client B to see StreamStarted presence delta");
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

    // Forwards every SubscriberCount/KeyframeRequest client A receives to
    // `capture_handle`, exactly like `ui.rs`'s event loop
    // (`ClientEvent::SubscriberCount`/`ClientEvent::KeyframeRequest` arms) —
    // this is the real wiring under test, so the test must exercise it for
    // real rather than calling `notify_*` directly.
    async fn forward_one_control_event(
        events_a: &mut tokio::sync::mpsc::UnboundedReceiver<ClientEvent>,
        capture_handle: &capture::CaptureHandle,
        start: std::time::Instant,
    ) -> bool {
        match events_a.recv().await {
            Some(ClientEvent::SubscriberCount { stream_id, count }) if stream_id == VIDEO_STREAM_ID => {
                eprintln!("[e2e-resub] t={:?} client A got SubscriberCount({count})", start.elapsed());
                capture_handle.notify_subscriber_count(capture::DestKey(0), count);
                true
            }
            Some(ClientEvent::KeyframeRequest { stream_id }) if stream_id == VIDEO_STREAM_ID => {
                eprintln!("[e2e-resub] t={:?} client A got KeyframeRequest", start.elapsed());
                capture_handle.notify_keyframe_request(capture::DestKey(0));
                true
            }
            Some(_) => true,
            None => false,
        }
    }

    async fn drain_media_frames_ignored(events_b: &mut tokio::sync::mpsc::UnboundedReceiver<ClientEvent>) {
        // Best-effort background drain so `events_b`'s unbounded channel
        // doesn't grow unbounded while B is unsubscribed and we're not
        // otherwise reading it (mirrors `ui.rs` always draining its event
        // channel every frame regardless of what's currently displayed).
        while events_b.recv().await.is_some() {}
    }

    eprintln!("[e2e-resub] phase 1: client B subscribing (first time)");
    client_b
        .subscribe(owner_conn_id, VIDEO_STREAM_ID)
        .await
        .expect("client B: first subscribe() failed");

    let mut decoder = StreamDecoder::new(VIDEO_STREAM_ID).expect("failed to build StreamDecoder");
    let mut initial_frames: u64 = 0;
    let mut first_decoded_at: Option<std::time::Duration> = None;

    let phase1 = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            tokio::select! {
                ok = forward_one_control_event(&mut events_a, &capture_handle, start) => {
                    if !ok { break; }
                }
                event = events_b.recv() => {
                    match event {
                        Some(ClientEvent::MediaFrame(frame))
                            if frame.conn_id == owner_conn_id && frame.stream_id == VIDEO_STREAM_ID =>
                        {
                            initial_frames += 1;
                            decoder.feed(&frame);
                        }
                        Some(_) => {}
                        None => break,
                    }
                }
            }
            if decoder.latest_frame().is_some() {
                first_decoded_at.get_or_insert(start.elapsed());
                if initial_frames >= 5 {
                    break;
                }
            }
        }
    })
    .await;
    eprintln!(
        "[e2e-resub] phase 1 done: {initial_frames} media frames, first decoded frame at {:?}",
        first_decoded_at
    );
    assert!(phase1.is_ok(), "phase 1 (initial subscribe) timed out waiting for frames");
    assert!(first_decoded_at.is_some(), "phase 1 never decoded a single frame");

    eprintln!("[e2e-resub] phase 2: client B unsubscribing (subscriber count should drop to 0, encoder pauses)");
    client_b
        .unsubscribe(owner_conn_id, VIDEO_STREAM_ID)
        .await
        .expect("client B: unsubscribe() failed");

    // Give the relay time to notify A and the pump loop time to act on it,
    // then confirm the pause was actually observed (not just "should have
    // happened") by forwarding the real SubscriberCount(0) event.
    let saw_pause = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events_a.recv().await {
                Some(ClientEvent::SubscriberCount { stream_id, count: 0 }) if stream_id == VIDEO_STREAM_ID => {
                    eprintln!("[e2e-resub] t={:?} client A got SubscriberCount(0)", start.elapsed());
                    capture_handle.notify_subscriber_count(capture::DestKey(0), 0);
                    return true;
                }
                Some(ClientEvent::SubscriberCount { stream_id, count }) if stream_id == VIDEO_STREAM_ID => {
                    eprintln!("[e2e-resub] t={:?} client A got unexpected SubscriberCount({count}) while waiting for 0", start.elapsed());
                    capture_handle.notify_subscriber_count(capture::DestKey(0), count);
                }
                Some(_) => {}
                None => return false,
            }
        }
    })
    .await;
    assert_eq!(saw_pause, Ok(true), "client A never observed SubscriberCount(0) after client B unsubscribed");

    // Drop B's decoder — a real re-watch builds a brand new one (see
    // `ui.rs`'s `toggle_watch`), and the fresh decoder's `seen_keyframe`
    // gate is exactly what makes this repro sensitive to the resume bug (no
    // keyframe ever arriving => stuck on "waiting for first decoded frame"
    // forever, matching the reported symptom).
    drop(decoder);

    // Simulate a real gap between stopping and re-watching, at the reported
    // live-test timescale (~35s) rather than a token 2s gap.
    //
    // `Why the length is load-bearing, not padding:` this test ran green at
    // 2s for the whole life of the bug it is named after. The failure is
    // *proportional to the pause* — the branch's `videorate` replayed the
    // gap as duplicated stale frames (see `capture.rs`'s
    // `BRANCH_MAX_DUPLICATION_TIME_NS`), so a 2s pause cost 2s of replay,
    // which is indistinguishable from a healthy resume. At 35s the same
    // mechanism produced ~1000 stale frames and the viewer never caught up.
    // Shortening this back to "keep the suite quick" would silently retire
    // the only end-to-end coverage of that bug.
    let pause_secs: u64 = std::env::var("E2E_PAUSE_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(35);
    eprintln!("[e2e-resub] pausing for {pause_secs}s before resubscribe");
    tokio::time::sleep(Duration::from_secs(pause_secs)).await;

    eprintln!("[e2e-resub] phase 3: client B re-subscribing (should resume encoder + get a fresh keyframe promptly)");
    let resubscribe_at = start.elapsed();
    client_b
        .subscribe(owner_conn_id, VIDEO_STREAM_ID)
        .await
        .expect("client B: second subscribe() failed");

    let mut decoder2 = StreamDecoder::new(VIDEO_STREAM_ID).expect("failed to build second StreamDecoder");
    let mut resumed_frames: u64 = 0;
    let mut first_resumed_decode_at: Option<std::time::Duration> = None;
    let mut resumed_images: Vec<egui::ColorImage> = Vec::new();

    let phase3 = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            tokio::select! {
                ok = forward_one_control_event(&mut events_a, &capture_handle, start) => {
                    if !ok { break; }
                }
                event = events_b.recv() => {
                    match event {
                        Some(ClientEvent::MediaFrame(frame))
                            if frame.conn_id == owner_conn_id && frame.stream_id == VIDEO_STREAM_ID =>
                        {
                            resumed_frames += 1;
                            decoder2.feed(&frame);
                        }
                        Some(_) => {}
                        None => break,
                    }
                }
            }
            if let Some(img) = decoder2.latest_frame() {
                first_resumed_decode_at.get_or_insert(start.elapsed());
                resumed_images.push(img);
                if resumed_images.len() >= 3 {
                    break;
                }
            }
        }
    })
    .await;

    eprintln!(
        "[e2e-resub] phase 3 done: {resumed_frames} media frames received after resubscribe (t={:?}), \
         first decoded frame at {:?} (resubscribed at t={:?})",
        start.elapsed(),
        first_resumed_decode_at,
        resubscribe_at
    );

    // Teardown before asserting, so a failing assertion still leaves things
    // cleanly stopped.
    if let Err(e) = capture_handle.stop().await {
        tracing::warn!("[e2e-resub] capture_handle.stop() reported an error (non-fatal to the test): {e:#}");
    }
    let _ = client_a.leave().await;
    let _ = client_b.leave().await;
    let _ = tokio::time::timeout(Duration::from_millis(200), drain_media_frames_ignored(&mut events_b)).await;

    assert!(
        phase3.is_ok(),
        "TIMED OUT waiting for frames to resume after resubscribe (got {resumed_frames} media frames, \
         {} decoded) — this reproduces the reported hang",
        resumed_images.len()
    );
    assert!(
        resumed_frames > 0,
        "client B never received a single media frame after resubscribing — encoder never resumed"
    );
    assert!(
        !resumed_images.is_empty(),
        "client B received media frames after resubscribing but never decoded one \
         (consistent with the encoder never producing a fresh keyframe, so the new \
         decoder's seen_keyframe gate never opens — 'Waiting for first decoded frame' forever)"
    );

    let resume_latency = first_resumed_decode_at.unwrap() - resubscribe_at;
    eprintln!("[e2e-resub] resume latency (resubscribe -> first decoded frame): {resume_latency:?}");
    assert!(
        resume_latency < Duration::from_secs(5),
        "resume took implausibly long ({resume_latency:?}) — matches the reported \
         'never actually resumes' symptom"
    );

    for (i, img) in resumed_images.iter().enumerate() {
        let variance = color_variance(img);
        eprintln!("[e2e-resub] resumed frame {i} color variance: {variance:.1}");
        assert!(
            variance > 100.0,
            "resumed decoded frame {i} looks flat/corrupt (luma variance {variance:.1}) — \
             matches the reported flat-grey-frame symptom"
        );
    }

    eprintln!("[e2e-resub] PASS: encoder resumed and frames flowed within {resume_latency:?} of resubscribe");
}

/// Same real server/client/pipeline setup as
/// [`resubscribe_after_pause_resumes_encoder_and_frames_flow`], but drives
/// *several* pause/resume cycles back to back (a real long-lived sharing
/// session would see many subscribe/unsubscribe transitions, not just one) —
/// checking whether a bug only surfaces on the second or later resume (e.g.
/// state left over from the first cycle: a stale `paused` flag, a debounced
/// keyframe request, an encoder-internal GOP/timestamp assumption that only
/// breaks after having already been forced once).
#[tokio::test]
#[ignore = "spawns a real relay server child process and real GStreamer capture/decode \
            pipelines; run explicitly with `cargo test end_to_end_test:: -- --ignored --nocapture`"]
async fn repeated_pause_resume_cycles_keep_resuming() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();

    let port = pick_free_udp_port().expect("failed to pick a free UDP port for the test relay");
    eprintln!("[e2e-cycles] starting relay server on 127.0.0.1:{port}");
    let _server = spawn_relay_server(port).expect("failed to spawn relay server child process");

    let server_addr = format!("127.0.0.1:{port}");
    let room_id: RoomId = rand::random();
    let room_key: [u8; 32] = rand::random();
    let join_string = crate::net::format_join_string(&server_addr, &room_id, &room_key);

    let (client_a, _snapshot_a, mut events_a) = connect_with_retry(&join_string, "sharer")
        .await
        .expect("client A failed to connect to the relay after retries");
    let (client_b, _snapshot_b, mut events_b) = connect_with_retry(&join_string, "viewer")
        .await
        .expect("client B failed to connect to the relay after retries");

    let source_fragment = format!(
        "videotestsrc is-live=true pattern=smpte \
         ! video/x-raw,width={WIDTH},height={HEIGHT},framerate=15/1"
    );

    let capture_handle = capture::start_sharing_from_test_source(
        &client_a,
        VIDEO_STREAM_ID,
        2000,
        capture::DEFAULT_FRAMERATE,
        &source_fragment,
    )
    .await
    .expect("start_sharing_from_test_source failed");

    let owner_conn_id = client_a.conn_id();
    let start = std::time::Instant::now();

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
    assert_eq!(saw_stream_started, Ok(true), "client B never observed StreamStarted");

    async fn forward_one_control_event(
        events_a: &mut tokio::sync::mpsc::UnboundedReceiver<ClientEvent>,
        capture_handle: &capture::CaptureHandle,
        start: std::time::Instant,
        tag: &str,
    ) -> bool {
        match events_a.recv().await {
            Some(ClientEvent::SubscriberCount { stream_id, count }) if stream_id == VIDEO_STREAM_ID => {
                eprintln!("[e2e-cycles][{tag}] t={:?} client A got SubscriberCount({count})", start.elapsed());
                capture_handle.notify_subscriber_count(capture::DestKey(0), count);
                true
            }
            Some(ClientEvent::KeyframeRequest { stream_id }) if stream_id == VIDEO_STREAM_ID => {
                eprintln!("[e2e-cycles][{tag}] t={:?} client A got KeyframeRequest", start.elapsed());
                capture_handle.notify_keyframe_request(capture::DestKey(0));
                true
            }
            Some(_) => true,
            None => false,
        }
    }

    const CYCLES: u32 = 5;
    for cycle in 1..=CYCLES {
        let tag = format!("cycle{cycle}");
        eprintln!("[e2e-cycles][{tag}] subscribing");
        client_b
            .subscribe(owner_conn_id, VIDEO_STREAM_ID)
            .await
            .unwrap_or_else(|e| panic!("[{tag}] subscribe failed: {e:#}"));

        let mut decoder = StreamDecoder::new(VIDEO_STREAM_ID).expect("failed to build StreamDecoder");
        let mut media_frames: u64 = 0;
        let mut decoded_at: Option<std::time::Duration> = None;
        let resubscribed_at = start.elapsed();

        let result = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                tokio::select! {
                    ok = forward_one_control_event(&mut events_a, &capture_handle, start, &tag) => {
                        if !ok { break; }
                    }
                    event = events_b.recv() => {
                        match event {
                            Some(ClientEvent::MediaFrame(frame))
                                if frame.conn_id == owner_conn_id && frame.stream_id == VIDEO_STREAM_ID =>
                            {
                                media_frames += 1;
                                decoder.feed(&frame);
                            }
                            Some(_) => {}
                            None => break,
                        }
                    }
                }
                if decoder.latest_frame().is_some() {
                    decoded_at.get_or_insert(start.elapsed());
                    if media_frames >= 3 {
                        break;
                    }
                }
            }
        })
        .await;

        eprintln!(
            "[e2e-cycles][{tag}] done: {media_frames} media frames, first decode at {decoded_at:?} \
             (resubscribed at {resubscribed_at:?})"
        );
        assert!(result.is_ok(), "[{tag}] TIMED OUT waiting for frames after subscribe (got {media_frames})");
        assert!(decoded_at.is_some(), "[{tag}] never decoded a frame after subscribe");

        eprintln!("[e2e-cycles][{tag}] unsubscribing");
        client_b
            .unsubscribe(owner_conn_id, VIDEO_STREAM_ID)
            .await
            .unwrap_or_else(|e| panic!("[{tag}] unsubscribe failed: {e:#}"));

        let saw_pause = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match events_a.recv().await {
                    Some(ClientEvent::SubscriberCount { stream_id, count: 0 }) if stream_id == VIDEO_STREAM_ID => {
                        capture_handle.notify_subscriber_count(capture::DestKey(0), 0);
                        return true;
                    }
                    Some(ClientEvent::SubscriberCount { stream_id, count }) if stream_id == VIDEO_STREAM_ID => {
                        capture_handle.notify_subscriber_count(capture::DestKey(0), count);
                    }
                    Some(_) => {}
                    None => return false,
                }
            }
        })
        .await;
        assert_eq!(saw_pause, Ok(true), "[{tag}] never observed SubscriberCount(0) after unsubscribe");

        drop(decoder);
        tokio::time::sleep(Duration::from_millis(800)).await;
    }

    if let Err(e) = capture_handle.stop().await {
        tracing::warn!("[e2e-cycles] capture_handle.stop() reported an error (non-fatal): {e:#}");
    }
    let _ = client_a.leave().await;
    let _ = client_b.leave().await;

    eprintln!("[e2e-cycles] PASS: all {CYCLES} pause/resume cycles resumed successfully");
}

/// **The real production share path, against a real portal and a real relay.**
///
/// Everything above this uses `start_sharing_from_test_source`. This one calls
/// the actual `capture::start_sharing` a "Start a Stream" click calls: real
/// xdg-desktop-portal session, real `pipewiresrc`, real announce, real
/// per-destination pump, real relay. It then walks the three things this
/// stage's bug reports were about — the first destination coming up at all,
/// a *second* dynamic branch being added to the live tee (fan-out), and a
/// destination resuming after a long subscriber-less pause.
///
/// `Why `flavor = "multi_thread"`:` this is the one test in this file driving
/// a *full-screen* capture, and its per-capture background work (self-preview
/// RGB conversion at 12fps on 1920x1080 buffers, preview JPEG encoding) is
/// heavy enough to starve `#[tokio::test]`'s default current-thread runtime —
/// the test's own timers stop firing and it hangs. The real app runs on a
/// multi-thread runtime (`main.rs`'s `tokio::runtime::Runtime::new()`), so
/// this is also the faithful configuration, not just the working one.
///
/// `#[ignore]`d: needs a real desktop session with a portal backend.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "opens a real xdg-desktop-portal screencast session and spawns a real relay server"]
async fn real_portal_share_fans_out_and_resumes() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();

    let port = pick_free_udp_port().expect("port");
    let _server = spawn_relay_server(port).expect("relay");
    let server_addr = format!("127.0.0.1:{port}");
    let room_a: RoomId = rand::random();
    let key_a: [u8; 32] = rand::random();
    let join_a = crate::net::format_join_string(&server_addr, &room_a, &key_a);
    let room_b: RoomId = rand::random();
    let key_b: [u8; 32] = rand::random();
    let join_b = crate::net::format_join_string(&server_addr, &room_b, &key_b);

    let (sharer_a, _s, mut events_a) = connect_with_retry(&join_a, "sharer").await.expect("sharer joins room A");
    let (sharer_b, _s, _e) = connect_with_retry(&join_b, "sharer").await.expect("sharer joins room B");
    let (viewer, _s, mut events_v) = connect_with_retry(&join_a, "viewer").await.expect("viewer joins room A");

    eprintln!("[real] calling capture::start_sharing (real portal)");
    let mut handle = match capture::start_sharing(&sharer_a, 1, capture::DestKey(0), 4_000, 30).await {
        Ok(h) => h,
        Err(e) => {
            eprintln!("skipping: no portal screencast session available here ({e:#})");
            return;
        }
    };
    eprintln!("[real] share started");

    // 1. The first destination must actually encode, sustained — not just
    //    survive `start_sharing` and then go silent (the shape the
    //    RECONFIGURE bug produced downstream).
    let mut best = 0;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        best = best.max(handle.encoder_fps(capture::DestKey(0)));
    }
    eprintln!("[real] destination 0 encoder fps: {best}");
    assert!(best > 0, "the first destination of a real portal share never encoded a frame");

    // 2. Fan out into a second room: a *second* dynamic branch on the live
    //    tee, which is exactly the operation that used to kill the source.
    let params = handle
        .destination_runtime(capture::DestKey(0))
        .expect("destination 0 runtime")
        .lock()
        .unwrap()
        .params();
    let rx = handle.spawn_add_destination(&sharer_b, 1, capture::DestKey(1), params);
    let second = tokio::time::timeout(Duration::from_secs(30), rx)
        .await
        .expect("fan-out should not hang")
        .expect("fan-out channel")
        .expect("fan-out should succeed");
    handle.register_destination(second);

    let mut best_a = 0;
    let mut best_b = 0;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        best_a = best_a.max(handle.encoder_fps(capture::DestKey(0)));
        best_b = best_b.max(handle.encoder_fps(capture::DestKey(1)));
    }
    eprintln!("[real] after fan-out: dest0={best_a} fps, dest1={best_b} fps");
    assert!(best_a > 0, "adding a second destination killed the first one's frames");
    assert!(best_b > 0, "the fanned-out destination never encoded a frame");

    // 3. Pause destination 0 for a long time, then resume it: it must come
    //    back at the live edge rather than replaying the pause.
    handle.notify_subscriber_count(capture::DestKey(0), 0);
    tokio::time::sleep(Duration::from_secs(20)).await;
    handle.notify_subscriber_count(capture::DestKey(0), 1);
    let resumed_at = std::time::Instant::now();
    let mut resumed = 0;
    while resumed_at.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(Duration::from_millis(200)).await;
        resumed = handle.encoder_fps(capture::DestKey(0));
        if resumed > 0 {
            break;
        }
    }
    eprintln!("[real] resumed after a 20s pause in {:?} at {resumed} fps", resumed_at.elapsed());
    assert!(resumed > 0, "a destination paused for 20s never resumed encoding");
    assert!(
        resumed_at.elapsed() < Duration::from_secs(5),
        "resuming took {:?} — the pause is being replayed rather than skipped",
        resumed_at.elapsed()
    );

    handle.stop().await.expect("the share should stop cleanly");
    drop(viewer);
    let _ = events_a.try_recv();
    let _ = events_v.try_recv();
}

/// TEMP DIAGNOSTIC (bug investigation): the real-portal counterpart of
/// `late_subscriber_after_unwatched_period_gets_a_real_keyframe` -- same
/// scenario (sharer broadcasts to an empty room past one GOP interval, then
/// a first-ever subscriber joins), but via `capture::start_sharing`'s real
/// `xdg-desktop-portal` ScreenCast session instead of `videotestsrc`, for
/// maximum fidelity to the originally reported bug.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "opens a real xdg-desktop-portal screencast session and spawns a real relay server"]
async fn late_subscriber_after_unwatched_period_gets_a_real_keyframe_real_portal() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();

    let port = pick_free_udp_port().expect("port");
    let _server = spawn_relay_server(port).expect("relay");
    let server_addr = format!("127.0.0.1:{port}");
    let room_id: RoomId = rand::random();
    let room_key: [u8; 32] = rand::random();
    let join_string = crate::net::format_join_string(&server_addr, &room_id, &room_key);

    let (sharer, _s, mut events_a) = connect_with_retry(&join_string, "sharer").await.expect("sharer connects");
    let (viewer, _s, mut events_b) = connect_with_retry(&join_string, "viewer").await.expect("viewer connects");
    let owner_conn_id = sharer.conn_id();

    eprintln!("[e2e-late-real] calling capture::start_sharing (real portal)");
    let capture_handle = match capture::start_sharing(&sharer, VIDEO_STREAM_ID, capture::DestKey(0), 8_000, 30).await {
        Ok(h) => h,
        Err(e) => {
            eprintln!("skipping: no portal screencast session available here ({e:#})");
            return;
        }
    };
    eprintln!("[e2e-late-real] share started");

    // Confirm the pipeline is actually healthy before trusting the rest.
    let mut best = 0;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        best = best.max(capture_handle.encoder_fps(capture::DestKey(0)));
    }
    eprintln!("[e2e-late-real] destination 0 encoder fps: {best}");
    assert!(best > 0, "the real-portal share never encoded a frame");

    let start = std::time::Instant::now();

    async fn forward_one_control_event(
        events_a: &mut tokio::sync::mpsc::UnboundedReceiver<ClientEvent>,
        capture_handle: &capture::CaptureHandle,
        start: std::time::Instant,
    ) -> bool {
        match events_a.recv().await {
            Some(ClientEvent::SubscriberCount { stream_id, count }) if stream_id == VIDEO_STREAM_ID => {
                eprintln!("[e2e-late-real] t={:?} sharer got SubscriberCount({count})", start.elapsed());
                capture_handle.notify_subscriber_count(capture::DestKey(0), count);
                true
            }
            Some(ClientEvent::KeyframeRequest { stream_id }) if stream_id == VIDEO_STREAM_ID => {
                eprintln!("[e2e-late-real] t={:?} sharer got KeyframeRequest", start.elapsed());
                capture_handle.notify_keyframe_request(capture::DestKey(0));
                true
            }
            Some(_) => true,
            None => false,
        }
    }

    let unwatched_secs: u64 = std::env::var("E2E_UNWATCHED_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(8);
    eprintln!("[e2e-late-real] letting the sharer broadcast unwatched for {unwatched_secs}s");
    let unwatched_deadline = tokio::time::sleep(Duration::from_secs(unwatched_secs));
    tokio::pin!(unwatched_deadline);
    loop {
        tokio::select! {
            _ = &mut unwatched_deadline => break,
            ok = forward_one_control_event(&mut events_a, &capture_handle, start) => {
                if !ok { break; }
            }
        }
    }

    eprintln!("[e2e-late-real] viewer subscribing for the first time (late)");
    let subscribed_at = start.elapsed();
    viewer.subscribe(owner_conn_id, VIDEO_STREAM_ID).await.expect("viewer: subscribe() failed");

    let mut total_media_frames: u64 = 0;
    let mut keyframe_media_frames: u64 = 0;
    let mut first_keyframe_frame_at: Option<std::time::Duration> = None;
    let mut decoder = StreamDecoder::new(VIDEO_STREAM_ID).expect("failed to build StreamDecoder");
    let mut first_decoded_at: Option<std::time::Duration> = None;

    let watch_secs: u64 = std::env::var("E2E_LATE_WATCH_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(15);
    let deadline = tokio::time::sleep(Duration::from_secs(watch_secs));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => break,
            ok = forward_one_control_event(&mut events_a, &capture_handle, start) => {
                if !ok { break; }
            }
            event = events_b.recv() => {
                match event {
                    Some(ClientEvent::MediaFrame(frame))
                        if frame.conn_id == owner_conn_id && frame.stream_id == VIDEO_STREAM_ID =>
                    {
                        total_media_frames += 1;
                        if frame.keyframe {
                            keyframe_media_frames += 1;
                            first_keyframe_frame_at.get_or_insert(start.elapsed());
                            eprintln!(
                                "[e2e-late-real] t={:?} viewer got a REASSEMBLED keyframe media frame ({} bytes)",
                                start.elapsed(), frame.payload.len()
                            );
                        }
                        decoder.feed(&frame);
                        if decoder.latest_frame().is_some() {
                            first_decoded_at.get_or_insert(start.elapsed());
                        }
                    }
                    Some(_) => {}
                    None => break,
                }
            }
        }
        if first_decoded_at.is_some() && keyframe_media_frames >= 1 {
            break;
        }
    }

    eprintln!(
        "[e2e-late-real] done: {total_media_frames} total media frames, {keyframe_media_frames} carried the \
         keyframe flag, first keyframe frame at {:?} (subscribed at {:?}), first decoded frame at {:?}",
        first_keyframe_frame_at, subscribed_at, first_decoded_at
    );

    if let Err(e) = capture_handle.stop().await {
        tracing::warn!("[e2e-late-real] capture_handle.stop() reported an error (non-fatal): {e:#}");
    }
    drop(viewer);
    let _ = sharer.leave().await;

    assert!(
        total_media_frames > 0,
        "viewer never received a single decrypted media frame after subscribing late"
    );
    assert!(
        keyframe_media_frames > 0,
        "BUG REPRODUCED (real portal): viewer received {total_media_frames} media frames after subscribing \
         late, but NONE carried a real reassembled keyframe"
    );
    assert!(first_decoded_at.is_some(), "viewer never decoded a single frame after subscribing late");

    let latency = first_keyframe_frame_at.unwrap() - subscribed_at;
    eprintln!("[e2e-late-real] keyframe latency (subscribe -> first real keyframe received): {latency:?}");
    assert!(latency < Duration::from_secs(5), "took {latency:?} to get a real keyframe after subscribing");

    eprintln!("[e2e-late-real] PASS: late subscriber received a real keyframe within {latency:?}");
}

/// TEMP DIAGNOSTIC (bug investigation, see `video_server.rs`'s doc comment
/// on the "no real keyframe reaches a late subscriber" bug): reproduces the
/// exact reported scenario — a sharer broadcasting to an *empty* room (zero
/// subscribers, encoder unpaused and running the whole time so its periodic
/// 2s-GOP keyframes are simply going nowhere) for well over one GOP
/// interval, then a *first-ever* subscriber joining. Unlike
/// `resubscribe_after_pause_resumes_encoder_and_frames_flow` (unsubscribe ->
/// resubscribe, which already passes), this never pauses the encoder at
/// all — it's the "was already running for a while" case from the bug
/// report, not the "was paused" case.
///
/// Uses a realistic 1080p resolution/bitrate (not the other tests' small
/// 320x240 test pattern) specifically because a real IDR at that size is
/// dramatically larger (many more wire fragments) than a delta frame, which
/// is exactly the axis the small-resolution tests never stress.
#[tokio::test]
#[ignore = "spawns a real relay server child process and real GStreamer capture/decode \
            pipelines; run explicitly with `cargo test end_to_end_test:: -- --ignored --nocapture`"]
async fn late_subscriber_after_unwatched_period_gets_a_real_keyframe() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();

    let port = pick_free_udp_port().expect("failed to pick a free UDP port for the test relay");
    eprintln!("[e2e-late] starting relay server on 127.0.0.1:{port}");
    let _server = spawn_relay_server(port).expect("failed to spawn relay server child process");

    let server_addr = format!("127.0.0.1:{port}");
    let room_id: RoomId = rand::random();
    let room_key: [u8; 32] = rand::random();
    let join_string = crate::net::format_join_string(&server_addr, &room_id, &room_key);

    eprintln!("[e2e-late] connecting client A (sharer)");
    let (client_a, _snapshot_a, mut events_a) = connect_with_retry(&join_string, "sharer")
        .await
        .expect("client A failed to connect to the relay after retries");
    eprintln!("[e2e-late] connecting client B (viewer)");
    let (client_b, _snapshot_b, mut events_b) = connect_with_retry(&join_string, "viewer")
        .await
        .expect("client B failed to connect to the relay after retries");

    const LATE_WIDTH: u32 = 1920;
    const LATE_HEIGHT: u32 = 1080;
    // TEMP DIAGNOSTIC (bug investigation): override pattern/bitrate to stress
    // the fragmentation/reassembly path harder -- a much larger, harder to
    // compress keyframe (more fragments, more wall-clock time in flight) is
    // exactly the axis a smooth `smpte` bar pattern at default bitrate
    // doesn't stress.
    let pattern = std::env::var("E2E_LATE_PATTERN").unwrap_or_else(|_| "smpte".to_string());
    let bitrate_kbps: u32 =
        std::env::var("E2E_LATE_BITRATE_KBPS").ok().and_then(|s| s.parse().ok()).unwrap_or(capture::DEFAULT_BITRATE_KBPS);
    let source_fragment = format!(
        "videotestsrc is-live=true pattern={pattern} \
         ! video/x-raw,width={LATE_WIDTH},height={LATE_HEIGHT},framerate={}/1",
        capture::DEFAULT_FRAMERATE
    );

    eprintln!("[e2e-late] starting real capture pipeline on client A (1080p, pattern={pattern}, bitrate={bitrate_kbps}kbps)");
    let capture_handle = capture::start_sharing_from_test_source(
        &client_a,
        VIDEO_STREAM_ID,
        bitrate_kbps,
        capture::DEFAULT_FRAMERATE,
        &source_fragment,
    )
    .await
    .expect("start_sharing_from_test_source failed");

    let owner_conn_id = client_a.conn_id();
    let start = std::time::Instant::now();

    eprintln!("[e2e-late] waiting for client B to see StreamStarted presence delta");
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
    assert_eq!(saw_stream_started, Ok(true), "client B never observed StreamStarted");

    // Forwards every SubscriberCount/KeyframeRequest client A receives to
    // `capture_handle`, exactly like `session.rs`'s real event loop
    // (`ClientEvent::SubscriberCount`/`ClientEvent::KeyframeRequest` arms) —
    // this is the real wiring under test. NOT spawned as a detached
    // background task awaited to completion: `events_a` only yields `None`
    // once client A's connection is torn down, which doesn't happen until
    // this test calls `client_a.leave()` at the very end -- awaiting such a
    // task first would deadlock. Instead it's polled inline, alongside
    // `events_b`, only while this test still has anything left to wait for.
    async fn forward_one_control_event(
        events_a: &mut tokio::sync::mpsc::UnboundedReceiver<ClientEvent>,
        capture_handle: &capture::CaptureHandle,
        start: std::time::Instant,
    ) -> bool {
        match events_a.recv().await {
            Some(ClientEvent::SubscriberCount { stream_id, count }) if stream_id == VIDEO_STREAM_ID => {
                eprintln!("[e2e-late] t={:?} client A got SubscriberCount({count})", start.elapsed());
                capture_handle.notify_subscriber_count(capture::DestKey(0), count);
                true
            }
            Some(ClientEvent::KeyframeRequest { stream_id }) if stream_id == VIDEO_STREAM_ID => {
                eprintln!("[e2e-late] t={:?} client A got KeyframeRequest", start.elapsed());
                capture_handle.notify_keyframe_request(capture::DestKey(0));
                true
            }
            Some(_) => true,
            None => false,
        }
    }

    // Let the sharer broadcast to an empty room for well over one GOP
    // interval (2s) before anyone ever subscribes — the exact "subscribes
    // after it's been running a while" scenario from the bug report.
    let unwatched_secs: u64 = std::env::var("E2E_UNWATCHED_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(8);
    eprintln!("[e2e-late] letting client A broadcast unwatched for {unwatched_secs}s ({} GOPs)", unwatched_secs / 2);
    tokio::time::sleep(Duration::from_secs(unwatched_secs)).await;

    eprintln!("[e2e-late] client B subscribing for the first time (late)");
    let subscribed_at = start.elapsed();
    client_b
        .subscribe(owner_conn_id, VIDEO_STREAM_ID)
        .await
        .expect("client B: subscribe() failed");

    let mut total_media_frames: u64 = 0;
    let mut keyframe_media_frames: u64 = 0;
    let mut first_keyframe_frame_at: Option<std::time::Duration> = None;
    let mut decoder = StreamDecoder::new(VIDEO_STREAM_ID).expect("failed to build StreamDecoder");
    let mut first_decoded_at: Option<std::time::Duration> = None;

    let watch_secs: u64 = std::env::var("E2E_LATE_WATCH_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(15);
    let deadline = tokio::time::sleep(Duration::from_secs(watch_secs));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => break,
            ok = forward_one_control_event(&mut events_a, &capture_handle, start) => {
                if !ok { break; }
            }
            event = events_b.recv() => {
                match event {
                    Some(ClientEvent::MediaFrame(frame))
                        if frame.conn_id == owner_conn_id && frame.stream_id == VIDEO_STREAM_ID =>
                    {
                        total_media_frames += 1;
                        if frame.keyframe {
                            keyframe_media_frames += 1;
                            first_keyframe_frame_at.get_or_insert(start.elapsed());
                            eprintln!(
                                "[e2e-late] t={:?} client B got a REASSEMBLED keyframe media frame ({} bytes)",
                                start.elapsed(), frame.payload.len()
                            );
                        }
                        decoder.feed(&frame);
                        if let Some(img) = decoder.latest_frame() {
                            first_decoded_at.get_or_insert(start.elapsed());
                            let _ = img;
                        }
                    }
                    Some(_) => {}
                    None => break,
                }
            }
        }
        if first_decoded_at.is_some() && keyframe_media_frames >= 1 {
            break;
        }
    }

    eprintln!(
        "[e2e-late] done: {total_media_frames} total media frames, {keyframe_media_frames} carried the \
         keyframe flag, first keyframe frame at {:?} (subscribed at {:?}), first decoded frame at {:?}",
        first_keyframe_frame_at, subscribed_at, first_decoded_at
    );

    if let Err(e) = capture_handle.stop().await {
        tracing::warn!("[e2e-late] capture_handle.stop() reported an error (non-fatal): {e:#}");
    }
    let _ = client_a.leave().await;
    let _ = client_b.leave().await;

    assert!(
        total_media_frames > 0,
        "client B never received a single decrypted media frame after subscribing late"
    );
    assert!(
        keyframe_media_frames > 0,
        "BUG REPRODUCED: client B received {total_media_frames} media frames after subscribing late, but \
         NONE of them ever carried a real reassembled keyframe -- this matches the reported symptom \
         (thousands of consecutive frames, zero keyframes) exactly"
    );
    assert!(
        first_decoded_at.is_some(),
        "client B never decoded a single frame after subscribing late (seen_keyframe gate never opened)"
    );

    let latency = first_keyframe_frame_at.unwrap() - subscribed_at;
    eprintln!("[e2e-late] keyframe latency (subscribe -> first real keyframe received): {latency:?}");
    assert!(
        latency < Duration::from_secs(5),
        "took {latency:?} to get a real keyframe after subscribing -- too slow to be the \
         on-demand force_keyframe() mechanism (should be near-immediate), consistent with \
         only the next scheduled 2s GOP boundary ever working, if even that"
    );

    eprintln!("[e2e-late] PASS: late subscriber received a real keyframe within {latency:?}");
}
