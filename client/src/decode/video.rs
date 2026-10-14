//! The video decode pipeline thread: codec detection, VA-first pipeline
//! construction with software fallback, and the feed loop.
//!
//! See `decode.rs`'s module docs for the big picture; this file is the
//! mechanism.
//!
//! # Pipelines
//!
//! ```text
//! HW:  appsrc ! <h264|h265|av1>parse ! va<h264|h265|av1>dec ! video/x-raw,format=NV12 ! appsink
//! SW:  appsrc ! <h264|h265|av1>parse ! <avdec_h264|avdec_h265|av1dec> ! videoconvert ! video/x-raw,format=NV12 ! appsink
//! ```
//!
//! `Why the caps filter says plain `video/x-raw,format=NV12`:` the VA
//! decoders' src pads offer `video/x-raw(memory:VAMemory)`, `(memory:DMABuf)`
//! and plain system-memory `video/x-raw`. A filter without a memory feature
//! only matches the last one, so negotiation lands on system-memory NV12 (the
//! GPU-uploadable layout the renderer wants) and the VA decoder does the
//! surface download itself. Whether the VA element is really the one running
//! is observable: [`DecoderInfo`] reports the active factory name, and the
//! decoder logs it at `info` on every (re)build.
//!
//! # Hardware first, software on failure
//!
//! For each codec the VA decoder is tried first. It is demoted to software
//! (and remembered, so a later switch back to the same codec does not retry)
//! when
//!
//! * the element can't be created or linked (no VA device, no VA support for
//!   the codec/profile), or the pipeline refuses to start;
//! * the pipeline posts a bus `Error` while the HW decoder is active; or
//! * (probation) it produced no frame within [`HW_PROBATION`] of being fed its
//!   first keyframe.
//!
//! During probation every access unit pushed is also kept so the software
//! pipeline can be replayed from the keyframe on and the picture appears
//! immediately. A failure *after* probation can't be replayed (frames since
//! the failure are gone), so the software pipeline starts at the next
//! keyframe instead.
//!
//! # Codec detection and switching
//!
//! No codec is assumed. Access units are dropped until a *keyframe-flagged*
//! one sniffs as a known codec ([`codec::sniff`]); then the pipeline for that
//! codec is built. On any later keyframe the sniff is repeated, and if the
//! codec changed (the sender edited a destination to a different codec) the
//! pipeline is torn down and rebuilt for the new one. Delta frames are never
//! sniffed — their codec can't be told reliably and costs time at 240 fps.

use std::collections::{HashSet, VecDeque};
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use gstreamer::prelude::*;
use gstreamer::{Bus, Buffer, BufferFlags, Caps, ClockTime, Element, ElementFactory, MessageView, Pipeline, State};
use gstreamer_app::{AppSink, AppSinkCallbacks, AppSrc};
use gstreamer_video::{VideoFrame, VideoInfo};

use super::codec::{self, VideoCodec};
use super::frame::{DecodedFrame, DecoderInfo, Shared};

/// Message sent from `feed()`/`Drop` to the pipeline-owning thread.
pub(super) enum PipelineMsg {
    AccessUnit { pts_us: u64, keyframe: bool, data: Vec<u8> },
    Shutdown,
}

/// How many access units may sit in `appsrc`'s own queue (not yet pushed into
/// the parser) before the pipeline thread stops pushing and holds further ones
/// in its own backlog.
///
/// `Why the backlog is ours and not a leaky `queue`/`appsrc`:` the old design
/// (a leaky-downstream `queue` in front of the parser) fixed the "big FPS jump
/// freezes the viewer" bug (a *blocking* queue made `push_buffer` stall and
/// replay seconds of stale input) by dropping the *oldest* queued access unit
/// when full. But it can't tell a keyframe from a delta frame, and the oldest
/// entry is very often the keyframe that a freshly built pipeline is about to
/// start from: while a VA decoder initialises (tens of ms, i.e. ~20 access
/// units at 240 fps) the keyframe was the first thing the leak threw away,
/// leaving the stream undecodable until the next GOP (measured: 15 of 30
/// frames decoded in the unit tests, 0 of 30 for HEVC). Holding the backlog in
/// our own deque lets [`trim_backlog`] drop *intelligently* (jump to the newest
/// keyframe) and keeps `appsrc` shallow so nothing is hidden inside it.
///
/// 3 = one buffer being decoded + two ready, enough to keep a 500 fps decoder
/// fed without adding noticeable latency.
const APPSRC_MAX_QUEUED: u64 = 3;

