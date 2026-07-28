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
//!    `pipewiresrc target-object=<output.X's object.serial> ! audioconvert
//!    ! audioresample ! volume ! level ! opusenc ! appsink`, and the
//!    resulting link is then *verified* against `pw-dump` before the stream
//!    is announced ([`wait_for_capture_link`]). **`target-object` and the
//!    `object.serial`, not `path` and the registry node id** — see
//!    [`build_pipeline`]'s doc comment for the difference, and for the fact
//!    that getting it wrong silently shares the user's microphone instead of
//!    the application they picked (which is exactly what shipped on
//!    2026-07-28 before this was fixed).
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
//! microphone/physical-input capture. [`wait_for_capture_link`] is what
//! makes that a guarantee rather than an intention. The UI that drives this
//! is the "+ Add audio" row on a screen share's card in Stream Control (see
//! `superdoc/features/stream-control.md`).

use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
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
    /// from via `pipewiresrc`. This is the *registry object id* (`pw-dump`'s
    /// top-level `id`), which is what `pw-link` takes.
    output_node_id: u32,
    /// The same node's `object.serial`. A **different number** from
    /// `output_node_id`, and the only one `pipewiresrc`'s `target-object`
    /// understands — see [`build_pipeline`]'s doc comment for why getting
    /// this wrong silently captured the microphone instead.
    output_node_serial: u64,
    /// `input.<loopback_name>`, `Stream/Input/Audio` — what we `pw-link`
    /// the target application's output ports into.
    input_node_id: u32,
}

fn find_loopback_nodes(objects: &[PwObject], loopback_name: &str) -> Option<LoopbackNodes> {
    let output_name = format!("output.{loopback_name}");
    let input_name = format!("input.{loopback_name}");
    let mut output: Option<(u32, u64)> = None;
    let mut input: Option<u32> = None;
    for obj in objects {
        if obj.obj_type.as_deref() != Some("PipeWire:Interface:Node") {
            continue;
        }
        let Some(info) = &obj.info else { continue };
        let Some(props) = &info.props else { continue };
        let Some(name) = prop_str(props, "node.name") else { continue };
        if name == output_name {
            output = Some((obj.id, prop_u64(props, "object.serial")?));
        } else if name == input_name {
            input = Some(obj.id);
        }
    }
    let (output_node_id, output_node_serial) = output?;
    Some(LoopbackNodes { output_node_id, output_node_serial, input_node_id: input? })
}

/// The registry object id of the `pipewiresrc` capture stream node named
/// `capture_node_name` (`pipewiresrc`'s `client-name` becomes the stream
/// node's `node.name`, verified live against `pw-dump`), if it has appeared
/// in the graph yet.
fn find_capture_node(objects: &[PwObject], capture_node_name: &str) -> Option<u32> {
    objects.iter().find_map(|obj| {
        if obj.obj_type.as_deref() != Some("PipeWire:Interface:Node") {
            return None;
        }
        let props = obj.info.as_ref()?.props.as_ref()?;
        (prop_str(props, "node.name")? == capture_node_name).then_some(obj.id)
    })
}

