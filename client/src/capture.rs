//! Wayland screen capture (xdg-desktop-portal `ScreenCast` + PipeWire) →
//! H.264 encode (GStreamer) → AEAD-encrypt → fragment → send, over the QUIC
//! media-datagram plane wired up in `net.rs`.
//!
//! # Pipeline
//!
//! ```text
//! pipewiresrc fd=<fd> path=<node_id> ! videoconvert ! video/x-raw,format=I420
//!   ! x264enc tune=zerolatency bitrate=<kbps> key-int-max=<gop> speed-preset=ultrafast
//!   ! video/x-h264,stream-format=byte-stream,alignment=au
//!   ! appsink name=sink
//! ```
//!
//! Two [`EncoderBackend`]s implement the encoder stage: software `x264enc`
//! (above) and, preferred when actually usable, VAAPI's `vah264enc` (format
//! NV12 instead of I420, `rate-control=cbr bitrate=<kbps> key-int-max=<gop>
//! target-usage=7` instead of x264enc's tune/speed-preset). [`start_sharing`]
//! auto-detects which to use via [`detect_encoder_backend`]/
//! [`probe_vaapi_encoder`] and falls back to software automatically if VAAPI
//! construction or startup fails for any reason — callers don't choose.
//!
//! Built via [`gst::parse::launch`] (see [`build_pipeline`]) rather than
//! hand-linked elements — it's the most direct way to keep the pipeline
//! description above (the thing worth eyeballing/diffing) as the actual
//! source of truth, and it's cheap to sanity-check independently of a live
//! portal session (swap `pipewiresrc fd=… path=…` for `videotestsrc`; see
//! this module's tests).
//!
//! # Threading / async model
//!
//! GStreamer drives buffer flow on its own element threads (`pipewiresrc`,
//! `x264enc`, …) once the pipeline is `Playing` — nothing here needs to poll
//! or pump that by hand. The two places this module needs to observe
//! GStreamer from async code (pulling encoded access units off `appsink`,
//! watching the bus for `Error`/`Eos`) use `gstreamer-app`'s
//! [`gst_app::AppSink::stream`] and `gstreamer`'s [`gst::Bus::stream`]
//! respectively: both bridge GStreamer's internal callback threads to a
//! `futures::Stream` via a waker, which is safe to `.await`/`select!` on from
//! any executor (here: the ambient tokio runtime `main.rs` keeps entered) —
//! no `std::thread`/`spawn_blocking` pump thread is needed. All of that
//! happens inside [`pump_loop`], spawned as one `tokio::spawn`ed task per
//! [`start_sharing`] call.
//!
//! # Scope of this pass
//!
//! Video only. See [`start_sharing`] for the portal/pipeline/announce flow
//! and [`CaptureHandle::stop`] for teardown.
//!
//! TODO(audio): PipeWire per-application audio tee + Opus encode is a
//! deliberate fast-follow, not an oversight — out of scope for this pass.
//! It'll likely live in a sibling module (or a second capture graph in this
//! one) that announces a second `StreamKind::Audio` stream on its own
//! `stream_id`, reusing the same encrypt/fragment/send path below.

use std::os::fd::{AsRawFd, OwnedFd};
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use ashpd::desktop::{PersistMode, Session};
use futures_util::StreamExt;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use crate::fragment::fragment_frame;
use crate::net::{encrypt_frame, ScreenshareClient};
use crate::protocol::StreamKind;

/// Element name of the VAAPI H.264 encoder in the installed GStreamer
/// version. GStreamer has shipped two generations of VAAPI plugin with
/// different element names across versions (`vaapih264enc` from the old
/// `gstreamer-vaapi` plugin, `vah264enc` from the newer `va` plugin in
/// `gst-plugins-bad`) — confirmed via `gst-inspect-1.0` on this dev machine
/// (GStreamer 1.28.5) that only `vah264enc` is present.
const VAAPI_ENCODER_ELEMENT: &str = "vah264enc";