/// Upper bound on access units held in the pipeline thread's backlog (about
/// 270 ms at 240 fps). Beyond it [`trim_backlog`] drops. Together with the
/// ingest channel (`VIDEO_INGEST_CHANNEL_CAPACITY`) this bounds how stale the
/// decoder can be.
const BACKLOG_MAX_AUS: usize = 64;

type Au = (u64, bool, Vec<u8>);

/// Keep a backlog under `cap`: first jump forward to the newest keyframe in it
/// (a keyframe resets decoder state, so everything older is irrelevant and the
/// picture stays clean); if that is not enough, drop the oldest access units,
/// sparing a keyframe at the very front. Returns how many were dropped.
///
/// A burst after pipeline start-up (keyframe at the front, 20-30 deltas
/// behind it) is *not* touched while under `cap`.
fn trim_backlog(backlog: &mut VecDeque<Au>, cap: usize) -> usize {
    if backlog.len() <= cap {
        return 0;
    }
    let before = backlog.len();
    if let Some(k) = backlog.iter().rposition(|(_, keyframe, _)| *keyframe) {
        backlog.drain(..k);
    }
    if backlog.len() > cap {
        let excess = backlog.len() - cap;
        let start = usize::from(backlog.front().is_some_and(|(_, keyframe, _)| *keyframe));
        backlog.drain(start..start + excess);
    }
    before - backlog.len()
}

/// How long a hardware decoder may take to emit its first frame, measured from
/// the first keyframe being pushed, before it is declared broken and replaced
/// by software. VA decoders emit within tens of milliseconds when they work.
const HW_PROBATION: Duration = Duration::from_millis(2000);

/// Upper bound on access units kept for replay during probation (2 s at
/// 240 fps is 480; round up).
const REPLAY_MAX_AUS: usize = 600;

/// Which implementation runs a codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Backend {
    Hardware,
    Software,
}

/// Element factory names for one codec, per backend.
struct Chain {
    appsrc_caps: Caps,
    parse: &'static str,
    hw_decoder: &'static str,
    sw_decoder: &'static str,
}

fn chain(codec: VideoCodec) -> Chain {
    let annex_b = |media: &str| {
        Caps::builder(media)
            .field("stream-format", "byte-stream")
            .field("alignment", "au")
            .build()
    };
    match codec {
        VideoCodec::H264 => Chain {
            appsrc_caps: annex_b("video/x-h264"),
            parse: "h264parse",
            hw_decoder: "vah264dec",
            sw_decoder: "avdec_h264",
        },
        VideoCodec::H265 => Chain {
            appsrc_caps: annex_b("video/x-h265"),
            parse: "h265parse",
            hw_decoder: "vah265dec",
            sw_decoder: "avdec_h265",
        },
        VideoCodec::Av1 => Chain {
            appsrc_caps: Caps::builder("video/x-av1")
                .field("stream-format", "obu-stream")
                .field("alignment", "tu")
                .build(),
            parse: "av1parse",
            hw_decoder: "vaav1dec",
            // libaom, wrapped by gst-plugins-bad's `av1`. (No dav1d on this
            // machine; if `dav1ddec` is ever present it is the faster choice.)
            sw_decoder: "av1dec",
        },
    }
}

/// A built, running pipeline for one codec on one backend.
struct Active {
    pipeline: Pipeline,
    appsrc: AppSrc,
    bus: Bus,
    codec: VideoCodec,
    backend: Backend,
    decoder_name: &'static str,
    built_at: Instant,
    /// `Shared::decoded_total` when this pipeline started, to tell whether
    /// *it* has produced anything yet.
    decoded_at_build: u64,
    /// Access units pushed since the first keyframe while the HW decoder is
    /// still on probation; `None` once it has proven itself (or for software).
    replay: Option<Vec<(u64, bool, Vec<u8>)>>,
}