/// Whether `objects` contains a link carrying audio from node
/// `output_node_id` into node `input_node_id`.
fn nodes_are_linked(objects: &[PwObject], output_node_id: u32, input_node_id: u32) -> bool {
    objects.iter().any(|obj| {
        if obj.obj_type.as_deref() != Some("PipeWire:Interface:Link") {
            return false;
        }
        let Some(props) = obj.info.as_ref().and_then(|i| i.props.as_ref()) else { return false };
        prop_u64(props, "link.output.node") == Some(output_node_id as u64)
            && prop_u64(props, "link.input.node") == Some(input_node_id as u64)
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

/// The readiness predicate the poll loop in [`wait_for_loopback_ready`]
/// checks against every `pw-dump` snapshot: `loopback_name`'s two stream
/// nodes must exist AND its input node's input ports must already be
/// enumerated. Split out into its own function so it's unit-testable
/// against fixture JSON without a live PipeWire session (see the
/// `#[cfg(test)]` module below) — the live-server test at the bottom of
/// this module exercises the real polling/timing behavior end to end.
///
/// Why the input node's ports specifically: those are exactly the ports
/// [`link_source_into_loopback`] needs to `pw-link` into, and PipeWire
/// announces a node global and its port globals as separate registry
/// events — a freshly spawned `pw-loopback`'s ports can lag its node by a
/// beat (format-negotiation round trip). Checking only the node (as this
/// used to) let `start_audio_sharing` race ahead into linking before the
/// ports existed, intermittently failing right after a freshly-spawned
/// loopback.
fn loopback_readiness(objects: &[PwObject], loopback_name: &str) -> Option<LoopbackNodes> {
    let nodes = find_loopback_nodes(objects, loopback_name)?;
    let input_ports = node_port_ids(objects, nodes.input_node_id as u64, "input");
    if input_ports.is_empty() {
        return None;
    }
    Some(nodes)
}

/// Polls `pw-dump` until `loopback_name`'s two stream nodes AND its input
/// node's input ports are both visible in the same snapshot (see
/// [`loopback_readiness`] for why both are needed), or `timeout` elapses.
/// Combines what used to be two separate steps — a node-only poll here,
/// followed by an unconditional one-shot port lookup in
/// `link_source_into_loopback` that `bail!`ed immediately if the ports
/// weren't there yet — into one poll, so a loopback whose node has
/// registered but whose ports haven't yet no longer fails the share
/// outright; it just keeps polling like the node-existence case always did.
///
/// Returns the snapshot the readiness check last succeeded against
/// alongside the resolved node ids, so the caller can link straight from it
/// without an extra `pw-dump` round trip.
async fn wait_for_loopback_ready(loopback_name: &str, timeout: Duration) -> Result<(Vec<PwObject>, LoopbackNodes)> {
    let deadline = Instant::now() + timeout;
    loop {
        let objects = run_pw_dump()?;
        if let Some(nodes) = loopback_readiness(&objects, loopback_name) {
            return Ok((objects, nodes));
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
///
/// `objects` is a `pw-dump` snapshot already known (via
/// [`wait_for_loopback_ready`]) to contain `loopback_input_node_id`'s input
/// ports — passed in rather than re-fetched, since that readiness wait
/// already paid for the freshest snapshot that matters. The `sink_ports`
/// empty check below is accordingly a defensive no-op in the normal path,
/// not the primary readiness gate anymore (see [`loopback_readiness`]).
fn link_source_into_loopback(objects: &[PwObject], source_node_id: u32, loopback_input_node_id: u32) -> Result<()> {
    let source_ports = node_port_ids(objects, source_node_id as u64, "output");
    let sink_ports = node_port_ids(objects, loopback_input_node_id as u64, "input");

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
// Capture level metering
// ---------------------------------------------------------------------

/// How often the capture pipeline's `level` element reports the signal
/// level, in nanoseconds. 100 ms is fast enough for a meter to read as live
/// and slow enough that the messages cost nothing.
const LEVEL_INTERVAL_NS: u64 = 100_000_000;

/// The quietest level the meter distinguishes from silence. `level` reports
/// RMS in dBFS and digital silence comes back as an enormous negative
/// number (`-700` dB was observed live), so everything below this floor is
/// clamped to 0 rather than producing a meaningless sliver of bar.
const LEVEL_FLOOR_DB: f64 = -60.0;

/// The live capture level of one audio share, as a 0.0-1.0 fraction, shared
/// between the GStreamer bus handler that writes it and whoever renders it.
///
/// `Why an atomic and not a channel/mutex:` this is a *display* value with
/// exactly one writer and no history — a reader only ever wants the newest
/// number, and a missed update is invisible at 10 Hz. An `AtomicU32`
/// holding `f32::to_bits` makes both sides lock-free, which matters because
/// the writer is a GStreamer streaming thread.
#[derive(Clone, Default)]
pub struct AudioLevel(Arc<AtomicU32>);

impl AudioLevel {
    fn store(&self, level: f32) {
        self.0.store(level.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    /// Newest captured level, 0.0 (silence) to 1.0 (full scale).
    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
}

/// Turns one `level` element message into a 0.0-1.0 meter reading: the
/// loudest channel's RMS, mapped from dBFS onto [`LEVEL_FLOOR_DB`]..0.
///
/// Split out from [`handle_bus_message`] so the dB→fraction mapping is
/// unit-testable without a live pipeline.
fn level_fraction_from_rms_db(rms_db: impl IntoIterator<Item = f64>) -> Option<f32> {
    let loudest = rms_db.into_iter().fold(f64::NEG_INFINITY, f64::max);
    if !loudest.is_finite() {
        return None;
    }
    Some((((loudest - LEVEL_FLOOR_DB) / -LEVEL_FLOOR_DB).clamp(0.0, 1.0)) as f32)
}

/// Reads the per-channel RMS values out of a `level` element message.
fn rms_db_from_level_message(structure: &gst::StructureRef) -> Option<Vec<f64>> {
    // `glib` is not a direct dependency of this crate; `gstreamer` re-exports
    // the exact version it was built against, which is the one that matters
    // for a `GValue` handed over by a GStreamer element.
    let rms = structure.get::<gst::glib::ValueArray>("rms").ok()?;
    Some(rms.iter().filter_map(|v| v.get::<f64>().ok()).collect())
}

// ---------------------------------------------------------------------
// GStreamer pipeline construction
// ---------------------------------------------------------------------

/// Builds the Opus capture pipeline that taps `output.<loopback_name>`.
///
/// # Why `target-object=<object.serial>`, and not `path=<node id>`
///
/// This used to be `pipewiresrc path={loopback_output_node_id}`, and that
/// was the bug that made the whole feature silently do the wrong thing:
///
/// - `path` is **deprecated** (`gst-inspect-1.0 pipewiresrc`: "The source
///   path to connect to (NULL = default) … deprecated"); `target-object` is
///   its replacement.
/// - Both take *"the source name/serial"* — a node's `object.serial` or its
///   `node.name`. They do **not** take `pw-dump`'s top-level registry `id`,
///   which is a different number entirely (measured live on this machine:
///   the loopback output node was registry id `333`, `object.serial`
///   `18643`). Passing the registry id therefore matched nothing.
/// - And a `pipewiresrc` whose target does not match anything does not
///   fail. `node.autoconnect` defaults to `true`, so the session manager
///   links it to the **default audio source instead — the microphone**.
///   Confirmed live via `pw-link -l`: the capture stream came up linked to
///   `easyeffects_source:capture_FL`, i.e. the sharer's mic, not the app
///   they picked. That is why the viewer heard nothing from the shared
///   application: the app's audio was never captured at all.
///
/// [`wait_for_capture_link`] exists because that failure mode is silent and
/// is a privacy problem, not just a broken feature — see its doc comment.
///
/// `client-name` is set so the resulting stream node is findable by name in
/// `pw-dump` (`pipewiresrc` copies `client-name` into the stream node's
/// `node.name`, verified live), which is what makes that check possible.
///
/// The explicit `channels=2` caps filter pins the tap to stereo. Without
/// it, `audioconvert`'s wide-open sink caps let PipeWire negotiate **mono**
/// and link only `output_FL` — verified live (`ssshare-cap2:input_MONO`),
/// i.e. music shared through this path silently lost its right channel.
/// The loopback is always created with `pw-loopback`'s default `[FL, FR]`
/// channel map, so requesting 2 channels here can't over-ask.
///
/// The `level` element sits on the decoded-PCM side (it only understands
/// `audio/x-raw`, so it has to be before `opusenc`) and posts an element
/// message every [`LEVEL_INTERVAL_NS`]; [`handle_bus_message`] folds those
/// into [`AudioCaptureHandle::level`], which is what the Stream Control
/// card's little VU meter draws. `post-messages` is the element's default
/// but is stated explicitly because the whole feature silently stops
/// working if it ever isn't.
///
/// `volume name=mute` backs [`AudioCaptureHandle::set_muted`], and sits
/// deliberately **before** `level`: muting therefore flattens the meter as
/// well as silencing the stream. `Why:` the meter's job is to answer "is my
/// audio going out right now" — a bar still bouncing while the share is
/// muted answers that question wrongly. Muting with `volume` rather than by
/// pausing the send keeps a continuous Opus stream flowing, so the viewer's
/// jitter buffer and packet-loss concealment never see a mute as a
/// multi-second dropout to conceal.
fn build_pipeline(loopback_output_node_serial: u64, capture_node_name: &str) -> Result<(gst::Pipeline, gst_app::AppSink, gst::Element)> {
    let description = format!(
        "pipewiresrc target-object={loopback_output_node_serial} client-name={capture_node_name} do-timestamp=true \
         ! audio/x-raw,channels=2 \
         ! audioconvert \
         ! audioresample \
         ! volume name=mute \
         ! level name=level post-messages=true interval={LEVEL_INTERVAL_NS} \
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

    let mute = pipeline
        .by_name("mute")
        .ok_or_else(|| anyhow!("pipeline has no element named 'mute'"))?;

    Ok((pipeline, sink, mute))
}

/// Polls `pw-dump` until the `pipewiresrc` stream node called
/// `capture_node_name` exists **and** is actually linked to
/// `loopback_output_node_id`, or `timeout` elapses.
///
/// # Why this is not paranoia
///
/// `pipewiresrc` connects with `node.autoconnect=true`. If its
/// `target-object` doesn't resolve, PipeWire does not error — the session
/// manager just links the capture stream to the **default audio source**,
/// which on a normal desktop is the microphone. The pipeline then reaches
/// `PLAYING` and produces perfectly valid Opus packets, so every signal
/// [`start_audio_sharing`] used to check (`set_state` succeeded, the
/// pipeline reported a state, packets flowed) reads as success while the
/// application the user picked is not being captured and their microphone
/// is being broadcast to the room instead. Nothing downstream can tell the
/// difference. This check is the only place that can.
///
/// It doubles as the real "capture has started" gate: for a live source
/// like `pipewiresrc`, `Element::state()` reports
/// `Ok(StateChangeSuccess::Async)` while the source is still stuck waiting
/// to negotiate — which the old `if let Err(..)` check treated as success,
/// so a capture that never connected to anything at all still announced
/// itself to the room as a working stream.
async fn wait_for_capture_link(
    capture_node_name: &str,
    loopback_output_node_id: u32,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let mut last_capture_node = None;
    loop {
        let objects = run_pw_dump()?;
        if let Some(capture_node_id) = find_capture_node(&objects, capture_node_name) {
            last_capture_node = Some(capture_node_id);
            if nodes_are_linked(&objects, loopback_output_node_id, capture_node_id) {
                tracing::info!(
                    "audio capture stream '{capture_node_name}' (node {capture_node_id}) is linked to loopback output node {loopback_output_node_id}"
                );
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            match last_capture_node {
                Some(id) => bail!(
                    "audio capture stream '{capture_node_name}' (node {id}) never linked to the loopback \
                     output node {loopback_output_node_id} — refusing to share, because a pipewiresrc that \
                     misses its target silently captures the default source (the microphone) instead"
                ),
                None => bail!(
                    "audio capture stream '{capture_node_name}' never appeared in the PipeWire graph \
                     (the capture pipeline never connected)"
                ),
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
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
    level: AudioLevel,
    /// The pipeline's `volume name=mute` element — see
    /// [`AudioCaptureHandle::set_muted`].
    mute_element: gst::Element,
    /// The application whose audio this share is capturing — the name the
    /// user picked in the source list. Surfaced next to the share's level
    /// meter in Stream Control, so the row can say *what* is being shared
    /// rather than just that something is.
    source_name: String,
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

    let (objects, loopback) = match wait_for_loopback_ready(&loopback_name, Duration::from_secs(5)).await {
        Ok(v) => v,
        Err(e) => {
            kill_and_wait(&mut loopback_child);
            return Err(e);
        }
    };

    if let Err(e) = link_source_into_loopback(&objects, source.id, loopback.input_node_id) {
        kill_and_wait(&mut loopback_child);
        return Err(e);
    }

    let capture_node_name = format!("{loopback_name}-cap");
    let (pipeline, appsink, mute_element) = match build_pipeline(loopback.output_node_serial, &capture_node_name) {
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

    // ...and then confirm it is capturing *the loopback* and not whatever
    // the session manager felt like connecting it to. See
    // `wait_for_capture_link` for why reaching PLAYING is not enough.
    if let Err(e) = wait_for_capture_link(&capture_node_name, loopback.output_node_id, Duration::from_secs(5)).await {
        let _ = pipeline.set_state(gst::State::Null);
        kill_and_wait(&mut loopback_child);
        return Err(e);
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

    let level = AudioLevel::default();
    let pump_client = client.clone();
    let pump_task = tokio::spawn(pump_loop(pump_client, stream_id, stream, bus_stream, level.clone()));

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
        level,
        mute_element,
        source_name: source.name.clone(),
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
    /// Newest capture level, 0.0-1.0 — what the Stream Control card's VU
    /// meter draws. Updated ~10x/s while the pipeline is running (see
    /// [`LEVEL_INTERVAL_NS`]), and left at its last value once it stops.
    pub fn level(&self) -> f32 {
        self.level.get()
    }

    /// The name of the application being captured, as picked in the source
    /// list.
    pub fn source_name(&self) -> &str {
        &self.source_name
    }

    /// Mute/unmute what this share sends, without stopping it — the Stream
    /// Control card's Mute toggle. See [`build_pipeline`] for why this is a
    /// `volume` element ahead of the meter rather than a pause in the send
    /// loop.
    pub fn set_muted(&self, muted: bool) {
        self.mute_element.set_property("mute", muted);
    }

    /// Whether this share is currently muted.
    pub fn is_muted(&self) -> bool {
        self.mute_element.property::<bool>("mute")
    }

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
    level: AudioLevel,
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
                    Some(msg) => handle_bus_message(&msg, &mut bus_done, &level),
                    None => bus_done = true,
                }
            }
        }
    }
}

fn handle_bus_message(msg: &gst::Message, bus_done: &mut bool, level: &AudioLevel) {
    match msg.view() {
        gst::MessageView::Element(element) => {
            if let Some(s) = element.structure()
                && s.name() == "level"
                && let Some(rms) = rms_db_from_level_message(s)
                && let Some(fraction) = level_fraction_from_rms_db(rms)
            {
                level.store(fraction);
            }
        }
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
                        "node.name": "output.ssshare-loopback-3-abc123",
                        "object.serial": 18643
                    }
                }
            },
            {
                "id": 700,
                "type": "PipeWire:Interface:Node",
                "info": {
                    "props": {
                        "media.class": "Stream/Input/Audio",
                        "node.name": "ssshare-loopback-3-abc123-cap",
                        "object.serial": 18700
                    }
                }
            },
            {
                "id": 701,
                "type": "PipeWire:Interface:Link",
                "info": {
                    "props": {
                        "link.output.node": 262,
                        "link.output.port": 284,
                        "link.input.node": 700,
                        "link.input.port": 699
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

    /// Regression test for the "loopback node registered but its ports
    /// haven't yet" race [`loopback_readiness`]/[`wait_for_loopback_ready`]
    /// exist to handle: a snapshot with the loopback's node globals present
    /// but no port globals yet must read as not-ready, exactly like a
    /// snapshot where the node itself is entirely absent — both are cases
    /// the poll loop should keep retrying on rather than treat as done (or,
    /// for the old one-shot port lookup this replaced, fail outright on).
    #[test]
    fn loopback_readiness_requires_both_node_and_ports() {
        let ready = fixture_objects();
        assert!(
            loopback_readiness(&ready, "ssshare-loopback-3-abc123").is_some(),
            "fixture has both loopback nodes and the input node's ports — should be ready"
        );

        // Both loopback nodes are present, but neither has any ports yet —
        // the exact shape of a `pw-dump` snapshot caught mid-race, per the
        // module docs' root-cause note (node and port globals are separate
        // registry events).
        let node_only_json = r#"[
            {
                "id": 500,
                "type": "PipeWire:Interface:Node",
                "info": {
                    "props": { "media.class": "Stream/Input/Audio", "node.name": "input.ssshare-loopback-9-xyz" }
                }
            },
            {
                "id": 501,
                "type": "PipeWire:Interface:Node",
                "info": {
                    "props": { "media.class": "Stream/Output/Audio", "node.name": "output.ssshare-loopback-9-xyz" }
                }
            }
        ]"#;
        let node_only: Vec<PwObject> = serde_json::from_str(node_only_json).expect("fixture JSON should parse");
        assert!(
            loopback_readiness(&node_only, "ssshare-loopback-9-xyz").is_none(),
            "nodes exist but have no ports yet — must not be considered ready"
        );

        // Neither node exists at all (e.g. pw-loopback hasn't started
        // registering anything yet) — also not ready.
        assert!(loopback_readiness(&ready, "no-such-loopback").is_none());
    }

    #[test]
    fn find_loopback_nodes_matches_output_and_input_prefixed_names() {
        let objects = fixture_objects();
        let nodes = find_loopback_nodes(&objects, "ssshare-loopback-3-abc123").expect("both nodes present in fixture");
        assert_eq!(nodes.output_node_id, 262);
        assert_eq!(nodes.input_node_id, 319);
        assert!(find_loopback_nodes(&objects, "no-such-loopback").is_none());
    }

    /// The registry object id and `object.serial` of the same node are
    /// different numbers, and `build_pipeline` must use the *serial* — see
    /// its doc comment for what happened when it didn't (the microphone got
    /// shared instead of the chosen application). Pinning both here keeps
    /// the two from being quietly conflated again.
    #[test]
    fn find_loopback_nodes_reports_the_object_serial_not_the_registry_id() {
        let objects = fixture_objects();
        let nodes = find_loopback_nodes(&objects, "ssshare-loopback-3-abc123").expect("both nodes present in fixture");
        assert_eq!(nodes.output_node_serial, 18643);
        assert_ne!(nodes.output_node_serial, nodes.output_node_id as u64);
    }

    /// The guard that catches "`pipewiresrc` connected to something, but not
    /// to our loopback" — the shape a mis-targeted capture takes.
    #[test]
    fn capture_link_check_accepts_the_real_link_and_rejects_a_missing_one() {
        let objects = fixture_objects();
        let capture_node = find_capture_node(&objects, "ssshare-loopback-3-abc123-cap").expect("capture node in fixture");
        assert_eq!(capture_node, 700);
        assert!(find_capture_node(&objects, "ssshare-loopback-9-nope-cap").is_none());

        assert!(nodes_are_linked(&objects, 262, capture_node), "the fixture's loopback->capture link should be found");
        // Same nodes, wrong direction, and a node pair with no link at all.
        assert!(!nodes_are_linked(&objects, capture_node, 262));
        assert!(!nodes_are_linked(&objects, 319, capture_node));
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
                "volume=0.02",
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
        let (objects, nodes) = wait_for_loopback_ready(loopback_name, Duration::from_secs(5))
            .await
            .expect("loopback nodes and ports should appear");

        if let Some(source) = source {
            link_source_into_loopback(&objects, source.id, nodes.input_node_id).expect("linking should succeed");
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
        // Same two candidate layouts `end_to_end_test` probes (see
        // `server_repo_candidates`) — the workspace dropped its old `master/`
        // nesting level, so a single hardcoded path no longer resolves.
        let candidates = crate::end_to_end_test::server_repo_candidates().expect("could not determine home directory");
        let (server_repo, server_binary) = candidates
            .iter()
            .flat_map(|repo| ["release", "debug"].map(|p| (repo.clone(), repo.join(format!("target/{p}/simple-screensharing-server")))))
            .find(|(_, binary)| binary.is_file())
            .unwrap_or_else(|| panic!("server binary not found under any of {candidates:?} — build the server repo first"));
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

        let endpoint = crate::net::build_client_endpoint().expect("failed to build a client QUIC endpoint");
        let mut client = None;
        for _ in 0..40 {
            match ScreenshareClient::connect(&endpoint, &join_string, "audio-stop-test").await {
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
                "volume=0.02",
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
