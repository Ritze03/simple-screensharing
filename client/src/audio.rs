//! Per-application audio capture: a PipeWire virtual loopback (`pw-loopback`)
//! + non-destructive port linking (`pw-link`) taps a specific application's
//! *existing* audio output, without touching its normal routing, and the
//! resulting virtual source is Opus-encoded (GStreamer) → AEAD-encrypted →
//! fragmented → sent, over the same QUIC media-datagram plane `capture.rs`
//! uses for video (see `net.rs` / `fragment.rs`).
//!
//! # Approach
//!
//! PipeWire's own CLI tools are shelled out to via [`std::process::Command`]
//! rather than binding `libpipewire` directly — simpler to get right and
//! maintain for this project, per the accepted design (based on how
//! ScreegoElectronScreenshare solves the same problem on Linux):
//!
//! 1. [`list_audio_sources`] runs `pw-dump` (PipeWire's full graph, as JSON)
//!    and returns every node with `media.class = "Stream/Output/Audio"` —
//!    i.e. every application currently producing audio — confirmed against
//!    this machine's actual `pw-dump` output (GStreamer 1.28.5 / PipeWire
//!    1.6.8), not assumed from documentation.
//! 2. [`start_audio_sharing`] spawns a `pw-loopback` process. Undecorated,
//!    `pw-loopback --name X` creates *two* stream nodes: `input.X`
//!    (`Stream/Input/Audio`, i.e. something other nodes can send audio
//!    *into*) and `output.X` (`Stream/Output/Audio`, i.e. something that can
//!    be captured *from*) — verified live on this machine via `pw-dump`.
//!    Both default to `node.autoconnect=true`, which would otherwise route
//!    `input.X` from the system's default mic and `output.X` out to the
//!    system's default speakers; both are disabled here
//!    (`-i node.autoconnect=false -o node.autoconnect=false`) since this
//!    loopback exists purely as a private tap, not a real playback/capture
//!    device.
//! 3. The target application's existing *output* ports are `pw-link`ed into
//!    `input.X`'s *input* ports (by numeric port id — `pw-link <id> <id>`
//!    works directly, verified live, and sidesteps channel-name/count
//!    mismatches that string-pattern port names could hit). This is
//!    additive: the application's original routing (e.g. to real speakers)
//!    is completely untouched — confirmed live by watching `pw-link -l`
//!    show both the original link and the new one simultaneously.
//! 4. `output.X` is then captured like any normal PipeWire audio source:
//!    `pipewiresrc path=<output.X's node id> ! audioconvert ! audioresample
//!    ! opusenc ! appsink`, verified live end-to-end (real `440`/`550`Hz
//!    `audiotestsrc` app → loopback → Opus packets out of `appsink`).
//!
//! # Threading / async model
//!
//! Identical to `capture.rs`: GStreamer drives buffer flow on its own
//! element threads once the pipeline is `Playing`; this module observes it
//! via `gstreamer-app`'s [`gst_app::AppSink::stream`] and `gstreamer`'s
//! [`gst::Bus::stream`], bridged into the ambient tokio runtime with
//! `tokio::select!` inside [`pump_loop`] — no separate pump thread.
//!
//! # Scope
//!
//! Application-output audio only, per this project's design — never
//! microphone/physical-input capture. UI wiring (a source-picker for
//! [`list_audio_sources`]) is a later task; this module is just the API.

use std::collections::HashMap;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use futures_util::StreamExt;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::fragment::fragment_frame;
use crate::net::{encrypt_frame, ScreenshareClient};
use crate::protocol::StreamKind;

/// Prefix every `pw-loopback --name` this module creates uses, so
/// [`list_audio_sources`] can recognize and exclude our own loopback nodes
/// (they otherwise show up as `Stream/Output/Audio` too, once running).
const LOOPBACK_NAME_PREFIX: &str = "ssshare-loopback-";

/// Ensures `gst::init()` runs exactly once no matter how many times
/// [`start_audio_sharing`] is called — same pattern as `capture.rs`'s
/// private helper of the same name (not shared: `capture.rs` is
/// intentionally not modified/imported from here).
fn ensure_gstreamer_initialized() -> Result<()> {
    use std::sync::OnceLock;
    static INIT: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    match INIT.get_or_init(|| gst::init().map_err(|e| e.to_string())) {
        Ok(()) => Ok(()),
        Err(e) => Err(anyhow!("gstreamer initialization failed: {e}")),
    }
}

// ---------------------------------------------------------------------
// pw-dump parsing
// ---------------------------------------------------------------------

/// One object out of `pw-dump`'s top-level JSON array. Deliberately minimal
/// — only the fields this module actually reads — so it tolerates whatever
/// extra fields/object kinds a given PipeWire version emits. `info.props` is
/// a raw JSON map rather than named fields since which properties a node/port
/// carries varies by object kind.
#[derive(Debug, Deserialize)]
struct PwObject {
    id: u32,
    #[serde(rename = "type", default)]
    obj_type: Option<String>,
    #[serde(default)]
    info: Option<PwInfo>,
}