impl Active {
    /// Access units sitting in `appsrc`'s queue, not yet pushed downstream.
    fn queued(&self) -> u64 {
        // The typed getter is behind gstreamer-app's `v1_20` feature; the
        // property itself exists on every GStreamer >= 1.20.
        if self.appsrc.has_property("current-level-buffers") {
            self.appsrc.property::<u64>("current-level-buffers")
        } else {
            0
        }
    }

    fn push(&self, pts_us: u64, keyframe: bool, data: Vec<u8>) {
        let mut buffer = Buffer::from_slice(data);
        if let Some(b) = buffer.get_mut() {
            b.set_pts(ClockTime::from_useconds(pts_us));
            if !keyframe {
                b.set_flags(BufferFlags::DELTA_UNIT);
            }
        }
        // A push failure (flushing during teardown, a decoder choking on a
        // corrupt access unit) is the "glitch and continue" case.
        if let Err(e) = self.appsrc.push_buffer(buffer) {
            tracing::debug!("appsrc rejected an access unit: {e:?}");
        }
    }
}

impl Drop for Active {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(State::Null);
    }
}

/// Build and start the pipeline for `codec` on `backend`. Frames leaving it
/// are published into `shared`.
fn build(codec: VideoCodec, backend: Backend, shared: &Arc<Shared>) -> Result<Active> {
    let spec = chain(codec);
    let decoder_name = match backend {
        Backend::Hardware => spec.hw_decoder,
        Backend::Software => spec.sw_decoder,
    };

    let make = |factory: &str| -> Result<Element> {
        ElementFactory::make(factory)
            .build()
            .with_context(|| format!("GStreamer element `{factory}` is unavailable"))
    };

    let pipeline = Pipeline::new();
    let appsrc = make("appsrc")?
        .dynamic_cast::<AppSrc>()
        .map_err(|_| anyhow!("appsrc element was not an AppSrc"))?;
    appsrc.set_caps(Some(&spec.appsrc_caps));
    appsrc.set_is_live(true);
    appsrc.set_format(gstreamer::Format::Time);
    // Unbounded and non-blocking: the pipeline thread keeps this queue shallow
    // itself (see APPSRC_MAX_QUEUED), and the default 200 kB `max-bytes` would
    // be smaller than one 1080p keyframe.
    appsrc.set_property("max-bytes", 0u64);

    let parse = make(spec.parse)?;
    let decoder = make(decoder_name)?;
    // Corrupt input (a dropped fragment upstream) must degrade, not abort.
    if decoder.has_property("max-errors") {
        decoder.set_property("max-errors", -1i32);
    }

    let nv12 = ElementFactory::make("capsfilter")
        .property("caps", Caps::builder("video/x-raw").field("format", "NV12").build())
        .build()
        .context("capsfilter element unavailable")?;

    let appsink = make("appsink")?
        .dynamic_cast::<AppSink>()
        .map_err(|_| anyhow!("appsink element was not an AppSink"))?;
    // Never wait for a clock (low latency over smoothness: pacing happens at
    // present time), keep at most the newest frame, and don't let the sink pin
    // an extra "last sample" buffer.
    appsink.set_property("sync", false);
    appsink.set_property("max-buffers", 1u32);
    appsink.set_property("drop", true);
    appsink.set_property("enable-last-sample", false);

    let mut chain_elements: Vec<Element> = vec![appsrc.clone().upcast(), parse, decoder];
    if backend == Backend::Software {
        // libav/libaom emit I420; one videoconvert makes NV12.
        chain_elements.push(make("videoconvert")?);
    }
    chain_elements.push(nv12);
    chain_elements.push(appsink.clone().upcast());

    pipeline
        .add_many(&chain_elements)
        .context("failed to add elements to pipeline")?;
    Element::link_many(&chain_elements).context("failed to link decode chain (caps did not negotiate)")?;

    // Decoded-frame counter at the decoder's output boundary: counts every
    // buffer arriving at the sink, whether or not anything consumes it.
    let sink_pad = appsink.static_pad("sink").ok_or_else(|| anyhow!("appsink has no sink pad"))?;
    let probe_shared = shared.clone();
    sink_pad.add_probe(gstreamer::PadProbeType::BUFFER, move |_, _| {
        probe_shared.record_decoded();
        gstreamer::PadProbeReturn::Ok
    });

    let cb_shared = shared.clone();
    let mut cached: Option<(Caps, VideoInfo)> = None;
    appsink.set_callbacks(
        AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gstreamer::FlowError::Eos)?;
                let (Some(buffer), Some(caps)) = (sample.buffer_owned(), sample.caps()) else {
                    return Ok(gstreamer::FlowSuccess::Ok);
                };
                if !cached.as_ref().is_some_and(|(c, _)| c.as_ref() == caps) {
                    match VideoInfo::from_caps(caps) {
                        Ok(info) => cached = Some((caps.to_owned(), info)),
                        Err(e) => {
                            tracing::warn!("decoded sample had unusable caps {caps}: {e}");
                            return Ok(gstreamer::FlowSuccess::Ok);
                        }
                    }
                }
                let info = &cached.as_ref().expect("just filled").1;
                let pts_us = buffer.pts().map(|p| p.useconds());
                let mapped = match VideoFrame::from_buffer_readable(buffer, info) {
                    Ok(f) => f,
                    Err(_) => {
                        tracing::warn!("failed to map decoded video buffer readable; dropping frame");
                        return Ok(gstreamer::FlowSuccess::Ok);
                    }
                };
                match DecodedFrame::new(mapped, info, pts_us, cb_shared.next_seq(), codec, cb_shared.display_hint()) {
                    Ok(frame) => cb_shared.publish(frame),
                    Err(why) => tracing::warn!("dropping decoded frame: {why}"),
                }
                Ok(gstreamer::FlowSuccess::Ok)
            })
            .build(),
    );

    let bus = pipeline.bus().ok_or_else(|| anyhow!("pipeline has no bus"))?;
    let decoded_at_build = shared.decoded_total();
    pipeline
        .set_state(State::Playing)
        .map_err(|e| anyhow!("{decoder_name}: pipeline failed to start: {e:?}"))?;

    Ok(Active {
        pipeline,
        appsrc,
        bus,
        codec,
        backend,
        decoder_name,
        built_at: Instant::now(),
        decoded_at_build,
        replay: (backend == Backend::Hardware).then(Vec::new),
    })
}

