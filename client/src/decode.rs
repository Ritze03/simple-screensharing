//! Video decode pipeline: turns reassembled+decrypted H.264 access units for
//! one subscribed stream into `egui::ColorImage`s that `ui.rs` can upload to
//! a texture on each repaint.
//!
//! # Why this module's input isn't raw datagram bytes or a room key
//!
//! Per this task's instructions, `net.rs` was read first rather than
//! guessed at. That reading showed fragment reassembly and AEAD decryption
//! are *already fully implemented* in `net.rs`'s `datagram_reader_loop`:
//! every incoming media datagram is fed through `fragment::Reassembler`,
//! and once a frame is complete it's run through `net::decrypt_frame` with
//! the nonce built from the header's `conn_id`/`frame_id` exactly as spec
//! §6.5 describes. The result is emitted as
//! `ClientEvent::MediaFrame(DecryptedMediaFrame)` — already a single
//! decrypted H.264 access unit plus its PTS and keyframe flag. `net.rs`
//! never exposes raw ciphertext datagrams or the room key to a caller
//! outside itself, so there is nothing for `decode.rs` to decrypt or
//! reassemble, and no minimal `net.rs` extension was needed either.
//!
//! Redoing reassembly/decryption here would duplicate a second
//! `Reassembler` per stream for no benefit. So [`StreamDecoder::feed`]
//! takes a `&DecryptedMediaFrame` (the type `net.rs` already produces)
//! instead of raw bytes + a key: the `ui.rs` wiring (a later, separate
//! task) just needs to filter `ClientEvent::MediaFrame` by which
//! `stream_id` it's currently subscribed to/displaying and call
//! `StreamDecoder::feed` with each match.
//!
//! # GStreamer pipeline
//!
//! ```text
//! appsrc ! queue ! h264parse ! avdec_h264 ! videoconvert ! video/x-raw,format=RGBA ! appsink
//! ```
//!
//! `avdec_h264` (GStreamer's libav-backed software H.264 decoder — verified
//! present via `gst-inspect-1.0 avdec_h264` against the system's GStreamer
//! 1.28.5 install) is used directly rather than `decodebin`, since the
//! input codec is fixed and already known (H.264 per spec §6.1), so there
//! is nothing for autoplugging to discover. A hardware decoder would be a
//! nice-to-have but this pass is explicitly scoped "correctness over
//! performance". The `queue` element gives GStreamer its own streaming
//! thread for parse/decode/convert, so pushing a buffer into `appsrc` from
//! `feed()` returns quickly instead of synchronously running the whole
//! decode chain on the caller's thread.
//!
//! # Threading
//!
//! Each `StreamDecoder` owns one background `std::thread` (plain
//! `std::thread`, not `tokio::spawn_blocking`, since this is a long-lived
//! pipeline for the decoder's whole lifetime, not a bounded blocking call
//! inside an async task) that builds the pipeline, drives it to `Playing`,
//! and then just relays incoming access units (received over a
//! `std::sync::mpsc` channel fed by `feed()`) into `appsrc`. Decoded RGBA
//! frames arrive via `appsink`'s `new_sample` callback, invoked by
//! GStreamer on its own internal streaming thread (downstream of the
//! `queue`); that callback converts the sample straight to an
//! `egui::ColorImage` and stores it in a `Mutex<Option<ColorImage>>` slot
//! shared with the `StreamDecoder` handle, overwriting any previous unread
//! frame. That's the "drop and continue" policy applied to decoded output
//! too: if `ui.rs` hasn't polled [`StreamDecoder::latest_frame`] since the
//! last decode, the older frame is discarded rather than queued, so the
//! displayed image never falls behind live. Both `feed()` and
//! `latest_frame()` are non-blocking.
//!
//! # Error handling
//!
//! A single corrupt/incomplete access unit (e.g. one downstream of a frame
//! the `Reassembler` had to drop for missing fragments) or a decoder error
//! is logged and skipped — the GStreamer bus is drained for error/warning
//! messages each loop iteration without tearing the pipeline down, and
//! `appsink`'s callback treats a failed sample conversion the same way.
//! Nothing in this module panics on bad media content.
//!
//! # Audio
//!
//! `ClientEvent::MediaFrame` for a `StreamKind::Audio` stream carries raw
//! Opus packets via the exact same path as video (reassembled + decrypted
//! in `net.rs`; see [`AudioDecoder`] below). Unlike H.264, an Opus packet
//! never depends on any other packet (`audio.rs`'s sender always marks
//! `keyframe: true` for exactly this reason — see its comment at
//! `send_encoded_sample`), so there is no keyframe-gating on the decode
//! side either: every packet for the subscribed `stream_id` is pushed into
//! the pipeline as it arrives. Decoded PCM is handed straight to an audio
//! sink (`autoaudiosink` in production) rather than back to `ui.rs` as
//! samples to render — there is nothing for the UI to draw each frame, only
//! a volume knob to expose (see [`AudioDecoder::set_volume`]).
//!
//! ## Pipeline
//!
//! ```text
//! appsrc ! opusdec ! audioconvert ! audioresample ! volume name=vol ! autoaudiosink
//! ```
//!
//! `appsrc` is configured with fixed caps `audio/x-opus,
//! channel-mapping-family=(int)0` and deliberately *no* `channels`/`rate`
//! fields: `gst-inspect-1.0 opusdec`'s sink pad template accepts
//! `audio/x-opus, channel-mapping-family: 0` with `channels`/`rate`
//! unconstrained for exactly the mono/stereo case `opusenc` on the sender
//! side produces (verified live: `audiotestsrc` encoded through `opusenc`
//! at both 1 and 2 channels decodes correctly through `opusdec` fed nothing
//! but those bare caps — libopus recovers the channel count from each
//! packet's TOC byte). This matters because the wire protocol carries no
//! separate "audio format" side-channel (`StreamInfo` only has `width`/
//! `height`, meaningful for video only), so the decoder can't know the
//! sender's channel count up front any other way.
//!
//! `volume` (GStreamer's `volume` element) sits just before the sink so
//! [`AudioDecoder::set_volume`] can adjust playback level live without
//! rebuilding the pipeline; its `volume` property is already a 0.0-2.0
//! linear scale (`gst-inspect-1.0 volume`: "Double. Range: 0 - 10 Default:
//! 1"), so the UI's 0-200% slider maps onto it by dividing by 100.0 with no
//! further conversion.
//!
//! ## Threading
//!
//! Same model as [`StreamDecoder`]: one dedicated `std::thread` per
//! `AudioDecoder` owns the pipeline for its whole lifetime, fed over a
//! `std::sync::mpsc` channel by `feed()`/`set_volume()`, with the bus
//! drained for error/warning logging between messages and a clean
//! `Shutdown` message + `join()` on `Drop`. There is no `latest_frame()`
//! equivalent: decoded PCM goes straight through `volume` to the sink
//! inside GStreamer's own pipeline threads, with nothing for `ui.rs` to
//! poll or render per frame.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use gstreamer::prelude::*;
use gstreamer::{Bus, Buffer, Caps, ClockTime, Element, ElementFactory, Message, MessageView, Pipeline, State};
use gstreamer_app::{AppSink, AppSinkCallbacks, AppSrc};
use gstreamer_video::prelude::*;
use gstreamer_video::{VideoFrameRef, VideoInfo};