#[derive(Debug, Deserialize)]
struct PwInfo {
    /// Present (as `"output"`/`"input"`) on `PipeWire:Interface:Port`
    /// objects; absent on nodes.
    #[serde(default)]
    direction: Option<String>,
    #[serde(default)]
    props: Option<Map<String, Value>>,
}

fn prop_str<'a>(props: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    props.get(key).and_then(Value::as_str)
}

fn prop_u64(props: &Map<String, Value>, key: &str) -> Option<u64> {
    props.get(key).and_then(Value::as_u64)
}

/// Runs `pw-dump` and parses its JSON output.
fn run_pw_dump() -> Result<Vec<PwObject>> {
    let output = Command::new("pw-dump")
        .output()
        .context("failed to run pw-dump (is PipeWire installed and running?)")?;
    if !output.status.success() {
        bail!(
            "pw-dump exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    serde_json::from_slice(&output.stdout).context("failed to parse pw-dump JSON output")
}

// ---------------------------------------------------------------------
// Public API: source enumeration
// ---------------------------------------------------------------------

/// One currently-running application audio stream, discoverable via
/// [`list_audio_sources`] and passed to [`start_audio_sharing`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioSource {
    /// The PipeWire node id (as of the moment it was listed — nodes can
    /// disappear if the application stops producing audio; a stale id just
    /// makes [`start_audio_sharing`] fail with a clear error).
    pub id: u32,
    /// Best-effort human-readable label: `node.description`, falling back
    /// to `application.name`, then `media.name`, then the raw `node.name`.
    pub name: String,
}

/// Lists every application currently producing audio: PipeWire nodes with
/// `media.class = "Stream/Output/Audio"` (confirmed against this machine's
/// real `pw-dump` output — see module docs), excluding this module's own
/// loopback nodes (see [`LOOPBACK_NAME_PREFIX`]) so an in-progress audio
/// share doesn't list itself as a shareable source.
pub fn list_audio_sources() -> Result<Vec<AudioSource>> {
    let objects = run_pw_dump()?;
    Ok(extract_audio_sources(&objects))
}

fn extract_audio_sources(objects: &[PwObject]) -> Vec<AudioSource> {
    let loopback_output_prefix = format!("output.{LOOPBACK_NAME_PREFIX}");
    let mut sources = Vec::new();
    for obj in objects {
        if obj.obj_type.as_deref() != Some("PipeWire:Interface:Node") {
            continue;
        }
        let Some(info) = &obj.info else { continue };
        let Some(props) = &info.props else { continue };
        if prop_str(props, "media.class") != Some("Stream/Output/Audio") {
            continue;
        }
        let node_name = prop_str(props, "node.name").unwrap_or_default();
        if node_name.starts_with(&loopback_output_prefix) {
            continue;
        }
        let name = prop_str(props, "node.description")
            .or_else(|| prop_str(props, "application.name"))
            .or_else(|| prop_str(props, "media.name"))
            .filter(|s| !s.is_empty())
            .unwrap_or(node_name)
            .to_string();
        let name = if name.is_empty() { format!("(unnamed stream {})", obj.id) } else { name };
        sources.push(AudioSource { id: obj.id, name });
    }
    sources
}

/// Output-direction port object ids of `node_id`, ordered by PipeWire's
/// `port.id` (i.e. channel order as the node declares it — typically
/// FL, FR, ...).
fn node_port_ids(objects: &[PwObject], node_id: u64, direction: &str) -> Vec<u32> {
    let mut ports: Vec<(u64, u32)> = Vec::new();
    for obj in objects {
        if obj.obj_type.as_deref() != Some("PipeWire:Interface:Port") {
            continue;
        }
        let Some(info) = &obj.info else { continue };
        if info.direction.as_deref() != Some(direction) {
            continue;
        }
        let Some(props) = &info.props else { continue };
        if prop_u64(props, "node.id") != Some(node_id) {
            continue;
        }
        let order = prop_u64(props, "port.id").unwrap_or(0);
        ports.push((order, obj.id));
    }
    ports.sort_by_key(|(order, _)| *order);
    ports.into_iter().map(|(_, id)| id).collect()
}

/// The two stream nodes one `pw-loopback --name <loopback_name>` process
/// creates (see module docs for what each end is for).
struct LoopbackNodes {
    /// `output.<loopback_name>`, `Stream/Output/Audio` — what we capture
    /// from via `pipewiresrc`.
    output_node_id: u32,
    /// `input.<loopback_name>`, `Stream/Input/Audio` — what we `pw-link`
    /// the target application's output ports into.
    input_node_id: u32,
}

fn find_loopback_nodes(objects: &[PwObject], loopback_name: &str) -> Option<LoopbackNodes> {
    let output_name = format!("output.{loopback_name}");
    let input_name = format!("input.{loopback_name}");
    let mut nodes: HashMap<&str, u32> = HashMap::new();
    for obj in objects {
        if obj.obj_type.as_deref() != Some("PipeWire:Interface:Node") {
            continue;
        }
        let Some(info) = &obj.info else { continue };
        let Some(props) = &info.props else { continue };
        let Some(name) = prop_str(props, "node.name") else { continue };
        if name == output_name {
            nodes.insert("output", obj.id);
        } else if name == input_name {
            nodes.insert("input", obj.id);
        }
    }
    Some(LoopbackNodes {
        output_node_id: *nodes.get("output")?,
        input_node_id: *nodes.get("input")?,
    })
}

// ---------------------------------------------------------------------
// pw-loopback / pw-link process management
// ---------------------------------------------------------------------

/// Spawns `pw-loopback --name <loopback_name> -i node.autoconnect=false -o
/// node.autoconnect=false` as a long-lived background process — it must
/// keep running for the duration of the capture; killing it (see
/// [`AudioCaptureHandle::stop`]) is what tears the loopback (and its
/// `pw-link` connections) down.
fn spawn_loopback(loopback_name: &str) -> Result<Child> {
    Command::new("pw-loopback")
        .args(["--name", loopback_name, "-i", "node.autoconnect=false", "-o", "node.autoconnect=false"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to spawn pw-loopback (is PipeWire's pw-loopback tool installed?)")
}

/// Polls `pw-dump` until `loopback_name`'s two stream nodes show up (they
/// take a moment to register after the process starts) or `timeout` elapses.
async fn wait_for_loopback_nodes(loopback_name: &str, timeout: Duration) -> Result<LoopbackNodes> {
    let deadline = Instant::now() + timeout;
    loop {
        let objects = run_pw_dump()?;
        if let Some(nodes) = find_loopback_nodes(&objects, loopback_name) {
            return Ok(nodes);
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for pw-loopback '{loopback_name}' to appear in the PipeWire graph");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Links `source_node_id`'s output ports into `loopback_input_node_id`'s
/// input ports, pairing them in port-id order (channel 0 → channel 0, ...),
/// up to however many pairs exist on the shorter side. Non-destructive: this
/// only adds a link, it never touches the source's existing routing (spec'd
/// approach — verified live, see module docs).
fn link_source_into_loopback(source_node_id: u32, loopback_input_node_id: u32) -> Result<()> {
    let objects = run_pw_dump()?;
    let source_ports = node_port_ids(&objects, source_node_id as u64, "output");
    let sink_ports = node_port_ids(&objects, loopback_input_node_id as u64, "input");

    if source_ports.is_empty() {
        bail!("audio source node {source_node_id} has no output ports to link (it may have stopped producing audio)");
    }
    if sink_ports.is_empty() {
        bail!("loopback sink node {loopback_input_node_id} has no input ports");
    }

    let pair_count = source_ports.len().min(sink_ports.len());
    for i in 0..pair_count {
        let (out_port, in_port) = (source_ports[i], sink_ports[i]);
        let status = Command::new("pw-link")
            .arg(out_port.to_string())
            .arg(in_port.to_string())
            .status()
            .context("failed to run pw-link")?;
        if !status.success() {
            bail!("pw-link failed to connect port {out_port} -> {in_port} (exit status: {status})");
        }
    }
    Ok(())
}

fn kill_and_wait(child: &mut Child) {
    if let Err(e) = child.kill() {
        tracing::warn!("error killing pw-loopback process: {e}");
    }
    let _ = child.wait();
}

// ---------------------------------------------------------------------
// GStreamer pipeline construction
// ---------------------------------------------------------------------

fn build_pipeline(loopback_output_node_id: u32) -> Result<(gst::Pipeline, gst_app::AppSink)> {
    let description = format!(
        "pipewiresrc path={loopback_output_node_id} do-timestamp=true \
         ! audioconvert \
         ! audioresample \
         ! opusenc \
         ! appsink name=sink emit-signals=false sync=false max-buffers=32 drop=true"
    );

    let element = gst::parse::launch(&description)
        .with_context(|| format!("failed to construct audio capture pipeline: {description}"))?;
    let pipeline = element
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("gst::parse::launch did not return a top-level Pipeline"))?;

    let sink = pipeline
        .by_name("sink")
        .ok_or_else(|| anyhow!("pipeline has no element named 'sink'"))?
        .downcast::<gst_app::AppSink>()
        .map_err(|_| anyhow!("'sink' element is not an AppSink"))?;

    Ok((pipeline, sink))
}

// ---------------------------------------------------------------------
// Public API: capture lifecycle
// ---------------------------------------------------------------------

/// A running per-application audio share started by [`start_audio_sharing`].
/// Prefer calling [`AudioCaptureHandle::stop`] for graceful teardown (it
/// also drains the pump task and notifies the relay); if the handle is
/// dropped without it, `Drop` still synchronously stops the pipeline and
/// kills the `pw-loopback` process as a backstop, so no process/tap is
/// leaked — it just skips the graceful drain/notify steps.
pub struct AudioCaptureHandle {
    stream_id: u8,
    client: ScreenshareClient,
    pipeline: gst::Pipeline,
    loopback_child: Child,
    pump_task: tokio::task::JoinHandle<()>,
}

/// Starts sharing `source`'s audio on `stream_id`: spins up a private
/// PipeWire loopback, non-destructively taps `source`'s existing output
/// ports into it, builds and starts the Opus capture/encode pipeline, and
/// (once the pipeline has actually reached `Playing` — mirroring
/// `capture.rs::start_sharing`'s "announce only after real capture starts"
/// ordering) calls [`ScreenshareClient::announce_stream`] so other room
/// members see the stream appear.
///
/// Returns `Err` — never panics — if `pw-loopback`/`pw-link`/`pw-dump` are
/// unavailable or fail, the source's ports have disappeared, or the
/// GStreamer pipeline fails to construct/start. Any partially-created
/// loopback process is cleaned up before returning an error.
pub async fn start_audio_sharing(client: &ScreenshareClient, stream_id: u8, source: &AudioSource) -> Result<AudioCaptureHandle> {
    ensure_gstreamer_initialized()?;

    // stream_id + a random suffix: unique even if two audio shares started
    // in quick succession reuse a stream_id across reconnects.
    let loopback_name = format!("{LOOPBACK_NAME_PREFIX}{stream_id}-{}", uuid::Uuid::new_v4().simple());

    let mut loopback_child = spawn_loopback(&loopback_name)?;

    let loopback = match wait_for_loopback_nodes(&loopback_name, Duration::from_secs(5)).await {
        Ok(v) => v,
        Err(e) => {
            kill_and_wait(&mut loopback_child);
            return Err(e);
        }
    };

    if let Err(e) = link_source_into_loopback(source.id, loopback.input_node_id) {
        kill_and_wait(&mut loopback_child);
        return Err(e);
    }

    let (pipeline, appsink) = match build_pipeline(loopback.output_node_id) {
        Ok(v) => v,
        Err(e) => {
            kill_and_wait(&mut loopback_child);
            return Err(e);
        }
    };

    if let Err(e) = pipeline.set_state(gst::State::Playing).context("failed to start audio capture pipeline") {
        let _ = pipeline.set_state(gst::State::Null);
        kill_and_wait(&mut loopback_child);
        return Err(e);
    }

    // Wait for the pipeline to actually get moving (or fail) before telling
    // the room this stream exists, same rationale as capture.rs.
    let (state_result, _current, _pending) = pipeline.state(gst::ClockTime::from_seconds(5));
    if let Err(e) = state_result {
        let _ = pipeline.set_state(gst::State::Null);
        kill_and_wait(&mut loopback_child);
        return Err(anyhow!("audio capture pipeline failed to reach PLAYING: {e:?}"));
    }

    let stream = appsink.stream();
    let bus = match pipeline.bus() {
        Some(b) => b,
        None => {
            let _ = pipeline.set_state(gst::State::Null);
            kill_and_wait(&mut loopback_child);
            return Err(anyhow!("audio pipeline has no bus"));
        }
    };
    let bus_stream = bus.stream();

    let pump_client = client.clone();
    let pump_task = tokio::spawn(pump_loop(pump_client, stream_id, stream, bus_stream));

    // No width/height for audio (spec's AnnounceStream fields exist for
    // video streams' benefit); 0x0 signals "not applicable".
    if let Err(e) = client
        .announce_stream(stream_id, StreamKind::Audio, &source.name, 0, 0)
        .await
        .context("failed to announce audio stream to the relay")
    {
        let _ = pipeline.set_state(gst::State::Null);
        pump_task.abort();
        kill_and_wait(&mut loopback_child);
        return Err(e);
    }

    Ok(AudioCaptureHandle {
        stream_id,
        client: client.clone(),
        pipeline,
        loopback_child,
        pump_task,
    })
}

impl Drop for AudioCaptureHandle {
    /// Synchronous backstop for the case where [`AudioCaptureHandle::stop`]
    /// was never called (a panic, an app crash, or a caller that just drops
    /// the handle instead of awaiting graceful teardown — confirmed to
    /// happen on the UI's abrupt-disconnect path). Without this, the
    /// `pw-loopback` child process (whose own `Drop` impl, per
    /// `std::process::Child`, does *not* kill the OS process) is orphaned
    /// and keeps tapping audio indefinitely.
    ///
    /// This is a backstop, not a replacement for `stop()`'s graceful
    /// teardown (which also drains the pump task and notifies the relay) —
    /// when `stop()` *did* run, `self` is already fully torn down by the
    /// time this runs, so setting an already-`Null` pipeline to `Null` again
    /// and killing an already-exited child are both harmless no-ops.
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
        let _ = self.loopback_child.kill();
        let _ = self.loopback_child.wait(); // reap, avoid a zombie
    }
}

impl AudioCaptureHandle {
    /// Tears the audio share down cleanly: stops the GStreamer pipeline,
    /// waits for the background send task to finish draining, kills the
    /// `pw-loopback` process (which also removes its `pw-link` connections —
    /// PipeWire drops a link automatically as soon as either endpoint's node
    /// disappears, verified live: no separate `pw-link -d` teardown needed),
    /// and tells the relay this stream is gone (best-effort — a network
    /// error here is logged, not returned, since capture has already
    /// stopped either way).
    pub async fn stop(mut self) -> Result<()> {
        // Ask the pipeline to end gracefully by sending it a real EOS event
        // *before* forcing it to `Null`, and wait for the pump task to
        // notice — same fix, same rationale, as `capture.rs::CaptureHandle::stop`:
        // `gstreamer_app::AppSinkStream` (what `pump_loop` awaits via
        // `appsink_stream.next()`) only ever yields its terminal `None` once
        // the appsink's `is_eos()` flag is actually set, which only happens
        // once a real EOS event has flowed all the way through the pipeline
        // into it. Jumping straight to `set_state(Null)` (as this used to
        // do) never sends that event — it just tears the elements down — so
        // the appsink's stream would sit `Pending` forever and `pump_task`
        // would never complete, hanging this whole function (and orphaning
        // the still-running `pw-loopback` child, since it was only killed
        // *after* the pump task join) indefinitely. Confirmed via a live
        // end-to-end "Stop Sharing Audio" test that hung in "Stopping..."
        // for 90+ seconds.
        //
        // The `timeout` is a backstop, not the expected path: a healthy
        // pipeline should drain and reach EOS in well under a second, but
        // this guarantees `stop()` itself can never hang the caller forever
        // if the source somehow never honors EOS.
        if !self.pipeline.send_event(gst::event::Eos::new()) {
            tracing::warn!("audio capture pipeline did not accept the EOS event; forcing a hard stop");
        }

        // `&mut` rather than moving `self.pump_task` out: `AudioCaptureHandle`
        // implements `Drop` (see below), which forbids moving individual
        // fields out of `self` by value — `JoinHandle` is `Unpin`, so
        // awaiting through `&mut` works identically.
        match tokio::time::timeout(Duration::from_secs(5), &mut self.pump_task).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!("audio capture send task did not shut down cleanly: {e}"),
            Err(_) => tracing::warn!(
                "audio capture send task did not observe EOS within 5s; forcing pipeline to Null anyway"
            ),
        }

        if let Err(e) = self.pipeline.set_state(gst::State::Null) {
            tracing::warn!("error stopping audio capture pipeline: {e}");
        }

        // Unconditional regardless of whether EOS was observed cleanly or
        // the timeout fallback fired above — "Stop Sharing Audio" must
        // never leave `pw-loopback` orphaned.
        kill_and_wait(&mut self.loopback_child);

        if let Err(e) = self.client.stop_stream(self.stream_id).await {
            tracing::warn!("error notifying relay that audio stream {} stopped: {e:#}", self.stream_id);
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------
// Encode → encrypt → fragment → send pump
// ---------------------------------------------------------------------

/// Drains encoded Opus packets from `appsink` and pipeline error/EOS
/// messages from `bus` concurrently — identical structure to
/// `capture.rs::pump_loop`, duplicated rather than shared since the two
/// modules are meant to stay independent (see this module's task scope).
async fn pump_loop(
    client: ScreenshareClient,
    stream_id: u8,
    mut appsink_stream: gst_app::app_sink::AppSinkStream,
    mut bus_stream: gst::bus::BusStream,
) {
    let start = Instant::now();
    let mut appsink_done = false;
    let mut bus_done = false;

    loop {
        if appsink_done && bus_done {
            return;
        }
        tokio::select! {
            sample = appsink_stream.next(), if !appsink_done => {
                match sample {
                    Some(sample) => {
                        if let Err(e) = send_encoded_sample(&client, stream_id, &sample, start) {
                            tracing::warn!("dropping encoded audio packet for stream {stream_id}: {e:#}");
                        }
                    }
                    None => appsink_done = true,
                }
            }
            msg = bus_stream.next(), if !bus_done => {
                match msg {
                    Some(msg) => handle_bus_message(&msg, &mut bus_done),
                    None => bus_done = true,
                }
            }
        }
    }
}

fn handle_bus_message(msg: &gst::Message, bus_done: &mut bool) {
    match msg.view() {
        gst::MessageView::Error(err) => {
            tracing::error!(
                "audio capture pipeline error from {}: {} ({})",
                msg.src().map(|s| s.path_string().to_string()).unwrap_or_else(|| "unknown".into()),
                err.error(),
                err.debug().unwrap_or_default(),
            );
            *bus_done = true;
        }
        gst::MessageView::Eos(_) => {
            tracing::info!("audio capture pipeline reached EOS");
            *bus_done = true;
        }
        _ => {}
    }
}

/// Encrypts, fragments, and sends one encoded Opus packet as media
/// datagrams — same encrypt/fragment/send path as
/// `capture.rs::send_encoded_sample`.
fn send_encoded_sample(client: &ScreenshareClient, stream_id: u8, sample: &gst::Sample, start: Instant) -> Result<()> {
    let buffer = sample.buffer().ok_or_else(|| anyhow!("sample had no buffer"))?;

    let pts_us = buffer
        .pts()
        .map(|t| t.useconds())
        .unwrap_or_else(|| start.elapsed().as_micros() as u64);

    // Unlike H.264's GOP structure, every Opus packet is independently
    // decodable — there's no delta/keyframe distinction for audio. Mark
    // every packet "keyframe" so a newly-subscribing receiver can start
    // playing audio immediately rather than waiting for one.
    let keyframe = true;

    let map = buffer.map_readable().context("failed to map encoded Opus packet")?;
    let payload = map.as_slice();

    let nonce_seq = client.nonce_sequence();
    let (nonce, counter) = nonce_seq
        .next_nonce(stream_id)
        .map_err(|e| anyhow!("{e}"))
        .context("cannot send another frame")?;

    let room_key = client.room_key();
    let ciphertext = encrypt_frame(&room_key, stream_id, nonce, pts_us, payload).context("failed to encrypt frame")?;

    let connection = client.connection();
    let max_datagram_size = connection
        .max_datagram_size()
        .ok_or_else(|| anyhow!("peer does not support QUIC datagrams"))?;

    let datagrams = fragment_frame(client.conn_id(), stream_id, counter, keyframe, &ciphertext, max_datagram_size)
        .context("failed to fragment encrypted frame")?;

    for datagram in datagrams {
        connection
            .send_datagram(datagram.into())
            .map_err(|e| anyhow!("failed to send media datagram: {e}"))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trimmed-down but structurally real `pw-dump` fixture (shapes
    /// copied from actual output captured on this dev machine — PipeWire
    /// 1.6.8 — via `gst-launch-1.0 audiotestsrc ! pipewiresink` plus a
    /// `pw-loopback` instance) exercising:
    ///   - a real application's `Stream/Output/Audio` node + its 2 output
    ///     ports (`test-audio-app`, node id 262, ports 284/279),
    ///   - a `Stream/Input/Audio` node that must NOT be listed (wrong
    ///     class),
    ///   - this module's own loopback output node (`output.ssshare-loopback-…`)
    ///     that must NOT be listed (self-filtering),
    ///   - the loopback's `input.ssshare-loopback-…` node + its 2 input
    ///     ports, for [`find_loopback_nodes`]/[`node_port_ids`].
    fn fixture_objects() -> Vec<PwObject> {
        let json = r#"[
            {
                "id": 262,
                "type": "PipeWire:Interface:Node",
                "info": {
                    "props": {
                        "media.class": "Stream/Output/Audio",
                        "node.name": "test-audio-app",
                        "node.description": "TestApp"
                    }
                }
            },
            {
                "id": 284,
                "type": "PipeWire:Interface:Port",
                "info": {
                    "direction": "output",
                    "props": { "node.id": 262, "port.id": 0, "port.name": "output_FL" }
                }
            },
            {
                "id": 279,
                "type": "PipeWire:Interface:Port",
                "info": {
                    "direction": "output",
                    "props": { "node.id": 262, "port.id": 1, "port.name": "output_FR" }
                }
            },
            {
                "id": 319,
                "type": "PipeWire:Interface:Node",
                "info": {
                    "props": {
                        "media.class": "Stream/Input/Audio",
                        "node.name": "input.ssshare-loopback-3-abc123"
                    }
                }
            },
            {
                "id": 318,
                "type": "PipeWire:Interface:Port",
                "info": {
                    "direction": "input",
                    "props": { "node.id": 319, "port.id": 0, "port.name": "input_FL" }
                }
            },
            {
                "id": 280,
                "type": "PipeWire:Interface:Port",
                "info": {
                    "direction": "input",
                    "props": { "node.id": 319, "port.id": 1, "port.name": "input_FR" }
                }
            },
            {
                "id": 262,
                "type": "PipeWire:Interface:Node",
                "info": {
                    "props": {
                        "media.class": "Stream/Output/Audio",
                        "node.name": "output.ssshare-loopback-3-abc123"
                    }
                }
            }
        ]"#;
        serde_json::from_str(json).expect("fixture JSON should parse as Vec<PwObject>")
    }

    #[test]
    fn extract_audio_sources_finds_real_app_excludes_own_loopback_and_wrong_class() {
        let objects = fixture_objects();
        let sources = extract_audio_sources(&objects);
        assert_eq!(sources, vec![AudioSource { id: 262, name: "TestApp".to_string() }]);
    }

    #[test]
    fn node_port_ids_orders_by_port_id_and_filters_direction() {
        let objects = fixture_objects();
        let outputs = node_port_ids(&objects, 262, "output");
        assert_eq!(outputs, vec![284, 279]);
        let inputs = node_port_ids(&objects, 319, "input");
        assert_eq!(inputs, vec![318, 280]);
        // Wrong direction / wrong node yields nothing.
        assert!(node_port_ids(&objects, 262, "input").is_empty());
        assert!(node_port_ids(&objects, 999, "output").is_empty());
    }

    #[test]
    fn find_loopback_nodes_matches_output_and_input_prefixed_names() {
        let objects = fixture_objects();
        let nodes = find_loopback_nodes(&objects, "ssshare-loopback-3-abc123").expect("both nodes present in fixture");
        assert_eq!(nodes.output_node_id, 262);
        assert_eq!(nodes.input_node_id, 319);
        assert!(find_loopback_nodes(&objects, "no-such-loopback").is_none());
    }

    /// Sanity-checks that the pipeline description this module builds is
    /// well-formed and every element it names (`audioconvert`,
    /// `audioresample`, `opusenc`, `appsink`) is actually available on this
    /// machine — with `audiotestsrc` standing in for `pipewiresrc
    /// path=<loopback node id>`, since a real loopback needs a live
    /// PipeWire graph this unit test doesn't set up (see the `#[ignore]`d
    /// live test below for that). Mirrors
    /// `capture::tests::pipeline_elements_link_and_produce_encoded_output`.
    #[test]
    fn pipeline_elements_link_and_produce_encoded_output() {
        if gst::init().is_err() {
            eprintln!("skipping: gstreamer not available in this environment");
            return;
        }

        let description = "audiotestsrc num-buffers=50 \
             ! audioconvert \
             ! audioresample \
             ! opusenc \
             ! appsink name=sink emit-signals=false sync=false";

        let element = gst::parse::launch(description).expect("pipeline should parse and link");
        let pipeline = element.downcast::<gst::Pipeline>().expect("top-level element should be a Pipeline");
        let sink = pipeline
            .by_name("sink")
            .expect("sink element should exist")
            .downcast::<gst_app::AppSink>()
            .expect("sink should be an AppSink");

        pipeline.set_state(gst::State::Playing).expect("pipeline should reach PLAYING");

        let mut got_buffer = false;
        let bus = pipeline.bus().unwrap();
        for msg in bus.iter_timed(gst::ClockTime::from_seconds(10)) {
            match msg.view() {
                gst::MessageView::Eos(_) => break,
                gst::MessageView::Error(e) => panic!("pipeline error: {} ({:?})", e.error(), e.debug()),
                _ => {}
            }
            if let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_mseconds(50)) {
                if sample.buffer().is_some() {
                    got_buffer = true;
                    break;
                }
            }
        }

        pipeline.set_state(gst::State::Null).expect("pipeline should stop cleanly");
        assert!(got_buffer, "expected at least one encoded Opus packet from the test pipeline");
    }

    /// Exercises the real PipeWire flow end to end: spawns a synthetic
    /// audio-producing app (`audiotestsrc ! pipewiresink`), finds it via
    /// [`list_audio_sources`], creates a real `pw-loopback`, links the app's
    /// output ports into it, and confirms `pw-dump` shows the tap in place
    /// — without touching the app's original routing. Not runnable
    /// unattended/in CI (spawns real system processes + needs a live
    /// PipeWire session), hence `#[ignore]`. Run manually with:
    /// `cargo test audio::tests::live_pipewire_loopback_and_link -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_pipewire_loopback_and_link() {
        let mut app = Command::new("gst-launch-1.0")
            .args([
                "audiotestsrc",
                "!",
                "audioconvert",
                "!",
                "pipewiresink",
                "client-name=ssshare-audio-test-live",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn synthetic audiotestsrc app");

        tokio::time::sleep(Duration::from_secs(2)).await;

        let sources = list_audio_sources().expect("list_audio_sources should succeed against a live pw-dump");
        let source = sources
            .iter()
            .find(|s| s.name.contains("ssshare-audio-test-live") || s.name == "audiotestsrc-live")
            .cloned();

        let loopback_name = "ssshare-loopback-live-test";
        let mut loopback_child = spawn_loopback(loopback_name).expect("failed to spawn pw-loopback");
        let nodes = wait_for_loopback_nodes(loopback_name, Duration::from_secs(5))
            .await
            .expect("loopback nodes should appear");

        if let Some(source) = source {
            link_source_into_loopback(source.id, nodes.input_node_id).expect("linking should succeed");
            eprintln!("linked live source id={} name={:?} into loopback input node {}", source.id, source.name, nodes.input_node_id);
        } else {
            eprintln!("did not find the synthetic test app by name; listed sources: {sources:?} — link step skipped");
        }

        kill_and_wait(&mut loopback_child);
        let _ = app.kill();
        let _ = app.wait();
    }

    /// Regression test for the "Stop Sharing Audio" hang: starts a real
    /// audio share (synthetic `audiotestsrc ! pipewiresink` source, real
    /// `pw-loopback`, real Opus capture pipeline, real relay connection —
    /// same "real server child process, in-process client" model as
    /// `end_to_end_test.rs`), then asserts [`AudioCaptureHandle::stop`]
    /// returns well within its 5s EOS-wait timeout (previously: hung
    /// indefinitely, confirmed live at 90+ seconds before this was force-
    /// killed) and that the `pw-loopback` child process is actually gone
    /// (not orphaned) afterward, checked both via `Child::try_wait()` and an
    /// independent `pgrep` against the loopback's `--name`. Not runnable
    /// unattended/in CI (spawns real system processes + needs a live
    /// PipeWire session + a built server binary), hence `#[ignore]`. Run
    /// manually with:
    /// `cargo test audio::tests::stop_returns_promptly_and_reaps_pw_loopback -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn stop_returns_promptly_and_reaps_pw_loopback() {
        use std::net::UdpSocket;

        let _ = tracing_subscriber::fmt().with_test_writer().try_init();

        // --- spin up a real relay server child process, same approach as
        // end_to_end_test.rs (duplicated in miniature here rather than
        // reaching into that module, to keep this test self-contained). ---
        let port = {
            let socket = UdpSocket::bind("127.0.0.1:0").expect("failed to bind ephemeral probe socket");
            socket.local_addr().expect("local_addr").port()
        };
        let home = dirs::home_dir().expect("could not determine home directory");
        let server_repo = home.join("Documents/Programming/simple-screensharing-server");
        let server_binary = {
            let release = server_repo.join("target/release/simple-screensharing-server");
            let debug = server_repo.join("target/debug/simple-screensharing-server");
            if release.is_file() {
                release
            } else if debug.is_file() {
                debug
            } else {
                panic!("server binary not found at {release:?} or {debug:?} — build the server repo first");
            }
        };
        let cmd_string = format!("{} {port}", server_binary.display());
        let mut server_child = Command::new("setsid")
            .args(["script", "-qc", &cmd_string, "/dev/null"])
            .current_dir(&server_repo)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn relay server child process (requires setsid/script)");

        let server_addr = format!("127.0.0.1:{port}");
        let room_id: crate::protocol::RoomId = rand::random();
        let room_key: [u8; 32] = rand::random();
        let join_string = crate::net::format_join_string(&server_addr, &room_id, &room_key);

        let mut client = None;
        for _ in 0..40 {
            match ScreenshareClient::connect(&join_string, "audio-stop-test").await {
                Ok(v) => {
                    client = Some(v);
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(250)).await,
            }
        }
        let (client, _snapshot, _events) = client.expect("client failed to connect to the relay after retries");

        // --- synthetic audio-producing app + real pw-loopback tap. ---
        let mut app = Command::new("gst-launch-1.0")
            .args([
                "audiotestsrc",
                "!",
                "audioconvert",
                "!",
                "pipewiresink",
                "client-name=ssshare-audio-stop-test",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn synthetic audiotestsrc app");
        tokio::time::sleep(Duration::from_secs(2)).await;

        let sources = list_audio_sources().expect("list_audio_sources should succeed against a live pw-dump");
        let source = sources
            .iter()
            .find(|s| s.name.contains("ssshare-audio-stop-test"))
            .cloned()
            .expect("did not find the synthetic test app among live audio sources");

        eprintln!("[audio-stop-test] starting real audio share for source {source:?}");
        let handle = start_audio_sharing(&client, 7, &source)
            .await
            .expect("start_audio_sharing failed");

        // Let it actually run/produce a few Opus packets before stopping,
        // like a real share would.
        tokio::time::sleep(Duration::from_secs(1)).await;

        let loopback_pid = handle.loopback_child.id();
        eprintln!("[audio-stop-test] pw-loopback pid={loopback_pid}; calling stop()");

        let stop_started = Instant::now();
        let stop_result = tokio::time::timeout(Duration::from_secs(10), handle.stop()).await;
        let stop_elapsed = stop_started.elapsed();
        eprintln!("[audio-stop-test] stop() returned after {stop_elapsed:?}");

        assert!(
            stop_result.is_ok(),
            "AudioCaptureHandle::stop() did not return within 10s — the hang is still present"
        );
        assert!(
            stop_elapsed < Duration::from_secs(6),
            "stop() took {stop_elapsed:?}, expected well under the 5s EOS-wait timeout plus teardown overhead"
        );
        if let Err(e) = stop_result.unwrap() {
            tracing::warn!("[audio-stop-test] stop() returned Err (non-fatal to this test): {e:#}");
        }

        // Confirm the pw-loopback OS process is actually gone, not orphaned
        // — both via the reaped Child's own exit status and an independent
        // process-table check by pid.
        let still_running_by_pid = Command::new("kill")
            .args(["-0", &loopback_pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        eprintln!("[audio-stop-test] pw-loopback pid={loopback_pid} still running per `kill -0`: {still_running_by_pid}");
        assert!(
            !still_running_by_pid,
            "pw-loopback process (pid {loopback_pid}) is still running after stop() — orphaned"
        );

        let _ = client.leave().await;
        let _ = app.kill();
        let _ = app.wait();
        let _ = server_child.kill();
        let _ = server_child.wait();
        let _ = Command::new("pkill")
            .args(["-f", &format!("simple-screensharing-server {port}")])
            .status();

        eprintln!("[audio-stop-test] PASS: stop() returned in {stop_elapsed:?}, pw-loopback (pid {loopback_pid}) reaped");
    }
}