/// Which H.264 encoder element a [`start_sharing`] pipeline uses.
///
/// [`start_sharing`] doesn't take this as a parameter — it auto-detects via
/// [`detect_encoder_backend`], preferring [`EncoderBackend::Vaapi`] and
/// falling back to [`EncoderBackend::Software`] when VAAPI isn't usable on
/// this machine (missing plugin, no VAAPI-capable device, etc).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EncoderBackend {
    /// libx264 software encoding. Always available; the guaranteed fallback.
    #[default]
    Software,
    /// VAAPI hardware encoding via `vah264enc` (see [`VAAPI_ENCODER_ELEMENT`]).
    Vaapi,
}

impl EncoderBackend {
    /// The `gst-launch`-syntax fragment for this backend's encoder element,
    /// including its bitrate/GOP properties. Deliberately returns just the
    /// element description (not surrounding `!`s) so [`build_pipeline`] stays
    /// the single place that assembles the full pipeline string.
    fn launch_fragment(self, bitrate_kbps: u32, gop_frames: u32) -> String {
        match self {
            EncoderBackend::Software => format!(
                "x264enc tune=zerolatency speed-preset=ultrafast bitrate={bitrate_kbps} key-int-max={gop_frames}"
            ),
            // vah264enc's `bitrate` property is kbps, same units as
            // x264enc's, and it has a `key-int-max` property with the exact
            // same name/semantics as x264enc's — verified via `gst-inspect-1.0
            // vah264enc`. `rate-control=cbr` matches x264enc's implicit
            // constant-bitrate behavior here (it's also vah264enc's default,
            // spelled out for clarity). `target-usage=7` is the fastest end
            // of vah264enc's 1 (best quality/slowest) – 7 (fastest/lowest
            // quality) speed/quality knob — the VAAPI analogue of x264enc's
            // `speed-preset=ultrafast`, appropriate for this pipeline's
            // zero-latency screen-share use case.
            EncoderBackend::Vaapi => format!(
                "{VAAPI_ENCODER_ELEMENT} rate-control=cbr bitrate={bitrate_kbps} key-int-max={gop_frames} target-usage=7"
            ),
        }
    }

    /// The raw-video pixel format to negotiate between `videoconvert` and
    /// this backend's encoder. `vah264enc`'s sink pad only accepts NV12 (or
    /// DMABuf/VAMemory, not relevant for the system-memory `videoconvert`
    /// output this pipeline produces) — see `gst-inspect-1.0 vah264enc`'s
    /// SINK pad template. `x264enc` is happy with I420, which is cheaper for
    /// `videoconvert` to produce from the portal's raw video.
    fn raw_format(self) -> &'static str {
        match self {
            EncoderBackend::Software => "I420",
            EncoderBackend::Vaapi => "NV12",
        }
    }
}

/// Auto-detects which [`EncoderBackend`] to use: prefers [`EncoderBackend::Vaapi`]
/// when a working VAAPI H.264 encoder is actually available on this machine,
/// falling back to [`EncoderBackend::Software`] otherwise. This is what makes
/// hardware encode "just work" without [`start_sharing`]'s callers having to
/// choose — see [`probe_vaapi_encoder`] for what "actually available" checks.
///
/// The probe result is cached for the process's lifetime (VAAPI device
/// availability doesn't change at runtime) so repeated [`start_sharing`]
/// calls don't pay the probe-pipeline cost every time.
fn detect_encoder_backend() -> EncoderBackend {
    use std::sync::OnceLock;
    static VAAPI_AVAILABLE: OnceLock<bool> = OnceLock::new();

    let available = *VAAPI_AVAILABLE.get_or_init(probe_vaapi_encoder);
    if available {
        EncoderBackend::Vaapi
    } else {
        EncoderBackend::Software
    }
}