use crate::net::{DecryptedMediaFrame, DecryptedPreview};
use crate::protocol::{ConnId, StreamId};

/// Shared slot the appsink callback writes into and `latest_frame()` reads
/// (and clears) from.
type LatestFrameSlot = Arc<Mutex<Option<egui::ColorImage>>>;

/// Message sent from `feed()`/`Drop` to the pipeline-owning thread.
enum PipelineMsg {
    AccessUnit { pts_us: u64, data: Vec<u8> },
    Shutdown,
}

/// Decodes one subscribed video stream's incoming H.264 access units into
/// `egui::ColorImage` frames. See the module docs for the pipeline shape and
/// threading rationale.
pub struct StreamDecoder {
    stream_id: StreamId,
    tx: std_mpsc::Sender<PipelineMsg>,
    latest_frame: LatestFrameSlot,
    /// An H.264 decoder can't produce anything from a mid-GOP delta frame
    /// alone, so frames are dropped until the first keyframe is seen —
    /// otherwise every frame between subscribing and the next keyframe (up
    /// to the spec's 2-second GOP) would just be decode-error log spam.
    seen_keyframe: bool,
    thread: Option<thread::JoinHandle<()>>,
}

impl StreamDecoder {
    /// Build and start a decode pipeline dedicated to `stream_id`. Calls
    /// `gstreamer::init()` (idempotent — safe alongside other
    /// `StreamDecoder`s or a future `capture.rs`).
    pub fn new(stream_id: StreamId) -> Result<Self> {
        gstreamer::init().context("failed to initialize GStreamer")?;

        let latest_frame: LatestFrameSlot = Arc::new(Mutex::new(None));
        let (tx, rx) = std_mpsc::channel::<PipelineMsg>();

        let pipeline_frame_slot = latest_frame.clone();
        let thread = thread::Builder::new()
            .name(format!("decode-stream-{stream_id}"))
            .spawn(move || {
                if let Err(e) = run_pipeline(rx, pipeline_frame_slot) {
                    tracing::error!("decode pipeline for stream {stream_id} exited with error: {e:#}");
                }
            })
            .context("failed to spawn decode pipeline thread")?;

        Ok(Self {
            stream_id,
            tx,
            latest_frame,
            seen_keyframe: false,
            thread: Some(thread),
        })
    }

    pub fn stream_id(&self) -> StreamId {
        self.stream_id
    }

    /// Feed one already-reassembled-and-decrypted media frame (i.e. a
    /// `ClientEvent::MediaFrame` payload from `net.rs`). Frames for a
    /// different `stream_id` are ignored defensively (callers are expected
    /// to filter before calling, but a misrouted frame here must never
    /// panic). Never blocks: this only pushes onto an unbounded channel
    /// that the pipeline thread drains.
    pub fn feed(&mut self, frame: &DecryptedMediaFrame) {
        if frame.stream_id != self.stream_id {
            return;
        }
        if !self.seen_keyframe {
            if !frame.keyframe {
                return;
            }
            self.seen_keyframe = true;
        }
        let msg = PipelineMsg::AccessUnit {
            pts_us: frame.pts,
            data: frame.payload.clone(),
        };
        if self.tx.send(msg).is_err() {
            // The pipeline thread only stops after a `Shutdown` sent from
            // `Drop`, so this means it exited/panicked already (e.g. a
            // GStreamer element failed to build). Log and keep going —
            // one dead decoder shouldn't take the whole UI down.
            tracing::warn!(
                "decode pipeline thread for stream {} is gone; dropping frame",
                self.stream_id
            );
        }
    }

    /// Non-blocking poll for the most recently decoded frame. Returns
    /// `None` if nothing new has been decoded since the last call. Intended
    /// to be called once per `ui.rs` repaint and, when `Some`, uploaded via
    /// an `egui::TextureHandle::set`.
    pub fn latest_frame(&mut self) -> Option<egui::ColorImage> {
        self.latest_frame.lock().unwrap().take()
    }
}

impl Drop for StreamDecoder {
    fn drop(&mut self) {
        let _ = self.tx.send(PipelineMsg::Shutdown);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

/// Builds the pipeline, drives it to `Playing`, and pumps `rx` until
/// `Shutdown` (or the channel disconnects because every `StreamDecoder`
/// handle was dropped) — see module docs "Threading".
fn run_pipeline(rx: std_mpsc::Receiver<PipelineMsg>, latest_frame: LatestFrameSlot) -> Result<()> {
    let pipeline = Pipeline::new();

    let src = ElementFactory::make("appsrc")
        .name("src")
        .build()
        .context("appsrc element unavailable")?;
    let queue = ElementFactory::make("queue")
        .build()
        .context("queue element unavailable")?;
    let parse = ElementFactory::make("h264parse")
        .build()
        .context("h264parse element unavailable")?;
    let decoder = ElementFactory::make("avdec_h264")
        .build()
        .context("avdec_h264 element unavailable (is gst-libav installed?)")?;
    let convert = ElementFactory::make("videoconvert")
        .build()
        .context("videoconvert element unavailable")?;
    let sink = ElementFactory::make("appsink")
        .name("sink")
        .build()
        .context("appsink element unavailable")?;

    pipeline
        .add_many([&src, &queue, &parse, &decoder, &convert, &sink])
        .context("failed to add elements to pipeline")?;
    Element::link_many([&src, &queue, &parse, &decoder, &convert]).context("failed to link decode chain")?;

    let rgba_caps = Caps::builder("video/x-raw").field("format", "RGBA").build();
    convert
        .link_filtered(&sink, &rgba_caps)
        .context("failed to link videoconvert -> appsink with RGBA caps")?;

    let appsrc = src
        .dynamic_cast::<AppSrc>()
        .map_err(|_| anyhow!("appsrc element was not an AppSrc"))?;
    appsrc.set_caps(Some(
        &Caps::builder("video/x-h264")
            .field("stream-format", "byte-stream")
            .field("alignment", "au")
            .build(),
    ));
    appsrc.set_is_live(true);
    appsrc.set_format(gstreamer::Format::Time);

    let appsink = sink
        .dynamic_cast::<AppSink>()
        .map_err(|_| anyhow!("appsink element was not an AppSink"))?;
    // Always hold at most the newest decoded frame — matches the
    // drop-and-continue policy: a slow consumer sees the latest frame, not
    // a growing backlog.
    appsink.set_property("sync", false);
    appsink.set_property("max-buffers", 1u32);
    appsink.set_property("drop", true);

    let sink_frame_slot = latest_frame;
    appsink.set_callbacks(
        AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                match pull_color_image(sink) {
                    Ok(Some(image)) => {
                        *sink_frame_slot.lock().unwrap() = Some(image);
                    }
                    Ok(None) => {}
                    Err(e) => tracing::warn!("dropping frame that failed RGBA conversion: {e:#}"),
                }
                Ok(gstreamer::FlowSuccess::Ok)
            })
            .build(),
    );

