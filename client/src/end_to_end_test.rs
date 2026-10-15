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
//! Requires: the relay built in this workspace
//! (`cargo build -p simple-screensharing-server`; found under
//! `$CARGO_TARGET_DIR` or `<workspace>/target`, see [`server_target_dir`]), a working GStreamer install (same requirement every other
//! `capture.rs`/`decode.rs` test already has), and the `script`/`setsid`
//! utilities (`util-linux`; present on essentially every Linux desktop).
//! Not runnable in a typical unattended CI container without those, and
//! it's slower/heavier than a unit test, so it's `#[ignore]`d like this
//! crate's other live/hardware-dependent tests
//! (`capture::tests::live_portal_probe`,
//! `capture::tests::vaapi_pipeline_links_and_produces_encoded_output`):
//!
//! ```sh
//! cargo test -p simple-screensharing-client --lib -- --ignored end_to_end_test:: --skip real_portal --nocapture
//! ```

use std::net::UdpSocket;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};

use crate::capture;
use crate::capture::destination::VideoParams;
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
    /// Scratch cwd we created (and so remove on drop); `None` if caller-owned.
    work_dir: Option<PathBuf>,
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
        if let Some(dir) = &self.work_dir {
            let _ = std::fs::remove_dir_all(dir);
        }
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

/// Directory holding the built relay binary's `debug/` and `release/` dirs:
/// the runtime `CARGO_TARGET_DIR` if set, otherwise the workspace target dir
/// (`<this crate>/../target`, the monorepo's shared target).
///
/// Why: the repo is one Cargo workspace (`client/` + `server/`), so
/// `simple-screensharing-server` lands in the same target dir as this test
/// binary. `CARGO_TARGET_DIR` is read at runtime (not `env!`) because it is
/// usually exported in the shell that runs `cargo test`, and a shared or
/// redirected target dir is exactly the case where the manifest-relative guess
/// is wrong.
pub(crate) fn server_target_dir() -> PathBuf {
    match std::env::var_os("CARGO_TARGET_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target"),
    }
}

/// Locates the built relay binary under [`server_target_dir`]. If both a
/// release and a debug build exist, the more recently built one wins (avoids
/// silently testing against a stale binary). Does not build it: a missing
/// binary is a clear error telling the user to build it, rather than a hidden
/// multi-minute `cargo build` (and a nested cargo lock) inside a test.
pub(crate) fn server_binary_path() -> Result<PathBuf> {
    // Explicit override wins (e.g. a binary built in a different worktree).
    if let Ok(p) = std::env::var("SSSHARE_SERVER_BIN") {
        return Ok(PathBuf::from(p));
    }
    let target = server_target_dir();
    let newest = ["release", "debug"]
        .iter()
        .map(|profile| target.join(profile).join("simple-screensharing-server"))
        .filter(|binary| binary.is_file())
        .max_by_key(|binary| std::fs::metadata(binary).and_then(|m| m.modified()).ok());
    newest.ok_or_else(|| {
        anyhow!(
            "relay server binary not found at {0}/{{debug,release}}/simple-screensharing-server - \
             build it first with `cargo build -p simple-screensharing-server`",
            target.display()
        )
    })
}

/// Spawns the real relay server binary as a child process listening on
/// `port`. See this module's doc comment ("Why the server is launched via
/// `setsid script -qc ...`") for why this isn't a plain `Command::spawn`.
pub(crate) fn spawn_relay_server(port: u16) -> Result<ServerGuard> {
    let binary = server_binary_path()?;
    // The server writes its `logs/` directory relative to its current
    // working directory (see its `main.rs`'s `LOG_DIR`). Give it a private
    // per-port temp dir (removed by `ServerGuard` on drop) so it litters
    // neither the workspace nor the test cwd. `SSSHARE_SERVER_CWD` overrides
    // it with a caller-owned directory, which is never deleted.
    let (work_dir, owned) = match std::env::var_os("SSSHARE_SERVER_CWD") {
        Some(dir) => (PathBuf::from(dir), false),
        None => (std::env::temp_dir().join(format!("sss-relay-{}-{port}", std::process::id())), true),
    };
    std::fs::create_dir_all(&work_dir).context("failed to create relay working dir")?;
    let cmd_string = format!("{} {port}", binary.display());
    let child = Command::new("setsid")
        .args(["script", "-qc", &cmd_string, "/dev/null"])
        .current_dir(&work_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context(
            "failed to spawn relay server child process \
             (requires `setsid` and `script` from util-linux)",
        )?;
    Ok(ServerGuard { child, port, work_dir: owned.then_some(work_dir) })
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
            pipelines; run explicitly with `cargo test -p simple-screensharing-client --lib -- --ignored end_to_end_test:: --skip real_portal --nocapture`"]
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
            pipelines; run explicitly with `cargo test -p simple-screensharing-client --lib -- --ignored end_to_end_test:: --skip real_portal --nocapture`"]
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
            pipelines; run explicitly with `cargo test -p simple-screensharing-client --lib -- --ignored end_to_end_test:: --skip real_portal --nocapture`"]
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
    handle.register_destination(Some(&sharer_b), second);

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

/// TEMP DIAGNOSTIC (bug investigation, the "no real keyframe reaches a late
/// subscriber" bug of the since-removed Electron video relay): reproduces the
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
            pipelines; run explicitly with `cargo test -p simple-screensharing-client --lib -- --ignored end_to_end_test:: --skip real_portal --nocapture`"]
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

/// Regression coverage (2026-07-28, investigation of a 1/2 full-smoke-test
/// failure against `unattended/goal-2026-07-28`'s tip): share, then toggle
/// all three live encoder-tuning controls (`tune`/`quality_level`/
/// `gop_millis`; the smoke test's third control was `rate_control`, removed in
/// #88; each a real `capture.rs` `BranchSwap`), THEN start
/// watching for the first time — the exact sequence the smoke test's one
/// failing run had.
///
/// `Why this lives at the `capture`/`session`/`net` layer:`
/// `capture::start_sharing_from_test_source` is `#[cfg(test)]`-gated (real
/// portal consent can't be automated), so only a test inside this crate can
/// drive a real `BranchSwap` through this exact seam. Synthetic coverage
/// can't exercise whether the real `force_keyframe()`/`SubscriberCount`
/// mechanism actually delivers a prompt, real keyframe when a first-ever
/// subscribe follows hard on the heels of three real, back-to-back
/// `BranchSwap`s — exactly the scenario this test drives, for real, in a loop.
///
/// Reran 14/14 clean (2 + 12 iterations) at this investigation's tip; the
/// original 1/2 smoke-test failure was concluded to be residual flakiness
/// in the (since removed) multi-window Electron/portal harness, not a regression in
/// this mechanism (see the 2026-07-28 entry in
/// `superdoc/features/video-pipeline.md` for the full writeup).
///
/// Loops `E2E_BRANCH_SWAP_LATE_WATCH_ITERATIONS` times (default 10),
/// reusing one relay server process but a fresh random room id/key and a
/// fresh sharer/viewer connection pair each iteration (cheaper than
/// spawning a new relay per round while still giving each round a clean
/// room — no state from a previous round can leak into the next one that
/// matters here). Fails loudly, with the failing iteration number, on the
/// first round that doesn't get a real keyframe promptly.
#[tokio::test]
#[ignore = "spawns a real relay server child process and real GStreamer capture/decode \
            pipelines; run explicitly with \
            `cargo test end_to_end_test::branch_swap -- --ignored --nocapture`"]
async fn late_watch_after_all_three_tuning_controls_toggled_gets_a_real_keyframe_every_time() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();

    let iterations: u32 =
        std::env::var("E2E_BRANCH_SWAP_LATE_WATCH_ITERATIONS").ok().and_then(|s| s.parse().ok()).unwrap_or(10);

    let port = pick_free_udp_port().expect("failed to pick a free UDP port for the test relay");
    eprintln!("[e2e-branch-swap-late] starting relay server on 127.0.0.1:{port} for {iterations} iteration(s)");
    let _server = spawn_relay_server(port).expect("failed to spawn relay server child process");
    let server_addr = format!("127.0.0.1:{port}");

    let mut failures: Vec<String> = Vec::new();

    for round in 1..=iterations {
        eprintln!("[e2e-branch-swap-late] ===== round {round}/{iterations} =====");
        let room_id: RoomId = rand::random();
        let room_key: [u8; 32] = rand::random();
        let join_string = crate::net::format_join_string(&server_addr, &room_id, &room_key);

        let (sharer, _snap_a, mut events_a) =
            connect_with_retry(&join_string, "sharer").await.expect("sharer failed to connect");
        let (viewer, _snap_b, mut events_b) =
            connect_with_retry(&join_string, "viewer").await.expect("viewer failed to connect");
        let owner_conn_id = sharer.conn_id();

        let source_fragment = format!(
            "videotestsrc is-live=true pattern=smpte ! video/x-raw,width=1920,height=1080,framerate={}/1",
            capture::DEFAULT_FRAMERATE
        );
        let capture_handle = capture::start_sharing_from_test_source(
            &sharer,
            VIDEO_STREAM_ID,
            capture::DEFAULT_BITRATE_KBPS,
            capture::DEFAULT_FRAMERATE,
            &source_fragment,
        )
        .await
        .expect("start_sharing_from_test_source failed");

        // Let the initial branch actually settle into real encoding before
        // touching it — matches a real user waiting to see "LIVE" before
        // reaching for the tuning controls.
        let mut warm_fps = 0;
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            warm_fps = warm_fps.max(capture_handle.encoder_fps(capture::DestKey(0)));
        }
        assert!(warm_fps > 0, "round {round}: initial share never encoded a frame");

        // The exact three live controls the bug report named, applied
        // sequentially and awaited to completion each time — "toggle Tune,
        // toggle Quality Level, toggle Keyframe interval", not three changes
        // batched into one swap.
        let base = capture_handle
            .destination_runtime(capture::DestKey(0))
            .expect("destination 0 runtime")
            .lock()
            .unwrap()
            .params();
        let after_tune = VideoParams { tune: capture::TuneMode::BestQuality, ..base };
        let after_quality = VideoParams { quality_level: capture::QualityLevel::Quality, ..after_tune };
        let after_gop = VideoParams { gop_millis: 750, ..after_quality };

        for (label, params) in [
            ("tune", after_tune),
            ("quality_level", after_quality),
            ("gop_millis", after_gop),
        ] {
            let req = capture_handle.change_destination(capture::DestKey(0), params).expect("classify/apply change");
            if let Some(done) = req.done {
                tokio::time::timeout(Duration::from_secs(15), done)
                    .await
                    .unwrap_or_else(|_| panic!("round {round}: BranchSwap for {label} timed out"))
                    .expect("swap channel")
                    .unwrap_or_else(|e| panic!("round {round}: BranchSwap for {label} failed: {e}"));
            }
            eprintln!("[e2e-branch-swap-late] round {round}: {label} toggle applied ({:?})", req.tier);
        }

        // ONLY NOW does the watcher start watching — the exact ordering the
        // smoke test's one failing run had (watch started after all three
        // toggles, not before).
        eprintln!("[e2e-branch-swap-late] round {round}: viewer subscribing for the first time (post-toggles)");
        let start = std::time::Instant::now();
        viewer.subscribe(owner_conn_id, VIDEO_STREAM_ID).await.expect("viewer: subscribe() failed");

        async fn forward_one_control_event(
            events_a: &mut tokio::sync::mpsc::UnboundedReceiver<ClientEvent>,
            capture_handle: &capture::CaptureHandle,
        ) -> bool {
            match events_a.recv().await {
                Some(ClientEvent::SubscriberCount { stream_id, count }) if stream_id == VIDEO_STREAM_ID => {
                    capture_handle.notify_subscriber_count(capture::DestKey(0), count);
                    true
                }
                Some(ClientEvent::KeyframeRequest { stream_id }) if stream_id == VIDEO_STREAM_ID => {
                    capture_handle.notify_keyframe_request(capture::DestKey(0));
                    true
                }
                Some(_) => true,
                None => false,
            }
        }

        let mut first_keyframe_at: Option<Duration> = None;
        let mut total_frames = 0u64;
        let deadline = tokio::time::sleep(Duration::from_secs(10));
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => break,
                ok = forward_one_control_event(&mut events_a, &capture_handle) => {
                    if !ok { break; }
                }
                event = events_b.recv() => {
                    match event {
                        Some(ClientEvent::MediaFrame(frame))
                            if frame.conn_id == owner_conn_id && frame.stream_id == VIDEO_STREAM_ID =>
                        {
                            total_frames += 1;
                            if frame.keyframe {
                                first_keyframe_at.get_or_insert(start.elapsed());
                            }
                        }
                        Some(_) => {}
                        None => break,
                    }
                }
            }
            if first_keyframe_at.is_some() {
                break;
            }
        }

        if let Err(e) = capture_handle.stop().await {
            tracing::warn!("[e2e-branch-swap-late] round {round}: capture_handle.stop() error (non-fatal): {e:#}");
        }
        drop(viewer);
        let _ = sharer.leave().await;

        match first_keyframe_at {
            Some(latency) if latency < Duration::from_secs(5) => {
                eprintln!("[e2e-branch-swap-late] round {round}: PASS — real keyframe within {latency:?}");
            }
            Some(latency) => {
                let msg = format!("round {round}: keyframe arrived but took {latency:?} (>= 5s)");
                eprintln!("[e2e-branch-swap-late] {msg}");
                failures.push(msg);
            }
            None => {
                let msg = format!(
                    "round {round}: BUG REPRODUCED — no keyframe ever arrived within 10s of subscribing \
                     ({total_frames} total frames received)"
                );
                eprintln!("[e2e-branch-swap-late] {msg}");
                failures.push(msg);
            }
        }
    }

    assert!(
        failures.is_empty(),
        "{}/{iterations} round(s) failed to get a prompt real keyframe after a late watch \
         following all three tuning-control toggles:\n{}",
        failures.len(),
        failures.join("\n")
    );
    eprintln!("[e2e-branch-swap-late] PASS: all {iterations} round(s) got a real keyframe promptly");
}