/// Best-effort runtime probe for whether [`EncoderBackend::Vaapi`] will
/// actually work here, not just whether the plugin is installed:
///
/// 1. Constructs the `vah264enc` element via [`gst::ElementFactory::make`] —
///    catches the plugin/element simply not being installed.
/// 2. Builds a tiny throwaway `videotestsrc ! videoconvert !
///    video/x-raw,format=NV12 ! vah264enc ! fakesink` pipeline and drives it
///    to `Paused` — catches runtime failures like "no VAAPI-capable device
///    found" that only surface on a state change, not at construction time —
///    then immediately tears it back down.
///
/// Never panics; any failure at either step just means "not available",
/// logged at `info` level (this is an expected, handled path on machines
/// without a supported GPU, not a warning-worthy problem).
fn probe_vaapi_encoder() -> bool {
    if let Err(e) = gst::ElementFactory::make(VAAPI_ENCODER_ELEMENT).build() {
        tracing::info!(
            "VAAPI encoder element '{VAAPI_ENCODER_ELEMENT}' is not available ({e}); using software encoder"
        );
        return false;
    }

    let probe_description = format!(
        "videotestsrc num-buffers=1 ! videoconvert ! video/x-raw,format=NV12 ! {VAAPI_ENCODER_ELEMENT} ! fakesink"
    );
    let pipeline = match gst::parse::launch(&probe_description) {
        Ok(element) => match element.downcast::<gst::Pipeline>() {
            Ok(pipeline) => pipeline,
            Err(_) => {
                tracing::info!("VAAPI probe pipeline was not a Pipeline; using software encoder");
                return false;
            }
        },
        Err(e) => {
            tracing::info!("failed to construct VAAPI probe pipeline ({e}); using software encoder");
            return false;
        }
    };

    let probe_ok = match pipeline.set_state(gst::State::Paused) {
        Ok(_) => {
            let (state_result, _current, _pending) = pipeline.state(gst::ClockTime::from_seconds(3));
            state_result.is_ok()
        }
        Err(_) => false,
    };

    let _ = pipeline.set_state(gst::State::Null);

    if !probe_ok {
        tracing::info!(
            "VAAPI encoder '{VAAPI_ENCODER_ELEMENT}' failed to reach PAUSED (no VAAPI-capable device found?); using software encoder"
        );
    } else {
        tracing::info!("VAAPI hardware H.264 encoder '{VAAPI_ENCODER_ELEMENT}' is available");
    }

    probe_ok
}

/// Fixed capture framerate for this pass. A future task may make this
/// (and bitrate, resolution, encoder backend) user-configurable; the
/// plumbing here already accepts `bitrate_kbps` per the task that scoped
/// this module.
const CAPTURE_FRAMERATE: u32 = 30;

/// Per protocol-spec's fixed 2-second GOP interval (§9: "the next scheduled
/// 2 s keyframe").
const GOP_SECONDS: u32 = 2;

/// Sensible default; full user-selectable bitrate UI (up to the spec's
/// 40 Mbit/s ceiling) is a later task — this is just the plumbing default.
pub const DEFAULT_BITRATE_KBPS: u32 = 8000;

/// Ensures `gst::init()` runs exactly once no matter how many times
/// [`start_sharing`] is called.
fn ensure_gstreamer_initialized() -> Result<()> {
    use std::sync::OnceLock;
    static INIT: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    match INIT.get_or_init(|| gst::init().map_err(|e| e.to_string())) {
        Ok(()) => Ok(()),
        Err(e) => Err(anyhow!("gstreamer initialization failed: {e}")),
    }
}

// ---------------------------------------------------------------------
// Portal (ashpd) — request a screencast session and its PipeWire remote
// ---------------------------------------------------------------------

/// Everything obtained from the xdg-desktop-portal `ScreenCast` interface
/// needed to build the capture pipeline.
struct PortalCapture {
    node_id: u32,
    /// Kept alive for the lifetime of the capture: `pipewiresrc` dups the fd
    /// internally when it connects to the PipeWire core, so this is not
    /// double-closed by dropping it after the pipeline is torn down — this
    /// matches the pattern in ashpd's own
    /// `examples/screen_cast_gstreamer.rs`.
    _pipewire_fd: OwnedFd,
    session: Session<Screencast>,
}