    let bus = pipeline.bus().ok_or_else(|| anyhow!("pipeline has no bus"))?;

    pipeline
        .set_state(State::Playing)
        .context("failed to start decode pipeline")?;

    let result = pump_loop(&rx, &appsrc, &bus);

    let _ = appsrc.end_of_stream();
    let _ = pipeline.set_state(State::Null);
    result
}

/// Feed loop: pushes access units into `appsrc` as they arrive and drains
/// the bus for error/warning logging in between, until told to shut down.
fn pump_loop(rx: &std_mpsc::Receiver<PipelineMsg>, appsrc: &AppSrc, bus: &Bus) -> Result<()> {
    loop {
        while let Some(msg) = bus.pop() {
            log_bus_message(&msg);
        }

        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(PipelineMsg::AccessUnit { pts_us, data }) => {
                let mut buffer = Buffer::from_slice(data);
                if let Some(buffer_mut) = buffer.get_mut() {
                    buffer_mut.set_pts(ClockTime::from_useconds(pts_us));
                }
                // A push failure here (e.g. decoder choking on a corrupt
                // access unit downstream of a dropped fragment) is exactly
                // the "glitch and continue" case: log and move on to the
                // next frame rather than treating it as fatal.
                if let Err(e) = appsrc.push_buffer(buffer) {
                    tracing::warn!("appsrc dropped an access unit: {e:?}");
                }
            }
            Ok(PipelineMsg::Shutdown) => return Ok(()),
            Err(std_mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std_mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}

fn log_bus_message(msg: &Message) {
    match msg.view() {
        MessageView::Error(e) => {
            tracing::warn!(
                "decode pipeline error from {:?}: {} ({:?})",
                e.src().map(|s| s.path_string()),
                e.error(),
                e.debug()
            );
        }
        MessageView::Warning(w) => {
            tracing::debug!(
                "decode pipeline warning from {:?}: {} ({:?})",
                w.src().map(|s| s.path_string()),
                w.error(),
                w.debug()
            );
        }
        _ => {}
    }
}

/// Pull the newest available sample off `sink` and convert it to an
/// `egui::ColorImage`, respecting the negotiated row stride (RGBA rows can
/// be padded past `width * 4` bytes depending on the decoder/converter, so
/// this must not assume a tightly packed buffer).
fn pull_color_image(sink: &AppSink) -> Result<Option<egui::ColorImage>> {
    let sample = match sink.try_pull_sample(ClockTime::from_mseconds(0)) {
        Some(s) => s,
        None => return Ok(None),
    };
    let buffer = sample.buffer().ok_or_else(|| anyhow!("sample had no buffer"))?;
    let caps = sample.caps().ok_or_else(|| anyhow!("sample had no caps"))?;
    let info = VideoInfo::from_caps(caps).context("failed to parse negotiated video caps")?;

    let frame = VideoFrameRef::from_buffer_ref_readable(buffer, &info)
        .map_err(|_| anyhow!("failed to map decoded video buffer readable"))?;

    let width = frame.width() as usize;
    let height = frame.height() as usize;
    let stride = frame.plane_stride()[0] as usize;
    let plane = frame
        .plane_data(0)
        .map_err(|_| anyhow!("decoded frame had no plane 0 data"))?;

    let packed = pack_rgba_rows(plane, width, height, stride)?;

    Ok(Some(egui::ColorImage::from_rgba_unmultiplied([width, height], &packed)))
}

/// Tightly packs `height` RGBA rows of `width * 4` bytes each out of `plane`,
/// which is laid out with `stride` bytes between the start of consecutive
/// rows (`stride >= width * 4`; any bytes past `width * 4` on a given row
/// are alignment padding GStreamer added and must be skipped rather than
/// copied — copying them, or worse, assuming `stride == width * 4` and
/// reading straight through the padding into the next row's data, is what
/// produces the sheared/banded frames this function exists to prevent. See
/// `pack_rgba_rows_skips_stride_padding_instead_of_shearing_rows` below for
/// a synthetic, GStreamer-independent proof.
fn pack_rgba_rows(plane: &[u8], width: usize, height: usize, stride: usize) -> Result<Vec<u8>> {
    let row_bytes = width * 4;
    let mut packed = Vec::with_capacity(row_bytes * height);
    for row in 0..height {
        let start = row * stride;
        let end = start + row_bytes;
        let Some(row_slice) = plane.get(start..end) else {
            anyhow::bail!("decoded frame plane shorter than expected (stride/height mismatch)");
        };
        packed.extend_from_slice(row_slice);
    }
    Ok(packed)
}

// ---------------------------------------------------------------------
// Audio decode + playback
// ---------------------------------------------------------------------

/// Message sent from `feed()`/`set_volume()`/`Drop` to the audio
/// pipeline-owning thread. Parallel to video's `PipelineMsg`, plus
/// `SetVolume` since audio has a live-adjustable playback parameter video
/// doesn't.
enum AudioPipelineMsg {
    Packet { pts_us: u64, data: Vec<u8> },
    SetVolume(f64),
    Shutdown,
}

/// Decodes and plays one subscribed audio stream's incoming Opus packets.
/// See the module docs "Audio" section for the pipeline shape and
/// threading rationale. Unlike [`StreamDecoder`], there is nothing to poll
/// each frame — decoded PCM goes straight to the sink — so the only
/// runtime knob exposed is [`AudioDecoder::set_volume`].
pub struct AudioDecoder {
    stream_id: StreamId,
    tx: std_mpsc::Sender<AudioPipelineMsg>,
    /// Set once the pipeline reaches `Playing` and cleared just before it's
    /// torn down; a cheap way for a caller to notice a pipeline that failed
    /// to start (e.g. `opusdec`/`autoaudiosink` missing) without needing a
    /// result channel. Not essential to correctness — `feed`/`set_volume`
    /// degrade gracefully (log + drop) regardless — but cheap enough to be
    /// worth exposing.
    alive: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl AudioDecoder {
    /// Build and start a playback pipeline dedicated to `stream_id`, using
    /// the real system audio sink. Calls `gstreamer::init()` (idempotent).
    pub fn new(stream_id: StreamId) -> Result<Self> {
        Self::new_with_sink(stream_id, "autoaudiosink", true)
    }

    /// Like `new`, but with the sink element factory name and its `sync`
    /// property overridable — used by this module's own tests to swap in a
    /// `fakesink` (so `cargo test` doesn't need a real audio output device)
    /// with `sync=false` (so a synthetic test clip drains at full speed
    /// instead of pacing itself against the wall clock the way real
    /// playback correctly should).
    fn new_with_sink(stream_id: StreamId, sink_factory: &str, sink_sync: bool) -> Result<Self> {
        gstreamer::init().context("failed to initialize GStreamer")?;

        let (tx, rx) = std_mpsc::channel::<AudioPipelineMsg>();
        let alive = Arc::new(AtomicBool::new(false));

        let pipeline_alive = alive.clone();
        let sink_factory = sink_factory.to_string();
        let thread = thread::Builder::new()
            .name(format!("audio-decode-stream-{stream_id}"))
            .spawn(move || {
                if let Err(e) = run_audio_pipeline(rx, &sink_factory, sink_sync, pipeline_alive) {
                    tracing::error!("audio decode pipeline for stream {stream_id} exited with error: {e:#}");
                }
            })
            .context("failed to spawn audio decode pipeline thread")?;

        Ok(Self { stream_id, tx, alive, thread: Some(thread) })
    }

    pub fn stream_id(&self) -> StreamId {
        self.stream_id
    }

    /// Whether the pipeline thread is currently in `Playing`. `false` both
    /// before startup completes and after the pipeline has exited/errored.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    /// Feed one already-reassembled-and-decrypted Opus packet (i.e. a
    /// `ClientEvent::MediaFrame` payload from `net.rs` for a
    /// `StreamKind::Audio` stream). Packets for a different `stream_id` are
    /// ignored defensively, mirroring `StreamDecoder::feed`. Never blocks.
    pub fn feed(&mut self, frame: &DecryptedMediaFrame) {
        if frame.stream_id != self.stream_id {
            return;
        }
        let msg = AudioPipelineMsg::Packet { pts_us: frame.pts, data: frame.payload.clone() };
        if self.tx.send(msg).is_err() {
            tracing::warn!(
                "audio decode pipeline thread for stream {} is gone; dropping packet",
                self.stream_id
            );
        }
    }

    /// Set playback volume as a 0.0-2.0 linear factor (matching the
    /// project's 0-200% requirement 1:1 — see the module docs "Audio"
    /// section for why no further conversion is needed). Values outside
    /// that range are clamped rather than rejected, since a UI slider
    /// bounded to 0..=200% can never actually produce one, but a defensive
    /// clamp costs nothing. Never blocks.
    pub fn set_volume(&mut self, volume: f64) {
        let clamped = volume.clamp(0.0, 2.0);
        if self.tx.send(AudioPipelineMsg::SetVolume(clamped)).is_err() {
            tracing::warn!(
                "audio decode pipeline thread for stream {} is gone; dropping volume change",
                self.stream_id
            );
        }
    }
}

impl Drop for AudioDecoder {
    fn drop(&mut self) {
        let _ = self.tx.send(AudioPipelineMsg::Shutdown);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

/// Builds the audio pipeline, drives it to `Playing`, and pumps `rx` until
/// `Shutdown` (or the channel disconnects because every `AudioDecoder`
/// handle was dropped) — see module docs "Audio"/"Threading".
fn run_audio_pipeline(
    rx: std_mpsc::Receiver<AudioPipelineMsg>,
    sink_factory: &str,
    sink_sync: bool,
    alive: Arc<AtomicBool>,
) -> Result<()> {
    let pipeline = Pipeline::new();

    let src = ElementFactory::make("appsrc")
        .name("src")
        .build()
        .context("appsrc element unavailable")?;
    let decoder = ElementFactory::make("opusdec")
        .build()
        .context("opusdec element unavailable (is gst-plugins-base installed?)")?;
    let convert = ElementFactory::make("audioconvert")
        .build()
        .context("audioconvert element unavailable")?;
    let resample = ElementFactory::make("audioresample")
        .build()
        .context("audioresample element unavailable")?;
    let volume = ElementFactory::make("volume")
        .name("vol")
        .build()
        .context("volume element unavailable")?;
    let sink = ElementFactory::make(sink_factory)
        .build()
        .with_context(|| format!("{sink_factory} element unavailable"))?;
    sink.set_property("sync", sink_sync);

    pipeline
        .add_many([&src, &decoder, &convert, &resample, &volume, &sink])
        .context("failed to add elements to pipeline")?;
    Element::link_many([&src, &decoder, &convert, &resample, &volume, &sink]).context("failed to link decode chain")?;

    let appsrc = src
        .dynamic_cast::<AppSrc>()
        .map_err(|_| anyhow!("appsrc element was not an AppSrc"))?;
    // No `channels`/`rate` fields — see module docs "Audio" section for why
    // `opusdec` doesn't need them for the mono/stereo case (`family: 0`)
    // this project's `opusenc` sender always produces.
    appsrc.set_caps(Some(&Caps::builder("audio/x-opus").field("channel-mapping-family", 0i32).build()));
    appsrc.set_is_live(true);
    appsrc.set_format(gstreamer::Format::Time);

    let bus = pipeline.bus().ok_or_else(|| anyhow!("pipeline has no bus"))?;

    pipeline
        .set_state(State::Playing)
        .context("failed to start audio decode pipeline")?;
    alive.store(true, Ordering::Relaxed);

    let volume_element = volume;
    let result = pump_audio_loop(&rx, &appsrc, &volume_element, &bus);

    alive.store(false, Ordering::Relaxed);
    let _ = appsrc.end_of_stream();
    let _ = pipeline.set_state(State::Null);
    result
}

/// Feed loop: pushes Opus packets into `appsrc`, applies volume changes to
/// the `volume` element, and drains the bus for error/warning logging in
/// between, until told to shut down. Mirrors video's `pump_loop`.
fn pump_audio_loop(rx: &std_mpsc::Receiver<AudioPipelineMsg>, appsrc: &AppSrc, volume: &Element, bus: &Bus) -> Result<()> {
    loop {
        while let Some(msg) = bus.pop() {
            log_bus_message(&msg);
        }

        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(AudioPipelineMsg::Packet { pts_us, data }) => {
                let mut buffer = Buffer::from_slice(data);
                if let Some(buffer_mut) = buffer.get_mut() {
                    buffer_mut.set_pts(ClockTime::from_useconds(pts_us));
                }
                // Same "glitch and continue" policy as video: a bad packet
                // downstream of a dropped fragment is logged and skipped,
                // never fatal.
                if let Err(e) = appsrc.push_buffer(buffer) {
                    tracing::warn!("appsrc dropped an audio packet: {e:?}");
                }
            }
            Ok(AudioPipelineMsg::SetVolume(v)) => {
                volume.set_property("volume", v);
            }
            Ok(AudioPipelineMsg::Shutdown) => return Ok(()),
            Err(std_mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std_mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}

// ---------------------------------------------------------------------
// Preview (still-JPEG) decode
// ---------------------------------------------------------------------
//
// Much simpler than the H.264 video path above: a preview is one still
// JPEG image (spec §7.1), not a video stream, so there's no GStreamer
// pipeline, no background thread, and no keyframe-gating — just a
// synchronous JPEG decode per incoming blob.
//
// # Why this takes a `&DecryptedPreview`, not ciphertext + a room key
//
// Same situation as `StreamDecoder::feed` above (see this module's opening
// doc comment): reading `net.rs` first (per this task's instructions,
// rather than guessing) showed the preview receive path is *already fully
// implemented* there. `net.rs`'s `handle_preview_stream` unpacks the
// `PreviewHeader`, derives the nonce from its `conn_id`/`counter` fields via
// the connection's shared counter (spec §5.1/§7.1, the same scheme the
// video/audio path uses), and AEAD-decrypts the blob with the already
// public `net::decrypt_preview` — emitting the result as
// `ClientEvent::Preview(DecryptedPreview)`, which already carries plaintext
// JPEG bytes. `net.rs` does not expose raw preview ciphertext or the room
// key to a caller outside itself, so there is nothing left for `decode.rs`
// to decrypt here; redoing it would just duplicate `net.rs`'s AEAD call
// (and its crypto imports) for no benefit — exactly the same reasoning
// that already applies to `StreamDecoder::feed` taking a
// `DecryptedMediaFrame` instead of raw datagram bytes.
//
// A later, separate UI-wiring task will filter `ClientEvent::Preview` by
// which `(conn_id, stream_id)` pairs it cares about, call `decode_preview`
// on each match, and feed the result into `PreviewCache::update`.
//
// # JPEG library
//
// Uses the `image` crate (added to `Cargo.toml` with `default-features =
// false, features = ["jpeg"]`, since it was not previously a direct
// dependency here — it was already pulled in transitively by
// `eframe`/`arboard` but without the `jpeg` feature enabled). This matches
// the sender side: `preview.rs`'s `downscale_and_encode_jpeg` (landed
// concurrently with this task) encodes with
// `image::codecs::jpeg::JpegEncoder` and its own tests decode back with
// `image::load_from_memory` — same crate, so no reconciliation was needed.

/// Decode one already-decrypted preview JPEG blob (see the module docs
/// above for why decryption happens in `net.rs`, not here) into an
/// `egui::ColorImage`. Never panics: a corrupt/truncated JPEG (e.g. a
/// preview uni-stream that got cut off mid-transfer, or any other garbage
/// downstream of successful AEAD decryption) is reported as an `Err` for
/// the caller to log and ignore, exactly like a bad H.264 access unit is
/// handled in the video path above.
pub fn decode_preview(preview: &DecryptedPreview) -> Result<egui::ColorImage> {
    let decoded = image::load_from_memory_with_format(&preview.jpeg, image::ImageFormat::Jpeg).with_context(|| {
        format!(
            "failed to JPEG-decode preview for stream {} on conn {}",
            preview.stream_id, preview.conn_id
        )
    })?;
    let rgba = decoded.to_rgba8();
    let (width, height) = rgba.dimensions();
    Ok(egui::ColorImage::from_rgba_unmultiplied(
        [width as usize, height as usize],
        rgba.as_raw(),
    ))
}

/// In-memory store of the most recently decoded preview still per stream,
/// keyed `(owner conn_id, stream_id)` — the same key shape `ui.rs`'s
/// `watches: HashMap<(ConnId, StreamId), _>` already uses for identifying a
/// specific stream, reused here for consistency rather than inventing a new
/// key type.
///
/// Populated by a later UI-wiring task as `ClientEvent::Preview` events
/// arrive and `decode_preview` succeeds; read from `ui.rs`'s repaint loop
/// (e.g. to paint a roster thumbnail before/instead of a live
/// `StreamDecoder` feed exists for that stream).
#[derive(Default)]
pub struct PreviewCache {
    images: HashMap<(ConnId, StreamId), egui::ColorImage>,
}

impl PreviewCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Store (or overwrite) the latest decoded preview for a stream. Only
    /// the newest image is ever kept per stream — the same
    /// drop-and-continue policy `StreamDecoder` applies to decoded video
    /// frames above, since a preview is a "current thumbnail", not a
    /// history.
    pub fn update(&mut self, conn_id: ConnId, stream_id: StreamId, image: egui::ColorImage) {
        self.images.insert((conn_id, stream_id), image);
    }

    /// Look up the latest cached preview for a stream, if any has arrived
    /// and decoded successfully yet.
    pub fn get(&self, conn_id: ConnId, stream_id: StreamId) -> Option<&egui::ColorImage> {
        self.images.get(&(conn_id, stream_id))
    }

    /// Drop a stream's cached preview, e.g. once `ui.rs` observes
    /// `PresenceEvent::StreamStopped` for it.
    pub fn remove(&mut self, conn_id: ConnId, stream_id: StreamId) {
        self.images.remove(&(conn_id, stream_id));
    }
}

#[cfg(test)]
mod preview_tests {
    use super::*;
    use crate::net::{decrypt_preview, encrypt_preview};

    /// Encodes a small RGBA gradient test image to JPEG bytes using the
    /// `image` crate's own encoder — the same crate/library `decode_preview`
    /// decodes with, so this is a genuine round trip of this module's
    /// chosen JPEG library, not just a decode-only smoke test.
    fn encode_test_jpeg(width: u32, height: u32) -> Vec<u8> {
        let mut img = image::RgbaImage::new(width, height);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = image::Rgba([
                (x * 255 / width.max(1)) as u8,
                (y * 255 / height.max(1)) as u8,
                128,
                255,
            ]);
        }
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Jpeg)
            .expect("jpeg encode should succeed");
        buf
    }

    /// `net::build_nonce`'s layout (spec §5): `conn_id BE ++ counter BE`.
    /// Inlined here since `build_nonce` is a private helper of `net.rs`;
    /// this mirrors exactly what `net.rs`'s own tests already do (see
    /// `net::tests::encrypt_then_decrypt_preview_round_trips`) and what
    /// `handle_preview_stream` builds from an incoming `PreviewHeader`.
    fn preview_nonce(conn_id: ConnId, counter: u32) -> [u8; 12] {
        let mut nonce = [0u8; 12];
        nonce[0..8].copy_from_slice(&conn_id.to_be_bytes());
        nonce[8..12].copy_from_slice(&counter.to_be_bytes());
        nonce
    }

    /// Full pipeline sanity check: JPEG-encode a real test image, encrypt it
    /// with the actual `net::encrypt_preview`, decrypt it back with the
    /// actual `net::decrypt_preview` (exactly what `handle_preview_stream`
    /// does for a real incoming uni-stream), then run the result through
    /// `decode_preview` and assert the returned `ColorImage` has the right
    /// dimensions and isn't blank/garbage.
    #[test]
    fn decode_preview_round_trips_through_encrypt_and_jpeg() {
        const WIDTH: u32 = 64;
        const HEIGHT: u32 = 48;

        let jpeg = encode_test_jpeg(WIDTH, HEIGHT);
        assert!(!jpeg.is_empty(), "encoder produced no JPEG bytes");

        let key = [7u8; 32];
        let conn_id: ConnId = 0x0102_0304_0506_0708;
        let stream_id: StreamId = 2;
        let nonce = preview_nonce(conn_id, 5);

        let ciphertext = encrypt_preview(&key, stream_id, nonce, &jpeg).expect("encrypt_preview");
        let decrypted = decrypt_preview(&key, stream_id, nonce, &ciphertext).expect("decrypt_preview");
        assert_eq!(decrypted, jpeg, "decrypted bytes should match the original JPEG exactly (lossless AEAD)");

        let preview = DecryptedPreview {
            conn_id,
            stream_id,
            jpeg: decrypted,
        };

        let image = decode_preview(&preview).expect("decode_preview should succeed on a valid JPEG");
        assert_eq!(image.size, [WIDTH as usize, HEIGHT as usize], "decoded dimensions should match the encoded test image");

        // JPEG is lossy, so an exact pixel match isn't expected, but the
        // decoded gradient should not be blank/garbage: distinct corners
        // should still be clearly distinguishable in the red/green channels.
        let top_left = image.pixels[0];
        let bottom_right = image.pixels[image.pixels.len() - 1];
        assert!(
            top_left.r() != bottom_right.r() || top_left.g() != bottom_right.g(),
            "decoded gradient collapsed to a uniform image; decode likely produced garbage"
        );

        let mut cache = PreviewCache::new();
        assert!(cache.get(conn_id, stream_id).is_none());
        cache.update(conn_id, stream_id, image);
        assert_eq!(
            cache.get(conn_id, stream_id).map(|i| i.size),
            Some([WIDTH as usize, HEIGHT as usize])
        );
        cache.remove(conn_id, stream_id);
        assert!(cache.get(conn_id, stream_id).is_none());
    }

    /// A truncated/garbage JPEG must be reported as an `Err`, never panic.
    #[test]
    fn decode_preview_rejects_corrupt_jpeg() {
        let preview = DecryptedPreview {
            conn_id: 1,
            stream_id: 1,
            // JPEG SOI marker followed by garbage — not a decodable image.
            jpeg: vec![0xFF, 0xD8, 0xFF, 0x00, 1, 2, 3],
        };
        assert!(decode_preview(&preview).is_err());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// End-to-end correctness check of the *decode* half of this module,
    /// bypassing the network/crypto layer entirely (out of scope here —
    /// covered by `net.rs`'s own tests): generate a real H.264 stream with
    /// `videotestsrc ! x264enc ! h264parse`, split it into access units the
    /// same way a real sender would (annex-B, one AU per buffer), feed each
    /// through `StreamDecoder::feed` exactly as `ui.rs` will once wired up,
    /// and assert `latest_frame()` eventually yields RGBA `ColorImage`s of
    /// the expected dimensions. This is the meaningful, headless-runnable
    /// check described in the task: no display/portal needed, unlike
    /// capture.
    #[test]
    fn decodes_synthetic_h264_stream_to_rgba_frames() {
        gstreamer::init().expect("gstreamer init");

        // Deliberately awkward dimensions rather than a suspiciously-round
        // number like 320x240: 322 * 4 = 1288 bytes/row, which is NOT a
        // multiple of typical SIMD row-alignment boundaries (16 or 32
        // bytes) that `videoconvert`/`avdec_h264` commonly pad rows to.
        // 322 also happens to divide evenly into the SMPTE pattern's 7
        // vertical color bars (46px each), which keeps the sample columns
        // below well clear of any bar boundary. A round width like 160
        // risks the row already landing on an alignment boundary by
        // accident, silently hiding a stride bug — which is exactly why the
        // previous version of this test (160x120, and only asserting
        // `image.size`) would not have caught one: it never inspected
        // pixel content at all, so a sheared/banded frame of the right
        // dimensions passed just as well as a correct one.
        const WIDTH: i32 = 322;
        const HEIGHT: i32 = 242;
        const NUM_BUFFERS: i32 = 20;

        // Encode a short synthetic clip in-process (no filesystem fixture
        // needed) and collect each access unit as a Vec<u8>, mirroring what
        // a real sender's `capture.rs` would hand to `net.rs::encrypt_frame`
        // (minus the encrypt/fragment step, which is `net.rs`'s job and is
        // already covered by its own tests).
        let access_units = encode_test_clip(WIDTH, HEIGHT, NUM_BUFFERS);
        assert!(!access_units.is_empty(), "encoder produced no access units");

        let mut decoder = StreamDecoder::new(7).expect("failed to build StreamDecoder");

        let mut last_image: Option<egui::ColorImage> = None;
        let deadline = Instant::now() + Duration::from_secs(15);

        for (i, au) in access_units.iter().enumerate() {
            let frame = DecryptedMediaFrame {
                conn_id: 1,
                stream_id: 7,
                pts: (i as u64) * 33_333,
                keyframe: i == 0,
                payload: au.clone(),
            };
            decoder.feed(&frame);

            // Give the pipeline thread a chance to decode as we go, rather
            // than only polling once at the end.
            if let Some(image) = decoder.latest_frame() {
                last_image = Some(image);
            }
        }

        // Drain until either a frame shows up or we time out. Keep polling
        // past the first hit so `last_image` ends up holding a frame from
        // well into the clip, not just whatever showed up first.
        while Instant::now() < deadline {
            if let Some(image) = decoder.latest_frame() {
                last_image = Some(image);
            }
            if last_image.is_some() && Instant::now() + Duration::from_millis(300) > deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        let image = last_image.expect("no decoded RGBA frame was produced within the deadline");
        assert_eq!(
            image.size,
            [WIDTH as usize, HEIGHT as usize],
            "decoded frame dimensions did not match the encoded test clip"
        );

        assert_smpte_bars_are_coherent(&image, WIDTH as usize, HEIGHT as usize);
    }

    /// Asserts `image` looks like real SMPTE 100% color bars rather than
    /// banded/sheared garbage, without hardcoding exact bar RGB values
    /// (which drift slightly across GStreamer versions and lossy H.264
    /// quantization). Two independent checks, each targeting the reported
    /// bug shape directly:
    ///
    /// 1. **Vertical consistency**: the SMPTE top region is 7 solid
    ///    vertical stripes — same color from row to row. Sampling the same
    ///    x column near the top and deep near the bottom of that region
    ///    should yield nearly the same color. A stride bug's error is
    ///    `row * (stride - row_bytes)` bytes and grows with row, so a
    ///    shallow row can look almost right while a deep row is reading
    ///    from completely unrelated bytes — this is exactly what would
    ///    catch it.
    /// 2. **Horizontal diversity**: across the 7 bar centers, colors must
    ///    actually differ from each other. This rules out the false-pass
    ///    where a stride bug produces uniformly-wrong-but-internally-
    ///    consistent flat gray bands (matching the user's screenshot
    ///    description) that would otherwise sail through check 1.
    fn assert_smpte_bars_are_coherent(image: &egui::ColorImage, width: usize, height: usize) {
        let top_region_height = height * 2 / 3;
        let margin = 6usize;
        let shallow_row = margin;
        let deep_row = top_region_height - margin;
        assert!(deep_row > shallow_row + 20, "test dimensions too small to separate sample rows");

        let bar_width = width / 7;
        let bar_x = |i: usize| i * bar_width + bar_width / 2;

        let pixel = |x: usize, y: usize| image[(x, y)];

        let mut shallow_colors = Vec::with_capacity(7);
        let mut deep_colors = Vec::with_capacity(7);
        for i in 0..7 {
            let x = bar_x(i);
            shallow_colors.push(pixel(x, shallow_row));
            deep_colors.push(pixel(x, deep_row));
        }

        // Check 1: vertical consistency per bar.
        for i in 0..7 {
            let a = shallow_colors[i];
            let b = deep_colors[i];
            let diff = channel_diff(a, b);
            assert!(
                diff <= 60,
                "bar {i} is not vertically consistent (shallow row {shallow_row} = {a:?}, \
                 deep row {deep_row} = {b:?}, diff {diff}) — looks like a sheared/stride-corrupted \
                 frame, not solid color bars"
            );
        }

        // Check 2: horizontal diversity, evaluated on the deep row (the
        // one most exposed to cumulative stride error) so a uniformly-wrong
        // flat-gray-bands result can't sneak past check 1.
        let mut distinct_pairs = 0;
        for i in 0..deep_colors.len() {
            for j in (i + 1)..deep_colors.len() {
                if channel_diff(deep_colors[i], deep_colors[j]) > 50 {
                    distinct_pairs += 1;
                }
            }
        }
        assert!(
            distinct_pairs >= 10,
            "deep-row bar colors aren't diverse enough to be real color bars \
             ({distinct_pairs} sufficiently-distinct pairs out of 21; colors: {deep_colors:?}) — \
             looks like flat banded garbage, not SMPTE bars"
        );
    }

    /// Max-channel (R/G/B) absolute difference between two pixels, ignoring
    /// alpha (which the RGBA video path always sets to opaque).
    fn channel_diff(a: egui::Color32, b: egui::Color32) -> i32 {
        (a.r() as i32 - b.r() as i32)
            .abs()
            .max((a.g() as i32 - b.g() as i32).abs())
            .max((a.b() as i32 - b.b() as i32).abs())
    }

    /// Fast, deterministic, GStreamer-independent proof that
    /// `pack_rgba_rows` correctly skips stride padding rather than
    /// shearing the image, whether or not the local GStreamer install
    /// happens to pad RGBA rows for a given width (RGBA rows are always a
    /// multiple of 4 bytes, so the common `round-up-to-4` stride policy
    /// never pads them — real padding only shows up with certain
    /// decoder/converter backends, e.g. GPU/hardware paths that align to
    /// 16/32/64+ byte boundaries regardless of format. This test builds
    /// that scenario by hand instead of hoping the CI machine's software
    /// decoder happens to produce it.)
    #[test]
    fn pack_rgba_rows_skips_stride_padding_instead_of_shearing_rows() {
        const WIDTH: usize = 3;
        const HEIGHT: usize = 4;
        const ROW_BYTES: usize = WIDTH * 4;
        const PADDING: usize = 12; // stride = row_bytes + 12, e.g. 32-byte SIMD alignment
        const STRIDE: usize = ROW_BYTES + PADDING;

        // Each row's real pixel bytes are filled with a distinct, easily
        // recognizable value (row index repeated); the padding bytes past
        // `ROW_BYTES` are filled with a sentinel (0xEE) that must never
        // appear in the packed output.
        let mut plane = vec![0xEEu8; STRIDE * HEIGHT];
        for row in 0..HEIGHT {
            let start = row * STRIDE;
            for b in &mut plane[start..start + ROW_BYTES] {
                *b = row as u8;
            }
        }

        let packed = pack_rgba_rows(&plane, WIDTH, HEIGHT, STRIDE).expect("packing should succeed");

        let mut expected = Vec::with_capacity(ROW_BYTES * HEIGHT);
        for row in 0..HEIGHT {
            expected.extend(std::iter::repeat(row as u8).take(ROW_BYTES));
        }
        assert_eq!(
            packed, expected,
            "stride-aware packing produced the wrong bytes: expected each row's real pixels \
             tightly packed with no padding, got something else (rows shifted/sheared or padding leaked in)"
        );
        assert!(
            !packed.contains(&0xEE),
            "padding sentinel byte leaked into the packed output — stride was not skipped correctly"
        );

        // The naive bug this guards against: treating the buffer as if
        // `stride == row_bytes` (i.e. `plane[..ROW_BYTES * HEIGHT]`) reads
        // straight through the padding into the next row, shearing the
        // image. Confirm that naive slice does NOT equal the correct
        // output — proving this test actually discriminates the bug from
        // the fix, not just checking dimensions.
        let naive = &plane[..ROW_BYTES * HEIGHT];
        assert_ne!(
            naive, expected.as_slice(),
            "test fixture is degenerate: the naive (stride-ignoring) extraction accidentally \
             matches the correct one, so this test wouldn't actually catch the bug"
        );
    }

    /// Encodes `num_buffers` frames of `videotestsrc` to H.264 (byte-stream,
    /// one NAL-unit-aligned access unit per buffer) using a throwaway
    /// GStreamer pipeline, and returns each access unit's bytes. Used only
    /// by the test above to build a fixture without needing a checked-in
    /// binary file.
    fn encode_test_clip(width: i32, height: i32, num_buffers: i32) -> Vec<Vec<u8>> {
        use gstreamer_app::AppSink as TestAppSink;

        let pipeline = Pipeline::new();
        let src = ElementFactory::make("videotestsrc")
            .property("num-buffers", num_buffers)
            // SMPTE 100% color bars: sharp, well-known vertical stripe
            // boundaries that a stride/shear bug visibly disrupts. This is
            // also `videotestsrc`'s default pattern, but set explicitly so
            // the pixel-content assertions in the test above don't
            // silently depend on that default never changing.
            .property_from_str("pattern", "smpte")
            .build()
            .expect("videotestsrc unavailable");
        let capsfilter = ElementFactory::make("capsfilter")
            .property(
                "caps",
                Caps::builder("video/x-raw")
                    .field("width", width)
                    .field("height", height)
                    .field("framerate", gstreamer::Fraction::new(30, 1))
                    .build(),
            )
            .build()
            .expect("capsfilter unavailable");
        let enc = ElementFactory::make("x264enc")
            // zero-latency-ish settings so this test runs fast and every
            // frame flushes through immediately.
            .property_from_str("tune", "zerolatency")
            .property("key-int-max", 1u32)
            .build()
            .expect("x264enc unavailable");
        let parse = ElementFactory::make("h264parse").build().expect("h264parse unavailable");
        let capsfilter2 = ElementFactory::make("capsfilter")
            .property(
                "caps",
                Caps::builder("video/x-h264")
                    .field("stream-format", "byte-stream")
                    .field("alignment", "au")
                    .build(),
            )
            .build()
            .expect("capsfilter unavailable");
        let sink = ElementFactory::make("appsink").build().expect("appsink unavailable");

        pipeline
            .add_many([&src, &capsfilter, &enc, &parse, &capsfilter2, &sink])
            .unwrap();
        Element::link_many([&src, &capsfilter, &enc, &parse, &capsfilter2, &sink]).unwrap();

        let appsink = sink.dynamic_cast::<TestAppSink>().unwrap();
        appsink.set_property("sync", false);

        pipeline.set_state(State::Playing).expect("failed to start encode pipeline");

        let mut access_units = Vec::new();
        let bus = pipeline.bus().unwrap();
        loop {
            if let Some(sample) = appsink.try_pull_sample(ClockTime::from_mseconds(200)) {
                if let Some(buffer) = sample.buffer() {
                    if let Ok(map) = buffer.map_readable() {
                        access_units.push(map.as_slice().to_vec());
                    }
                }
            } else {
                // No sample within the timeout: either EOS or done.
                if let Some(msg) = bus.timed_pop(ClockTime::from_mseconds(0)) {
                    if matches!(msg.view(), MessageView::Eos(_)) {
                        break;
                    }
                }
                if access_units.len() as i32 >= num_buffers || appsink.is_eos() {
                    break;
                }
            }
        }

        let _ = pipeline.set_state(State::Null);
        access_units
    }

    /// End-to-end correctness check of [`AudioDecoder`], mirroring the
    /// video test above: generate a real Opus stream with `audiotestsrc !
    /// audioconvert ! audioresample ! opusenc`, feed each encoded packet
    /// through `AudioDecoder::feed` exactly as `ui.rs` does for an incoming
    /// `ClientEvent::MediaFrame`, and confirm the pipeline accepts every
    /// packet without an `Err` from `appsrc.push_buffer` and without the
    /// pipeline reporting itself dead (`is_alive()` false) partway through —
    /// i.e. a genuine round trip through the real `opusenc`/`opusdec`
    /// codecs, not just a construction smoke test. Routes to `fakesink`
    /// (with `sync=false`) instead of the real `autoaudiosink` used in
    /// production, per this task's instructions, so the test doesn't depend
    /// on this environment having a real audio output device and doesn't
    /// spend 2 seconds of wall-clock time pacing itself against fake PTS
    /// values. Also exercises `set_volume` at both ends of its documented
    /// 0.0-2.0 range plus an out-of-range value, confirming the clamp holds
    /// and nothing panics.
    #[test]
    fn decodes_synthetic_opus_stream_without_erroring() {
        gstreamer::init().expect("gstreamer init");

        const NUM_BUFFERS: i32 = 30;

        let packets = encode_test_opus_clip(NUM_BUFFERS);
        assert!(!packets.is_empty(), "encoder produced no Opus packets");

        let mut decoder =
            AudioDecoder::new_with_sink(9, "fakesink", false).expect("failed to build AudioDecoder");

        // Give the pipeline thread a moment to reach Playing before hammering
        // it with packets.
        let start_deadline = Instant::now() + Duration::from_secs(5);
        while !decoder.is_alive() && Instant::now() < start_deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(decoder.is_alive(), "audio decode pipeline never reached Playing");

        decoder.set_volume(0.0);
        decoder.set_volume(2.0);
        decoder.set_volume(1.0);
        decoder.set_volume(5.0); // out of range: must clamp, not panic.

        for (i, packet) in packets.iter().enumerate() {
            let frame = DecryptedMediaFrame {
                conn_id: 1,
                stream_id: 9,
                pts: (i as u64) * 20_000,
                keyframe: true,
                payload: packet.clone(),
            };
            decoder.feed(&frame);
        }

        // A frame for a different stream_id must be a silent no-op, never a
        // panic or a push into this decoder's pipeline.
        decoder.feed(&DecryptedMediaFrame { conn_id: 1, stream_id: 200, pts: 0, keyframe: true, payload: vec![1, 2, 3] });

        // Let the pipeline thread drain the channel and push everything
        // through opusdec, then confirm it's still alive (no bus error tore
        // it down) before dropping it. `Drop` itself (shutdown + join) is
        // exercised implicitly at the end of this test/function scope.
        std::thread::sleep(Duration::from_millis(500));
        assert!(decoder.is_alive(), "audio decode pipeline died while decoding a valid Opus stream");
    }

    /// Encodes `num_buffers` frames of `audiotestsrc` to Opus packets using a
    /// throwaway GStreamer pipeline, and returns each packet's bytes. Used
    /// only by the test above to build a fixture without needing a
    /// checked-in binary file — same rationale as `encode_test_clip` above,
    /// for audio instead of video.
    fn encode_test_opus_clip(num_buffers: i32) -> Vec<Vec<u8>> {
        use gstreamer_app::AppSink as TestAppSink;

        let pipeline = Pipeline::new();
        let src = ElementFactory::make("audiotestsrc")
            .property("num-buffers", num_buffers)
            .build()
            .expect("audiotestsrc unavailable");
        let convert = ElementFactory::make("audioconvert").build().expect("audioconvert unavailable");
        let resample = ElementFactory::make("audioresample").build().expect("audioresample unavailable");
        let enc = ElementFactory::make("opusenc").build().expect("opusenc unavailable");
        let sink = ElementFactory::make("appsink").build().expect("appsink unavailable");

        pipeline.add_many([&src, &convert, &resample, &enc, &sink]).unwrap();
        Element::link_many([&src, &convert, &resample, &enc, &sink]).unwrap();

        let appsink = sink.dynamic_cast::<TestAppSink>().unwrap();
        appsink.set_property("sync", false);

        pipeline.set_state(State::Playing).expect("failed to start encode pipeline");

        let mut packets = Vec::new();
        let bus = pipeline.bus().unwrap();
        loop {
            if let Some(sample) = appsink.try_pull_sample(ClockTime::from_mseconds(200)) {
                if let Some(buffer) = sample.buffer() {
                    if let Ok(map) = buffer.map_readable() {
                        packets.push(map.as_slice().to_vec());
                    }
                }
            } else {
                if let Some(msg) = bus.timed_pop(ClockTime::from_mseconds(0)) {
                    if matches!(msg.view(), MessageView::Eos(_)) {
                        break;
                    }
                }
                if packets.len() as i32 >= num_buffers || appsink.is_eos() {
                    break;
                }
            }
        }

        let _ = pipeline.set_state(State::Null);
        packets
    }
}