/// Reproduces (and proves the fix for) the "retry resets a destination's
/// encoder tuning" bug at the `AppState` layer, against a real capture
/// pipeline and a real relay connection: a destination is brought up with
/// non-default `edit_destination`-style tuning (bitrate/tune/quality_level),
/// `AppState::retry_destination` is called on it (the same
/// path the "Retry" button on a Failed destination card drives), and the resulting *new* destination
/// must carry the *same* tuning — not the session's defaults. Also proves
/// the capture itself survives the retry rather than being torn down, which
/// is what happened before this fix for a capture's *only* destination (see
/// `AppState::retry_destination`'s `Why:` note for both bugs this closes).
///
/// Drives `AppState` directly. Bypasses `AppState::start_video_share`
/// (which only knows how to start from the real interactive portal) and
/// instead assembles the capture/destination records by hand around a real
/// `capture::start_sharing_from_test_source` handle, the same seam the other
/// tests in this file use.
#[tokio::test]
#[ignore = "spawns a real relay server child process and real GStreamer capture pipeline; \
            run explicitly with `cargo test -p simple-screensharing-client --lib -- --ignored end_to_end_test:: --skip real_portal --nocapture`"]
async fn retry_destination_preserves_prior_encoder_tuning() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();

    let port = pick_free_udp_port().expect("failed to pick a free UDP port for the test relay");
    eprintln!("[e2e-retry] starting relay server on 127.0.0.1:{port}");
    let _server = spawn_relay_server(port).expect("failed to spawn relay server child process");

    let server_addr = format!("127.0.0.1:{port}");
    let room_id: RoomId = rand::random();
    let room_key: [u8; 32] = rand::random();
    let join_string = crate::net::format_join_string(&server_addr, &room_id, &room_key);

    eprintln!("[e2e-retry] connecting sharer");
    let (client, snapshot, events) = connect_with_retry(&join_string, "sharer")
        .await
        .expect("sharer failed to connect to the relay after retries");

    let source_fragment = format!(
        "videotestsrc is-live=true pattern=smpte \
         ! video/x-raw,width={WIDTH},height={HEIGHT},framerate=15/1"
    );

    eprintln!("[e2e-retry] starting real capture pipeline (this destination's own DestKey(0))");
    let capture_handle = capture::start_sharing_from_test_source(
        &client,
        VIDEO_STREAM_ID,
        2000,
        15,
        &source_fragment,
    )
    .await
    .expect("start_sharing_from_test_source failed");
    let initial_runtime = capture_handle.destination_runtime(capture::DestKey(0));

    let mut state = crate::session::AppState::default();
    let room_handle = state.insert_room(client, snapshot, events, join_string);

    let capture_id = state.alloc_capture_id();
    let destination_id = state.alloc_destination_id();
    assert_eq!(
        destination_id.0, 0,
        "must line up with start_sharing_from_test_source's hardcoded DestKey(0)"
    );

    // Non-default tuning: what a user would have set via `edit_destination`
    // (low-latency's the default; best-quality/quality are not) before
    // this destination later failed and got retried.
    let tuned_params = VideoParams {
        width: 0,
        height: 0,
        framerate: 15,
        bitrate_kbps: 777,
        backend: capture::EncoderBackend::default(),
        tune: capture::TuneMode::BestQuality,
        quality_level: capture::QualityLevel::Quality,
        gop_millis: capture::DEFAULT_GOP_MILLIS,
        codec: capture::VideoCodec::default(),
    };

    {
        let room = state.rooms.get_mut(&room_handle).expect("room was just inserted");
        room.next_stream_id = 1;
        room.destinations.push(destination_id);
    }
    state.captures.insert(
        capture_id,
        crate::session::LocalCapture {
            kind: crate::protocol::StreamKind::Video,
            label: "Screen share".to_string(),
            status: crate::session::ShareStatus::Active(crate::session::ShareHandle::Video(capture_handle)),
            audio_for_capture: None,
            destinations: indexmap::IndexMap::from([(
                destination_id,
                crate::session::Destination {
                    room: room_handle,
                    stream_id: 0,
                    params: tuned_params,
                    runtime: initial_runtime,
                    pending: None,
                    edit: crate::session::Destination::edit_from(tuned_params),
                    apply_rx: None,
                },
            )]),
        },
    );

    eprintln!("[e2e-retry] retrying the (still-live) destination via AppState::retry_destination");
    state.retry_destination(destination_id);

    // The old destination_id is gone immediately...
    assert!(
        crate::session::find_destination(&state.captures, destination_id).is_none(),
        "the retried destination_id should no longer resolve"
    );

    // ...replaced by a new destination, still in the same room, off the same
    // capture, that AppState considers "pending" until the new branch's
    // spawned task confirms it. Poll until it resolves (or time out) —
    // mirrors `ui.rs`'s own per-frame `poll_pending_destinations` call.
    let new_id = {
        let capture = state.captures.get(&capture_id).expect("capture must still exist after retry");
        assert_eq!(capture.destinations.len(), 1, "retry should leave exactly one destination on this capture");
        *capture.destinations.keys().next().expect("just asserted len == 1")
    };
    assert_ne!(new_id, destination_id, "retry must allocate a fresh destination id, not reuse the old one");

    let resolved = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            state.poll_pending_destinations();
            let pending = state
                .captures
                .get(&capture_id)
                .and_then(|c| c.destinations.get(&new_id))
                .map(|d| d.pending);
            match pending {
                Some(None) => return true,
                Some(Some(_)) => tokio::time::sleep(Duration::from_millis(50)).await,
                None => return false,
            }
        }
    })
    .await;
    assert_eq!(resolved, Ok(true), "the retried destination's replacement branch never came up");

    let capture = state.captures.get(&capture_id).expect("capture must still exist");
    assert!(
        matches!(capture.status, crate::session::ShareStatus::Active(_)),
        "BUG: retrying a capture's only destination must not tear the whole capture down \
         (that made retry silently fail for the single-destination case)"
    );
    let new_dest = capture.destinations.get(&new_id).expect("resolved above");
    assert_eq!(new_dest.room, room_handle);
    assert_eq!(
        new_dest.params.bitrate_kbps, 777,
        "BUG REPRODUCED: retry reset the bitrate instead of preserving it"
    );
    assert_eq!(
        new_dest.params.tune,
        capture::TuneMode::BestQuality,
        "BUG REPRODUCED: retry reset tune to the default instead of preserving it"
    );
    assert_eq!(
        new_dest.params.quality_level,
        capture::QualityLevel::Quality,
        "BUG REPRODUCED: retry reset quality_level to the default instead of preserving it"
    );

    eprintln!("[e2e-retry] PASS: retried destination {new_id:?} preserved bitrate/tune/quality_level");
}

/// **The "audio follows video" contract, end to end against a real relay.**
///
/// This is the regression test for the feature the project owner asked for
/// verbatim: *"the streamer should be able to enable audio on his share and
/// the viewer should automatically hear that. No (visual) separate 'stream'
/// for Audio."* It asserts exactly that, in the order a real session
/// actually happens in — which is the order the previous implementation got
/// wrong:
///
/// 1. Sharer starts a **video-only** share. Viewer clicks Watch. No audio
///    watch exists (there's no audio to watch yet).
/// 2. Sharer **then** enables audio on that share — the normal order, since
///    you pick a monitor first and an app to capture audio from second.
/// 3. Without the viewer doing *anything*, an `AudioWatch` for the sharer's
///    audio stream appears in the viewer's room, marked `auto` (i.e. owned
///    by the video watch, not by a user action), and its real GStreamer
///    playback pipeline reaches `Playing` — the viewer is audibly hearing
///    it.
/// 4. The viewer stops watching the **video**, and the audio watch goes away
///    with it.
///
/// Step 3 is the part that used to fail: `toggle_watch` latched
/// `sibling_audio_stream(owner)` once, at watch time, so a viewer who was
/// already watching when the sharer enabled audio stayed silent forever.
/// See `session::RoomSession::sync_audio_watches`' doc comment.
///
/// Real everything: real relay server child process, real QUIC, real
/// H.264 capture/encode/encrypt/fragment, real Opus capture through a real
/// `pw-loopback` off a real PipeWire node, real Opus decode into
/// `autoaudiosink`. `#[ignore]`d for the same reason every other test in
/// this module is (needs a built server binary and a live PipeWire session).
///
/// Run with:
/// `cargo test end_to_end_test::watching_video_automatically_delivers -- --ignored --nocapture`
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "spawns a real relay server child process, real GStreamer capture/decode pipelines \
            and a real PipeWire audio node; run explicitly with \
            `cargo test -p simple-screensharing-client --lib -- --ignored end_to_end_test:: --skip real_portal --nocapture`"]