pub(super) struct Worker {
    shared: Arc<Shared>,
    active: Option<Active>,
    /// Codecs whose VA decoder failed; they go straight to software.
    hw_failed: HashSet<VideoCodec>,
    /// After a mid-stream HW failure the software decoder must start on a
    /// keyframe; delta frames are dropped until one arrives.
    need_keyframe: bool,
}

impl Worker {
    pub(super) fn new(shared: Arc<Shared>, allow_hardware: bool) -> Self {
        // "Software only" is "every codec's VA decoder has already failed".
        let hw_failed = if allow_hardware {
            HashSet::new()
        } else {
            [VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1].into_iter().collect()
        };
        Self { shared, active: None, hw_failed, need_keyframe: false }
    }

    /// (Re)build the pipeline for `codec`, hardware first unless it has
    /// already failed for this codec.
    fn start(&mut self, codec: VideoCodec) {
        self.active = None; // tears the old pipeline down
        if !self.hw_failed.contains(&codec) {
            match build(codec, Backend::Hardware, &self.shared) {
                Ok(active) => return self.adopt(active),
                Err(e) => {
                    tracing::info!("{} hardware decode unavailable, using software: {e:#}", codec.name());
                    self.hw_failed.insert(codec);
                }
            }
        }
        match build(codec, Backend::Software, &self.shared) {
            Ok(active) => self.adopt(active),
            Err(e) => {
                tracing::error!("no usable {} decoder: {e:#}", codec.name());
                self.shared.set_info(DecoderInfo { codec: Some(codec), decoder_name: None, hardware: false });
            }
        }
    }

    /// Backend of the running pipeline, if any (tests).
    #[cfg(test)]
    pub(super) fn backend(&self) -> Option<Backend> {
        self.active.as_ref().map(|a| a.backend)
    }

