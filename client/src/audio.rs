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
//! the ScreegoElectronScreenshare project solves the same problem on Linux):
//!
//! 1. [`list_audio_sources`] runs `pw-dump` (PipeWire's full graph, as JSON)
//!    and returns every node with `media.class = "Stream/Output/Audio"` —
//!    i.e. every application currently producing audio — confirmed against
//!    this machine's actual `pw-dump` output (GStreamer 1.28.5 / PipeWire
//!    1.6.8), not assumed from documentation. The list is deduplicated by
//!    name: the UI choice is a *name*, and every stream with that name is
//!    mixed into the share (task #105, see step 3).
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
//!    additive. *Every* stream named like the chosen source is linked into the
//!    same `input.X` (the loopback mixes them), and [`link_poll_loop`] re-checks
//!    once a second to link streams that appear later and forget ones that
//!    vanished. The application's original routing (e.g. to real speakers)
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
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use futures_util::StreamExt;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::decode::sender_clock_us;
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
    /// The PipeWire node id (as of the moment it was listed). In the
    /// dropdown list ([`list_audio_sources`]) this is only the *representative*
    /// stream of `name` (the level meter taps it); [`start_audio_sharing`]
    /// ignores it and mixes every stream called `name`.
    pub id: u32,
    /// Best-effort human-readable label: `node.description`, falling back
    /// to `application.name`, then `media.name`, then the raw `node.name`.
    pub name: String,
    /// The node's `object.serial`, if `pw-dump` reported one — the number
    /// [`start_level_tap`]'s `pipewiresrc target-object=` needs (see
    /// [`build_pipeline`]'s doc comment for why `target-object` takes the
    /// serial and not the registry `id` above). `None` on the rare node that
    /// doesn't carry the property yet (a brand new one `pw-dump` caught
    /// mid-registration); [`start_level_tap`] just fails with a clear error
    /// for that one entry rather than mis-tapping it.
    pub serial: Option<u64>,
}

/// Lists every application currently producing audio: PipeWire nodes with
/// `media.class = "Stream/Output/Audio"` (confirmed against this machine's
/// real `pw-dump` output — see module docs), excluding this module's own
/// loopback nodes (see [`LOOPBACK_NAME_PREFIX`]) so an in-progress audio
/// share doesn't list itself as a shareable source.
///
/// **One entry per name** (task #105): an app often has several streams with
/// the same name (browser tabs, a game plus its launcher); the choice in the
/// UI is the *name*, and [`start_audio_sharing`] mixes every stream that has
/// it. The entry returned is the first such node, a representative for the
/// level meter.
pub fn list_audio_sources() -> Result<Vec<AudioSource>> {
    let objects = run_pw_dump()?;
    Ok(dedup_by_name(extract_audio_sources(&objects)))
}

/// Keeps the first node of each name, in listing order.
fn dedup_by_name(sources: Vec<AudioSource>) -> Vec<AudioSource> {
    let mut seen = std::collections::HashSet::new();
    sources.into_iter().filter(|s| seen.insert(s.name.clone())).collect()
}

/// Registry ids of every stream named `name` in this snapshot.
fn node_ids_named(objects: &[PwObject], name: &str) -> Vec<u32> {
    extract_audio_sources(objects).into_iter().filter(|s| s.name == name).map(|s| s.id).collect()
}