/// Runs the portal permission flow: create a session, ask to record a
/// monitor (single-source picker for this pass — a full source picker UI is
/// a later concern per this module's scoping task), start it, and open the
/// PipeWire remote fd.
///
/// Returns a clear `Err` (never panics) if the portal is unavailable or the
/// user denies/cancels the permission dialog.
async fn open_portal_capture() -> Result<PortalCapture> {
    let proxy = Screencast::new()
        .await
        .context("could not reach the xdg-desktop-portal ScreenCast interface (is a portal backend, e.g. xdg-desktop-portal-gnome/-kde/-wlr, running?)")?;

    let session = proxy
        .create_session(Default::default())
        .await
        .context("failed to create a screencast portal session")?;

    proxy
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(CursorMode::Embedded)
                .set_sources(ashpd::enumflags2::BitFlags::from(SourceType::Monitor))
                .set_multiple(false)
                .set_persist_mode(PersistMode::DoNot),
        )
        .await
        .context("portal rejected screencast source selection")?;

    let response = proxy
        .start(&session, None, Default::default())
        .await
        .context("screencast portal 'Start' request failed")?
        .response()
        .map_err(|e| anyhow!("screencast permission was denied or cancelled: {e}"))?;

    let stream = response
        .streams()
        .first()
        .cloned()
        .ok_or_else(|| anyhow!("portal returned no capturable monitor (permission denied or nothing selected)"))?;

    let node_id = stream.pipe_wire_node_id();

    let pipewire_fd = proxy
        .open_pipe_wire_remote(&session, Default::default())
        .await
        .context("failed to open the PipeWire remote fd from the portal")?;

    Ok(PortalCapture {
        node_id,
        _pipewire_fd: pipewire_fd,
        session,
    })
}

// ---------------------------------------------------------------------
// GStreamer pipeline construction
// ---------------------------------------------------------------------