    #[cfg(test)]
    pub(super) fn hw_failed_contains(&self, codec: VideoCodec) -> bool {
        self.hw_failed.contains(&codec)
    }

    fn adopt(&mut self, active: Active) {
        let hardware = active.backend == Backend::Hardware;
        tracing::info!(
            "{} decoding via {} ({})",
            active.codec.name(),
            active.decoder_name,
            if hardware { "hardware" } else { "software" }
        );
        self.shared.set_info(DecoderInfo {
            codec: Some(active.codec),
            decoder_name: Some(active.decoder_name.to_string()),
            hardware,
        });
        self.active = Some(active);
    }

    /// Replace a failed HW decoder with software. If it never produced a
    /// frame the access units pushed so far are replayed into the new
    /// pipeline; otherwise decoding resumes at the next keyframe.
    pub(super) fn fall_back_to_software(&mut self, why: &str) {
        let Some(mut old) = self.active.take() else { return };
        if old.backend != Backend::Hardware {
            self.active = Some(old);
            return;
        }
        tracing::warn!("{} hardware decoder {} failed ({why}); falling back to software", old.codec.name(), old.decoder_name);
        let codec = old.codec;
        let replay = old.replay.take();
        drop(old);
        self.hw_failed.insert(codec);
        self.start(codec);
        match (replay, &self.active) {
            (Some(aus), Some(active)) => {
                for (pts, key, data) in aus {
                    active.push(pts, key, data);
                }
            }
            _ => self.need_keyframe = true,
        }
    }