async fn watching_video_automatically_delivers_the_sharers_audio_too() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();

    const AUDIO_STREAM_ID: u8 = 9;
    const PW_CLIENT_NAME: &str = "ssshare-audio-follows-video-test";

    let port = pick_free_udp_port().expect("failed to pick a free UDP port for the test relay");
    let _server = spawn_relay_server(port).expect("failed to spawn relay server child process");

    let server_addr = format!("127.0.0.1:{port}");
    let room_id: RoomId = rand::random();
    let room_key: [u8; 32] = rand::random();
    let join_string = crate::net::format_join_string(&server_addr, &room_id, &room_key);

    // --- Sharer (client A): a plain client, driven directly. ---
    let (client_a, _snapshot_a, events_a) =
        connect_with_retry(&join_string, "sharer").await.expect("client A failed to connect");
    let _events_a_keepalive = events_a;
    let owner = client_a.conn_id();

    // --- Viewer (client B): a real `AppState`/`RoomSession`, because that
    // is where the behavior under test lives. `insert_room` is the same
    // entry point `poll_connect_result` uses in production, so this viewer
    // gets the real media-ingestion task and the real watch maps. ---
    let (client_b, snapshot_b, events_b) =
        connect_with_retry(&join_string, "viewer").await.expect("client B failed to connect");
    let mut viewer = crate::session::AppState::default();
    let room_handle = viewer.insert_room(client_b, snapshot_b, events_b, join_string.clone());
    let ctx = egui::Context::default();

    // --- Step 1: video-only share, and the viewer watches it. ---
    let source_fragment = format!(
        "videotestsrc is-live=true pattern=smpte \
         ! video/x-raw,width={WIDTH},height={HEIGHT},framerate=15/1"
    );
    let video_handle = capture::start_sharing_from_test_source(
        &client_a,
        VIDEO_STREAM_ID,
        2000,
        capture::DEFAULT_FRAMERATE,
        &source_fragment,
    )
    .await
    .expect("start_sharing_from_test_source failed");

    // Wait for the viewer's roster to learn about the video stream.
    let mut saw_video = false;
    for _ in 0..80 {
        viewer.drain_events(&ctx);
        if viewer.rooms[&room_handle].streams.iter().any(|s| s.owner == owner && s.stream_id == VIDEO_STREAM_ID) {
            saw_video = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(saw_video, "viewer never saw the sharer's video stream announced");

    viewer.rooms.get_mut(&room_handle).unwrap().toggle_watch(owner, VIDEO_STREAM_ID);
    assert!(
        viewer.rooms[&room_handle].watches.lock().unwrap().contains_key(&(owner, VIDEO_STREAM_ID)),
        "toggle_watch did not create a video watch"
    );
    assert!(
        viewer.rooms[&room_handle].audio_watches.lock().unwrap().is_empty(),
        "an audio watch appeared before the sharer had any audio stream at all"
    );
    eprintln!("[audio-follows-video] step 1 OK: watching video, no audio anywhere yet");

    // --- Step 2: the sharer NOW enables audio, with the viewer already
    // watching. A synthetic `audiotestsrc ! pipewiresink` app stands in for
    // "some app on the sharer's machine is playing audio".
    //
    // `Why volume=0.02 and not 0:` this test runs against a real desktop
    // session and the *receiving* end is a real `autoaudiosink`, so a
    // default-amplitude `audiotestsrc` plays a 440Hz sine out of the
    // developer's actual speakers for the whole run. (It did. It was exactly
    // as unpleasant as it sounds, and it interrupted the project owner
    // mid-session.) 0.02 is ~34 dB down — inaudible in practice — but,
    // unlike digital silence, it is *distinguishable from nothing*.
    //
    // That distinction is the whole point. This test used to use `volume=0`
    // on the reasoning that "every assertion here is about pipeline liveness
    // and subscription state, none of which depend on amplitude" — which was
    // true, and was the bug: the test passed with flying colours while the
    // sender was capturing the wrong PipeWire node entirely and no audio
    // from the shared application ever reached the viewer (see
    // `audio::build_pipeline`'s doc comment). A test that cannot tell "audio
    // is flowing" from "audio is not flowing" cannot protect this feature,
    // so the assertions below now check the sender's real capture level and
    // the receiver's real packet count, both of which need a signal.
    let mut silent_audio_app = Command::new("gst-launch-1.0")
        .args(["audiotestsrc", "volume=0.02", "!", "audioconvert", "!", "pipewiresink", &format!("client-name={PW_CLIENT_NAME}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn the synthetic audio-producing app");
    tokio::time::sleep(Duration::from_secs(2)).await;

    let sources = crate::audio::list_audio_sources().expect("list_audio_sources failed against a live pw-dump");
    let source = sources
        .iter()
        .find(|s| s.name.contains(PW_CLIENT_NAME))
        .cloned()
        .expect("did not find the synthetic test app among live audio sources");
    let audio_handle = crate::audio::start_audio_sharing(&client_a, AUDIO_STREAM_ID, &source)
        .await
        .expect("start_audio_sharing failed");
    eprintln!("[audio-follows-video] step 2 OK: sharer enabled audio AFTER the viewer was already watching");

    // --- Step 3: the viewer must pick it up on its own. The ONLY thing
    // driving this is `drain_events`, i.e. the same polling both frontends
    // already do — no watch call, no listen call, no user action. ---
    let mut auto_watch_alive = false;
    for _ in 0..80 {
        viewer.drain_events(&ctx);
        {
            let audio_watches = viewer.rooms[&room_handle].audio_watches.lock().unwrap();
            if let Some(watch) = audio_watches.get(&(owner, AUDIO_STREAM_ID)) {
                assert!(watch.auto, "the auto-subscribed audio watch should be marked `auto`, not a standalone Listen");
                if watch.decoder.is_alive() {
                    auto_watch_alive = true;
                }
            }
        }
        if auto_watch_alive {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        auto_watch_alive,
        "the viewer never started playing the sharer's audio after it was enabled mid-watch — \
         this is the exact bug `sync_audio_watches` exists to fix (audio latched at watch time)"
    );
    // And the link back to the video watch is recorded, which is what the
    // frontend's per-tile volume control resolves through.
    assert_eq!(
        viewer.rooms[&room_handle].watches.lock().unwrap()[&(owner, VIDEO_STREAM_ID)].audio_stream_id,
        Some(AUDIO_STREAM_ID),
        "the video watch should point at the sharer's audio stream"
    );
    eprintln!("[audio-follows-video] step 3 OK: viewer is playing the sharer's audio with zero extra actions");

    // --- Step 3b: and the audio is REAL. Liveness and bookkeeping were all
    // this used to check, and all of it stayed true while nothing was
    // audible. These two assertions are the ones that would have caught it:
    // a non-zero capture level on the sender (the tap is picking up the
    // application's actual signal) and a non-zero packet count on the
    // viewer's decoder (those packets crossed the relay and arrived). ---
    let mut capture_level = 0.0;
    let mut packets = 0;
    for _ in 0..80 {
        viewer.drain_events(&ctx);
        capture_level = audio_handle.level();
        packets = viewer.rooms[&room_handle]
            .audio_watches
            .lock()
            .unwrap()
            .get(&(owner, AUDIO_STREAM_ID))
            .map(|w| w.decoder.packets_received())
            .unwrap_or(0);
        if capture_level > 0.0 && packets > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        capture_level > 0.0,
        "the sharer's audio capture reported a level of {capture_level} — it is not picking up the \
         application's signal at all (mis-targeted `pipewiresrc`? see `audio::build_pipeline`)"
    );
    assert!(
        packets > 0,
        "the viewer's audio decoder received {packets} packets — the pipeline is alive but no audio \
         is actually arriving, which is exactly the failure this test used to sail straight past"
    );
    eprintln!(
        "[audio-follows-video] step 3b OK: real audio is flowing (sender capture level {capture_level:.3}, \
         viewer received {packets} Opus packets)"
    );

    // --- Step 4: stopping the VIDEO watch stops the audio too. ---
    viewer.rooms.get_mut(&room_handle).unwrap().toggle_watch(owner, VIDEO_STREAM_ID);
    assert!(
        viewer.rooms[&room_handle].watches.lock().unwrap().is_empty(),
        "toggle_watch did not remove the video watch"
    );
    assert!(
        viewer.rooms[&room_handle].audio_watches.lock().unwrap().is_empty(),
        "the audio watch outlived the video watch it was riding along with"
    );
    eprintln!("[audio-follows-video] step 4 OK: audio torn down together with the video watch");

    let _ = audio_handle.stop().await;
    let _ = video_handle.stop().await;
    let _ = silent_audio_app.kill();
    let _ = silent_audio_app.wait();
    eprintln!("[audio-follows-video] PASS");
}

/// Regression test: a viewer's chosen volume for a sender must survive that
/// sender restarting or switching their audio source.
///
/// Root cause this guards against: `AudioWatch::volume_percent` — the
/// actually-playing copy of a viewer's volume choice — lives inside
/// `audio_watches`, keyed by `(owner, audio stream_id)`. The audio
/// stream_id is volatile: every time a sender stops and starts an audio
/// share (a manual restart, or switching which app they're sharing audio
/// from) it gets a fresh id from `next_stream_id`. `sync_audio_watches`
/// correctly notices the old `(owner, old_stream_id)` is no longer
/// announced and tears the whole `AudioWatch` down (dropping
/// `volume_percent` with it), then creates a brand new one for
/// `(owner, new_stream_id)` — which, before this fix, always started at
/// the hardcoded 100 in `start_audio_watch`, silently resetting whatever
/// the viewer had chosen. `RoomSession::audio_volume_by_owner` is the
/// owner-scoped copy that survives this churn; `start_audio_watch`
/// consults it instead of hardcoding 100.
///
/// Real everything, same shape as
/// `watching_video_automatically_delivers_the_sharers_audio_too` just above:
/// real relay server, real QUIC, real video capture, real Opus audio
/// capture off a real PipeWire node via a synthetic `audiotestsrc`.
/// `#[ignore]`d for the same reason every other test in this module is.
///
/// Run with:
/// `cargo test end_to_end_test::volume_survives_sender_audio_restart -- --ignored --nocapture`
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "spawns a real relay server child process, real GStreamer capture/decode pipelines \
            and a real PipeWire audio node; run explicitly with \
            `cargo test -p simple-screensharing-client --lib -- --ignored end_to_end_test:: --skip real_portal --nocapture`"]
async fn volume_survives_sender_audio_restart_or_switch() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();

    const AUDIO_STREAM_ID_A: u8 = 9;
    const AUDIO_STREAM_ID_B: u8 = 10;
    const PW_CLIENT_NAME_A: &str = "ssshare-volume-persists-test-a";
    const PW_CLIENT_NAME_B: &str = "ssshare-volume-persists-test-b";
    const CHOSEN_VOLUME: u32 = 50;

    let port = pick_free_udp_port().expect("failed to pick a free UDP port for the test relay");
    let _server = spawn_relay_server(port).expect("failed to spawn relay server child process");

    let server_addr = format!("127.0.0.1:{port}");
    let room_id: RoomId = rand::random();
    let room_key: [u8; 32] = rand::random();
    let join_string = crate::net::format_join_string(&server_addr, &room_id, &room_key);

    // --- Sharer (client A): a plain client, driven directly. ---
    let (client_a, _snapshot_a, events_a) =
        connect_with_retry(&join_string, "sharer").await.expect("client A failed to connect");
    let _events_a_keepalive = events_a;
    let owner = client_a.conn_id();

    // --- Viewer (client B): a real `AppState`/`RoomSession`. ---
    let (client_b, snapshot_b, events_b) =
        connect_with_retry(&join_string, "viewer").await.expect("client B failed to connect");
    let mut viewer = crate::session::AppState::default();
    let room_handle = viewer.insert_room(client_b, snapshot_b, events_b, join_string.clone());
    let ctx = egui::Context::default();

    // --- Step 1: sharer starts video, viewer watches it. ---
    let source_fragment = format!(
        "videotestsrc is-live=true pattern=smpte \
         ! video/x-raw,width={WIDTH},height={HEIGHT},framerate=15/1"
    );
    let video_handle = capture::start_sharing_from_test_source(
        &client_a,
        VIDEO_STREAM_ID,
        2000,
        capture::DEFAULT_FRAMERATE,
        &source_fragment,
    )
    .await
    .expect("start_sharing_from_test_source failed");

    let mut saw_video = false;
    for _ in 0..80 {
        viewer.drain_events(&ctx);
        if viewer.rooms[&room_handle].streams.iter().any(|s| s.owner == owner && s.stream_id == VIDEO_STREAM_ID) {
            saw_video = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(saw_video, "viewer never saw the sharer's video stream announced");

    viewer.rooms.get_mut(&room_handle).unwrap().toggle_watch(owner, VIDEO_STREAM_ID);
    eprintln!("[volume-persists] step 1 OK: video shared and watched");

    // --- Step 2: sharer starts its FIRST audio share (volume=0.02, same
    // "audible but inaudible" rationale as the sibling test above). ---
    let mut silent_audio_app_a = Command::new("gst-launch-1.0")
        .args(["audiotestsrc", "volume=0.02", "!", "audioconvert", "!", "pipewiresink", &format!("client-name={PW_CLIENT_NAME_A}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn the first synthetic audio-producing app");
    tokio::time::sleep(Duration::from_secs(2)).await;

    let sources = crate::audio::list_audio_sources().expect("list_audio_sources failed against a live pw-dump");
    let source_a = sources
        .iter()
        .find(|s| s.name.contains(PW_CLIENT_NAME_A))
        .cloned()
        .expect("did not find the first synthetic test app among live audio sources");
    let audio_handle_a = crate::audio::start_audio_sharing(&client_a, AUDIO_STREAM_ID_A, &source_a)
        .await
        .expect("start_audio_sharing (first) failed");

    let mut auto_watch_alive = false;
    for _ in 0..80 {
        viewer.drain_events(&ctx);
        if viewer.rooms[&room_handle]
            .audio_watches
            .lock()
            .unwrap()
            .get(&(owner, AUDIO_STREAM_ID_A))
            .is_some_and(|w| w.decoder.is_alive())
        {
            auto_watch_alive = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(auto_watch_alive, "the viewer never auto-subscribed the sharer's first audio stream");
    eprintln!("[volume-persists] step 2 OK: first audio stream ({AUDIO_STREAM_ID_A}) auto-subscribed");

    // --- Step 3: the viewer sets a non-default volume. This is exactly
    // what the real call site (the egui volume slider)
    // does: update the live `AudioWatch` AND record
    // the owner-scoped standing preference. ---
    {
        let room = viewer.rooms.get_mut(&room_handle).unwrap();
        {
            let mut audio_watches = room.audio_watches.lock().unwrap();
            let watch = audio_watches.get_mut(&(owner, AUDIO_STREAM_ID_A)).expect("first audio watch vanished");
            watch.volume_percent = CHOSEN_VOLUME;
            watch.decoder.set_volume(CHOSEN_VOLUME as f64 / 100.0);
        }
        room.audio_volume_by_owner.insert(owner, CHOSEN_VOLUME);
    }
    assert_eq!(
        viewer.rooms[&room_handle].audio_watches.lock().unwrap()[&(owner, AUDIO_STREAM_ID_A)].volume_percent,
        CHOSEN_VOLUME,
        "setting the volume did not take effect on the live AudioWatch"
    );
    eprintln!("[volume-persists] step 3 OK: viewer set volume to {CHOSEN_VOLUME}%");

    // --- Step 4: the sharer restarts/switches their audio source — stop
    // the first share, start a second one under a DIFFERENT stream_id
    // (exactly what a real restart/switch does: `next_stream_id` hands out
    // a fresh id every time). ---
    audio_handle_a.stop().await.expect("stopping the first audio share failed");
    let _ = silent_audio_app_a.kill();
    let _ = silent_audio_app_a.wait();

    let mut old_torn_down = false;
    for _ in 0..80 {
        viewer.drain_events(&ctx);
        if !viewer.rooms[&room_handle].audio_watches.lock().unwrap().contains_key(&(owner, AUDIO_STREAM_ID_A)) {
            old_torn_down = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(old_torn_down, "the old audio watch never got torn down after the sharer stopped its audio stream");
    eprintln!("[volume-persists] step 4a OK: old audio watch ({AUDIO_STREAM_ID_A}) torn down");

    let mut silent_audio_app_b = Command::new("gst-launch-1.0")
        .args(["audiotestsrc", "volume=0.02", "!", "audioconvert", "!", "pipewiresink", &format!("client-name={PW_CLIENT_NAME_B}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn the second synthetic audio-producing app");
    tokio::time::sleep(Duration::from_secs(2)).await;

    let sources = crate::audio::list_audio_sources().expect("list_audio_sources (second) failed against a live pw-dump");
    let source_b = sources
        .iter()
        .find(|s| s.name.contains(PW_CLIENT_NAME_B))
        .cloned()
        .expect("did not find the second synthetic test app among live audio sources");
    let audio_handle_b = crate::audio::start_audio_sharing(&client_a, AUDIO_STREAM_ID_B, &source_b)
        .await
        .expect("start_audio_sharing (second) failed");

    // --- Step 5: the NEW audio watch must come up already at the viewer's
    // chosen volume, not reset to 100. This is the exact regression. ---
    let mut new_watch_volume: Option<u32> = None;
    for _ in 0..80 {
        viewer.drain_events(&ctx);
        if let Some(w) = viewer.rooms[&room_handle].audio_watches.lock().unwrap().get(&(owner, AUDIO_STREAM_ID_B)) {
            new_watch_volume = Some(w.volume_percent);
            if w.decoder.is_alive() {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        new_watch_volume,
        Some(CHOSEN_VOLUME),
        "the viewer's volume reset to {new_watch_volume:?} after the sharer restarted/switched audio — \
         expected it to stay at {CHOSEN_VOLUME}% (this is the exact bug `audio_volume_by_owner` exists to fix)"
    );
    eprintln!(
        "[volume-persists] step 5 OK: new audio stream ({AUDIO_STREAM_ID_B}) came up at {CHOSEN_VOLUME}%, not reset to 100"
    );

    let _ = audio_handle_b.stop().await;
    let _ = video_handle.stop().await;
    let _ = silent_audio_app_b.kill();
    let _ = silent_audio_app_b.wait();
    eprintln!("[volume-persists] PASS");
}

/// Regression test for the 2026-07-28 "Go live" bugs — see
/// `superdoc/features/stream-control.md`'s dated note.
///
/// Reproduces `AppState::go_live` attaching the **first** destination to an
/// already-running, zero-destination capture (exactly `stage_video_share`'s
/// "Go live" flow, minus the interactive portal picker —
/// `capture::start_capture_from_test_source` stands in for
/// `capture::start_capture` the same way `start_sharing_from_test_source`
/// stands in for `start_sharing` elsewhere in this module) and asserts the
/// resulting destination's live params show the **real** negotiated
/// resolution and the **real** hardware-probed encoder backend, not the
/// `width: 0, height: 0, backend: EncoderBackend::default()` placeholder
/// `go_live` used to hardcode:
///
/// - **Bug 1** (resolution 0×0 after "Go live"): before the fix, `go_live`
///   built its `VideoParams` with a hardcoded `width: 0, height: 0` even
///   though the capture had already been running since `stage_video_share`
///   and its real negotiated size was available the whole time via
///   `CaptureHandle::source_caps` — the same real size the staged card had
///   just correctly displayed. Nothing downstream ever resolves that 0
///   placeholder back to a real value for a fresh (non-swapped) destination,
///   so the "Go live" card was stuck at 0×0 forever.
/// - **Bug 2** (falls back to software x264 instead of real GPU/VAAPI
///   encode): before the fix, `go_live` used the static
///   `EncoderBackend::default()` (`Software`, no hardware probing at all)
///   instead of `detect_encoder_backend()` — unlike
///   `capture::start_sharing_with_source` (the legacy `start_video_share`
///   path), which already called `detect_encoder_backend()` internally and
///   so was never affected by `session.rs`'s static default.
///
/// `#[ignore]`d for the same reason every other test in this module is
/// (needs a built server binary); does **not** need a real xdg-desktop-portal
/// prompt, PipeWire, or any of DP-1/DP-2/DP-3/HDMI — `TestSource` is a plain
/// `videotestsrc`.
#[tokio::test]
#[ignore = "spawns a real relay server child process and a real GStreamer capture pipeline; \
            run explicitly with `cargo test -p simple-screensharing-client --lib -- --ignored end_to_end_test:: --skip real_portal --nocapture`"]
async fn go_live_attaches_the_first_destination_with_real_resolution_and_backend() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();

    const GO_LIVE_WIDTH: u32 = 640;
    const GO_LIVE_HEIGHT: u32 = 480;
    const GO_LIVE_FRAMERATE: u32 = 30;

    let port = pick_free_udp_port().expect("failed to pick a free UDP port for the test relay");
    eprintln!("[go-live] starting relay server on 127.0.0.1:{port}");
    let _server = spawn_relay_server(port).expect("failed to spawn relay server child process");

    let server_addr = format!("127.0.0.1:{port}");
    let room_id: RoomId = rand::random();
    let room_key: [u8; 32] = rand::random();
    let join_string = crate::net::format_join_string(&server_addr, &room_id, &room_key);

    eprintln!("[go-live] connecting sharer");
    let (client, snapshot, events) =
        connect_with_retry(&join_string, "sharer").await.expect("sharer failed to connect to the relay after retries");

    let source_fragment = format!(
        "videotestsrc is-live=true pattern=smpte \
         ! video/x-raw,width={GO_LIVE_WIDTH},height={GO_LIVE_HEIGHT},framerate={GO_LIVE_FRAMERATE}/1"
    );

    // The staged half: a real, running capture with zero destinations —
    // exactly `stage_video_share`'s shape.
    eprintln!("[go-live] starting a zero-destination TestSource capture (the staged half)");
    let capture_handle = capture::start_capture_from_test_source(GO_LIVE_FRAMERATE, &source_fragment)
        .await
        .expect("start_capture_from_test_source failed");

    // Sanity check on this test's own premise: the real negotiated size is
    // already known on the capture handle, with zero destinations attached —
    // the exact fact `go_live`'s fix relies on instead of hardcoding 0×0.
    let staged_caps = capture_handle.source_caps();
    assert_eq!(
        (staged_caps.width, staged_caps.height),
        (GO_LIVE_WIDTH, GO_LIVE_HEIGHT),
        "a zero-destination capture must already report its real negotiated size \
         (this is what the staged card displays, and what go_live should read instead of 0×0)"
    );

    let mut state = crate::session::AppState::default();
    let room_handle = state.insert_room(client, snapshot, events, join_string);

    let capture_id = state.alloc_capture_id();
    state.captures.insert(
        capture_id,
        crate::session::LocalCapture {
            kind: crate::protocol::StreamKind::Video,
            label: "Screen share".to_string(),
            status: crate::session::ShareStatus::Active(crate::session::ShareHandle::Video(capture_handle)),
            audio_for_capture: None,
            destinations: indexmap::IndexMap::new(),
        },
    );

    eprintln!("[go-live] calling AppState::go_live on the staged, zero-destination capture");
    state.framerate = GO_LIVE_FRAMERATE;
    state.go_live(capture_id, room_handle);

    let destination_id = {
        let capture = state.captures.get(&capture_id).expect("capture must still exist right after go_live");
        assert_eq!(capture.destinations.len(), 1, "go_live should attach exactly one destination");
        *capture.destinations.keys().next().expect("just asserted len == 1")
    };

    // Poll until the spawned branch resolves — mirrors `ui.rs`'s own
    // per-frame `poll_pending_destinations` call, and the identical pattern
    // in `retry_destination_preserves_prior_encoder_tuning` above.
    let resolved = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            state.poll_pending_destinations();
            let pending =
                state.captures.get(&capture_id).and_then(|c| c.destinations.get(&destination_id)).map(|d| d.pending);
            match pending {
                Some(None) => return true,
                Some(Some(_)) => tokio::time::sleep(Duration::from_millis(50)).await,
                None => return false,
            }
        }
    })
    .await;
    assert_eq!(resolved, Ok(true), "go_live's destination branch never came up");

    let capture = state.captures.get(&capture_id).expect("capture must still exist");
    let dest = capture.destinations.get(&destination_id).expect("resolved above");
    let live = dest.live_params();

    assert_eq!(
        (live.width, live.height),
        (GO_LIVE_WIDTH, GO_LIVE_HEIGHT),
        "BUG REPRODUCED: go_live's destination shows {}×{} instead of the real negotiated \
         {GO_LIVE_WIDTH}×{GO_LIVE_HEIGHT} — the 'Go live' card would show 0×0",
        live.width,
        live.height
    );
    assert_eq!(
        live.backend,
        capture::detect_encoder_backend(),
        "BUG REPRODUCED: go_live's destination used {:?} instead of this machine's real \
         detect_encoder_backend() result ({:?}) — every 'Go live' share would silently fall back \
         to CPU/software encoding regardless of real GPU availability",
        live.backend,
        capture::detect_encoder_backend()
    );
    eprintln!(
        "[go-live] PASS: destination live params are {}×{} @ {}fps, backend {:?}",
        live.width, live.height, live.framerate, live.backend
    );

    if let crate::session::ShareStatus::Active(crate::session::ShareHandle::Video(handle)) =
        state.captures.shift_remove(&capture_id).unwrap().status
    {
        let _ = handle.stop().await;
    }
}

/// Reproduces (and proves the fix for) the reported **"big FPS jumps freeze
/// the viewer"** bug: a real viewer is actively watching (already past its
/// first keyframe, decoding normally) when the streamer changes its
/// destination's framerate by a large jump — both up (15 -> 120, a
/// [`capture::destination::ChangeTier::SourceRestart`] per
/// [`capture::destination::classify_change`]'s fps rule) and back down (120
/// -> 15, a [`capture::destination::ChangeTier::BranchSwap`]) — and keeps
/// decoding frames.
///
/// `Why this differs from the two pre-existing branch-swap regression
/// tests:` `applying_a_preset_never_bursts_a_branch_above_its_configured_framerate`
/// and `late_watch_after_all_three_tuning_controls_toggled_gets_a_real_keyframe_every_time`
/// both hold the destination's `framerate` field **constant** across their
/// swap (only `tune`/`quality_level` change) — neither one
/// exercises a branch swap whose `framerate` itself is the changed field, or
/// a viewer that is already watching (decoding, not just subscribing) across
/// the seam. This test targets exactly that gap: a real `StreamDecoder`
/// consuming real, continuously-arriving `MediaFrame`s from a real relay,
/// spanning a real `change_destination` framerate jump in both directions.
///
/// What must hold throughout: [`decode::StreamDecoder::latest_frame`] must
/// keep yielding new decoded frames — the gap between two consecutive
/// newly-yielded frames must never exceed a few seconds (measured against
/// [`capture::destination::STALL_RESEND_INTERVAL`]/GOP-length-scale
/// budgets, not the tight sub-frame timing `decode.rs`'s own pacing tests
/// use) either right after the jump, or ever.
#[tokio::test]
#[ignore = "spawns a real relay server child process and real GStreamer capture/decode \
            pipelines; run explicitly with `cargo test -p simple-screensharing-client --lib -- --ignored end_to_end_test:: --skip real_portal --nocapture`"]
async fn big_fps_jump_mid_stream_keeps_viewer_frames_flowing() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();

    const NATIVE_FPS: u32 = 30;
    const LOW_FPS: u32 = 15;
    const HIGH_FPS: u32 = capture::MAX_FRAMERATE;
    // Stress harder than the default test resolution: full HD, forced to the
    // software encoder (`detect_encoder_backend` would otherwise prefer
    // VAAPI on this machine, which has plenty of headroom to absorb a big
    // fps jump) so a jump that outpaces real-time encode speed has a chance
    // to actually show up.
    const STRESS_WIDTH: u32 = 1920;
    const STRESS_HEIGHT: u32 = 1080;

    let port = pick_free_udp_port().expect("failed to pick a free UDP port for the test relay");
    eprintln!("[e2e-fps-jump] starting relay server on 127.0.0.1:{port}");
    let _server = spawn_relay_server(port).expect("failed to spawn relay server child process");
    let server_addr = format!("127.0.0.1:{port}");
    let room_id: RoomId = rand::random();
    let room_key: [u8; 32] = rand::random();
    let join_string = crate::net::format_join_string(&server_addr, &room_id, &room_key);

    let (sharer, _snap_a, mut events_a) =
        connect_with_retry(&join_string, "sharer").await.expect("sharer failed to connect");
    let (viewer, _snap_b, mut events_b) =
        connect_with_retry(&join_string, "viewer").await.expect("viewer failed to connect");
    let owner_conn_id = sharer.conn_id();

    let source_fragment = format!(
        "videotestsrc is-live=true pattern=smpte \
         ! video/x-raw,width={STRESS_WIDTH},height={STRESS_HEIGHT},framerate={NATIVE_FPS}/1"
    );
    eprintln!("[e2e-fps-jump] starting real capture at {LOW_FPS}fps (native source {NATIVE_FPS}fps)");
    let capture_handle =
        capture::start_sharing_from_test_source(&sharer, VIDEO_STREAM_ID, capture::DEFAULT_BITRATE_KBPS, LOW_FPS, &source_fragment)
            .await
            .expect("start_sharing_from_test_source failed");

    // Force the destination down to LOW_FPS explicitly rather than trusting
    // whatever the source negotiated at (matches `start_sharing`'s
    // `source.framerate` default, which may not equal `LOW_FPS`).
    let base = capture_handle
        .destination_runtime(capture::DestKey(0))
        .expect("destination 0 runtime")
        .lock()
        .unwrap()
        .params();
    if base.framerate != LOW_FPS {
        let req = capture_handle
            .change_destination(capture::DestKey(0), VideoParams { framerate: LOW_FPS, ..base })
            .expect("classify/apply initial low-fps change");
        if let Some(done) = req.done {
            tokio::time::timeout(Duration::from_secs(15), done)
                .await
                .expect("initial low-fps swap timed out")
                .expect("swap channel")
                .expect("initial low-fps swap failed");
        }
    }

    eprintln!("[e2e-fps-jump] waiting for viewer to see StreamStarted");
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
    assert_eq!(saw_stream_started, Ok(true), "viewer never observed StreamStarted over the real relay");

    viewer.subscribe(owner_conn_id, VIDEO_STREAM_ID).await.expect("viewer: subscribe() failed");
    let mut decoder = StreamDecoder::new(VIDEO_STREAM_ID).expect("failed to build StreamDecoder");

    // Drains `events_b` for `duration`, forwarding subscriber-count/keyframe
    // events from `events_a` (mirrors the real `ui.rs` event loop, same
    // pattern as `late_watch_after_all_three_tuning_controls_toggled_...`
    // above) and feeding every `MediaFrame` into `decoder`. Returns
    // (largest gap between two consecutive *newly decoded* frames, total
    // media frames received, total decoded frames yielded).
    async fn pump_and_measure(
        events_a: &mut tokio::sync::mpsc::UnboundedReceiver<ClientEvent>,
        events_b: &mut tokio::sync::mpsc::UnboundedReceiver<ClientEvent>,
        capture_handle: &capture::CaptureHandle,
        decoder: &mut StreamDecoder,
        owner_conn_id: crate::protocol::ConnId,
        duration: Duration,
    ) -> (Duration, u64, u64) {
        let mut media_frames = 0u64;
        let mut decoded_frames = 0u64;
        let mut last_decoded_at: Option<std::time::Instant> = None;
        let mut max_gap = Duration::ZERO;
        let deadline = tokio::time::sleep(duration);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => break,
                event = events_a.recv() => {
                    match event {
                        Some(ClientEvent::SubscriberCount { stream_id, count }) if stream_id == VIDEO_STREAM_ID => {
                            capture_handle.notify_subscriber_count(capture::DestKey(0), count);
                        }
                        Some(ClientEvent::KeyframeRequest { stream_id }) if stream_id == VIDEO_STREAM_ID => {
                            capture_handle.notify_keyframe_request(capture::DestKey(0));
                        }
                        Some(_) => {}
                        None => break,
                    }
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
                _ = tokio::time::sleep(Duration::from_millis(20)) => {}
            }
            if let Some(_img) = decoder.latest_frame() {
                decoded_frames += 1;
                let now = std::time::Instant::now();
                if let Some(last) = last_decoded_at {
                    max_gap = max_gap.max(now.saturating_duration_since(last));
                }
                last_decoded_at = Some(now);
            }
        }
        (max_gap, media_frames, decoded_frames)
    }

    eprintln!("[e2e-fps-jump] warming up at {LOW_FPS}fps before touching anything");
    let (warm_gap, warm_media, warm_decoded) =
        pump_and_measure(&mut events_a, &mut events_b, &capture_handle, &mut decoder, owner_conn_id, Duration::from_secs(3)).await;
    eprintln!("[e2e-fps-jump] warmup: max_gap={warm_gap:?}, media={warm_media}, decoded={warm_decoded}");
    assert!(warm_decoded > 0, "viewer never decoded a single frame before any fps change");

    for (label, from_fps, to_fps) in [("jump UP", LOW_FPS, HIGH_FPS), ("jump DOWN", HIGH_FPS, LOW_FPS)] {
        eprintln!("[e2e-fps-jump] {label}: {from_fps}fps -> {to_fps}fps");
        let current = capture_handle
            .destination_runtime(capture::DestKey(0))
            .expect("destination 0 runtime")
            .lock()
            .unwrap()
            .params();
        let target = VideoParams { framerate: to_fps, backend: capture::EncoderBackend::Software, ..current };
        let req = capture_handle.change_destination(capture::DestKey(0), target).expect("classify/apply fps jump");
        eprintln!("[e2e-fps-jump] {label}: classified as {:?}", req.tier);
        if let Some(done) = req.done {
            tokio::time::timeout(Duration::from_secs(15), done)
                .await
                .unwrap_or_else(|_| panic!("{label}: swap timed out"))
                .expect("swap channel")
                .unwrap_or_else(|e| panic!("{label}: swap failed: {e}"));
        }

        let (max_gap, media_frames, decoded_frames) = pump_and_measure(
            &mut events_a,
            &mut events_b,
            &capture_handle,
            &mut decoder,
            owner_conn_id,
            Duration::from_secs(8),
        )
        .await;
        eprintln!(
            "[e2e-fps-jump] {label}: after the swap, max_gap={max_gap:?}, media={media_frames}, decoded={decoded_frames} \
             over 8s"
        );

        assert!(
            decoded_frames > 0,
            "BUG REPRODUCED: {label} ({from_fps}fps -> {to_fps}fps) — the viewer decoded ZERO new frames \
             in the 8s after the fps change (received {media_frames} raw media frames over the relay)"
        );
        assert!(
            max_gap < Duration::from_secs(2),
            "BUG REPRODUCED: {label} ({from_fps}fps -> {to_fps}fps) — the viewer's decoder stalled for \
             {max_gap:?} between two consecutive decoded frames, well past a single dropped frame's worth \
             of latency ({decoded_frames} decoded, {media_frames} raw media frames received)"
        );
    }

    if let Err(e) = capture_handle.stop().await {
        tracing::warn!("[e2e-fps-jump] capture_handle.stop() error (non-fatal): {e:#}");
    }
    drop(viewer);
    let _ = sharer.leave().await;

    eprintln!("[e2e-fps-jump] PASS: viewer kept decoding frames across both big fps jumps");
}

/// Regression test for the "orphan audio-only stream" bug (2026-07-29 — see
/// this task's dispatch notes / `superdoc/features/stream-control.md`):
/// `AppState::start_audio_share` used to announce a live audio stream
/// against **any** `for_capture` id that merely existed, without checking
/// whether that capture was actually live (had a destination). Clicking
/// "+ Add audio" on a staged (pre-"Go live") video card — a real capture
/// with **zero** destinations, per `stage_video_share`'s doc comment — used
/// to create exactly that: a video-less "LIVE" stream visible forever in
/// the room panel with nothing to pair it to.
///
/// Needs no relay server, no real GStreamer pipeline, and no PipeWire — the
/// rejection happens before any of that is touched (see `start_audio_share`'s
/// "Why this checks liveness, not just existence" doc note), so unlike every
/// other test in this module this one is cheap and NOT `#[ignore]`d.
#[tokio::test]
async fn start_audio_share_rejects_a_merely_staged_video_capture() {
    let mut state = crate::session::AppState::default();

    // A real `stage_video_share` call needs a real portal-backed capture
    // pipeline; the only fact this test's gate actually depends on is "zero
    // destinations", so a bare `LocalCapture` with an empty `destinations`
    // map reproduces the exact shape `stage_video_share` leaves behind
    // without needing one.
    let capture_id = state.alloc_capture_id();
    let (_tx, rx) = tokio::sync::oneshot::channel();
    state.captures.insert(
        capture_id,
        crate::session::LocalCapture {
            kind: crate::protocol::StreamKind::Video,
            label: "Screen share".to_string(),
            status: crate::session::ShareStatus::Starting(rx),
            audio_for_capture: None,
            destinations: indexmap::IndexMap::new(),
        },
    );

    state.selected_audio_source = Some(crate::audio::AudioSource { id: 0, name: "Test app".to_string(), serial: None });
    // `room_handle` never gets looked up: the gate must reject before that,
    // so an id naming no real room at all is deliberate here, not sloppy —
    // if this ever *didn't* error, the next line inside `start_audio_share`
    // would be a room lookup against a handle that doesn't exist.
    let bogus_room_handle = crate::session::RoomHandle(0);

    let result = state.start_audio_share(bogus_room_handle, capture_id);
    assert!(
        result.is_err(),
        "BUG REPRODUCED: start_audio_share must reject a for_capture that is only staged \
         (zero destinations), not silently announce an orphan audio-only stream"
    );
    assert_eq!(
        state.captures.len(),
        1,
        "the rejected call must not have created a new (audio) capture entry"
    );
    eprintln!("[audio-gate] PASS: start_audio_share({result:?}) rejected a staged for_capture");
}

/// Companion to the rejection test above: once the paired video capture is
/// actually live (has at least one destination — the exact fact `go_live`/
/// `add_destination_with_params` establish), `start_audio_share` must still
/// work. Uses a real `videotestsrc`-backed capture (no interactive portal,
/// no PipeWire) wired up with a manually-attached destination to reach
/// "live" directly, the same construction `go_live_attaches_the_first_
/// destination_with_real_resolution_and_backend` above uses for the
/// "staged" half — this test just skips straight to the post-`go_live`
/// shape instead of calling `go_live` itself, since that path is already
/// covered by that other test.
#[tokio::test]
#[ignore = "spawns a real relay server child process and a real GStreamer capture pipeline; \
            run explicitly with `cargo test -p simple-screensharing-client --lib -- --ignored end_to_end_test:: --skip real_portal --nocapture`"]
async fn start_audio_share_succeeds_once_the_video_capture_is_live() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();

    let port = pick_free_udp_port().expect("failed to pick a free UDP port for the test relay");
    let _server = spawn_relay_server(port).expect("failed to spawn relay server child process");

    let server_addr = format!("127.0.0.1:{port}");
    let room_id: RoomId = rand::random();
    let room_key: [u8; 32] = rand::random();
    let join_string = crate::net::format_join_string(&server_addr, &room_id, &room_key);

    let (client, snapshot, events) =
        connect_with_retry(&join_string, "sharer").await.expect("sharer failed to connect to the relay after retries");

    let source_fragment = "videotestsrc is-live=true pattern=smpte \
         ! video/x-raw,width=640,height=480,framerate=30/1";
    let capture_handle =
        capture::start_capture_from_test_source(30, source_fragment).await.expect("start_capture_from_test_source failed");
    let source = capture_handle.source_caps();

    let mut state = crate::session::AppState::default();
    let room_handle = state.insert_room(client, snapshot, events, join_string);

    let video_capture_id = state.alloc_capture_id();
    let destination_id = state.alloc_destination_id();
    let params = VideoParams {
        width: source.width,
        height: source.height,
        framerate: 30,
        bitrate_kbps: 2000,
        backend: capture::detect_encoder_backend(),
        tune: capture::TuneMode::default(),
        quality_level: capture::QualityLevel::default(),
        gop_millis: capture::DEFAULT_GOP_MILLIS,
        codec: capture::VideoCodec::default(),
    };
    {
        let room = state.rooms.get_mut(&room_handle).unwrap();
        let stream_id = room.next_stream_id;
        room.next_stream_id = room.next_stream_id.wrapping_add(1);
        room.destinations.push(destination_id);
        state.captures.insert(
            video_capture_id,
            crate::session::LocalCapture {
                kind: crate::protocol::StreamKind::Video,
                label: "Screen share".to_string(),
                status: crate::session::ShareStatus::Active(crate::session::ShareHandle::Video(capture_handle)),
                audio_for_capture: None,
                destinations: indexmap::IndexMap::from([(
                    destination_id,
                    crate::session::Destination {
                        room: room_handle,
                        stream_id,
                        params,
                        runtime: None,
                        pending: None,
                        edit: crate::session::Destination::edit_from(params),
                        apply_rx: None,
                    },
                )]),
            },
        );
    }
    assert_eq!(state.captures[&video_capture_id].destinations.len(), 1, "test setup: video capture must be live");

    state.selected_audio_source = Some(crate::audio::AudioSource { id: 0, name: "Test app".to_string(), serial: None });
    let result = state.start_audio_share(room_handle, video_capture_id);
    assert!(result.is_ok(), "start_audio_share must succeed against a live for_capture, got {result:?}");

    let audio_capture_id = state
        .captures
        .iter()
        .find(|(id, c)| **id != video_capture_id && c.audio_for_capture == Some(video_capture_id))
        .map(|(id, _)| *id)
        .expect("start_audio_share must have created a new capture paired to the video one");
    assert_eq!(
        state.captures[&audio_capture_id].destinations.len(),
        1,
        "the paired audio capture must have been announced with exactly one destination"
    );
    eprintln!("[audio-gate] PASS: start_audio_share succeeded once the paired capture was live");

    if let crate::session::ShareStatus::Active(crate::session::ShareHandle::Video(handle)) =
        state.captures.shift_remove(&video_capture_id).unwrap().status
    {
        let _ = handle.stop().await;
    }
}

/// Regression test for the "no teardown cascade" half of the same bug:
/// stopping/cancelling a video capture must also stop any audio capture
/// paired to it (`LocalCapture::audio_for_capture`), so cancelling a staged
/// (or stopping a live) video share can never leave its audio share running
/// orphaned. Stands two real `videotestsrc` captures in for "video" and
/// "audio" — the cascade logic only ever looks at `audio_for_capture`
/// linkage and `ShareStatus`, never `kind`, so a second video-kind test
/// capture exercises the exact same code path a real audio capture would.
#[tokio::test]
#[ignore = "spawns a real relay server child process and two real GStreamer capture pipelines; \
            run explicitly with `cargo test -p simple-screensharing-client --lib -- --ignored end_to_end_test:: --skip real_portal --nocapture`"]
async fn stop_share_tears_down_a_paired_audio_capture_too() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();

    let port = pick_free_udp_port().expect("failed to pick a free UDP port for the test relay");
    let _server = spawn_relay_server(port).expect("failed to spawn relay server child process");

    let server_addr = format!("127.0.0.1:{port}");
    let room_id: RoomId = rand::random();
    let room_key: [u8; 32] = rand::random();
    let join_string = crate::net::format_join_string(&server_addr, &room_id, &room_key);

    let (client, snapshot, events) =
        connect_with_retry(&join_string, "sharer").await.expect("sharer failed to connect to the relay after retries");

    let source_fragment = "videotestsrc is-live=true pattern=smpte \
         ! video/x-raw,width=640,height=480,framerate=30/1";
    let video_handle =
        capture::start_capture_from_test_source(30, source_fragment).await.expect("video start_capture_from_test_source failed");
    let audio_stand_in_handle =
        capture::start_capture_from_test_source(30, source_fragment).await.expect("audio-stand-in start_capture_from_test_source failed");

    let mut state = crate::session::AppState::default();
    let room_handle = state.insert_room(client, snapshot, events, join_string);

    let make_destination = |state: &mut crate::session::AppState| -> (crate::session::DestinationId, crate::protocol::StreamId) {
        let destination_id = state.alloc_destination_id();
        let room = state.rooms.get_mut(&room_handle).unwrap();
        let stream_id = room.next_stream_id;
        room.next_stream_id = room.next_stream_id.wrapping_add(1);
        room.destinations.push(destination_id);
        (destination_id, stream_id)
    };
    let params = VideoParams {
        width: 640,
        height: 480,
        framerate: 30,
        bitrate_kbps: 2000,
        backend: capture::detect_encoder_backend(),
        tune: capture::TuneMode::default(),
        quality_level: capture::QualityLevel::default(),
        gop_millis: capture::DEFAULT_GOP_MILLIS,
        codec: capture::VideoCodec::default(),
    };

    let video_capture_id = state.alloc_capture_id();
    let (video_dest_id, video_stream_id) = make_destination(&mut state);
    state.captures.insert(
        video_capture_id,
        crate::session::LocalCapture {
            kind: crate::protocol::StreamKind::Video,
            label: "Screen share".to_string(),
            status: crate::session::ShareStatus::Active(crate::session::ShareHandle::Video(video_handle)),
            audio_for_capture: None,
            destinations: indexmap::IndexMap::from([(
                video_dest_id,
                crate::session::Destination {
                    room: room_handle,
                    stream_id: video_stream_id,
                    params,
                    runtime: None,
                    pending: None,
                    edit: crate::session::Destination::edit_from(params),
                    apply_rx: None,
                },
            )]),
        },
    );

    let audio_capture_id = state.alloc_capture_id();
    let (audio_dest_id, audio_stream_id) = make_destination(&mut state);
    state.captures.insert(
        audio_capture_id,
        crate::session::LocalCapture {
            kind: crate::protocol::StreamKind::Audio,
            label: "Test app".to_string(),
            status: crate::session::ShareStatus::Active(crate::session::ShareHandle::Video(audio_stand_in_handle)),
            audio_for_capture: Some(video_capture_id),
            destinations: indexmap::IndexMap::from([(
                audio_dest_id,
                crate::session::Destination {
                    room: room_handle,
                    stream_id: audio_stream_id,
                    params,
                    runtime: None,
                    pending: None,
                    edit: crate::session::Destination::edit_from(params),
                    apply_rx: None,
                },
            )]),
        },
    );

    eprintln!("[stop-cascade] calling stop_share on the VIDEO capture only");
    state.stop_share(video_capture_id);

    assert!(
        matches!(state.captures[&video_capture_id].status, crate::session::ShareStatus::Stopping(_)),
        "stop_share must have moved the video capture itself into Stopping"
    );
    assert!(
        matches!(state.captures[&audio_capture_id].status, crate::session::ShareStatus::Stopping(_)),
        "BUG REPRODUCED: stop_share on a video capture must cascade to any paired audio capture \
         (LocalCapture::audio_for_capture) too, instead of leaving it Active/orphaned"
    );

    // Let both spawned `.stop()` tasks land, same poll-to-completion pattern
    // `go_live_attaches_the_first_destination_with_real_resolution_and_backend`
    // uses for its destination branch above.
    let reaped = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            state.poll_capture_states();
            if !state.captures.contains_key(&video_capture_id) && !state.captures.contains_key(&audio_capture_id) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert_eq!(reaped, Ok(true), "both the video capture and its cascaded-stop audio capture must eventually be reaped");
    eprintln!("[stop-cascade] PASS: paired audio capture was torn down together with its video capture");
}

/// The relay's certificate hash reaches native clients' invite links: connecting
/// with a plain 3-field link yields `current_cert_hash()`, [`ScreenshareClient::invite_link`]
/// round-trips through the 4-field parser, a *stale* 4-field link still connects
/// (native does not enforce the pin), and the hash is stable across connections
/// (the relay persists its certificate). Needs only the relay, no GStreamer.
#[tokio::test]
#[ignore = "spawns a real relay server child process; run with `cargo test end_to_end_test:: -- --ignored`"]
async fn native_client_learns_the_relay_cert_hash_and_tolerates_a_stale_one() {
    let port = pick_free_udp_port().expect("free UDP port");
    let _server = spawn_relay_server(port).expect("spawn relay");
    let server_addr = format!("127.0.0.1:{port}");
    let room_id: RoomId = rand::random();
    let room_key: [u8; 32] = rand::random();

    let plain = crate::net::format_join_string(&server_addr, &room_id, &room_key);
    let (a, _s, _ev_a) = connect_with_retry(&plain, "a").await.expect("connect with a 3-field link");
    let hash = a.current_cert_hash().expect("relay presented a certificate");

    let invite = a.invite_link(&server_addr, Some("Friday | Films"));
    let parsed = crate::net::parse_join_string(&invite).expect("invite link parses");
    assert_eq!(parsed.cert_hash, Some(hash));
    assert_eq!(parsed.name.as_deref(), Some("Friday | Films"));
    assert_eq!(parsed.room_id, room_id);
    assert_eq!(parsed.room_key, room_key);

    // A second client joining through the 4-field invite sees the same hash.
    let (b, _s, _ev_b) = connect_with_retry(&invite, "b").await.expect("connect with a 4-field link");
    assert_eq!(b.current_cert_hash(), Some(hash));

    // A link whose hash is stale (relay rotated) must still connect.
    let stale = crate::net::format_join_string_with_hash(&server_addr, &room_id, &room_key, Some(&[0xEE; 32]));
    let (c, _s, _ev_c) = connect_with_retry(&stale, "c").await.expect("a stale pin must not block a native client");
    assert_eq!(c.current_cert_hash(), Some(hash), "reports what the relay really presented, not what the link said");
}

// ---------------------------------------------------------------------------
// Session-logic tests driving `session::AppState` directly (no UI).
//
// Ported from the old egui `ui::App` test module. They exercise the same
// production session code the UI front-ends call (`insert_room`,
// `add_destination`, `transfer_destination`, `apply_bandwidth_allocation`,
// `drain_events`, ...) against a real relay child process and a real
// GStreamer capture/decode pipeline.
// ---------------------------------------------------------------------------

mod session_logic {
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use indexmap::IndexMap;

    use super::{connect_with_retry, pick_free_udp_port, spawn_relay_server};
    use crate::capture::{self, DestKey};
    use crate::net::ClientEvent;
    use crate::protocol::{ConnId, PresenceEvent, StreamKind};
    use crate::rooms::BandwidthMode;
    use crate::session::{
        AppState, Destination, DestinationId, LocalCapture, PendingRole, RoomHandle, ShareHandle, ShareStatus,
        WatchEntry,
    };

    const TEST_STREAM_ID: u8 = 1;
    /// High enough that a starved ingestion path is obvious, low enough for a
    /// software x264enc/avdec_h264 round trip on ordinary test hardware.
    const TEST_FPS: u32 = 100;

    /// Points settings/rooms files at a throwaway dir.
    ///
    /// `Why:` `set_bandwidth_mode` and `create_room` *persist*, and `rooms.rs`
    /// resolves its config path through `dirs::config_dir()` at call time, so
    /// without this a test would rewrite the developer's own
    /// `~/.config/simple-screensharing/`. `set_var` is `unsafe` because another
    /// thread could read the environment concurrently; this runs before any
    /// room, capture or task of the calling test exists. (Both tests set the
    /// same variable to a per-process dir, so a concurrent run is harmless.)
    fn isolate_config(tag: &str) -> std::path::PathBuf {
        let config_home = std::env::temp_dir().join(format!("sss-{tag}-test-{}", std::process::id()));
        std::fs::create_dir_all(&config_home).expect("temp config dir");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &config_home) };
        config_home
    }

    /// Polls `state` (draining room events and pending work, the way a
    /// front-end's frame tick does) until `done` or the deadline passes.
    async fn wait_for(state: &mut AppState, timeout: Duration, done: impl Fn(&AppState) -> bool) {
        let ctx = egui::Context::default();
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            state.drain_events(&ctx);
            state.poll_capture_states();
            state.poll_pending_destinations();
            state.poll_destination_edits();
            if done(state) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("condition not met within {timeout:?}");
    }

    /// Waits for the watcher to see the sharer's `StreamStarted` presence delta
    /// (the relay silently drops a `Subscribe` for a stream it doesn't know
    /// about yet), then subscribes.
    async fn subscribe_when_announced(state: &mut AppState, room: RoomHandle, owner: ConnId) {
        let session = state.rooms.get_mut(&room).expect("room must be present");
        let saw = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match session.event_rx.recv().await {
                    Some(ClientEvent::PresenceDelta(PresenceEvent::StreamStarted(info)))
                        if info.owner == owner && info.stream_id == TEST_STREAM_ID =>
                    {
                        return true;
                    }
                    Some(_) => continue,
                    None => return false,
                }
            }
        })
        .await;
        assert_eq!(saw, Ok(true), "watcher never observed the sharer's StreamStarted delta");
        session.client.subscribe(owner, TEST_STREAM_ID).await.expect("subscribe() failed");
    }

    /// Regression test for the "fast-forward catch-up" fps bug: media
    /// ingestion (`insert_room`'s `spawn_media_ingestion_task`) must feed the
    /// `StreamDecoder` straight off the raw connection channel, independent of
    /// `AppState::drain_events` ever being called.
    ///
    /// Before the fix nothing but `drain_events` called `feed()`, so with
    /// `drain_events` starved (an unfocused window throttling the frame tick)
    /// the decoder saw nothing. Here `drain_events` is never called, yet the
    /// decoder must keep decoding at close to the sender's rate. Frames are
    /// counted at the decoder output via `FrameSource::frames_decoded()`, so no
    /// consumer polling is involved at all.
    #[tokio::test]
    #[ignore = "spawns a real relay server child process and a real GStreamer capture/decode pipeline; \
                run with `cargo test -p simple-screensharing-client --lib -- --ignored end_to_end_test:: --skip real_portal`"]
    async fn media_ingestion_keeps_feeding_the_decoder_while_drain_events_is_starved() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        gstreamer::init().expect("gstreamer init");
        let config_home = isolate_config("ingestion");

        let port = pick_free_udp_port().expect("failed to pick a free UDP port for the test relay");
        let _server = spawn_relay_server(port).expect("failed to spawn relay server child process");
        let mut state = AppState::default();
        let join_string =
            state.create_room(&format!("127.0.0.1:{port}"), "ingestion").expect("create_room").join_string;

        let (client_a, _snapshot_a, events_a) =
            connect_with_retry(&join_string, "sharer").await.expect("sharer failed to connect");
        let (client_b, snapshot_b, events_b) =
            connect_with_retry(&join_string, "watcher").await.expect("watcher failed to connect");
        let _events_a_keepalive = events_a;

        let source_fragment = format!(
            "videotestsrc is-live=true pattern=ball \
             ! clockoverlay time-format=\"%H:%M:%S%.3f\" \
             ! video/x-raw,width=320,height=240,framerate={TEST_FPS}/1"
        );
        let capture_handle =
            capture::start_sharing_from_test_source(&client_a, TEST_STREAM_ID, 4000, TEST_FPS, &source_fragment)
                .await
                .expect("start_sharing_from_test_source failed");
        let owner = client_a.conn_id();

        // `insert_room` is where the ingestion task is spawned in production;
        // using it (not a hand-rolled `RoomSession`) tests the real wiring.
        state.framerate = TEST_FPS;
        let room = state.insert_room(client_b, snapshot_b, events_b, join_string.clone());
        state.active_room = Some(room);
        let decoder = crate::decode::StreamDecoder::new(TEST_STREAM_ID).expect("failed to build StreamDecoder");
        let frames = decoder.frames();
        state.rooms[&room].watches.lock().unwrap().insert(
            (owner, TEST_STREAM_ID),
            WatchEntry {
                decoder: Arc::new(Mutex::new(decoder)),
                audio_stream_id: None,
            },
        );

        // Observe the StreamStarted delta straight off the room's forward
        // channel (not via `drain_events`), then subscribe. Nothing drains
        // events through `AppState` for the rest of the test.
        subscribe_when_announced(&mut state, room, owner).await;

        let start_count = frames.frames_decoded();
        let started = Instant::now();
        tokio::time::sleep(Duration::from_secs(5)).await;
        let decoded = frames.frames_decoded() - start_count;
        let elapsed = started.elapsed().as_secs_f64();
        let decoded_fps = frames.stats().decoded_fps;
        eprintln!(
            "[ingestion] drain_events never called: {decoded} frames in {elapsed:.1}s \
             ({:.1} fps average), rolling decoded_fps = {decoded_fps:.1}",
            decoded as f64 / elapsed
        );

        if let Err(e) = capture_handle.stop().await {
            tracing::warn!("[ingestion] capture_handle.stop() reported an error (non-fatal): {e:#}");
        }
        let _ = std::fs::remove_dir_all(&config_home);

        assert!(
            decoded as f64 / elapsed > 40.0 && decoded_fps > 40.0,
            "decoder starved ({decoded} frames in {elapsed:.1}s, rolling {decoded_fps:.1} fps) \
             with drain_events never called - media delivery still depends on drain_events, \
             i.e. the 'fast-forward catch-up' backlog bug is not fixed"
        );
    }

    /// Stream Control, end to end against a real relay and a real GStreamer
    /// capture: **fan-out** (one capture, a second independently encoded and
    /// encrypted destination in another room), **transfer** (make-before-break
    /// to a third room) and **Absolute-Maximum bandwidth**
    /// (`bandwidth::allocate` applied to the live encoders).
    ///
    /// `Why` all three in one test: they share one expensive fixture (a relay
    /// child process, three real QUIC connections, an encode pipeline), and the
    /// interesting failures are in how they interact - a fan-out that leaves the
    /// allocator with a destination it can't find, or a transfer that completes
    /// but leaves a stale destination behind, would pass a narrower test.
    #[tokio::test]
    #[ignore = "spawns a real relay server child process and a real GStreamer capture pipeline; \
                run with `cargo test -p simple-screensharing-client --lib -- --ignored end_to_end_test:: --skip real_portal`"]
    async fn fan_out_transfer_and_absolute_bandwidth_across_three_rooms() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        gstreamer::init().expect("gstreamer init");
        let config_home = isolate_config("stream-control");

        let port = pick_free_udp_port().expect("failed to pick a free UDP port for the test relay");
        let _server = spawn_relay_server(port).expect("failed to spawn relay server child process");
        let server_addr = format!("127.0.0.1:{port}");

        let mut state = AppState::default();
        let mut handles = Vec::new();
        let mut clients = Vec::new();
        for name in ["room-a", "room-b", "room-c"] {
            let join_string = state.create_room(&server_addr, name).expect("create_room").join_string;
            let (client, snapshot, events) = connect_with_retry(&join_string, name)
                .await
                .unwrap_or_else(|e| panic!("{name} failed to connect: {e:#}"));
            clients.push(client.clone());
            handles.push(state.insert_room(client, snapshot, events, join_string));
        }
        let (room_a, room_b, room_c) = (handles[0], handles[1], handles[2]);

        // Start a capture into room A as `start_video_share` does, then adopt
        // the handle the way `poll_capture_states` does. (The real
        // `start_video_share` goes through the interactive portal, which no
        // unattended test can drive - hence the `TestSource` seam.)
        let capture_id = state.alloc_capture_id();
        let destination_a = state.alloc_destination_id();
        assert_eq!(destination_a, DestinationId(0), "the test source's DestKey(0) must match the first destination id");
        let stream_id = {
            let room = state.rooms.get_mut(&room_a).expect("room A");
            let stream_id = room.next_stream_id;
            room.next_stream_id += 1;
            room.destinations.push(destination_a);
            stream_id
        };
        let source_fragment = "videotestsrc is-live=true pattern=ball \
             ! video/x-raw,width=320,height=240,framerate=30/1";
        let capture_handle =
            capture::start_sharing_from_test_source(&clients[0], stream_id, 4_000, 30, source_fragment)
                .await
                .expect("start_sharing_from_test_source failed");
        let runtime = capture_handle.destination_runtime(DestKey(destination_a.0));
        let params = runtime.as_ref().expect("the first destination must exist").lock().unwrap().params();
        state.captures.insert(
            capture_id,
            LocalCapture {
                kind: StreamKind::Video,
                label: "test capture".to_string(),
                status: ShareStatus::Active(ShareHandle::Video(capture_handle)),
                audio_for_capture: None,
                destinations: IndexMap::from([(
                    destination_a,
                    Destination {
                        room: room_a,
                        stream_id,
                        params,
                        runtime,
                        pending: None,
                        edit: Destination::edit_from(params),
                        apply_rx: None,
                    },
                )]),
            },
        );

        // --- Fan-out: the same capture into room B as a second stream. ---
        state.add_destination(capture_id, room_b, PendingRole::FanOut);
        wait_for(&mut state, Duration::from_secs(20), |s| s.pending_destinations.is_empty()).await;
        assert!(state.share_error.is_none(), "fan-out reported an error: {:?}", state.share_error);
        {
            let capture = state.captures.get(&capture_id).expect("capture must still exist");
            assert_eq!(capture.destinations.len(), 2, "fan-out must add a second destination to the same capture");
            let rooms: HashSet<RoomHandle> = capture.destinations.values().map(|d| d.room).collect();
            assert_eq!(rooms, HashSet::from([room_a, room_b]), "the two destinations must be in different rooms");
            for (id, dest) in &capture.destinations {
                assert!(dest.runtime.is_some(), "destination {id:?} has no live runtime after fan-out");
            }
        }
        // Both destinations must reach `Live` - each has its own encode branch
        // producing IDRs, which is what makes fan-out two real encoded streams
        // rather than one stream claimed twice.
        wait_for(&mut state, Duration::from_secs(20), |s| {
            s.captures[&capture_id]
                .destinations
                .values()
                .all(|d| d.state() == capture::destination::DestinationState::Live)
        })
        .await;

        // --- Absolute-Maximum bandwidth over both destinations. ---
        state.set_bandwidth_total(6_000);
        state.set_bandwidth_floor(500);
        state.set_bandwidth_mode(BandwidthMode::Absolute);
        state.apply_bandwidth_allocation();
        let applied: Vec<u32> = state.captures[&capture_id]
            .destinations
            .keys()
            .map(|id| state.allocated_kbps(*id).unwrap_or_else(|| panic!("{id:?} got no allocation")))
            .collect();
        assert_eq!(applied.iter().sum::<u32>(), 6_000, "the whole per-client budget should be assigned: {applied:?}");
        assert_eq!(state.bandwidth_overshoot_kbps(), 0, "6000 kbps comfortably covers two 500 kbps floors");

        // A second immediate call must be rate-limited away entirely.
        let before = state.bandwidth.applied_kbps(destination_a);
        state.set_bandwidth_total(40_000);
        state.apply_bandwidth_allocation();
        assert_eq!(
            state.bandwidth.applied_kbps(destination_a),
            before,
            "a recompute inside the rate-limit window must not reach the encoders"
        );

        // --- Transfer room B's destination to room C, make-before-break. ---
        let destination_b = *state.captures[&capture_id]
            .destinations
            .iter()
            .find(|(_, d)| d.room == room_b)
            .map(|(id, _)| id)
            .expect("room B destination");
        state.transfer_destination(destination_b, room_c);
        // While in flight the *old* destination is still there and still marked
        // as the transfer's source - the "make" half being visible rather than
        // a silent swap.
        assert!(
            matches!(
                state.captures[&capture_id].destinations[&destination_b].pending,
                Some(PendingRole::TransferSource(room)) if room == room_c
            ),
            "the source destination should be marked mid-transfer"
        );

        wait_for(&mut state, Duration::from_secs(20), |s| s.pending_destinations.is_empty()).await;
        assert!(state.share_error.is_none(), "transfer reported an error: {:?}", state.share_error);
        {
            let capture = state.captures.get(&capture_id).expect("capture must survive a transfer");
            let rooms: HashSet<RoomHandle> = capture.destinations.values().map(|d| d.room).collect();
            assert_eq!(
                rooms,
                HashSet::from([room_a, room_c]),
                "after the transfer the capture must feed room A and room C, and no longer room B"
            );
            assert_eq!(capture.destinations.len(), 2, "a transfer must not leave the old destination behind");
            assert!(
                capture.destinations.values().all(|d| d.pending.is_none()),
                "no destination should still be marked mid-transfer once it settled"
            );
        }
        // Room A's destination - the one nobody touched - must have kept
        // streaming across both the fan-out and the transfer.
        assert_eq!(
            state.captures[&capture_id].destinations[&destination_a].state(),
            capture::destination::DestinationState::Live,
            "an unrelated destination of the same capture must be untouched by fan-out/transfer"
        );
        // Mode switch back with settled destinations must not panic.
        state.set_bandwidth_mode(BandwidthMode::PerStream);

        state.stop_all_captures_fire_and_forget();
        tokio::time::sleep(Duration::from_secs(1)).await;
        let _ = std::fs::remove_dir_all(&config_home);
    }
}