/// Builds and links (but does not start) the capture/encode pipeline
/// described in this module's doc comment, returning the pipeline and its
/// `appsink` element.
fn build_pipeline(
    pipewire_fd: std::os::fd::RawFd,
    node_id: u32,
    bitrate_kbps: u32,
    backend: EncoderBackend,
) -> Result<(gst::Pipeline, gst_app::AppSink)> {
    let gop_frames = GOP_SECONDS * CAPTURE_FRAMERATE;
    let encoder = backend.launch_fragment(bitrate_kbps, gop_frames);
    let raw_format = backend.raw_format();

    let description = format!(
        "pipewiresrc fd={pipewire_fd} path={node_id} do-timestamp=true \
         ! videoconvert \
         ! video/x-raw,format={raw_format} \
         ! {encoder} \
         ! video/x-h264,stream-format=byte-stream,alignment=au \
         ! appsink name=sink emit-signals=false sync=false max-buffers=4 drop=true"
    );

    let element = gst::parse::launch(&description)
        .with_context(|| format!("failed to construct capture pipeline: {description}"))?;
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

/// Builds `backend`'s capture pipeline and drives it to `Playing`, waiting
/// for the state change to actually settle (success or error) before
/// returning — used by [`start_sharing`] both for the preferred backend and,
/// on failure, the software fallback retry.
///
/// On any failure (construction or state-change), tears the pipeline back
/// down to `Null` before returning `Err`, so a failed attempt never leaks a
/// half-started pipeline into the fallback attempt.
fn start_pipeline(
    pipewire_fd: std::os::fd::RawFd,
    node_id: u32,
    bitrate_kbps: u32,
    backend: EncoderBackend,
) -> Result<(gst::Pipeline, gst_app::AppSink)> {
    let (pipeline, appsink) = build_pipeline(pipewire_fd, node_id, bitrate_kbps, backend)?;

    if let Err(e) = pipeline.set_state(gst::State::Playing) {
        let _ = pipeline.set_state(gst::State::Null);
        return Err(anyhow!("failed to start capture pipeline ({backend:?}): {e}"));
    }

    // Wait for the pipeline to actually get moving (or fail) before telling
    // the room this stream exists — announcing a stream that then
    // immediately errors out would be a confusing "stream with no frames"
    // for other members. `state()` blocks until the async state change
    // settles (success or error) or the timeout elapses.
    let (state_result, _current, _pending) = pipeline.state(gst::ClockTime::from_seconds(5));
    if let Err(e) = state_result {
        let _ = pipeline.set_state(gst::State::Null);
        return Err(anyhow!("capture pipeline ({backend:?}) failed to reach PLAYING: {e:?}"));
    }

    Ok((pipeline, appsink))
}

// ---------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------

/// A running screen-share capture started by [`start_sharing`]. Drop alone
/// does *not* tear this down cleanly (GStreamer/portal cleanup is async) —
/// call [`CaptureHandle::stop`].
pub struct CaptureHandle {
    stream_id: u8,
    client: ScreenshareClient,
    pipeline: gst::Pipeline,
    portal_session: Session<Screencast>,
    pump_task: tokio::task::JoinHandle<()>,
}

/// Starts sharing this machine's screen on `stream_id`: runs the portal
/// permission flow, builds and starts the capture/encode pipeline, and (once
/// the first encoded frame has actually gone out, i.e. capture is really
/// flowing — not just "pipeline exists") calls
/// [`ScreenshareClient::announce_stream`] so other room members see the
/// stream appear.
///
/// `bitrate_kbps` is plumbed straight into the encoder (see
/// [`DEFAULT_BITRATE_KBPS`] for a reasonable default); a bitrate-selection UI
/// is a later task.
///
/// Returns `Err` — never panics — if the portal is unavailable, the user
/// denies/cancels the screencast permission dialog, or the GStreamer
/// pipeline fails to construct/start.
pub async fn start_sharing(client: &ScreenshareClient, stream_id: u8, bitrate_kbps: u32) -> Result<CaptureHandle> {
    ensure_gstreamer_initialized()?;

    let portal = open_portal_capture().await?;
    let fd = portal._pipewire_fd.as_raw_fd();

    // Prefer hardware (VAAPI) encode when it's actually usable on this
    // machine, per detect_encoder_backend's probe; fall back to software.
    // Beyond that up-front probe, also fall back if the *real* pipeline
    // (with the real portal fd/node_id) fails to construct or reach
    // PLAYING — belt-and-suspenders against VAAPI issues the cheap probe
    // pipeline didn't happen to catch — so a flaky/partial VAAPI setup
    // never fails the whole capture when software encode would have worked.
    let preferred_backend = detect_encoder_backend();
    let (pipeline, appsink) = match start_pipeline(fd, portal.node_id, bitrate_kbps, preferred_backend) {
        Ok(started) => started,
        Err(e) if preferred_backend == EncoderBackend::Vaapi => {
            tracing::warn!("VAAPI capture pipeline failed to start ({e:#}); falling back to software encoder");
            start_pipeline(fd, portal.node_id, bitrate_kbps, EncoderBackend::Software)
                .context("software fallback capture pipeline also failed to start")?
        }
        Err(e) => return Err(e),
    };

    // Width/height for AnnounceStream: read off the negotiated sink caps
    // now that the pipeline is playing, falling back to 0x0 (unknown) if
    // caps haven't landed yet rather than failing the whole call over a
    // cosmetic field.
    let (width, height) = negotiated_size(&appsink).unwrap_or((0, 0));

    let stream = appsink.stream();
    let bus = pipeline.bus().ok_or_else(|| anyhow!("pipeline has no bus"))?;
    let bus_stream = bus.stream();

    let pump_client = client.clone();
    let pump_task = tokio::spawn(pump_loop(pump_client, stream_id, stream, bus_stream));

    client
        .announce_stream(stream_id, StreamKind::Video, "Screen", width, height)
        .await
        .context("failed to announce video stream to the relay")?;

    Ok(CaptureHandle {
        stream_id,
        client: client.clone(),
        pipeline,
        portal_session: portal.session,
        pump_task,
    })
}

fn negotiated_size(appsink: &gst_app::AppSink) -> Option<(u16, u16)> {
    let pad = appsink.static_pad("sink")?;
    let caps = pad.current_caps()?;
    let s = caps.structure(0)?;
    let width: i32 = s.get("width").ok()?;
    let height: i32 = s.get("height").ok()?;
    Some((width.clamp(0, u16::MAX as i32) as u16, height.clamp(0, u16::MAX as i32) as u16))
}

impl CaptureHandle {
    /// Tears the capture down cleanly: stops the GStreamer pipeline, closes
    /// the portal session, tells the relay this stream is gone (best-effort
    /// — a network error here is logged, not returned, since the pipeline is
    /// already stopped either way), and waits for the background send task
    /// to finish draining.
    pub async fn stop(self) -> Result<()> {
        // Stopping the pipeline (Null) unblocks the appsink's stream (EOS/
        // None) and the pump task exits on its own; no separate cancel
        // signal needed.
        if let Err(e) = self.pipeline.set_state(gst::State::Null) {
            tracing::warn!("error stopping capture pipeline: {e}");
        }

        if let Err(e) = self.pump_task.await {
            tracing::warn!("capture send task did not shut down cleanly: {e}");
        }

        if let Err(e) = self.portal_session.close().await {
            tracing::warn!("error closing screencast portal session: {e}");
        }

        if let Err(e) = self.client.stop_stream(self.stream_id).await {
            tracing::warn!("error notifying relay that stream {} stopped: {e:#}", self.stream_id);
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------
// Encode → encrypt → fragment → send pump
// ---------------------------------------------------------------------

/// Drains encoded access units from `appsink` and pipeline error/EOS
/// messages from `bus` concurrently, for as long as either stream keeps
/// producing. Exits when both streams end (normal teardown, see
/// [`CaptureHandle::stop`]) or the bus reports a pipeline error.
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
                            tracing::warn!("dropping encoded frame for stream {stream_id}: {e:#}");
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
                "capture pipeline error from {}: {} ({})",
                msg.src().map(|s| s.path_string().to_string()).unwrap_or_else(|| "unknown".into()),
                err.error(),
                err.debug().unwrap_or_default(),
            );
            *bus_done = true;
        }
        gst::MessageView::Eos(_) => {
            tracing::info!("capture pipeline reached EOS");
            *bus_done = true;
        }
        _ => {}
    }
}