/// What the 1 s poll does with a fresh snapshot: the matching nodes not
/// linked yet (to link), and `linked` with the vanished ones dropped (PipeWire
/// removes a node's links itself when the node goes, so forgetting is all
/// that is left to do).
fn link_diff(matching: &[u32], linked: &std::collections::HashSet<u32>) -> (Vec<u32>, std::collections::HashSet<u32>) {
    let to_add = matching.iter().copied().filter(|id| !linked.contains(id)).collect();
    let kept = linked.iter().copied().filter(|id| matching.contains(id)).collect();
    (to_add, kept)
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
        let serial = prop_u64(props, "object.serial");
        sources.push(AudioSource { id: obj.id, name, serial });
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
///
/// The child is set to die with this process (see [`die_with_parent`]), so a
/// SIGKILLed or crashed client cannot leave an orphaned `ssshare-loopback-…`
/// node behind in the PipeWire graph.
fn spawn_loopback(loopback_name: &str) -> Result<Child> {
    let mut cmd = Command::new("pw-loopback");
    cmd.args(["--name", loopback_name, "-i", "node.autoconnect=false", "-o", "node.autoconnect=false"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    die_with_parent(&mut cmd);
    cmd.spawn().context("failed to spawn pw-loopback (is PipeWire's pw-loopback tool installed?)")
}

/// Makes `cmd`'s child receive SIGTERM when its parent dies, however the
/// parent dies (including SIGKILL, which no `Drop` or signal handler of ours
/// can intercept). Linux only (`PR_SET_PDEATHSIG`); a no-op elsewhere.
///
/// `Why:` `pw-loopback` is a long-lived helper process. Before this, killing
/// the client with SIGKILL (or a crash) left it running as an orphan
/// (ppid 1) with its `ssshare-loopback-…` nodes still in the PipeWire graph;
/// it happened twice overnight and the orphans held other sessions' locks.
/// `pw-loopback` quits cleanly on SIGTERM.
///
/// Caveat: the kernel ties the signal to the *thread* that spawned the child,
/// not the process. That is fine for the callers here (tokio runtime worker
/// threads and the test thread live as long as the process), but it would
/// not be for a thread that exits early, such as a `spawn_blocking` thread
/// after its idle timeout: call this only from long-lived threads.
///
/// Race: if the parent dies between `fork` and the `prctl`, the signal is
/// never delivered, so the child compares `getppid()` against the parent it
/// was forked from and refuses to exec when it is already orphaned.
#[cfg(target_os = "linux")]
fn die_with_parent(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;

    // SAFETY: `getpid` has no preconditions.
    let parent = unsafe { libc::getpid() };
    // SAFETY: the closure runs in the forked child before exec and calls only
    // async-signal-safe functions (`prctl`, `getppid`) and builds an
    // `io::Error` from errno without allocating.
    unsafe {
        cmd.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM as libc::c_ulong) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != parent {
                return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
            }
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
fn die_with_parent(_cmd: &mut Command) {}

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

/// Links every not-yet-linked stream named `name` in `objects` into the
/// loopback input and records it in `linked`; forgets nodes that vanished.
/// A node that cannot be linked yet (no ports, `pw-link` error) is not
/// recorded, so the next tick retries it. Never links a node of another name.
fn sync_links(objects: &[PwObject], name: &str, loopback_input_node_id: u32, linked: &mut std::collections::HashSet<u32>) {
    let matching = node_ids_named(objects, name);
    let (to_add, kept) = link_diff(&matching, linked);
    *linked = kept;
    for id in to_add {
        match link_source_into_loopback(objects, id, loopback_input_node_id) {
            Ok(()) => {
                tracing::info!("audio share '{name}': mixed in stream node {id}");
                linked.insert(id);
            }
            Err(e) => tracing::debug!("audio share '{name}': stream node {id} not linked yet: {e:#}"),
        }
    }
}

/// Whether the poll must rebuild the loopback + capture pipeline before linking a stream (task
/// #117). `gap` is sticky: it is set when nothing is linked into the loopback any more (the app
/// closed, or muted and Firefox destroyed its stream) and is only cleared by a rebuild. Returns
/// `(gap, rebuild)`: rebuild when there was a gap and a stream is ready to be linked again.
///
/// `Why:` a loopback that has been through a stretch with no input and then gets a new stream
/// linked in sounded "really broken" until the user re-selected the source (which spawns a fresh
/// loopback and pipeline with the stream already linked). The poll now does that same reset itself.
fn gap_decision(gap: bool, nothing_linked: bool, linkable_new: bool) -> (bool, bool) {
    let gap = gap || nothing_linked;
    (gap, gap && linkable_new)
}

/// Kills its child on drop, so a rebuild cancelled by `abort()` (stop) cannot leak a `pw-loopback`.
struct ChildGuard(Option<Child>);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(c) = self.0.as_mut() {
            kill_and_wait(c);
        }
    }
}

/// What [`link_poll_loop`] needs to replace the loopback + pipeline source of a running share.
struct Chain {
    stream_id: u8,
    pipeline: gst::Pipeline,
    loopback_child: Arc<Mutex<Child>>,
    pts_offset_us: Arc<AtomicU64>,
}

/// Replaces the loopback with a fresh one, links the streams named `name` into it and restarts the
/// capture pipeline on it, exactly the order of [`start_audio_sharing`] (what a manual re-select
/// does). Returns the new loopback input node id and the linked set. On error the old loopback
/// process is still held by the handle; the poll retries next tick.
async fn rebuild_chain(chain: &Chain, name: &str) -> Result<(u32, std::collections::HashSet<u32>)> {
    let loopback_name = format!("{LOOPBACK_NAME_PREFIX}{}-{}", chain.stream_id, uuid::Uuid::new_v4().simple());
    let mut new_child = ChildGuard(Some(spawn_loopback(&loopback_name)?));
    let (objects, lb) = wait_for_loopback_ready(&loopback_name, Duration::from_secs(5)).await?;
    let mut linked = std::collections::HashSet::new();
    sync_links(&objects, name, lb.input_node_id, &mut linked);

    let capture_node_name = format!("{loopback_name}-cap");
    let src = chain.pipeline.by_name("src").ok_or_else(|| anyhow!("pipeline has no element named 'src'"))?;
    chain.pipeline.set_state(gst::State::Null).context("failed to stop the audio pipeline for a rebuild")?;
    src.set_property("target-object", lb.output_node_serial.to_string());
    src.set_property("client-name", capture_node_name.as_str());
    chain.pipeline.set_state(gst::State::Playing).context("failed to restart the audio pipeline")?;
    let (state, _, _) = chain.pipeline.state(gst::ClockTime::from_seconds(5));
    state.map_err(|e| anyhow!("audio pipeline failed to reach PLAYING after a rebuild: {e:?}"))?;
    // Running time restarted at 0 with the pipeline: re-anchor the wire PTS (same as at start).
    chain.pts_offset_us.store(
        sender_clock_offset_us(chain.pipeline.current_running_time().map(|t| t.useconds()).unwrap_or(0), Instant::now()),
        Ordering::Relaxed,
    );
    wait_for_capture_link(&capture_node_name, lb.output_node_id, Duration::from_secs(5)).await?;

    let mut old = std::mem::replace(&mut *chain.loopback_child.lock().unwrap_or_else(|p| p.into_inner()), new_child.0.take().expect("set above"));
    kill_and_wait(&mut old);
    tracing::info!("audio share '{name}': rebuilt the loopback ({loopback_name}) after the source reappeared");
    Ok((lb.input_node_id, linked))
}

/// Once a second, re-lists the graph and links streams that now carry the
/// shared name (new browser tab, restarted app). A stream that reappears after
/// a stretch with none is linked into a rebuilt loopback ([`gap_decision`]).
async fn link_poll_loop(name: String, mut loopback_input_node_id: u32, mut linked: std::collections::HashSet<u32>, chain: Chain) {
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tick.tick().await; // the first tick is immediate; start_audio_sharing just synced
    // Started with the app absent: the loopback has never had input, same situation as a gap.
    let mut gap = linked.is_empty();
    loop {
        tick.tick().await;
        let objects = match tokio::task::spawn_blocking(run_pw_dump).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => {
                tracing::debug!("audio link poll: {e:#}");
                continue;
            }
            Err(_) => return,
        };
        // Only link while our loopback input is still in the graph.
        if node_port_ids(&objects, loopback_input_node_id as u64, "input").is_empty() {
            continue;
        }
        let (to_add, kept) = link_diff(&node_ids_named(&objects, &name), &linked);
        let linkable = to_add.iter().any(|&id| !node_port_ids(&objects, id as u64, "output").is_empty());
        let (g, rebuild) = gap_decision(gap, kept.is_empty(), linkable);
        gap = g;
        if rebuild {
            match rebuild_chain(&chain, &name).await {
                Ok((input, l)) => {
                    loopback_input_node_id = input;
                    linked = l;
                    gap = false;
                }
                Err(e) => tracing::warn!("audio share '{name}': rebuilding the loopback failed, will retry: {e:#}"),
            }
        } else {
            sync_links(&objects, &name, loopback_input_node_id, &mut linked);
        }
    }
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
    level_db_array(structure, "rms")
}

/// Reads one per-channel dB array (`rms`, `peak`, `decay`) out of a `level` element message.
fn level_db_array(structure: &gst::StructureRef, field: &str) -> Option<Vec<f64>> {
    // `glib` is not a direct dependency of this crate; `gstreamer` re-exports
    // the exact version it was built against, which is the one that matters
    // for a `GValue` handed over by a GStreamer element.
    let values = structure.get::<gst::glib::ValueArray>(field).ok()?;
    Some(values.iter().filter_map(|v| v.get::<f64>().ok()).collect())
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
///
/// `volume name=gain` + `rglimiter` ([`GAIN_STAGE`]) sit before the mute and the meter: the
/// sender-side boost of Settings (task #114, `audio_gain.rs`), so the meter shows what is sent.
fn build_pipeline(loopback_output_node_serial: u64, capture_node_name: &str, gain: f64, muted: bool) -> Result<BuiltPipeline> {
    build_pipeline_from_source(
        &format!("pipewiresrc name=src target-object={loopback_output_node_serial} client-name={capture_node_name} do-timestamp=true"),
        gain,
        muted,
    )
}

/// [`build_pipeline`] with the source element(s) given as a description
/// fragment, so a test can put `audiotestsrc` in front of the *real* chain
/// (no PipeWire involved).
fn build_pipeline_from_source(source: &str, gain: f64, muted: bool) -> Result<BuiltPipeline> {
    let description = pipeline_description(source);

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

    let gain_stage = GainStage::from_pipeline(&pipeline)?;
    gain_stage.set(gain);
    // Set before PLAYING so a share that replaces a muted one (source switch, reconnect) never
    // sends a single unmuted buffer.
    mute.set_property("mute", muted);

    Ok(BuiltPipeline { pipeline, sink, mute, gain_stage })
}

/// What [`build_pipeline`] hands back.
struct BuiltPipeline {
    pipeline: gst::Pipeline,
    sink: gst_app::AppSink,
    mute: gst::Element,
    gain_stage: GainStage,
}

/// The gain stage of the sender pipeline: F32 samples, a `volume` for the boost and an
/// `rglimiter` after it (task #114). See [`GainStage`].
///
/// `Why F32LE:` `rglimiter` only takes 32-bit float, and a boost above 1 on 16-bit samples
/// would clip before the limiter could see it; float carries the overshoot to the limiter.
/// `opusenc` (1.28) only takes S16LE, so an `audioconvert` after the meter ([`pipeline_description`])
/// converts back; the float stage is what lets the limiter, not a 16-bit wrap or clip, see the
/// overshoot. `Why rglimiter and not audiodynamic:` it is a zero-latency waveshaper (soft knee
/// from -6 dBFS up, output never above full scale), which is what "catch the peaks the boost
/// made" needs; `audiodynamic` is a per-sample compressor whose ratio-0 setting is a hard clip
/// and whose soft knee only softens the ratio, not the ceiling. Checked in
/// `audio::tests::the_limiter_keeps_a_boosted_full_scale_sine_under_full_scale`.
///
/// `rglimiter` lives in gst-plugins-good's `replaygain` plugin, a plugin file of its own. A system
/// without it still shares audio: [`gain_stage_description`] leaves the limiter out (a boost
/// above 1x can then clip, a logged warning says so) instead of failing the whole share.
const GAIN_STAGE: &str = "audio/x-raw,format=F32LE ! volume name=gain ! rglimiter name=limiter";
const GAIN_STAGE_NO_LIMITER: &str = "audio/x-raw,format=F32LE ! volume name=gain";

/// [`GAIN_STAGE`], or [`GAIN_STAGE_NO_LIMITER`] when this GStreamer has no `rglimiter`.
fn gain_stage_description() -> &'static str {
    if gst::ElementFactory::find("rglimiter").is_some() {
        GAIN_STAGE
    } else {
        tracing::warn!("GStreamer element rglimiter (gst-plugins-good, replaygain) is missing: a boosted audio share can clip");
        GAIN_STAGE_NO_LIMITER
    }
}

/// The sender pipeline's description (everything of [`build_pipeline`] but the element lookups),
/// split out so a test can check the order of the elements.
fn pipeline_description(source: &str) -> String {
    let gain_stage = gain_stage_description();
    format!(
        "{source} \
         ! audio/x-raw,channels=2 \
         ! audioconvert \
         ! audioresample \
         ! {gain_stage} \
         ! volume name=mute \
         ! level name=level post-messages=true interval={LEVEL_INTERVAL_NS} \
         ! audioconvert \
         ! opusenc \
         ! appsink name=sink emit-signals=false sync=false max-buffers=32 drop=true"
    )
}

/// The two live elements of the boost: `volume name=gain` and `rglimiter name=limiter`.
///
/// [`GainStage::set`] changes both at once and is what a Settings change during a live share
/// calls (no pipeline restart: both are plain element properties). The limiter is only enabled
/// above 1x ([`crate::audio_gain::limiter_needed`]); disabled, `rglimiter` is a pass-through,
/// so at the default 100 % the audio is exactly what it was before the setting existed.
#[derive(Clone)]
struct GainStage {
    volume: gst::Element,
    /// `None` on a system without `rglimiter` ([`gain_stage_description`]).
    limiter: Option<gst::Element>,
}

impl GainStage {
    fn from_pipeline(pipeline: &gst::Pipeline) -> Result<Self> {
        let volume = pipeline.by_name("gain").ok_or_else(|| anyhow!("pipeline has no element named 'gain'"))?;
        Ok(Self { volume, limiter: pipeline.by_name("limiter") })
    }

    fn set(&self, gain: f64) {
        let gain = crate::audio_gain::clamp_gain(gain);
        self.volume.set_property("volume", gain);
        if let Some(limiter) = &self.limiter {
            limiter.set_property("enabled", crate::audio_gain::limiter_needed(gain));
        }
    }

    fn gain(&self) -> f64 {
        self.volume.property::<f64>("volume")
    }
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
    loopback_child: Arc<Mutex<Child>>,
    pump_task: tokio::task::JoinHandle<()>,
    /// The 1 s poll that links streams of the chosen name as they appear
    /// ([`link_poll_loop`]); aborted on stop/drop.
    link_task: tokio::task::JoinHandle<()>,
    level: AudioLevel,
    /// The pipeline's `volume name=mute` element — see
    /// [`AudioCaptureHandle::set_muted`].
    mute_element: gst::Element,
    /// The sender-side boost ([`AudioCaptureHandle::set_gain`]).
    gain_stage: GainStage,
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
pub async fn start_audio_sharing(client: &ScreenshareClient, stream_id: u8, source: &AudioSource, gain: f64, muted: bool) -> Result<AudioCaptureHandle> {
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

    // Mix in every stream that has the chosen name right now. Zero matches is
    // fine: the share starts silent and the poll below links the app when it
    // (re)appears. The capture pipeline reads only the loopback's output, so
    // an unlinked loopback is silence, never the microphone.
    let mut linked = std::collections::HashSet::new();
    sync_links(&objects, &source.name, loopback.input_node_id, &mut linked);

    let capture_node_name = format!("{loopback_name}-cap");
    let BuiltPipeline { pipeline, sink: appsink, mute: mute_element, gain_stage } =
        match build_pipeline(loopback.output_node_serial, &capture_node_name, gain, muted) {
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
    // Anchor this pipeline's running time onto the process-wide sender clock that video's wire PTS
    // also uses, so a viewer can line audio up with the picture (`decode/avsync.rs`).
    let pts_offset_us = Arc::new(AtomicU64::new(sender_clock_offset_us(
        pipeline.current_running_time().map(|t| t.useconds()).unwrap_or(0),
        Instant::now(),
    )));
    let pump_task = tokio::spawn(pump_loop(pump_client, stream_id, stream, bus_stream, level.clone(), pts_offset_us.clone()));
    let loopback_child = Arc::new(Mutex::new(loopback_child));
    let chain = Chain { stream_id, pipeline: pipeline.clone(), loopback_child: loopback_child.clone(), pts_offset_us };
    let link_task = tokio::spawn(link_poll_loop(source.name.clone(), loopback.input_node_id, linked, chain));

    // No width/height for audio (spec's AnnounceStream fields exist for
    // video streams' benefit); 0x0 signals "not applicable".
    if let Err(e) = client
        .announce_stream(stream_id, StreamKind::Audio, &source.name, 0, 0)
        .await
        .context("failed to announce audio stream to the relay")
    {
        let _ = pipeline.set_state(gst::State::Null);
        pump_task.abort();
        link_task.abort();
        kill_and_wait(&mut loopback_child.lock().unwrap_or_else(|p| p.into_inner()));
        return Err(e);
    }

    Ok(AudioCaptureHandle {
        stream_id,
        client: client.clone(),
        pipeline,
        loopback_child,
        pump_task,
        link_task,
        level,
        mute_element,
        gain_stage,
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
        self.link_task.abort();
        let _ = self.pipeline.set_state(gst::State::Null);
        let mut child = self.loopback_child.lock().unwrap_or_else(|p| p.into_inner());
        let _ = child.kill();
        let _ = child.wait(); // reap, avoid a zombie
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

    /// Set the sender-side boost of this running share (Settings > Audio boost, task #114):
    /// takes effect on the next buffer, no restart. Clamped to the setting's range.
    pub fn set_gain(&self, gain: f64) {
        self.gain_stage.set(gain);
    }

    /// The boost this share is applying now.
    pub fn gain(&self) -> f64 {
        self.gain_stage.gain()
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
        self.link_task.abort();
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
        kill_and_wait(&mut self.loopback_child.lock().unwrap_or_else(|p| p.into_inner()));

        if let Err(e) = self.client.stop_stream(self.stream_id).await {
            tracing::warn!("error notifying relay that audio stream {} stopped: {e:#}", self.stream_id);
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------
// Non-announcing level taps (dropdown-entry meters)
// ---------------------------------------------------------------------

/// A lightweight, private level reading for one *candidate* audio source —
/// **not** a share: this never loopbacks, links, encodes, or announces
/// anything to the relay. It exists purely so the "Add audio"/"Switch"
/// dropdown can show a live meter next to each option, so the user can tell
/// which running application is actually making sound before picking it —
/// see `superdoc/features/stream-control.md` for the feature this backs.
///
/// # Why a separate, much simpler pipeline than [`AudioCaptureHandle`]
///
/// [`start_audio_sharing`]'s pipeline is the real capture path: it has to
/// prove (via [`wait_for_capture_link`]) it's tapping the exact node the
/// user picked, because getting that wrong would silently broadcast the
/// wrong audio (or the microphone) to the whole room. A level tap has no
/// such stake — nothing downstream of it ever leaves this process, so a
/// mistargeted tap is a wrong-looking meter, not a privacy incident. That's
/// what justifies skipping the `pw-loopback` + `pw-link` indirection
/// entirely and tapping the candidate node directly with `pipewiresrc
/// target-object=<serial>` — one pipeline per candidate, no extra process,
/// nothing else in the graph to build or tear down, and no
/// [`wait_for_capture_link`]-style verification step needed either.
///
/// `Why `autoconnect` is left at its default (`true`), unlike this doc
/// comment originally assumed:` the obvious-looking hardening —
/// `autoconnect=false`, so a mistargeted tap reads silence instead of
/// falling back to whatever the session manager likes best — turns out not
/// to mean what it sounds like for `pipewiresrc`. Confirmed live (a bare
/// `gst-launch-1.0 pipewiresrc target-object=<serial> autoconnect=false ...`
/// against a real running source): the pipeline hangs forever in
/// `READY -> PAUSED`, because `autoconnect=false` disables `pipewiresrc`
/// from connecting to *anything* at all, including the `target-object` it
/// was explicitly given — it isn't "only skip the fallback", it's "never
/// connect, ever", and nothing in this element's GStreamer properties
/// offers "use `target-object` but never fall back". So `autoconnect`
/// stays at its default; a level tap's `target-object` either resolves (the
/// normal case) or — per [`build_pipeline`]'s doc comment on the same
/// failure mode — falls back to the session manager's default source. For
/// a real share that's a privacy bug ([`wait_for_capture_link`] exists
/// because of it); for a tap that never leaves this process, it is, at
/// worst, one dropdown entry's meter looking momentarily wrong, so it's
/// left as a known, harmless edge case rather than solved with a property
/// that turns out to break the pipeline outright.
pub struct AudioLevelTapHandle {
    pipeline: gst::Pipeline,
    level: AudioLevel,
    pump_task: tokio::task::JoinHandle<()>,
}

/// Starts a non-announcing level tap on the PipeWire node whose
/// `object.serial` is `node_serial` (see [`AudioSource::serial`]). Returns
/// once the pipeline has been asked to play — deliberately does **not**
/// wait for it to reach `Playing` or verify what it linked to (see
/// [`AudioLevelTapHandle`]'s doc comment for why that's fine here): a
/// dropdown entry's meter just reads 0 until the tap actually starts
/// producing `level` messages, which is a fine "not making sound yet"
/// starting state for a UI meter to show.
pub fn start_level_tap(node_serial: u64) -> Result<AudioLevelTapHandle> {
    ensure_gstreamer_initialized()?;

    let description = format!(
        "pipewiresrc target-object={node_serial} do-timestamp=true \
         ! audio/x-raw \
         ! audioconvert \
         ! level name=level post-messages=true interval={LEVEL_INTERVAL_NS} \
         ! fakesink sync=false"
    );
    let element = gst::parse::launch(&description)
        .with_context(|| format!("failed to construct audio level-tap pipeline: {description}"))?;
    let pipeline = element
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("gst::parse::launch did not return a top-level Pipeline"))?;

    if let Err(e) = pipeline.set_state(gst::State::Playing).context("failed to start audio level-tap pipeline") {
        let _ = pipeline.set_state(gst::State::Null);
        return Err(e);
    }

    let bus = match pipeline.bus() {
        Some(b) => b,
        None => {
            let _ = pipeline.set_state(gst::State::Null);
            return Err(anyhow!("audio level-tap pipeline has no bus"));
        }
    };

    let level = AudioLevel::default();
    let pump_task = tokio::spawn(level_tap_pump_loop(bus.stream(), level.clone()));

    Ok(AudioLevelTapHandle { pipeline, level, pump_task })
}

impl AudioLevelTapHandle {
    /// Newest tap reading, 0.0-1.0 — same scale/curve as
    /// [`AudioCaptureHandle::level`] (both go through
    /// [`level_fraction_from_rms_db`]), so a dropdown-entry meter and the
    /// always-visible share meter read consistently.
    pub fn level(&self) -> f32 {
        self.level.get()
    }

    /// Tears the tap down: stops the pipeline and aborts the bus-pump task.
    /// No relay/EOS drain needed (unlike [`AudioCaptureHandle::stop`]) —
    /// there's no downstream consumer to hand a graceful EOS to, just a
    /// `fakesink`, so a hard stop is the whole teardown.
    pub async fn stop(self) {
        let _ = self.pipeline.set_state(gst::State::Null);
        self.pump_task.abort();
    }
}

impl Drop for AudioLevelTapHandle {
    /// Backstop mirroring [`AudioCaptureHandle`]'s `Drop`: if a caller drops
    /// this handle instead of calling `stop()` (e.g. the dropdown closing
    /// tears down every tap in a batch without awaiting each one), the
    /// pipeline still stops synchronously so no `pipewiresrc` element is
    /// left running.
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
        self.pump_task.abort();
    }
}

/// Bus-only pump for a level tap: no appsink/encode/send side at all, just
/// folding `level` element messages into `level` (mirrors [`pump_loop`]'s
/// bus half, minus the `appsink_stream` side pump_loop also has to drive).
async fn level_tap_pump_loop(mut bus_stream: gst::bus::BusStream, level: AudioLevel) {
    let mut bus_done = false;
    while let Some(msg) = bus_stream.next().await {
        handle_bus_message(&msg, &mut bus_done, &level);
        if bus_done {
            return;
        }
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
    pts_offset_us: Arc<AtomicU64>,
) {
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
                        if let Err(e) = send_encoded_sample(&client, stream_id, &sample, pts_offset_us.load(Ordering::Relaxed)) {
                            tracing::warn!("dropping encoded audio packet for stream {stream_id}: {e:#}");
                        }
                    }
                    None => {
                        tracing::info!("audio capture stream {stream_id}: appsink ended (no more packets; the level meter stops with it)");
                        appsink_done = true;
                    }
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

/// Offset that maps this audio pipeline's running time onto the sender clock
/// ([`crate::decode::sender_clock_epoch`]): `(now - epoch) - running_time`, read at one instant.
///
/// `Why:` audio PTS used to be the pipeline's own running time (0 at share start) while video PTS
/// counted from its destination's creation, so a viewer could not tell which audio packet belongs
/// to which frame. Both now count from the process-wide epoch, as capture time. Saturates to 0 if
/// the running time somehow exceeds the process age (clock reads are not atomic).
fn sender_clock_offset_us(running_time_us: u64, now: Instant) -> u64 {
    sender_clock_us(now).saturating_sub(running_time_us)
}

/// Wire PTS of one Opus packet: its capture time on the sender clock. A buffer without a PTS gets
/// the send time (`now_clock_us`), which is late by the encode latency but never wrong by more.
fn audio_wire_pts_us(buffer_pts_us: Option<u64>, pts_offset_us: u64, now_clock_us: u64) -> u64 {
    match buffer_pts_us {
        Some(p) => p.saturating_add(pts_offset_us),
        None => now_clock_us,
    }
}

/// Encrypts, fragments, and sends one encoded Opus packet as media
/// datagrams — same encrypt/fragment/send path as
/// `capture.rs::send_encoded_sample`.
fn send_encoded_sample(client: &ScreenshareClient, stream_id: u8, sample: &gst::Sample, pts_offset_us: u64) -> Result<()> {
    let buffer = sample.buffer().ok_or_else(|| anyhow!("sample had no buffer"))?;

    let pts_us = audio_wire_pts_us(buffer.pts().map(|t| t.useconds()), pts_offset_us, sender_clock_us(Instant::now()));

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

    // ---- the sender-side boost (task #114) --------------------------------------------------
    // No test here touches PipeWire (hard rule: no audio tests): the chain is built around
    // `audiotestsrc` and read out of an `appsink`, which is plain memory.

    /// `audiotestsrc` (a full-scale sine unless `amplitude` says otherwise) through the real gain
    /// stage into an appsink that keeps the newest two buffers.
    fn gain_test_pipeline(amplitude: f64, is_live: bool) -> (gst::Pipeline, gst_app::AppSink, GainStage) {
        ensure_gstreamer_initialized().unwrap();
        let description = format!(
            "audiotestsrc is-live={is_live} wave=sine freq=440 volume={amplitude} samplesperbuffer=480 \
             ! audio/x-raw,rate=48000,channels=2 ! audioconvert ! audioresample ! {} \
             ! appsink name=sink sync=false max-buffers=2 drop=true",
            gain_stage_description()
        );
        let pipeline = gst::parse::launch(&description).unwrap().downcast::<gst::Pipeline>().unwrap();
        let sink = pipeline.by_name("sink").unwrap().downcast::<gst_app::AppSink>().unwrap();
        let stage = GainStage::from_pipeline(&pipeline).unwrap();
        (pipeline, sink, stage)
    }

    /// The largest absolute sample of a buffer of interleaved F32LE samples.
    fn peak_of(sample: &gst::Sample) -> f32 {
        let buffer = sample.buffer().unwrap();
        let map = buffer.map_readable().unwrap();
        map.as_slice().chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]).abs()).fold(0.0, f32::max)
    }

    /// Peak over the first `n` buffers `sink` yields.
    fn peak_of_first(sink: &gst_app::AppSink, n: usize) -> f32 {
        (0..n)
            .map(|_| peak_of(&sink.try_pull_sample(gst::ClockTime::from_seconds(5)).expect("no sample from the test pipeline")))
            .fold(0.0, f32::max)
    }

    fn db(x: f32) -> f64 {
        20.0 * (x as f64).log10()
    }

    /// The point of the limiter: +6 dB (2x, the range top) on a full-scale sine would be a peak of 2.0; the
    /// output must stay under full scale, and still be loud (not a muted or collapsed signal).
    #[test]
    fn the_limiter_keeps_a_boosted_full_scale_sine_under_full_scale() {
        let (pipeline, sink, stage) = gain_test_pipeline(1.0, false);
        stage.set(crate::audio_gain::MAX_GAIN);
        pipeline.set_state(gst::State::Playing).unwrap();
        let peak = peak_of_first(&sink, 2);
        let _ = pipeline.set_state(gst::State::Null);
        // The limiter's ceiling is full scale itself: a float 1.0 is exactly 0 dBFS (and 32767 in
        // 16 bit), never above it. Without the limiter this peak would be 2.0 (+6 dBFS).
        assert!(peak <= 1.0, "boosted sine peaked at {} ({:.1} dBFS), the limiter let it past full scale", peak, db(peak));
        assert!(peak > 0.9, "the limiter flattened the signal: peak {peak}");
    }

    /// The control for the test above: the same chain without the limiter does go far past full
    /// scale, so the limiter is what holds the peak (and the fallback without `rglimiter` is
    /// really the unprotected one).
    #[test]
    fn without_the_limiter_the_same_boost_peaks_far_past_full_scale() {
        ensure_gstreamer_initialized().unwrap();
        let description = format!(
            "audiotestsrc wave=sine freq=440 volume=1.0 samplesperbuffer=480 \
             ! audio/x-raw,rate=48000,channels=2 ! audioconvert ! {GAIN_STAGE_NO_LIMITER} \
             ! appsink name=sink sync=false max-buffers=2 drop=true"
        );
        let pipeline = gst::parse::launch(&description).unwrap().downcast::<gst::Pipeline>().unwrap();
        let sink = pipeline.by_name("sink").unwrap().downcast::<gst_app::AppSink>().unwrap();
        let stage = GainStage::from_pipeline(&pipeline).unwrap();
        assert!(stage.limiter.is_none());
        stage.set(crate::audio_gain::MAX_GAIN);
        pipeline.set_state(gst::State::Playing).unwrap();
        let peak = peak_of_first(&sink, 2);
        let _ = pipeline.set_state(gst::State::Null);
        assert!(peak > 1.9, "peak {peak}");
    }

    /// Everything from 1.0x to 2.0x, a loud (-1 dBFS) and a quiet (-20 dBFS) source: never past full scale.
    #[test]
    fn no_boost_in_the_range_clips_past_full_scale() {
        for amplitude in [0.9, 0.1] {
            for gain in [1.01, 1.25, 1.5, 2.0] {
                let (pipeline, sink, stage) = gain_test_pipeline(amplitude, false);
                stage.set(gain);
                pipeline.set_state(gst::State::Playing).unwrap();
                let peak = peak_of_first(&sink, 2);
                let _ = pipeline.set_state(gst::State::Null);
                assert!(peak <= 1.0, "amplitude {amplitude} x{gain}: peak {peak}");
            }
        }
    }

    /// A quiet source boosted so its peak stays below the limiter's knee is only multiplied:
    /// +12 dB on -20 dBFS is -8 dBFS (the soft knee starts at -6).
    #[test]
    fn a_boost_below_the_knee_is_a_plain_multiplication() {
        let (pipeline, sink, stage) = gain_test_pipeline(0.1, false);
        let plain = {
            pipeline.set_state(gst::State::Playing).unwrap();
            let p = peak_of_first(&sink, 2);
            let _ = pipeline.set_state(gst::State::Null);
            p
        };
        let (pipeline, sink, _stage) = gain_test_pipeline(0.1, false);
        _stage.set(3.98);
        pipeline.set_state(gst::State::Playing).unwrap();
        let boosted = peak_of_first(&sink, 2);
        let _ = pipeline.set_state(gst::State::Null);
        drop(stage);
        assert!((db(boosted) - db(plain) - 12.0).abs() < 0.2, "{} dB -> {} dB", db(plain), db(boosted));
    }

    /// The default (1.0x) must leave the audio exactly alone, even a full-scale signal: the limiter
    /// is bypassed (it is a waveshaper from -6 dBFS up and would otherwise colour loud audio).
    #[test]
    fn the_default_gain_leaves_a_full_scale_signal_untouched() {
        let (pipeline, sink, stage) = gain_test_pipeline(1.0, false);
        stage.set(1.0);
        pipeline.set_state(gst::State::Playing).unwrap();
        let peak = peak_of_first(&sink, 2);
        let _ = pipeline.set_state(gst::State::Null);
        assert!(peak > 0.999, "default gain changed a full-scale sine: peak {peak}");
        // And below 1x it is a plain attenuation.
        let (pipeline, sink, stage) = gain_test_pipeline(1.0, false);
        stage.set(0.5);
        pipeline.set_state(gst::State::Playing).unwrap();
        let peak = peak_of_first(&sink, 2);
        let _ = pipeline.set_state(gst::State::Null);
        assert!((peak - 0.5).abs() < 0.01, "peak {peak}");
    }

    /// A change while the pipeline runs takes effect without a restart.
    #[test]
    fn the_gain_changes_live_without_restarting_the_pipeline() {
        let (pipeline, sink, stage) = gain_test_pipeline(0.1, true);
        pipeline.set_state(gst::State::Playing).unwrap();
        let before = peak_of_first(&sink, 2);
        stage.set(2.0);
        assert_eq!(stage.gain(), 2.0);
        // The appsink holds at most two buffers (10 ms each): after 200 ms they are post-change.
        std::thread::sleep(Duration::from_millis(200));
        let after = peak_of_first(&sink, 2);
        let state = pipeline.state(gst::ClockTime::ZERO).1;
        let _ = pipeline.set_state(gst::State::Null);
        assert_eq!(state, gst::State::Playing, "the pipeline kept running");
        assert!((db(after) - db(before) - 6.02).abs() < 0.3, "{} dB -> {} dB", db(before), db(after));
    }

    /// The setter clamps (a bad caller cannot push 100x into the encoder).
    #[test]
    fn the_stage_clamps_what_it_is_given() {
        let (_pipeline, _sink, stage) = gain_test_pipeline(1.0, false);
        stage.set(100.0);
        assert_eq!(stage.gain(), crate::audio_gain::MAX_GAIN);
        stage.set(0.0);
        assert_eq!(stage.gain(), crate::audio_gain::MIN_GAIN);
        stage.set(f64::NAN);
        assert_eq!(stage.gain(), 1.0);
    }

    /// The real sender description: gain, then limiter, then mute, then meter, then the encoder.
    #[test]
    fn the_sender_pipeline_boosts_then_limits_before_mute_meter_and_encoder() {
        ensure_gstreamer_initialized().unwrap();
        let d = pipeline_description("pipewiresrc name=src target-object=42 client-name=cap do-timestamp=true");
        let at = |needle: &str| d.find(needle).unwrap_or_else(|| panic!("{needle:?} missing from {d}"));
        assert!(at("format=F32LE") < at("volume name=gain"), "{d}");
        assert!(at("volume name=gain") < at("rglimiter name=limiter"), "{d}");
        assert!(at("rglimiter name=limiter") < at("volume name=mute"), "{d}");
        assert!(at("volume name=mute") < at("level name=level"), "{d}");
        assert!(at("level name=level") < at("opusenc"), "{d}");
        assert!(d.rfind("audioconvert").unwrap() > at("level name=level"), "opusenc needs S16 back: {d}");
        assert!(d.rfind("audioconvert").unwrap() < at("opusenc"), "{d}");
        assert!(at("audioresample") < at("format=F32LE"), "{d}");
    }

    /// The whole sender chain, with `audiotestsrc` in place of `pipewiresrc`, links, negotiates F32
    /// all the way into `level` and `opusenc`, and produces Opus packets.
    #[test]
    fn the_sender_chain_negotiates_and_encodes_opus() {
        ensure_gstreamer_initialized().unwrap();
        let BuiltPipeline { pipeline, sink, gain_stage, .. } =
            build_pipeline_from_source("audiotestsrc num-buffers=20 samplesperbuffer=960", 3.0, false).expect("chain builds");
        assert_eq!(gain_stage.gain(), 3.0);
        pipeline.set_state(gst::State::Playing).unwrap();
        let sample = sink.try_pull_sample(gst::ClockTime::from_seconds(5)).expect("no Opus packet");
        let caps = sample.caps().unwrap().to_string();
        let _ = pipeline.set_state(gst::State::Null);
        assert!(caps.starts_with("audio/x-opus"), "{caps}");
    }

    /// Task #117: a stream that comes back after a gap is linked into a rebuilt loopback, once; a
    /// stream that merely replaces another one (another stays linked) and a failed link do not
    /// rebuild. Driven with the same `link_diff` the poll uses.
    #[test]
    fn a_stream_reappearing_after_a_gap_rebuilds_the_loopback_once() {
        use std::collections::HashSet;
        let set = |v: &[u32]| v.iter().copied().collect::<HashSet<u32>>();
        // One poll tick: (gap, linked) in, (gap, rebuild) out, `ports` = nodes whose ports exist.
        let tick = |gap: bool, matching: &[u32], linked: &HashSet<u32>, ports: &[u32]| {
            let (to_add, kept) = link_diff(matching, linked);
            let linkable = to_add.iter().any(|id| ports.contains(id));
            gap_decision(gap, kept.is_empty(), linkable)
        };
        // Floorp playing and linked: nothing to do.
        assert_eq!(tick(false, &[82], &set(&[82]), &[82]), (false, false));
        // Muted: the node is gone, nothing linked. Gap noted, no node to link yet.
        assert_eq!(tick(false, &[], &set(&[82]), &[]), (true, false));
        assert_eq!(tick(true, &[], &set(&[]), &[]), (true, false));
        // Unmuted: a new node, not linkable until it has ports...
        assert_eq!(tick(true, &[90], &set(&[]), &[]), (true, false));
        // ...then rebuild (the caller clears the gap and starts from the rebuilt linked set).
        assert_eq!(tick(true, &[90], &set(&[]), &[90]), (true, true));
        assert_eq!(tick(false, &[90], &set(&[90]), &[90]), (false, false));
        // One of two tabs closes and a third opens: the loopback never went empty, no rebuild.
        assert_eq!(tick(false, &[11, 12], &set(&[10, 11]), &[11, 12]), (false, false));
        // Started with the app absent (gap from the start): rebuild when it shows up.
        assert_eq!(tick(true, &[5], &set(&[]), &[5]), (true, true));
    }

    /// The rebuild stops the capture pipeline, changes the source's properties and starts it again
    /// (task #117). The appsink must keep delivering and the running time restarts at 0, which is
    /// why `rebuild_chain` re-anchors the wire PTS offset. `audiotestsrc` stands in for
    /// `pipewiresrc`, which `pipeline_description` callers also name `src`.
    #[test]
    fn the_pipeline_delivers_again_after_a_null_playing_restart() {
        ensure_gstreamer_initialized().unwrap();
        let BuiltPipeline { pipeline, sink, .. } =
            build_pipeline_from_source("audiotestsrc name=src is-live=true samplesperbuffer=960", 1.0, false).expect("chain builds");
        let pull = || sink.try_pull_sample(gst::ClockTime::from_seconds(5)).expect("no Opus packet");
        pipeline.set_state(gst::State::Playing).unwrap();
        for _ in 0..30 {
            pull();
        }
        let late_pts = pull().buffer().unwrap().pts().unwrap();
        pipeline.set_state(gst::State::Null).unwrap();
        pipeline.by_name("src").unwrap().set_property("freq", 880.0f64);
        pipeline.set_state(gst::State::Playing).unwrap();
        let first = pull().buffer().unwrap().pts().unwrap();
        let _ = pipeline.set_state(gst::State::Null);
        assert!(first < late_pts, "running time restarts at 0 after Null: {first} vs {late_pts}");
    }

    /// A share that replaces a muted one (source switch, reconnect) starts muted, and the mute is
    /// the same `volume name=mute` element the toggle drives (task #118).
    #[test]
    fn a_pipeline_can_be_built_muted() {
        ensure_gstreamer_initialized().unwrap();
        for muted in [true, false] {
            let built = build_pipeline_from_source("audiotestsrc", 1.0, muted).expect("chain builds");
            assert_eq!(built.mute.property::<bool>("mute"), muted);
        }
    }

    #[test]
    fn audio_wire_pts_is_capture_time_on_the_sender_clock() {
        // Pipeline started when the process was 90 s old, so offset = 90 s; a packet captured
        // 1.5 s into the pipeline is at 91.5 s on the sender clock, the same axis video uses.
        assert_eq!(audio_wire_pts_us(Some(1_500_000), 90_000_000, 0), 91_500_000);
        // No buffer PTS: fall back to the send time.
        assert_eq!(audio_wire_pts_us(None, 90_000_000, 123_456), 123_456);
    }

    #[test]
    fn sender_clock_offset_never_underflows() {
        // A running time larger than the process age (non-atomic clock reads) saturates to 0.
        assert_eq!(sender_clock_offset_us(u64::MAX / 2, Instant::now()), 0);
        let off = sender_clock_offset_us(0, Instant::now());
        assert!(off < 3_600_000_000, "offset is the process age in us, got {off}");
    }

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
                        "node.description": "TestApp",
                        "object.serial": 90210
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
        assert_eq!(sources, vec![AudioSource { id: 262, name: "TestApp".to_string(), serial: Some(90210) }]);
    }

    /// Three app streams: two named "Firefox" (ids 10, 11), one "Spotify" (20),
    /// plus a `Stream/Input/Audio` that is also called "Firefox" (wrong class).
    fn duplicate_name_objects() -> Vec<PwObject> {
        let node = |id: u32, class: &str, desc: &str| {
            format!(
                r#"{{"id":{id},"type":"PipeWire:Interface:Node","info":{{"props":{{"media.class":"{class}","node.name":"n{id}","node.description":"{desc}","object.serial":{}}}}}}}"#,
                id + 1000
            )
        };
        let json = format!(
            "[{},{},{},{}]",
            node(10, "Stream/Output/Audio", "Firefox"),
            node(20, "Stream/Output/Audio", "Spotify"),
            node(11, "Stream/Output/Audio", "Firefox"),
            node(30, "Stream/Input/Audio", "Firefox"),
        );
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn dropdown_lists_each_name_once_with_the_first_node_as_representative() {
        let objects = duplicate_name_objects();
        let all = extract_audio_sources(&objects);
        assert_eq!(all.len(), 3, "extraction keeps every stream");
        let listed = dedup_by_name(all);
        let names: Vec<_> = listed.iter().map(|s| (s.name.as_str(), s.id)).collect();
        assert_eq!(names, vec![("Firefox", 10), ("Spotify", 20)]);
    }

    #[test]
    fn a_name_matches_all_its_output_streams_and_nothing_else() {
        let objects = duplicate_name_objects();
        assert_eq!(node_ids_named(&objects, "Firefox"), vec![10, 11]);
        assert_eq!(node_ids_named(&objects, "Spotify"), vec![20]);
        assert!(node_ids_named(&objects, "Discord").is_empty());
        // The real fixture's own loopback output is never matched, whatever the name.
        assert!(node_ids_named(&fixture_objects(), "output.ssshare-loopback-3-abc123").is_empty());
    }

    #[test]
    fn link_diff_adds_new_matches_and_forgets_vanished_ones() {
        use std::collections::HashSet;
        let set = |v: &[u32]| v.iter().copied().collect::<HashSet<u32>>();
        // Nothing linked yet: link everything that matches.
        assert_eq!(link_diff(&[10, 11], &set(&[])), (vec![10, 11], set(&[])));
        // Steady state: nothing to do.
        assert_eq!(link_diff(&[10, 11], &set(&[10, 11])), (vec![], set(&[10, 11])));
        // A new tab appears (12), tab 10 closes.
        assert_eq!(link_diff(&[11, 12], &set(&[10, 11])), (vec![12], set(&[11])));
        // The app is gone entirely: the share stays up with nothing linked.
        assert_eq!(link_diff(&[], &set(&[10, 11])), (vec![], set(&[])));
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

    /// The share's real capture chain (`build_pipeline_from_source`, here with
    /// `audiotestsrc` for `pipewiresrc`) delivers `level` messages through the
    /// same bus stream + `handle_bus_message` the live share's `pump_loop`
    /// uses, so [`AudioCaptureHandle::level`] moves off zero. No PipeWire.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn capture_chain_level_messages_reach_the_level_value() {
        if gst::init().is_err() {
            eprintln!("skipping: gstreamer not available in this environment");
            return;
        }
        let BuiltPipeline { pipeline, sink, .. } =
            build_pipeline_from_source("audiotestsrc is-live=true do-timestamp=true", 1.0, false).expect("chain builds");
        let level = AudioLevel::default();
        assert_eq!(level.get(), 0.0);
        // Same order as `start_audio_sharing`: PLAYING first, the bus and appsink
        // streams only later (after the link check), so messages are already queued.
        pipeline.set_state(gst::State::Playing).expect("playing");
        let _ = pipeline.state(gst::ClockTime::from_seconds(5));
        tokio::time::sleep(Duration::from_millis(1200)).await;
        let mut bus_stream = pipeline.bus().expect("bus").stream();
        let mut appsink_stream = sink.stream();
        let mut bus_done = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut packets = 0;
        while level.get() == 0.0 && tokio::time::Instant::now() < deadline {
            tokio::select! {
                s = appsink_stream.next() => { if s.is_some() { packets += 1; } }
                m = bus_stream.next() => { if let Some(m) = m { handle_bus_message(&m, &mut bus_done, &level); } }
                _ = tokio::time::sleep(Duration::from_millis(200)) => {}
            }
        }
        let got = level.get();
        let _ = pipeline.set_state(gst::State::Null);
        assert!(packets > 0, "no opus packets");
        assert!(got > 0.5, "audiotestsrc (~-5 dBFS RMS) should read well above half scale, got {got}");
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

    /// Exercises [`start_level_tap`] against two real, simultaneously
    /// running synthetic audio sources with visibly different volumes,
    /// confirming: the two taps' readings actually differ (proving the
    /// meter reflects *which* source is making sound, not just "some
    /// source somewhere"), and that `stop()` leaves no `pipewiresrc`
    /// element/pipeline behind — checked via `pgrep` for this test's own
    /// `gst-launch-1.0` processes staying up (the taps must not touch
    /// those) alongside confirming the tap pipelines' own state after
    /// `stop()`. Not runnable unattended/in CI (spawns real system
    /// processes + needs a live PipeWire session), hence `#[ignore]`. Run
    /// manually with: `cargo test audio::tests::level_tap_reflects_the_right_source_and_tears_down_cleanly -- --ignored --nocapture`
    ///
    /// `Why the client-name carries a random suffix:` confirmed live,
    /// WirePlumber's stream-restore keeps a per-`node.name`/
    /// `application.name` volume that silently attenuates a *fixed*
    /// client-name's signal further and further across repeated test runs
    /// during this feature's development — a real environment gotcha, not
    /// a bug in [`start_level_tap`]. A random suffix sidesteps it by never
    /// reusing a name WirePlumber has state for.
    ///
    /// `Why `stream-properties=props,node.autoconnect=(boolean)false` on
    /// both source apps, and why the volumes are capped at 5%/2%:` a plain
    /// `pipewiresink` (no `stream-properties` override) is real, live,
    /// audibly-routed audio — WirePlumber's default policy auto-links any
    /// new `Stream/Output/Audio` node to the real default sink (speakers),
    /// same as any other application. Earlier revisions of this test (and
    /// of the ad hoc verification scripts around this feature) missed that
    /// and were genuinely audible on the dev machine — confirmed the hard
    /// way, corrected here. `node.autoconnect=false` stops WirePlumber's
    /// policy from linking this node to the real sink at all (it only
    /// controls the *policy* auto-link, not whether our own
    /// `pipewiresrc target-object=` can still explicitly target it — that
    /// keeps working, confirmed live), and the 5%/2% volumes are a second,
    /// independent safety margin in case autoconnect is ever re-enabled by
    /// a future edit.
    #[tokio::test]
    #[ignore]
    async fn level_tap_reflects_the_right_source_and_tears_down_cleanly() {
        let run_id = uuid::Uuid::new_v4().simple().to_string();
        let loud_name = format!("ssshare-leveltap-loud-{run_id}");
        let quiet_name = format!("ssshare-leveltap-quiet-{run_id}");
        let mut loud = Command::new("gst-launch-1.0")
            .args([
                "audiotestsrc",
                "volume=0.05",
                "!",
                "audioconvert",
                "!",
                "pipewiresink",
                &format!("client-name={loud_name}"),
                "stream-properties=props,node.autoconnect=(boolean)false",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn loud synthetic app");
        let mut quiet = Command::new("gst-launch-1.0")
            .args([
                "audiotestsrc",
                "volume=0.02",
                "!",
                "audioconvert",
                "!",
                "pipewiresink",
                &format!("client-name={quiet_name}"),
                "stream-properties=props,node.autoconnect=(boolean)false",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn quiet synthetic app");

        tokio::time::sleep(Duration::from_secs(2)).await;

        let sources = list_audio_sources().expect("list_audio_sources should succeed against a live pw-dump");
        let loud_source = sources.iter().find(|s| s.name.contains(&loud_name)).cloned().expect("loud source not listed");
        let quiet_source = sources.iter().find(|s| s.name.contains(&quiet_name)).cloned().expect("quiet source not listed");
        let loud_serial = loud_source.serial.expect("loud source should have an object.serial");
        let quiet_serial = quiet_source.serial.expect("quiet source should have an object.serial");

        let loud_tap = start_level_tap(loud_serial).expect("failed to start loud tap");
        let quiet_tap = start_level_tap(quiet_serial).expect("failed to start quiet tap");

        // Give both pipelines time to negotiate and start posting `level`
        // messages (LEVEL_INTERVAL_NS = 100ms; a few seconds is comfortably
        // enough even on a loaded CI-like box).
        tokio::time::sleep(Duration::from_secs(3)).await;

        let loud_level = loud_tap.level();
        let quiet_level = quiet_tap.level();
        eprintln!("[level-tap-test] loud_level={loud_level} quiet_level={quiet_level}");
        assert!(loud_level > 0.05, "loud source's tap should read a clearly non-zero level, got {loud_level}");
        assert!(loud_level > quiet_level, "loud source's tap ({loud_level}) should read louder than the quiet one ({quiet_level})");

        loud_tap.stop().await;
        quiet_tap.stop().await;

        // Teardown check: neither this test's own synthetic apps (which the
        // taps must never touch — non-destructive by design, see
        // `AudioLevelTapHandle`'s doc comment) nor any tap pipeline element
        // should still show up in the PipeWire graph.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let objects_after = run_pw_dump().expect("pw-dump should still succeed after teardown");
        let still_has_loud_app = objects_after.iter().any(|o| {
            o.info.as_ref().and_then(|i| i.props.as_ref()).and_then(|p| prop_str(p, "node.name")).map(|n| n == loud_name).unwrap_or(false)
        });
        assert!(still_has_loud_app, "the synthetic source app itself should still be running/listed — taps must be non-destructive");

        let _ = loud.kill();
        let _ = loud.wait();
        let _ = quiet.kill();
        let _ = quiet.wait();
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
        // Located via `end_to_end_test::server_binary_path` (honours
        // `SSSHARE_SERVER_BIN` and `CARGO_TARGET_DIR`); the guard kills the
        // relay on drop.
        let _server = crate::end_to_end_test::spawn_relay_server(port).expect("failed to spawn the relay server (build it first)");

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
        let handle = start_audio_sharing(&client, 7, &source, 1.0, false)
            .await
            .expect("start_audio_sharing failed");

        // Let it actually run/produce a few Opus packets before stopping,
        // like a real share would.
        tokio::time::sleep(Duration::from_secs(1)).await;

        let loopback_pid = handle.loopback_child.lock().unwrap().id();
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

        eprintln!("[audio-stop-test] PASS: stop() returned in {stop_elapsed:?}, pw-loopback (pid {loopback_pid}) reaped");
    }

    // ---- die_with_parent (PR_SET_PDEATHSIG) ------------------------------
    //
    // A child that must die with its parent can only be tested by killing a
    // parent, and the test process cannot be that parent. So the test
    // re-executes this very test binary as a "parent" process (the helper
    // below, selected by an env var), waits for it to publish its child's
    // pid, SIGKILLs it, and asserts the child follows. Uses `sleep` as the
    // child, so it needs neither PipeWire nor a display.

    /// Env var that turns [`die_with_parent_helper_process`] from a no-op
    /// into the helper "parent": its value is the file to publish pids to.
    #[cfg(target_os = "linux")]
    const PDEATHSIG_HELPER_ENV: &str = "SSSHARE_TEST_PDEATHSIG_PIDFILE";

    /// Not a test on its own: the process [`child_dies_when_its_parent_is_sigkilled`]
    /// re-executes and kills. Without the env var it does nothing.
    #[cfg(target_os = "linux")]
    #[test]
    fn die_with_parent_helper_process() {
        let Some(pidfile) = std::env::var_os(PDEATHSIG_HELPER_ENV) else { return };
        let mut cmd = Command::new("sleep");
        cmd.arg("300").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        die_with_parent(&mut cmd);
        let child = cmd.spawn().expect("spawn sleep");
        // Publish atomically: the watcher must never read a half-written file.
        let tmp = std::path::PathBuf::from(&pidfile).with_extension("tmp");
        std::fs::write(&tmp, child.id().to_string()).expect("write pidfile");
        std::fs::rename(&tmp, &pidfile).expect("publish pidfile");
        // Wait to be SIGKILLed (the parent test's job); bounded so a broken
        // test run cannot leave this process around forever.
        std::thread::sleep(Duration::from_secs(60));
    }

    /// `true` while `pid` is a live process (a zombie awaiting its reaper
    /// counts as dead: it is no longer running anything).
    #[cfg(target_os = "linux")]
    fn pid_is_running(pid: u32) -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            // "pid (comm) S ..." - comm may contain spaces/parens, so the
            // state is the first field after the *last* ')'.
            Ok(stat) => stat.rsplit(')').next().and_then(|rest| rest.split_whitespace().next()) != Some("Z"),
            Err(_) => false,
        }
    }

    /// **Regression:** `pw-loopback` survived a SIGKILLed client as an
    /// orphan (ppid 1). A child spawned through [`die_with_parent`] must die
    /// when its parent is SIGKILLed.
    #[cfg(target_os = "linux")]
    #[test]
    fn child_dies_when_its_parent_is_sigkilled() {
        let dir = std::env::temp_dir().join(format!("sss-pdeathsig-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let pidfile = dir.join("child.pid");

        let mut parent = Command::new(std::env::current_exe().expect("current_exe"))
            .args(["--exact", "audio::tests::die_with_parent_helper_process", "--nocapture", "--test-threads=1"])
            .env(PDEATHSIG_HELPER_ENV, &pidfile)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("re-exec the test binary as the helper parent");

        let deadline = Instant::now() + Duration::from_secs(20);
        let child_pid: u32 = loop {
            if let Some(pid) = std::fs::read_to_string(&pidfile).ok().and_then(|s| s.trim().parse().ok()) {
                break pid;
            }
            if Instant::now() > deadline {
                let _ = parent.kill();
                let _ = parent.wait();
                panic!("the helper parent never published its child's pid");
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        assert!(pid_is_running(child_pid), "the helper's child (pid {child_pid}) should be running before the kill");

        // SIGKILL: no Drop, no signal handler, exactly the failure case.
        parent.kill().expect("SIGKILL the helper parent");
        let _ = parent.wait();

        let deadline = Instant::now() + Duration::from_secs(5);
        while pid_is_running(child_pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        let survived = pid_is_running(child_pid);
        if survived {
            // Do not leak the 300s sleeper if the assertion below fails.
            // SAFETY: plain kill(2) on a pid this test created.
            unsafe { libc::kill(child_pid as libc::pid_t, libc::SIGKILL) };
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!survived, "child (pid {child_pid}) outlived its SIGKILLed parent: PR_SET_PDEATHSIG is not in effect");
    }
}