    fn poll_bus(&mut self) {
        let mut failure: Option<String> = None;
        if let Some(active) = &self.active {
            while let Some(msg) = active.bus.pop() {
                match msg.view() {
                    MessageView::Error(e) => {
                        let text = format!("{} ({:?})", e.error(), e.debug());
                        tracing::warn!("decode pipeline error from {:?}: {text}", e.src().map(|s| s.path_string()));
                        failure.get_or_insert(text);
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
        }
        if let Some(text) = failure {
            self.fall_back_to_software(&text);
        }
    }

    /// Probation bookkeeping for a HW decoder: close it once the first frame
    /// is out, give up on it if none shows up in time.
    fn check_probation(&mut self) {
        let Some(active) = &mut self.active else { return };
        if active.replay.is_none() {
            return;
        }
        if self.shared.decoded_total() > active.decoded_at_build {
            active.replay = None;
        } else if active.built_at.elapsed() > HW_PROBATION {
            self.fall_back_to_software("no frame within the probation window");
        }
    }

    /// Whether the next access unit can be handed over now: always when no
    /// pipeline exists yet (it is then just sniffed/dropped or starts one),
    /// otherwise only while `appsrc`'s own queue is shallow.
    fn can_accept(&self) -> bool {
        self.active.as_ref().is_none_or(|a| a.queued() < APPSRC_MAX_QUEUED)
    }

    pub(super) fn handle_access_unit(&mut self, pts_us: u64, keyframe: bool, data: Vec<u8>) {
        self.poll_bus();

        if keyframe {
            if let Some(found) = codec::sniff(&data) {
                if self.active.as_ref().map(|a| a.codec) != Some(found) {
                    if let Some(a) = &self.active {
                        tracing::info!("codec changed {} -> {}; rebuilding decoder", a.codec.name(), found.name());
                    }
                    self.start(found);
                }
            }
            self.need_keyframe = false;
        } else if self.need_keyframe {
            return;
        }

        self.check_probation();
        let Some(active) = &mut self.active else {
            return; // codec not known yet: drop
        };
        if let Some(replay) = &mut active.replay {
            if replay.len() >= REPLAY_MAX_AUS {
                // Far past any sane probation; stop hoarding.
                active.replay = None;
            } else {
                replay.push((pts_us, keyframe, data.clone()));
            }
        }
        active.push(pts_us, keyframe, data);
    }
}

/// Pipeline thread body: runs until `Shutdown` or the sender side is gone.
///
/// Access units arrive on `rx`, wait in a local backlog, and are handed to the
/// decoder only while `appsrc`'s queue is shallow (see [`APPSRC_MAX_QUEUED`]).
pub(super) fn run(rx: std_mpsc::Receiver<PipelineMsg>, shared: Arc<Shared>, allow_hardware: bool) -> Result<()> {
    let mut worker = Worker::new(shared.clone(), allow_hardware);
    let mut backlog: VecDeque<Au> = VecDeque::new();
    'run: loop {
        // Idle: block (briefly, so bus/probation housekeeping still runs).
        // Backed up: just a short nap until the decoder has room again.
        let wait = if backlog.is_empty() { Duration::from_millis(20) } else { Duration::from_micros(150) };
        let mut next = rx.recv_timeout(wait);
        loop {
            match next {
                Ok(PipelineMsg::AccessUnit { pts_us, keyframe, data }) => backlog.push_back((pts_us, keyframe, data)),
                Ok(PipelineMsg::Shutdown) | Err(std_mpsc::RecvTimeoutError::Disconnected) => break 'run,
                Err(std_mpsc::RecvTimeoutError::Timeout) => {
                    worker.poll_bus();
                    worker.check_probation();
                    break;
                }
            }
            // Drain whatever else is already waiting without blocking.
            next = match rx.try_recv() {
                Ok(msg) => Ok(msg),
                Err(std_mpsc::TryRecvError::Empty) => break,
                Err(std_mpsc::TryRecvError::Disconnected) => Err(std_mpsc::RecvTimeoutError::Disconnected),
            };
        }

        let dropped = trim_backlog(&mut backlog, BACKLOG_MAX_AUS);
        if dropped > 0 {
            shared.add_dropped(dropped as u64);
            tracing::debug!("decoder is behind: dropped {dropped} queued access units");
        }
        while !backlog.is_empty() && worker.can_accept() {
            let (pts_us, keyframe, data) = backlog.pop_front().expect("checked non-empty");
            worker.handle_access_unit(pts_us, keyframe, data);
        }
    }
    if let Some(active) = &worker.active {
        let _ = active.appsrc.end_of_stream();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn au(id: u64, keyframe: bool) -> Au {
        (id, keyframe, vec![])
    }

    fn ids(b: &VecDeque<Au>) -> Vec<u64> {
        b.iter().map(|a| a.0).collect()
    }

    #[test]
    fn a_backlog_under_the_cap_is_left_alone_even_with_the_keyframe_first() {
        // The start-up burst: keyframe, then deltas while the decoder warms up.
        let mut b: VecDeque<Au> = (0..30).map(|i| au(i, i == 0)).collect();
        assert_eq!(trim_backlog(&mut b, 64), 0);
        assert_eq!(b.len(), 30);
    }

    #[test]
    fn over_the_cap_it_jumps_to_the_newest_keyframe() {
        let mut b: VecDeque<Au> = (0..100).map(|i| au(i, i == 0 || i == 50)).collect();
        let dropped = trim_backlog(&mut b, 64);
        assert_eq!(dropped, 50);
        assert_eq!(ids(&b).first(), Some(&50), "must start at the newest keyframe");
        assert_eq!(b.len(), 50);
    }

    #[test]
    fn over_the_cap_without_a_keyframe_it_drops_the_oldest() {
        let mut b: VecDeque<Au> = (0..80).map(|i| au(i, false)).collect();
        assert_eq!(trim_backlog(&mut b, 64), 16);
        assert_eq!(ids(&b).first(), Some(&16));
        assert_eq!(b.len(), 64);
    }

    #[test]
    fn a_keyframe_at_the_front_survives_trimming() {
        let mut b: VecDeque<Au> = (0..80).map(|i| au(i, i == 0)).collect();
        assert_eq!(trim_backlog(&mut b, 64), 16);
        assert_eq!(ids(&b)[..3], [0, 17, 18], "keyframe kept, oldest deltas after it dropped");
        assert_eq!(b.len(), 64);
    }

    #[test]
    fn jumping_to_a_keyframe_that_still_leaves_too_many_trims_further() {
        // Keyframe near the front of a long run.
        let mut b: VecDeque<Au> = (0..200).map(|i| au(i, i == 10)).collect();
        trim_backlog(&mut b, 64);
        assert_eq!(b.len(), 64);
        assert_eq!(b.front().map(|a| a.0), Some(10), "keyframe still first");
    }
}