/// Encrypts, fragments, and sends one encoded access unit as media
/// datagrams, per protocol-spec §5/§6: PTS-prefixed AEAD encryption via
/// `net::encrypt_frame` (nonce drawn from the connection's single shared
/// [`crate::net::NonceSequence`]), then split via `fragment::fragment_frame`
/// using the frame's nonce counter as `frame_id` — the receiver reconstructs
/// the same nonce from `(conn_id, frame_id)`, so these must match.
fn send_encoded_sample(client: &ScreenshareClient, stream_id: u8, sample: &gst::Sample, start: Instant) -> Result<()> {
    let buffer = sample.buffer().ok_or_else(|| anyhow!("sample had no buffer"))?;

    // Presentation timestamp in microseconds, per protocol-spec §6.1's
    // 8-byte PTS prefix. `do-timestamp=true` on pipewiresrc gives buffers a
    // pipeline running-time PTS; fall back to wall-clock-since-start on the
    // rare buffer that arrives without one rather than dropping the frame.
    let pts_us = buffer
        .pts()
        .map(|t| t.useconds())
        .unwrap_or_else(|| start.elapsed().as_micros() as u64);

    let keyframe = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);

    let map = buffer.map_readable().context("failed to map encoded H.264 access unit")?;
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

    /// Shared by [`pipeline_elements_link_and_produce_encoded_output`] and
    /// [`vaapi_pipeline_links_and_produces_encoded_output`]: builds a
    /// `videotestsrc ! videoconvert ! video/x-raw,format=<backend's format> !
    /// <backend's encoder> ! video/x-h264,... ! appsink` pipeline (the same
    /// chain [`build_pipeline`] produces, with `videotestsrc` standing in for
    /// `pipewiresrc fd=… path=…` since a real portal fd needs an interactive
    /// permission grant this test environment can't provide), runs it, and
    /// returns whether at least one encoded H.264 access unit came out.
    fn run_backend_and_expect_encoded_buffer(backend: EncoderBackend) -> bool {
        let encoder = backend.launch_fragment(DEFAULT_BITRATE_KBPS, GOP_SECONDS * CAPTURE_FRAMERATE);
        let raw_format = backend.raw_format();
        let description = format!(
            "videotestsrc num-buffers=10 \
             ! videoconvert \
             ! video/x-raw,format={raw_format} \
             ! {encoder} \
             ! video/x-h264,stream-format=byte-stream,alignment=au \
             ! appsink name=sink emit-signals=false sync=false"
        );

        let element = gst::parse::launch(&description).expect("pipeline should parse and link");
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
        got_buffer
    }

    /// Sanity-checks that the pipeline description this module builds is
    /// well-formed and that every element it names (`videoconvert`,
    /// `x264enc`, `appsink`, capsfilters) is actually available on this
    /// machine. Runs the pipeline briefly and asserts at least one encoded
    /// H.264 access unit comes out the other end, which exercises the exact
    /// `videoconvert ! I420 ! x264enc ! byte-stream ! appsink` chain
    /// `start_sharing`'s software fallback path uses.
    #[test]
    fn pipeline_elements_link_and_produce_encoded_output() {
        if gst::init().is_err() {
            eprintln!("skipping: gstreamer not available in this environment");
            return;
        }

        assert!(
            run_backend_and_expect_encoded_buffer(EncoderBackend::Software),
            "expected at least one encoded H.264 access unit from the software encoder test pipeline"
        );
    }

    /// Real-hardware proof that VAAPI encode actually works on this machine
    /// (not just that the plugin/element is installed): exercises the exact
    /// `videoconvert ! NV12 ! vah264enc ! byte-stream ! appsink` chain
    /// `start_sharing`'s preferred path uses, and asserts a real encoded
    /// H.264 access unit comes out the other end. Skipped (not failed) with
    /// an explanatory message on machines without a working VAAPI encoder —
    /// [`detect_encoder_backend`]'s probe-then-fall-back-to-software behavior
    /// is what makes that a non-issue for `start_sharing` itself.
    #[test]
    fn vaapi_pipeline_links_and_produces_encoded_output() {
        if gst::init().is_err() {
            eprintln!("skipping: gstreamer not available in this environment");
            return;
        }

        if !probe_vaapi_encoder() {
            eprintln!(
                "skipping: VAAPI encoder '{VAAPI_ENCODER_ELEMENT}' is not available/functional on this machine"
            );
            return;
        }

        assert!(
            run_backend_and_expect_encoded_buffer(EncoderBackend::Vaapi),
            "VAAPI probe reported available, but the real vah264enc pipeline produced no encoded output"
        );
    }

    /// [`detect_encoder_backend`] must never panic and must always return
    /// *some* usable backend, whichever this machine actually supports.
    #[test]
    fn detect_encoder_backend_returns_without_panicking() {
        if gst::init().is_err() {
            eprintln!("skipping: gstreamer not available in this environment");
            return;
        }

        let backend = detect_encoder_backend();
        eprintln!("detect_encoder_backend() -> {backend:?}");
    }

    /// Exercises the real xdg-desktop-portal `ScreenCast` flow end to end,
    /// including the interactive permission dialog — not runnable
    /// unattended/in CI, hence `#[ignore]`. Run manually with:
    /// `cargo test capture::tests::live_portal_probe -- --ignored --nocapture`
    /// and click "Share" in the dialog that appears; a `node_id` printed to
    /// stderr means the whole portal handshake (session creation, source
    /// selection, start, PipeWire remote fd) worked.
    #[tokio::test]
    #[ignore]
    async fn live_portal_probe() {
        match open_portal_capture().await {
            Ok(p) => eprintln!("portal succeeded: node_id={}", p.node_id),
            Err(e) => eprintln!("portal failed: {e:#}"),
        }
    }
}
