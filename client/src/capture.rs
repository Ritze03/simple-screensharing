//! Screen capture (Linux: xdg-desktop-portal `ScreenCast` + PipeWire;
//! Windows: GStreamer's `d3d11screencapturesrc`) → H.264 encode (GStreamer) →
//! AEAD-encrypt → fragment → send, over the QUIC media-datagram plane wired
//! up in `net.rs`.
//!
//! # Pipeline
//!
//! The tee carries frames in one of two [`CaptureDomain`]s. **GPU** (Linux
//! portal capture on a machine with a working VA stack, the normal case on
//! desktop Linux) keeps everything in GPU memory:
//!
//! ```text
//! pipewiresrc (DMA-BUF) ! queue max-size-buffers=2   (see `decoupled_source`: stock xdpw)
//!   ! vapostproc video-direction=auto
//!   ! video/x-raw(memory:VAMemory),format=NV12 ! tee name=t
//!   t. ! queue leaky=downstream max-size-buffers=1 ! videorate drop-only=true ! vapostproc
//!      ! video/x-raw,format=BGRx,framerate=1/1 ! videoconvert ! video/x-raw,format=RGB
//!      ! appsink name=preview_sink
//!   (+ one dynamic branch per destination, see below)
//! ```
//!
//! and **System** (Windows, software `x264enc` machines, tests) is the
//! original CPU shape:
//!
//! ```text
//! <platform source> ! videoconvert ! video/x-raw,format=BGRx
//!   [! <vapostproc|videoflip> video-direction=auto  -- only while the source is rotated]
//!   ! video/x-raw,format=BGRx ! tee name=t
//!   t. ! queue leaky=downstream max-size-buffers=1 ! videorate drop-only=true ! video/x-raw,framerate=1/1
//!      ! videoconvert ! video/x-raw,format=RGB
//!      ! appsink name=preview_sink
//!   (+ one dynamically added branch per destination — see below)
//! ```
//!
//! See [`CaptureDomain`] for why the GPU domain exists (it is what makes
//! 120-240 fps of *unique* frames possible at all).
//!
//! That branch is the pipeline's only *fixed* one. **There is no
//! fixed encoder branch**: every outgoing destination — including the first
//! — is a dynamic branch added to the same `tee` at runtime by
//! [`branch::add_branch`]:
//!
//! ```text
//!   t. ! queue leaky=downstream ! valve drop-mode=forward-sticky-events   (+ a RateCap probe on the valve's src)
//!      ! <vapostproc | videoconvert ! videoscale>
//!      ! video/x-raw[(memory:VAMemory)],format=<backend's>,width=..,height=..
//!      ! capssetter framerate=<this destination's>/1
//!      ! <x264enc|vah264enc|vah265enc|vaav1enc> bitrate=<this destination's> key-int-max=<gop>
//!      ! video/x-h264,stream-format=byte-stream,alignment=au
//!      ! appsink drop=false
//! ```
//!
//! The destination's fps is a **cap** ([`branch::RateCap`]), never a floor:
//! nothing in a branch ever duplicates a frame.
//!
//! so one capture feeds N rooms as N independently encoded and independently
//! encrypted streams (a room key can never be shared across rooms), each with
//! its own [`DestinationHandle`], its own [`destination_pump`] task, and its
//! own pause `valve` — subscriber counts are per stream, so a destination
//! nobody is watching pauses its own encoder (see [`DestCmd`]) without
//! touching any sibling destination of the same capture. See
//! `superdoc/features/stream-control.md`.
//!
//! The bracketed `video-direction=auto` stage makes a **rotated or mirrored
//! monitor arrive upright** in every destination and preview: `pipewiresrc`
//! turns the stream's `SPA_META_VideoTransform` into an `image-orientation`
//! tag, and the stage follows it (swapping width/height for 90/270). In the
//! GPU domain the source's `vapostproc` *is* that stage (it has to convert the
//! colour space anyway, so rotating costs nothing extra). In the System domain
//! the stage is spliced in only when such a tag says the picture is rotated, so
//! an un-rotated source pays nothing. See [`orientation_fragment`] for the why,
//! the GPU-vs-CPU choice and the measurements. Verified live (#39): the patched
//! `xdg-desktop-portal-wlr` does send the transform for the 180-degree-rotated
//! DP-2 (`image-orientation=rotate-180`), and the viewer sees it upright.
//!
//! The pixel format *is* pinned before the `tee`, and for a closely related
//! reason: it keeps what the source negotiates independent of which branches
//! happen to exist at that moment. See [`SOURCE_RAW_FORMAT`].
//!
//! **Nothing converts the frame rate before the `tee`** — the tee carries
//! whatever the source natively produces, and every rate conversion happens
//! inside a destination's own branch, downstream of that branch's `queue`.
//! This is load-bearing rather than incidental: a `videorate` upstream of the
//! tee stalls a real portal source dead (0 fps past the tee, no error
//! anywhere) while behaving perfectly against `videotestsrc`. See
//! [`build_pipeline`]'s "Why there is no rate conversion before the `tee`"
//! for the measurements, and
//! `capture::tests::live_portal_capture_feeds_a_destination_branch` for the
//! regression test that would have caught it.
//!
//! `<platform source>` is [`CaptureSource::launch_fragment`]:
//! `pipewiresrc fd=<fd> path=<node_id> do-timestamp=true keepalive-time=<ms>`
//! on Linux/Unix (behind `#[cfg(unix)]`, fed by the portal flow below), or
//! `d3d11screencapturesrc monitor-index=0 do-timestamp=true` on Windows
//! (behind `#[cfg(windows)]`, no portal/consent flow needed — see
//! [`acquire_capture_source`]). Everything downstream of that first element
//! is identical on both platforms. `keepalive-time` (Linux only — see
//! [`CaptureSource::launch_fragment`]'s doc comment) is what keeps frames
//! flowing at a constant, selected rate even while the desktop is
//! completely static: a portal `ScreenCast` PipeWire stream otherwise only
//! pushes a buffer when the compositor reports screen damage, which — left
//! unfixed — starves both the encoder branch's `videorate` cap (nothing to
//! duplicate from) and the encoder's rate control (no frames to converge
//! quality back down on after a motion burst).
//!
//! The fixed, low-rate `preview_sink` branch's RGB8 samples back
//! [`preview::spawn_preview_task`] (see [`start_sharing`]) — the
//! already-implemented periodic thumbnail/preview sender in `preview.rs`,
//! which itself has zero GStreamer dependency and only samples the latest
//! frame off whatever [`preview::FrameStream`] it's handed every 30s, so
//! capping the tee branch to ~1fps here is purely a CPU-saving measure, not
//! something the preview task requires.
//!
//! Two [`EncoderBackend`]s implement the encoder stage: software `x264enc`
//! (above) and, preferred when actually usable, VAAPI's `vah264enc` (format
//! NV12 instead of I420, `rate-control=cbr bitrate=<kbps> key-int-max=<gop>
//! target-usage=<1|2|4>` instead of x264enc's tune/speed-preset). [`start_sharing`]
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
//!
//! # Dynamic tee branches ([`branch`])
//!
//! Everything above describes the pipeline's three *fixed* tee branches,
//! built once by [`build_pipeline`] and never changed for the life of the
//! pipeline. The [`branch`] submodule is the mechanism for a fourth kind:
//! branches added and removed at runtime, on a live `Playing` pipeline,
//! without disturbing the fixed branches or each other — the GStreamer
//! mechanics behind multi-destination fan-out (one capture, several
//! independently-encoded outgoing streams), per-destination live settings
//! changes, and stream transfer, per
//! `superdoc/brainstorm/ui-redesign-multiroom-streaming.md`'s "branch-swap"
//! design.
//!
//! # Per-destination lifecycle ([`destination`])
//!
//! [`destination`] is the state half of the same design: the
//! [`destination::DestinationState`] machine (`Starting`/`Live`/`Rebuilding`/
//! `Stalled`/`Failed`), the monotonic wire-PTS invariant that makes a capture
//! restart look like low framerate rather than a glitch, the cached-IDR stall
//! protocol, and the pure functions classifying a settings change into the
//! plan doc's three tiers ([`destination::classify_change`]). It holds no
//! GStreamer types and no clock of its own, so all of it is unit-testable
//! against injected `Instant`s.
//!
//! The two halves are joined by [`branch::swap_to_new_branch`] (tier 2) and
//! [`branch::stall_all`] + [`CaptureHandle::set_source_framerate`] (tier 3),
//! and every user-driven settings change is routed through
//! [`CaptureHandle::change_destination`], which classifies it rather than
//! letting a call site assume a tier. Each destination owns a
//! [`destination::DestinationRuntime`] that its [`destination_pump`] drives
//! for real: wire PTS, IDR caching, first-IDR `Starting` → `Live`, and the
//! stall keepalive.
//!
//! See `superdoc/architecture/overview.md`'s "Per-destination lifecycle"
//! section for the full model.

#[cfg(unix)]
use std::os::fd::{AsRawFd, OwnedFd};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
#[cfg(unix)]
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
#[cfg(unix)]
use ashpd::desktop::{PersistMode, Session};
use futures_util::StreamExt;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use gstreamer_video::prelude::*;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::fragment::fragment_frame_with_flags;
use crate::net::{encrypt_frame, ScreenshareClient};
use crate::preview::{self, FrameStream, PreviewHandle, RawFrame};
use crate::protocol::StreamKind;

/// Element name of the VAAPI H.264 encoder in the installed GStreamer
/// version. GStreamer has shipped two generations of VAAPI plugin with
/// different element names across versions (`vaapih264enc` from the old
/// `gstreamer-vaapi` plugin, `vah264enc` from the newer `va` plugin in
/// `gst-plugins-bad`) — confirmed via `gst-inspect-1.0` on this dev machine
/// (GStreamer 1.28.5) that only `vah264enc` is present.
const VAAPI_ENCODER_ELEMENT: &str = "vah264enc";

/// [`EncoderBackend::Vaapi`]'s H.265 encoder element — confirmed present via
/// `gst-inspect-1.0 vah265enc` on this dev machine (AMD RX 7900 XTX, RDNA3,
/// GStreamer 1.28.5, `va` plugin). Its `rate-control` (`cbr`/`vbr`/`cqp`),
/// `bitrate`, `key-int-max` and `target-usage` properties have the exact same
/// names/ranges/semantics as [`VAAPI_ENCODER_ELEMENT`]'s (re-verified by
/// direct `gst-inspect-1.0` comparison, not assumed) — see
/// [`EncoderBackend::launch_fragment`].
const VAAPI_H265_ENCODER_ELEMENT: &str = "vah265enc";

/// [`EncoderBackend::Vaapi`]'s AV1 encoder element — confirmed present via
/// `gst-inspect-1.0 vaav1enc` on this dev machine. Its `rate-control`/
/// `bitrate`/`key-int-max`/`target-usage` properties are also
/// name-and-semantics-compatible with [`VAAPI_ENCODER_ELEMENT`]'s — but its
/// SRC caps are `video/x-av1` with `stream-format=obu-stream,alignment=tu`
/// (no Annex-B start codes at all), unlike `vah264enc`/`vah265enc`'s
/// `stream-format=byte-stream,alignment=au` — see
/// [`EncoderBackend::launch_fragment`] and [`branch::add_branch`]'s output
/// caps selection.
const VAAPI_AV1_ENCODER_ELEMENT: &str = "vaav1enc";

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
    ///
    /// `tune`/`quality_level`/`rate_control` are the three live-adjustable
    /// (routed through [`destination::ChangeTier::BranchSwap`] — see
    /// that type's doc comment) encoder tuning knobs; each is mapped to the
    /// matching property for whichever backend `self` is, per the mapping
    /// documented on [`TuneMode`], [`QualityLevel`] and [`RateControlMode`].
    ///
    /// `codec` selects which encoder element [`EncoderBackend::Vaapi`] uses
    /// (`vah264enc`/`vah265enc`/`vaav1enc` — see [`VideoCodec`]); it is
    /// silently ignored on [`EncoderBackend::Software`], which only ever has
    /// `x264enc` (H.264) available — see [`supported_codecs`] and
    /// [`EncoderBackend::actual_codec`], which is what callers must consult
    /// to know what this backend *actually* produced.
    fn launch_fragment(
        self,
        bitrate_kbps: u32,
        gop_frames: u32,
        tune: TuneMode,
        quality_level: QualityLevel,
        rate_control: RateControlMode,
        codec: VideoCodec,
    ) -> String {
        match self {
            EncoderBackend::Software => {
                // Software backend only ever has `x264enc` (H.264) available
                // — see [`supported_codecs`] — so `codec` is a no-op here,
                // exactly like `tune` is a no-op on [`EncoderBackend::Vaapi`]
                // below. Callers that need to know what codec actually came
                // out must go through [`EncoderBackend::actual_codec`], not
                // assume the requested `codec` was honored.
                let _ = codec;
                // `tune=zerolatency` is x264enc's low-latency knob; leaving
                // `tune` unset entirely is its best-quality (general-purpose
                // rate/distortion tuning) mode — there is no explicit
                // "best-quality" value to set, only the absence of the
                // low-latency one. See [`TuneMode`].
                let tune_flag = match tune {
                    TuneMode::LowLatency => "tune=zerolatency ",
                    TuneMode::BestQuality => "",
                };
                let speed_preset = match quality_level {
                    QualityLevel::Performance => "ultrafast",
                    QualityLevel::Balanced => "medium",
                    QualityLevel::Quality => "veryslow",
                };
                // x264enc has no rate-control *enum* like vah264enc's — its
                // closest control is `pass` (`cbr`/`quant`/`qual`) +
                // `quantizer`. `Qvbr` maps to this backend's existing
                // implicit default (plain `bitrate=`, no `pass` override —
                // x264enc's CBR mode): x264enc has no native QVBR-equivalent
                // knob either, so its own default single-pass rate control —
                // already the closest thing it offers to "seek quality within
                // a bitrate ceiling" — is the best available analogue (the
                // same reasoning the old `Adaptive`/`Constant` no-op relied
                // on; see [`RateControlMode`]'s doc comment). `Cqp` switches
                // to `pass=qual`, x264enc's constant-quality mode driven by
                // `quantizer` rather than a bitrate target (the `bitrate=`
                // property above is simply ignored by x264enc while
                // `pass=qual` is active).
                let rate_control_flag = match rate_control {
                    RateControlMode::Qvbr => "",
                    RateControlMode::Cqp => " pass=qual quantizer=21",
                };
                // `bframes=0`: pinned explicitly rather than relied upon as
                // x264enc's own default (also 0) so it can never silently
                // drift if that default ever changes — every preset wants 0
                // B-frames today (lowest latency has no B-frame budget to
                // spend; the Gaming preset specifically needs this pinned so
                // fast-motion content can't pick up encode-latency-costing
                // B-frames from an upstream default change either).
                format!(
                    "x264enc {tune_flag}speed-preset={speed_preset} bitrate={bitrate_kbps} key-int-max={gop_frames} bframes=0{rate_control_flag}"
                )
            }
            // vah264enc's `bitrate` property is kbps, same units as
            // x264enc's, and it has a `key-int-max` property with the exact
            // same name/semantics as x264enc's — verified via `gst-inspect-1.0
            // vah264enc`. `target-usage` is the VAAPI analogue of x264enc's
            // `speed-preset` — 1 (best quality/slowest) – 7 (fastest/lowest
            // quality) — see [`QualityLevel`]. `rate-control` is vah264enc's
            // real, native rate-control enum — see [`RateControlMode`].
            //
            // `tune` has no VAAPI analogue at all (see [`TuneMode`]'s doc
            // comment) — deliberately not read here; the field still exists
            // on this backend's caller-facing settings so the control can be
            // uniformly present in a request, it's simply a no-op when the
            // active branch is VAAPI.
            EncoderBackend::Vaapi => {
                let _ = tune;
                // `Why Performance is 2, not 7 (#46):` the VA spec reads
                // target-usage as 1 = best quality … 7 = fastest, but Mesa's
                // radeonsi driver does not follow that order. Measured on the
                // dev machine (RX 7900 XTX, Mesa 26.2.4, GStreamer 1.28.8,
                // `client/scripts/enc-bench.sh`, 1080p NV12, 8 Mbit/s CBR and
                // VBR alike, fps of raw encoder throughput):
                //
                //   target-usage    1     2     3     4     5     6     7
                //   vah264enc     246   307   307   172   172   172   172
                //   vah265enc     482   536   536   476   476   476   476
                //   vaav1enc      526   535   532   217   217   117   117
                //
                // The driver groups the values into bands (2-3 / 4-5 / 6-7,
                // with 1 its own), and 7 was the *slowest* AV1 band: the Gaming
                // and Low-latency presets (both Performance) encoded AV1 at
                // 117 fps and H.264 at 172. 2 is the fastest band for all three
                // codecs. Its quality cost is noise: PSNR on a 1080p60
                // `testsrc2` clip at 8 Mbit/s, H.265: 40.83 dB at 2, 40.84 at
                // 4, 40.90 at 7. 1 is not used for Performance: it is slower
                // than 2 and its rate control overshoots (H.264 wrote 65 %
                // more bytes than 2 at the same bitrate setting).
                // Balanced (4) and Quality (1) are unchanged. A driver that
                // does follow the VA order (Intel iHD) gets a slower,
                // higher-quality encode for Performance from this; the
                // encoder-limit hint (`destination::EncoderLimit`) still
                // tells the user when the encoder caps the framerate.
                let target_usage = match quality_level {
                    QualityLevel::Performance => 2,
                    QualityLevel::Balanced => 4,
                    QualityLevel::Quality => 1,
                };
                let rate_control_value = match rate_control {
                    RateControlMode::Cqp => "cqp",
                    // Despite the variant's name, this now maps to
                    // vah264enc's `vbr` (plain Variable Bitrate), not the
                    // literal `qvbr` enum value — see [`RateControlMode::Qvbr`]'s
                    // doc comment for the full story (a real, measured driver
                    // bug means `qvbr`'s bitrate ceiling is silently ignored
                    // on this hardware, while `vbr`'s is genuinely honored)
                    // and for why the identifier itself was kept anyway.
                    RateControlMode::Qvbr => "vbr",
                };
                // `vah265enc`/`vaav1enc` both expose the exact same
                // `rate-control`/`bitrate`/`key-int-max`/`target-usage`/
                // `target-percentage`/`cpb-size` property names, ranges and
                // enum values as `vah264enc` — all three derive from the same
                // GStreamer VA-plugin base encoder class — confirmed via
                // `gst-inspect-1.0` on this dev machine (see
                // [`VAAPI_H265_ENCODER_ELEMENT`]/[`VAAPI_AV1_ENCODER_ELEMENT`]'s
                // doc comments) — so only the element name varies by codec.
                let element = match codec {
                    VideoCodec::H264 => VAAPI_ENCODER_ELEMENT,
                    VideoCodec::H265 => VAAPI_H265_ENCODER_ELEMENT,
                    VideoCodec::Av1 => VAAPI_AV1_ENCODER_ELEMENT,
                };
                // `Qvbr`/`vbr` needs two more properties `Cqp` doesn't:
                // - `target-percentage`: VBR-only property, "the percentage
                //   for 'target bitrate'/'maximum bitrate'" — the real encode
                //   target is `bitrate * target-percentage / 100`, with
                //   `bitrate` acting as the ceiling. Left at the element's
                //   default of 66%, this is exactly the previously-removed
                //   `Adaptive` mode's diagnosed pop-in cause (the real target
                //   swinging between ~66-100% of the ceiling as content
                //   changed) — see [`RateControlMode::Qvbr`]'s doc comment.
                //   Set to [`VBR_TARGET_PERCENTAGE`], deliberately high (close
                //   to the ceiling) so the real target sits near `bitrate`
                //   instead of swinging down toward two-thirds of it.
                // - `cpb-size`: vah264enc's VBV buffer size in Kb, `0` (its
                //   default) means "auto-calculate" (empirically, roughly 1x
                //   `bitrate` here — the same naive "1-second-equivalent"
                //   sizing the old `qvbr` implementation used explicitly).
                //   Set deliberately *tighter* than that via
                //   [`VBR_CPB_SIZE_NUMERATOR`]/[`VBR_CPB_SIZE_DENOMINATOR`] —
                //   see that constant's doc comment for the exact fraction
                //   and the measured rationale — so the encoder re-evaluates
                //   its rate target against a smaller buffer, adjusting more
                //   often in finer steps instead of swinging widely between
                //   infrequent large-buffer refill cycles.
                // `Cqp` deliberately leaves `qpi`/`qpp`/`qpb`/`target-percentage`
                // unset at the element's own defaults — see
                // [`RateControlMode::Cqp`]'s doc comment for why that stays
                // untuned.
                let quality_extra = match rate_control {
                    RateControlMode::Cqp => String::new(),
                    RateControlMode::Qvbr => {
                        let cpb_size = bitrate_kbps * VBR_CPB_SIZE_NUMERATOR / VBR_CPB_SIZE_DENOMINATOR;
                        format!(" target-percentage={VBR_TARGET_PERCENTAGE} cpb-size={cpb_size}")
                    }
                };
                // `b-frames=0`: see the software branch's matching `Why:`
                // note above — pinned explicitly on both backends, but only
                // for H.264/H.265: `vaav1enc` has no `b-frames` property at
                // all (confirmed via `gst-inspect-1.0 vaav1enc` and a real
                // pipeline-parse failure — "no property \"b-frames\" in
                // element \"vaav1enc\"" — when this was tried unconditionally
                // on real VAAPI hardware), so it's simply omitted for AV1
                // rather than pinned to a property that doesn't exist.
                let b_frames_flag = match codec {
                    VideoCodec::H264 | VideoCodec::H265 => " b-frames=0",
                    VideoCodec::Av1 => "",
                };
                format!(
                    "{element} rate-control={rate_control_value} bitrate={bitrate_kbps} key-int-max={gop_frames} target-usage={target_usage}{b_frames_flag}{quality_extra}"
                )
            }
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

    /// The codec this backend will *actually* produce for a requested
    /// `codec` — as opposed to `codec` itself, which is merely what was
    /// asked for. [`EncoderBackend::Software`] only ever has `x264enc`
    /// available (see [`supported_codecs`]), so it silently produces H.264
    /// regardless of what was requested, exactly like it silently ignores
    /// [`TuneMode`] differences [`EncoderBackend::Vaapi`] has none of.
    /// [`EncoderBackend::Vaapi`] honors whatever [`VideoCodec`] was
    /// requested.
    ///
    /// Callers that need to know what's really coming out of the pipeline —
    /// [`branch::add_branch`]'s output-caps selection, and anything
    /// reporting the codec upstream — must call this rather than trust the
    /// requested [`VideoCodec`] as-is, since a `Vaapi` request can silently
    /// downgrade to `Software`/H.264 via [`branch::add_branch_with_fallback`]'s
    /// per-branch fallback.
    pub fn actual_codec(self, requested: VideoCodec) -> VideoCodec {
        match self {
            EncoderBackend::Software => VideoCodec::H264,
            EncoderBackend::Vaapi => requested,
        }
    }
}

/// Which video codec an encoder produces. Threaded through the same
/// settings pipeline as [`RateControlMode`]/[`QualityLevel`]
/// (`VideoParams` → `EncodeSettings` → `DestEdit` → `dto.rs` → persisted
/// `rooms::AppSettings`), live-adjustable per destination via
/// `edit_destination` like those two (a codec change is always a
/// [`destination::ChangeTier::BranchSwap`] — see [`destination::
/// classify_change`] — never a live property write, since swapping the
/// encoder *element* itself is unavoidably a branch rebuild).
///
/// `#[default]` is [`Self::H264`] — the safest choice: it's this pipeline's
/// original, only-ever-shipped codec, so every existing user's persisted
/// settings (no `codec` field in their on-disk `settings.toml` before this)
/// deserialize to exactly the behavior they already had. Same reasoning as
/// why `RateControlMode::Adaptive`'s wire string stayed `"qvbr"`-adjacent
/// rather than renamed after the VBR fix — don't break existing configs.
///
/// See [`supported_codecs`] for which codecs are actually usable on a given
/// [`EncoderBackend`] — H.265/AV1 require [`EncoderBackend::Vaapi`], since
/// no software H.265/AV1 encoder is confirmed installed on this machine (no
/// `x265enc`/`aomenc`/`svtav1enc`/etc. assumed present — see this module's
/// task brief). The UI's codec selector should call
/// [`supported_codecs`] to grey out unavailable options rather than let a
/// user pick one that silently falls back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoCodec {
    /// `x264enc` (software) / `vah264enc` (VAAPI). The only codec available
    /// on [`EncoderBackend::Software`].
    #[default]
    H264,
    /// `vah265enc` (VAAPI only — see this type's doc comment).
    H265,
    /// `vaav1enc` (VAAPI only — see this type's doc comment).
    Av1,
}

/// Every [`VideoCodec`] variant, in the stable order [`supported_codecs`]
/// returns.
const ALL_CODECS: [VideoCodec; 3] = [VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1];

/// Which [`VideoCodec`]s are actually usable on `backend`. The mechanism
/// the codec selector should call to grey out/disable options the current
/// backend can't produce, rather than letting a user pick one that silently downgrades —
/// see [`EncoderBackend::actual_codec`] for what happens if a mismatched
/// combination reaches the pipeline anyway (it doesn't error, it silently
/// downgrades, which is exactly the surprise this function lets a caller
/// avoid).
///
/// Deliberately backend-only, not a live per-machine capability probe of
/// `vah265enc`/`vaav1enc` specifically (unlike [`detect_encoder_backend`]'s
/// own `vah264enc`-availability probe): every machine with a working
/// [`EncoderBackend::Vaapi`] this session has been tested on has both
/// (they're both part of the same `va` GStreamer plugin as `vah264enc`), and
/// a static, no-I/O function is simpler and enough for Stage 2 to gate the
/// UI correctly. A machine where `vah264enc` works but `vah265enc`/
/// `vaav1enc` mysteriously don't would still fail at
/// [`branch::add_branch_with_fallback`]'s per-branch fallback (silently
/// dropping to software/H.264) rather than crash — a real gap, but a narrow
/// and already-handled one, not worth a second runtime probe pipeline for
/// this stage.
pub fn supported_codecs(backend: EncoderBackend) -> &'static [VideoCodec] {
    match backend {
        EncoderBackend::Software => &ALL_CODECS[0..1],
        EncoderBackend::Vaapi => &ALL_CODECS,
    }
}

/// Low-latency vs. best-quality encoder tuning — [`EncoderBackend::Software`]
/// only. Maps to x264enc's `tune` property (`zerolatency` vs. unset — see
/// [`EncoderBackend::launch_fragment`]). [`EncoderBackend::Vaapi`]'s
/// `vah264enc` has no real equivalent control, so this field is simply
/// ignored on that backend (not an error, not surfaced to the caller — a
/// destination watching a VAAPI-backed branch can still hold any
/// [`TuneMode`] value, it just has no effect until/unless that destination's
/// backend becomes [`EncoderBackend::Software`]).
///
/// Testing-only knob, like [`QualityLevel`]/[`RateControlMode`]: all three
/// are live-adjustable per destination via `edit_destination`, routed through
/// [`destination::ChangeTier::BranchSwap`] like every other
/// resolution/fps/backend change.
///
/// `Serialize`/`Deserialize` (snake_case via `rename_all`, so a hand-edited
/// `settings.toml` and the in-app strings agree on spelling): needed
/// so `rooms::AppSettings` can persist the "Start a Stream" form's
/// last-used value the same way it already stores `BandwidthMode`/
/// `PresentationMode` directly, rather than going through a separate
/// string-parsing layer just for this one settings field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TuneMode {
    /// x264enc `tune=zerolatency`. Opt in explicitly when low latency matters
    /// more than compression efficiency.
    LowLatency,
    /// No `tune` property set — x264enc's general-purpose rate/distortion
    /// tuning, trading some latency for better compression. The default:
    /// the original product requirement was "low latency should be an
    /// option, but quality also has to be achievable — by default a good
    /// mix", and a few tens of milliseconds of extra latency in exchange for
    /// meaningfully better compression is a better out-of-the-box tradeoff
    /// for most streams than defaulting to the latency extreme.
    #[default]
    BestQuality,
}

/// Performance vs. quality encode-speed tradeoff, real on both backends:
/// x264enc's `speed-preset` and vah264enc's `target-usage` are direct
/// analogues of the same 1 (best quality/slowest) – 7 (fastest/lowest
/// quality) knob (see [`EncoderBackend::launch_fragment`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualityLevel {
    /// x264enc `speed-preset=ultrafast` / VA `target-usage=2` — the fastest
    /// setting of both backends. An explicit opt-in for callers that need to
    /// minimize encode cost above all else. `2`, not the VA spec's "fastest"
    /// `7`: Mesa radeonsi is fastest at 2-3 and slowest at 6-7 (measured
    /// table in [`EncoderBackend::launch_fragment`], #46).
    Performance,
    /// x264enc `speed-preset=medium` / vah264enc `target-usage=4` — the
    /// midpoint of both scales (x264's own upstream default preset is
    /// `medium`). The default: per the "good mix" product requirement (see
    /// [`TuneMode`]'s doc comment), neither speed nor quality extreme should
    /// be the out-of-the-box behavior — this sits between [`Self::Performance`]
    /// and [`Self::Quality`].
    #[default]
    Balanced,
    /// x264enc `speed-preset=veryslow` / vah264enc `target-usage=1` — the
    /// slowest, best-quality end of both scales.
    Quality,
}

/// CQP/QVBR rate control (2026-07-30: replaces the earlier Constant/Adaptive/
/// Quality three-mode design — `Adaptive`/vah264enc `vbr` never actually got
/// its `target-percentage` tuned, defaulted to 66%, and produced visible
/// bitrate-swing artifacts, and `Constant`/`cbr` added a third mode without a
/// real quality-seeking option; the product decision was to collapse to two
/// clearly distinct, deliberately-tuned modes instead of three where one was
/// half-finished).
///
/// **2026-07-30 follow-up — `Self::Qvbr` no longer literally means `qvbr`.**
/// A real driver limitation was confirmed on the reference hardware (AMD RX
/// 7900 XTX, Mesa radeonsi VA-API): `vah264enc`'s actual `rate-control=qvbr`
/// mode silently ignores the configured `bitrate` ceiling entirely (a 2000
/// vs. 8000 kbps ceiling produced identical, much-higher real output; a
/// driver warning — `Driver does not support attribute
/// VAConfigAttribEncQuantization` — confirms the driver doesn't honor it).
/// Plain `rate-control=vbr` was measured to genuinely respect the ceiling on
/// the same hardware. So this variant now emits `rate-control=vbr` (tuned
/// with [`VBR_TARGET_PERCENTAGE`]/[`VBR_CPB_SIZE_NUMERATOR`]+
/// [`VBR_CPB_SIZE_DENOMINATOR`] — see [`EncoderBackend::launch_fragment`]) —
/// deliberately tuned to avoid the *original* reason `vbr` was removed (the
/// old `Adaptive` mode's unset-`target-percentage` pop-in artifact).
///
/// Why the variant keeps the name `Qvbr` rather than being renamed to match:
/// the wire string `"qvbr"` (via `#[serde(rename_all = "snake_case")]` below)
/// is both the old web frontend's `rateControl` value and the on-disk
/// persisted field in `AppSettings`/`StartStreamDefaults` (`rooms.rs`) —
/// renaming would touch every call site *and* silently break existing users' persisted
/// settings files (an unknown enum variant fails that field's
/// deserialization, not a graceful per-field fallback). That cost was judged
/// disproportionate to a purely internal/cosmetic rename; the user-visible
/// "Max Quality" label and the wire contract are unaffected either way. Read
/// `Self::Qvbr` as "the max-quality mode", not literally "the mode that sets
/// `rate-control=qvbr`" — this comment is the map from one to the other.
///
/// [`EncoderBackend::Software`] has no equivalent enum — its closest control
/// is `pass` (`cbr`/`quant`/`qual`) + `quantizer`; see
/// [`EncoderBackend::launch_fragment`]'s `Why:` note for how each variant
/// maps there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateControlMode {
    /// vah264enc `rate-control=cqp` (constant quantizer, QP fixed at the
    /// element's own default of 26 — deliberately left unset/untuned, see
    /// [`EncoderBackend::launch_fragment`]). x264enc: `pass=qual
    /// quantizer=21` (constant-quality, driven by `quantizer` rather than a
    /// bitrate target). Conservative/"resourceful" bandwidth use: a fixed
    /// per-frame quality that never spends more of the configured bitrate
    /// ceiling than a static QP happens to need.
    Cqp,
    /// vah264enc `rate-control=vbr` (plain Variable Bitrate — see this type's
    /// doc comment for why this variant, still named `Qvbr`, no longer emits
    /// literal `rate-control=qvbr`): targets [`VBR_TARGET_PERCENTAGE`]% of
    /// `bitrate` as the real encode target, with `bitrate` itself acting as a
    /// ceiling genuinely respected on the reference hardware (unlike `qvbr`'s
    /// — see this type's doc comment), and a deliberately tightened
    /// `cpb-size` (see [`VBR_CPB_SIZE_NUMERATOR`]/[`VBR_CPB_SIZE_DENOMINATOR`])
    /// so the encoder adjusts against a smaller VBV buffer instead of
    /// swinging widely between infrequent large-buffer refill cycles — "spend
    /// up to the bandwidth budget for the best quality that fits", the mode
    /// `Adaptive`/`vbr` was meant to be but never got tuned to be (see this
    /// type's doc comment). x264enc has no native QVBR-equivalent knob
    /// either, so this variant maps to x264enc's plain untouched `bitrate=`
    /// property (no `pass` override) — its default single-pass rate control
    /// is already the closest analogue x264enc offers to "quality-seeking
    /// within a bitrate ceiling", the same rationale the old
    /// `Adaptive`/`Constant` no-op relied on. The default: the product's "max
    /// out available bandwidth for the best quality that fits" is the
    /// desired out-of-the-box behavior, rather than [`Self::Cqp`]'s
    /// deliberately conservative bandwidth use.
    #[default]
    Qvbr,
}

/// [`RateControlMode::Qvbr`]'s `target-percentage` on `vah264enc`'s
/// `rate-control=vbr` ([`EncoderBackend::launch_fragment`]) — "the percentage
/// for 'target bitrate'/'maximum bitrate'" (element doc), i.e. the real
/// encode target is `bitrate * target-percentage / 100`. Left at the
/// element's own default of 66%, this is exactly the previously-removed
/// `Adaptive` mode's diagnosed pop-in cause (see
/// [`RateControlMode::Qvbr`]'s doc comment). Set high (close to the 50-100
/// range's ceiling) so the real target sits near the configured `bitrate`
/// ceiling rather than swinging down toward two-thirds of it — measured live
/// (see `superdoc/features/stream-control.md`'s 2026-07-30 follow-up entry)
/// to hold real output bitrate stable over changing content, unlike the
/// unset-default 66% baseline.
const VBR_TARGET_PERCENTAGE: u32 = 95;

/// [`RateControlMode::Qvbr`]'s `cpb-size` numerator/denominator on
/// `vah264enc`'s `rate-control=vbr` ([`EncoderBackend::launch_fragment`]):
/// `cpb-size = bitrate_kbps * NUMERATOR / DENOMINATOR` Kb. `cpb-size` is in
/// Kb and `bitrate` is in kbps, so a ratio of 1 is exactly a 1-second VBV
/// buffer; the previous (`qvbr`-era) implementation used exactly that (a
/// naive "1x bitrate" ~1s buffer). This constant deliberately picks something
/// meaningfully *smaller* — 3/8 of a second's worth — on the hypothesis (see
/// this project's task history) that a smaller VBV buffer forces more
/// frequent, finer-grained rate adjustment, smoothing out the bitrate swings
/// that caused the original pop-in complaint, rather than letting bitrate
/// swing dramatically between infrequent large-buffer refill cycles.
/// Measured live (see `superdoc/features/stream-control.md`'s 2026-07-30
/// follow-up entry) to reduce achieved-bitrate variance relative to the
/// untuned (default `cpb-size`, default `target-percentage`) VBR baseline.
const VBR_CPB_SIZE_NUMERATOR: u32 = 3;
const VBR_CPB_SIZE_DENOMINATOR: u32 = 8;

/// Auto-detects which [`EncoderBackend`] to use: prefers [`EncoderBackend::Vaapi`]
/// when a working VAAPI H.264 encoder is actually available on this machine,
/// falling back to [`EncoderBackend::Software`] otherwise. This is what makes
/// hardware encode "just work" without [`start_sharing`]'s callers having to
/// choose — see [`probe_vaapi_encoder`] for what "actually available" checks.
///
/// The probe result is cached for the process's lifetime (VAAPI device
/// availability doesn't change at runtime) so repeated [`start_sharing`]
/// calls don't pay the probe-pipeline cost every time.
///
/// On Windows this naturally always resolves to [`EncoderBackend::Software`]:
/// [`probe_vaapi_encoder`]'s `vah264enc` element simply isn't installed
/// there, so the same "construct the element, fail gracefully" probe below
/// does the right thing with no `#[cfg]` needed. Windows hardware encode via
/// Media Foundation's `mfvideoenc` (the natural Windows analogue of VAAPI
/// here) is a deliberate fast-follow, not an oversight — same staging as
/// VAAPI itself was after software-only worked first on Linux.
/// `pub`: the UI needs this to know which [`VideoCodec`]s
/// [`supported_codecs`] should report for *this* machine's real backend,
/// not just accept a caller-supplied guess.
pub fn detect_encoder_backend() -> EncoderBackend {
    // TEST-ONLY escape hatch: lets `end_to_end_test.rs` force the software
    // backend even on a machine where VAAPI is actually available, so the
    // live repro can be run against both backends. Never consulted outside
    // `#[cfg(test)]` builds, so this changes nothing about production
    // behavior.
    #[cfg(test)]
    if let Ok(forced) = std::env::var("E2E_FORCE_BACKEND") {
        return if forced == "software" {
            EncoderBackend::Software
        } else {
            EncoderBackend::Vaapi
        };
    }

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
///
/// `Why `ensure_gstreamer_initialized()` first:` this can now run before any
/// capture ever starts — the codec selector's `supported_codecs`/
/// `StagedCaptureRow::supported_codecs` call `detect_encoder_backend`
/// (transitively, this fn) as soon as the Stream Control tab loads, not just
/// from [`start_sharing`]. Without this, `gst::ElementFactory::make` below
/// panics ("GStreamer has not been initialized") on that very first call of
/// a session, since nothing else has necessarily called `gst::init()` yet by
/// then. `gst::init()` is idempotent/cheap after the first call, so paying
/// for it here on every `probe_vaapi_encoder` call (itself cached behind
/// [`detect_encoder_backend`]'s `OnceLock`, so this really only runs once
/// per process) is free.
fn probe_vaapi_encoder() -> bool {
    if let Err(e) = ensure_gstreamer_initialized() {
        tracing::info!("GStreamer failed to initialize ({e}); using software encoder");
        return false;
    }
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

/// Default encoder-branch framerate cap, preselected in `ui.rs`'s framerate
/// dropdown. Deliberately conservative (matches this pipeline's previous
/// hardcoded `CAPTURE_FRAMERATE` value) rather than jumping straight to the
/// [`MAX_FRAMERATE`] ceiling: a higher framerate multiplies both the
/// encoder's CPU cost and (per this module's doc comment on
/// `build_pipeline`'s `videorate` cap) how finely the same configured
/// bitrate gets sliced, so a cautious default — especially so soon after the
/// bitrate-starvation bug this same knob was originally added to fix (see
/// this module's doc comment) — is safer than maximizing it by default.
/// Users who want up to [`MAX_FRAMERATE`]'s value (desktop-only per the
/// original design; the browser client can stay lower) can select it
/// explicitly.
pub const DEFAULT_FRAMERATE: u32 = 30;

/// Hard ceiling on the user-selectable capture framerate. The original
/// project design's ceiling was 240 ("desktop clients support up to 240Hz
/// streaming"), later raised to 280 so users can explicitly experiment above
/// that spec target (the framerate dropdown) — this is *not* the value 67b2c85 capped: that fix was about
/// the encoder branch having no framerate lock at all (raw variable,
/// content-driven source rate feeding straight into the encoder's
/// bitrate/frame division), fixed by the `videorate` cap in
/// [`build_pipeline`] that locks the branch to whatever `framerate` was
/// selected — a mechanism that holds at any selected rate, so raising this
/// ceiling doesn't reintroduce that bug. What raising it *does* do: pushes
/// CPU/encoder cost higher and slices the same configured bitrate more
/// finely (see `DEFAULT_FRAMERATE`'s doc comment) with no benefit to viewers
/// today, since MJPEG-over-HTTP delivery bottlenecks around 20-30fps
/// regardless of capture framerate (a separate, known follow-up). The
/// browser client isn't held to this, but this is the native desktop
/// client. [`build_pipeline`] clamps whatever `framerate` it's given into
/// `MIN_FRAMERATE..=MAX_FRAMERATE` as a defensive belt-and-suspenders
/// measure (`ui.rs`'s dropdown already only offers values in range), so a
/// bad/malicious caller can never hand this a `framerate` of 0 (which would
/// make `key-int-max` 0, i.e. "never insert a keyframe past the first") or
/// something absurdly large.
pub const MAX_FRAMERATE: u32 = 280;

/// See [`MAX_FRAMERATE`].
pub const MIN_FRAMERATE: u32 = 1;

/// `pipewiresrc` `min-buffers` for the portal stream. See
/// [`CaptureSource::launch_fragment`]: with `keepalive-time` set, one buffer is
/// always held back, and the portal's default pool of two then halves the
/// delivered rate.
const PORTAL_MIN_BUFFERS: u32 = 6;

/// Frame rate a *static* screen is kept alive at: `pipewiresrc`'s
/// `keepalive-time` is `1000 / min(selected fps, this)` ms (see
/// [`CaptureSource::launch_fragment`] for the why). The keepalive only exists
/// so an idle desktop still produces periodic frames (the encoder converges,
/// the viewer sees the stream is alive); it must stay well below any rate a
/// real source delivers, or its resends compete with real frames.
const STATIC_KEEPALIVE_FPS: u32 = 30;

/// Protocol-spec's 2-second GOP interval (§9: "the next scheduled 2 s
/// keyframe") as this pipeline's *default*, in milliseconds — every
/// destination gets this unless its
/// [`destination::VideoParams::gop_millis`] says otherwise. Milliseconds
/// (not a `f64` seconds count) so [`destination::VideoParams`]/
/// [`branch::EncodeSettings`] can keep deriving `Eq`/`Hash` like every other
/// field on them — `f64` can't.
///
/// `Why a per-destination override exists at all` (2026-07-30): the Gaming
/// preset (the UI's encoder presets) deliberately
/// asks for a shorter GOP for faster recovery
/// from motion artifacts — fast-paced/gaming content rarely holds still long
/// enough for the 2s self-heal ceiling to matter, and the wait for the *next*
/// keyframe is far more noticeable during constant motion. Every other
/// preset keeps this default untouched — the "self-heal ceiling bounding how
/// long a decode glitch can visibly persist" the protocol spec's 2s figure
/// documents is still exactly right for them.
pub const DEFAULT_GOP_MILLIS: u32 = 2000;

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
// Capture source acquisition — platform-specific. Linux/Unix goes through
// the xdg-desktop-portal `ScreenCast` + PipeWire flow (consent dialog
// required); Windows captures directly via GStreamer's
// `d3d11screencapturesrc` (no portal/consent system to go through for a
// native desktop app on Windows). Both converge on [`CaptureSource`], whose
// [`CaptureSource::launch_fragment`] is the only thing [`build_pipeline`]
// needs to know about — see this module's doc comment for the full
// pipeline shape both platforms share downstream of that first element.
// ---------------------------------------------------------------------

/// Everything needed to know how raw video enters the capture pipeline on
/// this platform, plus any resources whose lifetime must span the capture
/// (Linux: the portal session + PipeWire remote fd; Windows: nothing extra —
/// `d3d11screencapturesrc` needs no persistent handle of its own here).
enum CaptureSource {
    #[cfg(unix)]
    Portal(PortalCapture),
    #[cfg(windows)]
    Windows,
    /// TEST-ONLY seam: bypasses the interactive portal/pipewiresrc (Linux)
    /// or `d3d11screencapturesrc` (Windows) acquisition entirely, launching
    /// [`build_pipeline`] directly from a caller-supplied `gst-launch`-syntax
    /// source fragment (e.g. `"videotestsrc is-live=true pattern=smpte"`)
    /// instead. Exists solely so an automated integration test can drive
    /// this module's real tee→encode→appsink→encrypt→fragment→send pipeline
    /// end to end without a human clicking the Wayland portal consent
    /// dialog. Never constructed outside `#[cfg(test)]` code — see
    /// [`start_sharing_from_test_source`].
    #[cfg(any(test, feature = "test-source"))]
    TestSource(String),
}

impl CaptureSource {
    /// The `gst-launch`-syntax fragment for this platform's capture source
    /// element (see this module's doc comment) — the only piece of
    /// [`build_pipeline`]'s description that differs by platform.
    ///
    /// `framerate` is the user-selected target rate ([`build_pipeline`]'s own
    /// `framerate` parameter) — needed here (not just by the `videorate`
    /// stage further downstream) because of `keepalive-time` below: this is
    /// the only place in the pipeline that can plug the *upstream* half of
    /// the "static screen ⇒ frames stop entirely" bug (see
    /// [`build_pipeline`]'s doc comment for the downstream half).
    fn launch_fragment(&self, framerate: u32) -> String {
        match self {
            #[cfg(unix)]
            CaptureSource::Portal(portal) => {
                // `keepalive-time=<ms>` is load-bearing, not cosmetic: a
                // portal `ScreenCast` PipeWire stream is damage-driven by the
                // compositor (Mutter/KWin/wlroots backends) — it only pushes
                // a new buffer when something visually changed on screen. When
                // the desktop is static, `pipewiresrc` pushes *nothing at
                // all*, so the encoder's rate control has no frames to
                // converge quality back down on after a motion burst (the
                // blockiest frame of the burst stays on screen indefinitely),
                // the viewer sees no sign of life, and the destination's
                // silence watchdog would fire. `keepalive-time` (ms, 0 =
                // disabled) makes the element itself resend its last buffer
                // when the compositor delivers nothing new, turning "zero
                // frames during an idle gap of arbitrary length" into "the
                // last frame again on a steady clock".
                //
                // `Why the interval is capped at STATIC_KEEPALIVE_FPS rather
                // than following the selected rate (measured, #39):` the
                // timer fires whenever the *portal* is slower than it. On a
                // 280 Hz output the portal delivers a buffer every 3.6 ms;
                // with `keepalive-time=4` any 4 ms hiccup injects a resend of
                // the previous frame, and those resends are indistinguishable
                // from real frames downstream. They took queue slots and
                // `videorate` slots from real frames ("pipewiresrc 200 fps",
                // of which only 55 were unique). A static screen needs a
                // heartbeat, not the full selected rate, so it is 30 fps (or
                // the selected rate when that is lower). Never 0 for any
                // in-range `framerate`, so it can never disable the keepalive.
                let keepalive_fps = framerate.clamp(MIN_FRAMERATE, MAX_FRAMERATE).min(STATIC_KEEPALIVE_FPS);
                let keepalive_ms = (1000 / keepalive_fps).max(1);
                //
                // `Why min-buffers={PORTAL_MIN_BUFFERS} (measured, #39 — this is half of the "portal
                // delivers exactly half the output's rate" ceiling):`
                // `keepalive-time` makes the element keep its last buffer
                // (it resends it), so that PipeWire buffer is never handed
                // back to the producer. The portal negotiates a pool of only
                // two buffers by default, so with one held back the portal can
                // only fill one while we process the other, i.e. it waits for
                // a release every frame. Measured on `xdg-desktop-portal-wlr`
                // against a 280 Hz output, `client/scripts/portal-probe.py`,
                // DMA-BUF path (cursor mode and keepalive swept):
                //
                // | keepalive-time | min-buffers | fps |
                // | --- | --- | --- |
                // | 0 | default | 279 |
                // | 33 | default | **140** (any cursor mode) |
                // | 33 | 3, 4, 8 | 279 |
                //
                // A few spare buffers cost a few MB of GPU memory and give the
                // full rate back.
                format!(
                    "pipewiresrc fd={} path={} do-timestamp=true keepalive-time={keepalive_ms} min-buffers={PORTAL_MIN_BUFFERS}",
                    portal._pipewire_fd.as_raw_fd(),
                    portal.node_id
                )
            }
            #[cfg(windows)]
            CaptureSource::Windows => {
                // monitor-index=0: single primary-monitor capture, matching
                // the Linux path's current "capture this monitor" scope. A
                // full monitor-picker UI (mirroring a fuller portal source
                // picker) is a future enhancement, not required for this
                // pass. `d3d11screencapturesrc` is part of gst-plugins-bad's
                // D3D11 plugin — GStreamer's own wrapper around DXGI Desktop
                // Duplication, keeping this project's "use GStreamer
                // elements, not hand-rolled platform FFI" principle
                // consistent with how VAAPI hardware encode was already
                // handled via `vah264enc` on Linux.
                //
                // No `keepalive-time`-equivalent property was applied here:
                // unlike `pipewiresrc` (see the Linux arm above),
                // `d3d11screencapturesrc` has no such property to
                // `gst-inspect-1.0` on this (Linux) dev machine, since the
                // plugin isn't installed here — verifying/fixing the same
                // idle-gap behavior on Windows, if DXGI Desktop Duplication's
                // `AcquireNextFrame` exhibits it too, needs a Windows
                // environment and is out of this pass's scope. The
                // downstream `videorate` fix (see [`build_pipeline`]) still
                // applies here regardless, so Windows is no worse off than
                // before.
                let _ = framerate;
                "d3d11screencapturesrc monitor-index=0 do-timestamp=true".to_string()
            }
            #[cfg(any(test, feature = "test-source"))]
            CaptureSource::TestSource(fragment) => {
                let _ = framerate;
                fragment.clone()
            }
        }
    }
}

/// Acquires this platform's [`CaptureSource`]. On Linux/Unix this runs the
/// interactive portal permission flow (see [`open_portal_capture`]); on
/// Windows there's no portal/consent dialog system for a native desktop app
/// to go through, so this captures directly with no user interaction beyond
/// whatever the app's own UI already asked for.
#[cfg(unix)]
async fn acquire_capture_source() -> Result<CaptureSource> {
    Ok(CaptureSource::Portal(open_portal_capture().await?))
}

#[cfg(windows)]
async fn acquire_capture_source() -> Result<CaptureSource> {
    Ok(CaptureSource::Windows)
}

/// Everything obtained from the xdg-desktop-portal `ScreenCast` interface
/// needed to build the capture pipeline.
#[cfg(unix)]
struct PortalCapture {
    node_id: u32,
    /// Where the portal says the picked monitor sits in the compositor layout
    /// (`None` when the backend does not report it). Diagnostic only.
    #[cfg_attr(not(test), allow(dead_code))]
    position: Option<(i32, i32)>,
    /// Kept alive for the lifetime of the capture: `pipewiresrc` dups the fd
    /// internally when it connects to the PipeWire core, so this is not
    /// double-closed by dropping it after the pipeline is torn down — this
    /// matches the pattern in ashpd's own
    /// `examples/screen_cast_gstreamer.rs`.
    _pipewire_fd: OwnedFd,
    session: Session<Screencast>,
}

/// The user dismissed the OS screen picker. Carried as the root cause of the
/// error [`open_portal_capture`] returns, so callers can tell a deliberate
/// cancel from a real failure with [`is_picker_cancelled`] instead of showing
/// the user an error for something they chose to do.
#[derive(Debug)]
pub struct PickerCancelled;

impl std::fmt::Display for PickerCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the screen picker was cancelled")
    }
}

impl std::error::Error for PickerCancelled {}

/// Whether `e` (anywhere in its chain) is a [`PickerCancelled`].
pub fn is_picker_cancelled(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| cause.is::<PickerCancelled>())
}

/// Whether a portal error from the `Start` step means "the user dismissed the
/// picker" rather than "the portal failed".
///
/// Three shapes count as a dismissal:
/// - `Error::Response(ResponseError::Cancelled)`: the standard answer, the
///   backend responded with response code 1.
/// - `PortalError::Cancelled`: `org.freedesktop.portal.Error.Cancelled`.
/// - **`org.freedesktop.DBus.Error.AccessDenied: Invalid session`**: what this
///   machine's `xdg-desktop-portal-wlr-patched` + `wlr-share-picker` actually
///   produce when the picker is dismissed (Esc, the Cancel button, or the
///   picker being SIGTERMed). Measured against the real portal: `Start` itself
///   fails, about a second after the dismissal, with exactly this error. The
///   UI has already torn the session down by then, so "the session is
///   gone" is the portal's only way of saying "nobody is going to be sharing
///   anything".
///
/// Everything else (`Failed`, `NotFound`, `InvalidArgument`, any other D-Bus
/// error, a missing portal) is a real failure and must still surface.
#[cfg(unix)]
fn is_portal_user_cancel(e: &ashpd::Error) -> bool {
    use ashpd::desktop::ResponseError;
    match e {
        ashpd::Error::Response(ResponseError::Cancelled) => true,
        ashpd::Error::Portal(ashpd::PortalError::Cancelled(_)) => true,
        ashpd::Error::Portal(ashpd::PortalError::ZBus(e)) | ashpd::Error::Zbus(e) => is_invalid_session_denial(e),
        _ => false,
    }
}

/// The "session no longer exists" denial described on [`is_portal_user_cancel`],
/// in both the shapes zbus can hand it back as (a raw `MethodError`, or
/// already mapped onto `fdo::Error::AccessDenied`).
#[cfg(unix)]
fn is_invalid_session_denial(e: &ashpd::zbus::Error) -> bool {
    const DENIED: &str = "org.freedesktop.DBus.Error.AccessDenied";
    const INVALID_SESSION: &str = "Invalid session";
    match e {
        ashpd::zbus::Error::MethodError(name, description, _) => {
            name.as_str() == DENIED && description.as_deref() == Some(INVALID_SESSION)
        }
        ashpd::zbus::Error::FDO(fdo) => {
            matches!(&**fdo, ashpd::zbus::fdo::Error::AccessDenied(m) if m == INVALID_SESSION)
        }
        _ => false,
    }
}

/// Runs the portal permission flow: create a session, ask to record a
/// monitor (single-source picker for this pass — a full source picker UI is
/// a later concern per this module's scoping task), start it, and open the
/// PipeWire remote fd.
///
/// Returns a clear `Err` (never panics) if the portal is unavailable or the
/// user denies/cancels the permission dialog.
#[cfg(unix)]
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

    let request = match proxy.start(&session, None, Default::default()).await {
        Ok(request) => request,
        Err(e) => {
            let _ = session.close().await;
            return Err(if is_portal_user_cancel(&e) {
                anyhow::Error::new(PickerCancelled)
            } else {
                anyhow::Error::new(e).context("screencast portal 'Start' request failed")
            });
        }
    };
    let response = match request.response() {
        Ok(response) => response,
        Err(e) => {
            let _ = session.close().await;
            return Err(if is_portal_user_cancel(&e) {
                anyhow::Error::new(PickerCancelled)
            } else {
                anyhow!("screencast permission was denied: {e}")
            });
        }
    };

    let stream = response
        .streams()
        .first()
        .cloned()
        .ok_or_else(|| anyhow!("portal returned no capturable monitor (permission denied or nothing selected)"))?;

    let node_id = stream.pipe_wire_node_id();
    let position = stream.position();
    tracing::info!("portal granted node {node_id} (position {position:?}, size {:?})", stream.size());

    let pipewire_fd = proxy
        .open_pipe_wire_remote(&session, Default::default())
        .await
        .context("failed to open the PipeWire remote fd from the portal")?;

    Ok(PortalCapture {
        node_id,
        position,
        _pipewire_fd: pipewire_fd,
        session,
    })
}

// ---------------------------------------------------------------------
// GStreamer pipeline construction
// ---------------------------------------------------------------------

/// Preview branch is capped to this framerate off the tee (see this
/// module's doc comment) — [`preview::spawn_preview_task`] only ever samples
/// the latest frame every 30s, so anything above "occasionally" here is
/// wasted `videoconvert`/appsink work.
const PREVIEW_BRANCH_FRAMERATE: u32 = 1;

/// `max-size-buffers` for each encode branch's `queue leaky=downstream`
/// right after the tee (see [`branch::add_branch`]'s doc comment for the
/// full "why" — briefly: without a small bounded/leaky config there, a real
/// capture source's incoming raw-frame rate spiking under motion can
/// outpace the encoder and the queue's own defaults, blocking the whole
/// pipeline solid rather than dropping raw frames). A couple of frames'
/// worth of slack (unlike the preview branch's `max-size-buffers=1`, which
/// only ever needs the single newest frame) smooths brief scheduling jitter
/// without ever letting a real backlog accumulate.
const ENCODER_QUEUE_MAX_BUFFERS: u32 = 2;


/// The one raw pixel format the whole capture is pinned to between the
/// source and the `tee` (see [`build_pipeline`]). Every branch converts away
/// from it as it likes; nothing negotiates *back* through it.
///
/// `Why pinning a format here is load-bearing — the "real sharing produces no
/// frames" bug:` without it, the caps `pipewiresrc` negotiates against are
/// whatever the tee's branches happen to prefer at that instant — and since
/// every branch begins with converters (`videoconvert`/`videoscale`/
/// `videorate`), that is an enormous ordered caps list whose *preferred*
/// first entry is some branch's own fixed `format=…,framerate=…`.
/// `pipewiresrc` turns that list, in order, into the SPA format params it
/// offers the producer. Offered a branch-derived preference, a real portal
/// producer intermittently accepts a format it then never delivers a single
/// buffer for: the stream reaches PipeWire's `streaming` state, the
/// GStreamer bus stays completely silent, and nothing ever arrives —
/// measured with `GST_DEBUG=pipewiresrc:6` as `popped buffer (nil)` for the
/// pipeline's entire life. Downstream that surfaces only as every encode
/// branch timing out on its first IDR, i.e. sharing failing with a bare
/// "encode branch produced no IDR within 5s" and no clue as to why.
///
/// Pinning one explicit format immediately after the source makes that offer
/// deterministic and independent of the branch set — which matters far more
/// now that *every* destination is a dynamic branch added after the pipeline
/// is already playing, so the branch set at negotiation time is not even
/// stable. Measured against one real portal capture, shapes interleaved in a
/// single session to cancel out drift: **1/5 runs produced frames without
/// the pin, 5/5 with it**. Two independent shapes that also went 5/5
/// (pinning format *and* `framerate=0/1`, and pinning nothing but attaching
/// no caps-constrained branch at all) confirm the mechanism is the branch
/// caps reaching the source, not the specific format chosen.
///
/// `Why `BGRx` specifically:` it is what both real sources already produce
/// (the portal negotiated `BGRA`; Windows' `d3d11screencapturesrc` is also
/// BGRA), so the pin costs an alpha-drop rather than a real colour-space
/// conversion, and it stays a packed 4-byte format so the buffer sizes on
/// the tee are unchanged.
const SOURCE_RAW_FORMAT: &str = "BGRx";

/// Where the raw frames on the capture's `tee` live, and so which elements the
/// branches hanging off it are made of.
///
/// `Why two domains (the #39 capture/encode rework):` the original pipeline
/// was `pipewiresrc ! videoconvert ! BGRx ! tee` with a CPU `videoconvert`
/// BGRx to NV12 in every encode branch. That shape caps real 1080p screen
/// sharing well under 240 fps for three measured reasons, all removed by
/// [`Self::Gpu`]:
///
/// 1. `pipewiresrc ! videoconvert` only offers system-memory caps, so the
///    portal (`xdg-desktop-portal-wlr`) hands out **shared-memory** buffers.
///    The compositor then has to read the whole 8 MB frame back into RAM, which
///    takes longer than one vblank on a 280 Hz output, so every second
///    `copy_with_damage` is missed: the portal delivered **140 fps from a
///    280 Hz output**, exactly half, in four runs. Offering DMA-BUF (any GPU
///    element downstream does) makes the very same portal deliver **280 fps**
///    (`client/scripts/portal-probe.py`, tail starting with `vapostproc`,
///    measured).
/// 2. CPU `videoconvert` BGRx to NV12 costs ~4.7 ms per 1080p frame (a ~210
///    fps ceiling) on the thread that also runs the encoder.
/// 3. Rotating needed a second GPU round trip (see [`orientation_fragment`]).
///
/// [`Self::Gpu`] is `pipewiresrc (DMA-BUF) ! vapostproc video-direction=auto !
/// video/x-raw(memory:VAMemory),format=NV12 ! tee`: one GPU pass does colour
/// conversion and orientation, the PipeWire buffer is released immediately
/// (so the portal's small buffer pool never starves behind slow branches), and
/// the VA encoders take the VAMemory frames with no download at all.
/// [`Self::System`] is the original shape and stays the fallback for machines
/// without a working VA stack (software `x264enc`) and for Windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureDomain {
    /// System-memory BGRx on the tee; branches convert on the CPU.
    System,
    /// VAMemory NV12 on the tee; branches use `vapostproc`.
    Gpu,
}

/// Name of the source-side `vapostproc` of a [`CaptureDomain::Gpu`] pipeline.
/// Its presence is how [`branch::add_branch`] tells which domain the `tee`
/// carries.
const GPU_STAGE_NAME: &str = "gpu_in";

/// Caps of the `tee` in [`CaptureDomain::Gpu`].
const GPU_TEE_CAPS: &str = "video/x-raw(memory:VAMemory),format=NV12";

impl CaptureDomain {
    /// The domain a running pipeline was built in.
    fn of(pipeline: &gst::Pipeline) -> Self {
        if pipeline.by_name(GPU_STAGE_NAME).is_some() { Self::Gpu } else { Self::System }
    }

    /// The domain to capture `source` in. [`Self::Gpu`] needs a real portal
    /// source (the only one that can hand out DMA-BUF), a working VA encoder
    /// stack and `vapostproc`; `SSS_CAPTURE_PATH=system` forces the CPU path
    /// for A/B measurements.
    fn for_source(source: &CaptureSource) -> Self {
        #[cfg(unix)]
        if matches!(source, CaptureSource::Portal(_)) && gpu_capture_available() {
            return Self::Gpu;
        }
        let _ = source;
        Self::System
    }
}

/// Whether [`CaptureDomain::Gpu`] can be used on this machine (cached).
#[cfg_attr(not(unix), allow(dead_code))]
fn gpu_capture_available() -> bool {
    use std::sync::OnceLock;
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        if std::env::var("SSS_CAPTURE_PATH").is_ok_and(|v| v == "system") {
            return false;
        }
        let _ = ensure_gstreamer_initialized();
        detect_encoder_backend() == EncoderBackend::Vaapi && gst::ElementFactory::find("vapostproc").is_some()
    })
}

/// Orientation stage, implementation 1: VA-API post-processing. It rotates and
/// flips on the GPU, so it is the one to use whenever VA works.
const ORIENTATION_VAPOSTPROC: &str = "vapostproc video-direction=auto";

/// Orientation stage, implementation 2: the CPU fallback for machines without
/// a working VA stack.
const ORIENTATION_VIDEOFLIP: &str = "videoflip video-direction=auto";

/// Names of the two capsfilters the orientation stage is spliced between
/// ([`arm_orientation_stage`]).
const ORIENT_IN_NAME: &str = "orient_in";
const ORIENT_OUT_NAME: &str = "orient_out";

/// Which element does the rotating when the source turns out to be rotated:
/// the orientation stage that sits between the source and the `tee`, so every
/// destination branch and both preview branches see an upright picture.
///
/// `Why this exists:` a compositor that drives a monitor rotated (e.g.
/// MangoWC's `rr:2` = 180 degrees) hands the portal/PipeWire stream the
/// *physical* scan-out orientation and only tells us about it as stream
/// metadata (`SPA_META_VideoTransform`). Nothing in the old pipeline looked at
/// it, so a rotated monitor arrived upside down (or sideways) in every viewer.
///
/// `pipewiresrc` already reads that metadata and, whenever it changes, sends a
/// `GST_TAG_IMAGE_ORIENTATION` tag downstream (`rotate-0/90/180/270` and the
/// four `flip-rotate-*`, one for each of the eight `wl_output` transforms).
/// Both elements understand `video-direction=auto`, which follows that tag,
/// so this crate has no transform table of its own to get wrong (it would
/// need the CW/CCW and flip-then-rotate conventions to match PipeWire's;
/// `capture::tests` pins what each of the eight tags does). For 90/270 the
/// element also swaps width and height, so everything downstream just sees
/// the upright size.
///
/// `Why vapostproc first, videoflip second:` measured on 1080p BGRx
/// (`gst-launch videotestsrc ! ... ! <element> ! fakesink sync=false`, 4000
/// frames, one core): `videoflip` rotate-180 tops out at about 180 fps, i.e.
/// it cannot keep a 240 fps source busy, and 90/270 is slower still (about
/// 110 fps). `vapostproc` (copy up, rotate on the GPU, copy down) does about
/// 850 fps for 180 and about 270 for 90. It is therefore preferred whenever
/// the VA encoder probe passed ([`detect_encoder_backend`]) and the element
/// exists; `videoflip` is the fallback for machines (and Windows) without VA.
///
/// `Why the stage is spliced in lazily instead of sitting in the pipeline:`
/// the requirement is that an un-rotated output pays nothing. `vapostproc` is
/// close to free in pass-through (2.86 s vs 2.66 s for 4000 frames), but
/// `videoflip` is *not*: even untagged or with `video-direction=identity` it
/// costs about 0.8 ms per 1080p frame (5.9 s vs 2.7 s for 4000 frames), and
/// on Windows no tag ever arrives. So the pipeline is built without the
/// stage, and [`arm_orientation_stage`] inserts it the moment a non-identity
/// orientation tag passes.
///
/// `Why two capsfilters around the stage:` `vapostproc` is otherwise free to
/// pick another output format (it likes NV12), which would silently un-pin
/// [`SOURCE_RAW_FORMAT`]. The second one also gives the stage a fixed output
/// format to negotiate against; its size is kept from being rescaled by the
/// tee's caps-query guard (see [`isolate_source_from_branch_renegotiation`]).
fn orientation_fragment() -> &'static str {
    use std::sync::OnceLock;
    static FRAGMENT: OnceLock<&'static str> = OnceLock::new();
    FRAGMENT.get_or_init(|| {
        let _ = ensure_gstreamer_initialized();
        let va_ok = detect_encoder_backend() == EncoderBackend::Vaapi
            && gst::ElementFactory::find("vapostproc").is_some();
        if va_ok { ORIENTATION_VAPOSTPROC } else { ORIENTATION_VIDEOFLIP }
    })
}

/// Splices `stage` (a `gst-launch` fragment, see [`orientation_fragment`])
/// between the `orient_in` and `orient_out` capsfilters the first time an
/// `image-orientation` tag other than `rotate-0` passes between them, then
/// removes its own probe. Until then those two capsfilters are linked
/// directly (a capsfilter in pass-through costs nothing).
///
/// The splice happens inside a blocking probe on `orient_in`'s src pad, on
/// the source's streaming thread, before the first buffer that follows the
/// tag. The new element's sink pad then receives the pad's sticky events
/// (caps, segment, and the tag itself) as for any newly linked pad, so
/// `video-direction=auto` sees the tag. If building or linking fails the
/// stream keeps flowing un-rotated rather than dying, and the failure is
/// logged.
fn arm_orientation_stage(pipeline: &gst::Pipeline, stage: &str) -> Result<()> {
    let before = pipeline.by_name(ORIENT_IN_NAME).ok_or_else(|| anyhow!("no element '{ORIENT_IN_NAME}'"))?;
    let after = pipeline.by_name(ORIENT_OUT_NAME).ok_or_else(|| anyhow!("no element '{ORIENT_OUT_NAME}'"))?;
    let src = before.static_pad("src").ok_or_else(|| anyhow!("'{ORIENT_IN_NAME}' has no src pad"))?;
    let pipeline = pipeline.downgrade();
    let stage = stage.to_string();

    src.add_probe(gst::PadProbeType::BLOCK | gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
        let Some(gst::PadProbeData::Event(event)) = &info.data else { return gst::PadProbeReturn::Pass };
        let gst::EventView::Tag(tag) = event.view() else { return gst::PadProbeReturn::Pass };
        let Some(orientation) = tag.tag().get::<gst::tags::ImageOrientation>() else {
            return gst::PadProbeReturn::Pass;
        };
        if orientation.get() == "rotate-0" {
            return gst::PadProbeReturn::Pass;
        }
        tracing::info!("source is rotated ({}); inserting the orientation stage '{stage}'", orientation.get());
        let result = (|| -> Result<()> {
            let pipeline = pipeline.upgrade().ok_or_else(|| anyhow!("pipeline is gone"))?;
            let bin = gst::parse::bin_from_description(&stage, true)?;
            let peer = after.static_pad("sink").ok_or_else(|| anyhow!("no sink pad on '{ORIENT_OUT_NAME}'"))?;
            pad.unlink(&peer)?;
            pipeline.add(&bin)?;
            bin.sync_state_with_parent()?;
            let sink = bin.static_pad("sink").ok_or_else(|| anyhow!("stage has no sink pad"))?;
            let out = bin.static_pad("src").ok_or_else(|| anyhow!("stage has no src pad"))?;
            pad.link(&sink)?;
            out.link(&peer)?;
            Ok(())
        })();
        if let Err(e) = result {
            tracing::error!("could not insert the orientation stage, the picture stays as the source sends it: {e:#}");
            // Best effort: make sure the direct link is back.
            if let Some(peer) = after.static_pad("sink")
                && !pad.is_linked()
            {
                let _ = pad.link(&peer);
            }
        }
        gst::PadProbeReturn::Remove
    })
    .ok_or_else(|| anyhow!("failed to arm the orientation stage"))?;
    Ok(())
}

/// Element name of the `tee` every branch — fixed (preview) and
/// dynamic (one per destination, see [`branch`]) — hangs off.
const TEE_NAME: &str = "t";

/// Everything [`build_pipeline`] hands back: the pipeline itself plus the
/// elements a running capture keeps talking to.
struct CapturePipeline {
    pipeline: gst::Pipeline,
    /// Where every encode branch is added ([`branch::add_branch`]).
    tee: gst::Element,
    /// ~1fps RGB branch feeding [`preview::spawn_preview_task`].
    preview_sink: gst_app::AppSink,
    /// Flipped by the first buffer to reach the `tee`; see
    /// [`CaptureHandle::awaiting_first_frame`].
    frame_seen: Arc<std::sync::atomic::AtomicBool>,
}

/// Builds and links (but does not start) the capture pipeline described in
/// this module's doc comment: source → `videoconvert` → `tee`, plus the one
/// *fixed* preview branch. It builds **no encoder at all** — every encoder
/// in this pipeline belongs to a destination and is added dynamically by
/// [`branch::add_branch`].
///
/// `Why no fixed encoder branch:` a capture can feed N rooms, each with its
/// own resolution/framerate/bitrate/encoder-backend, its own room key, and
/// its own `stream_id`. A statically-built branch could only ever serve one
/// of them, so a second, structurally different code path would be needed
/// for every other destination — and every operation the plan doc defines
/// (per-destination settings changes, transfer, fan-out, per-branch VAAPI
/// fallback) would then have to be implemented twice, with the fixed branch's
/// version unable to use the branch-swap primitive at all (you cannot swap
/// out an element `gst::parse::launch` welded into the pipeline). With every
/// destination dynamic, there is exactly one path, and the *first*
/// destination is not special in any way.
///
/// # Why there is no rate conversion *before* the `tee`
///
/// `Why:` the obvious-looking shape — cap the shared source to the selected
/// framerate once, before the tee, so every branch inherits it — **breaks
/// real screen capture outright**, while looking perfectly healthy against
/// `videotestsrc`. Measured directly against a live portal/`pipewiresrc`
/// source (this is what `live_portal_capture_feeds_a_destination_branch`
/// guards):
///
/// | shape | raw rate arriving at the tee |
/// | --- | --- |
/// | `pipewiresrc ! videoconvert ! tee` | 29.5 fps |
/// | `pipewiresrc ! videoconvert ! videorate ! capsfilter(30/1) ! tee` | **0.0 fps** |
///
/// Nothing errors on the broken shape: the pipeline reaches `Playing`, the
/// bus stays quiet, and not one buffer ever reaches the tee — so every
/// destination branch times out waiting for its first IDR and sharing fails
/// with a bare "encode branch produced no IDR within 5s". A portal source
/// negotiates `framerate=0/1` (variable, damage-driven) plus a `max-framerate`
/// hint, and a `videorate` asked to turn that into a fixed rate *on the
/// source's own streaming thread, upstream of any queue* stalls instead of
/// converting. The identical conversion works fine **downstream of the tee's
/// per-branch `queue`**, which is where it has always lived
/// ([`branch::add_branch`]) and where it stays: two branches off one real
/// portal capture were measured holding 31.5 fps and 5.5 fps simultaneously.
///
/// So the tee carries whatever the source natively produces, and **rate is a
/// per-destination concern**, converted inside each branch. The selected
/// framerate still reaches the source itself — as `keepalive-time` on the
/// Linux fragment (see [`CaptureSource::launch_fragment`]), which is what
/// guarantees a floor rate while the desktop is completely static.
///
/// `source_fragment` is [`CaptureSource::launch_fragment`]'s output — the
/// only platform-specific piece of the description below. There is
/// deliberately no `framerate` parameter: the selected rate reaches the
/// source through `source_fragment`'s own `keepalive-time` (see
/// [`CaptureSource::launch_fragment`]) and each destination through its own
/// branch's `videorate` ([`branch::add_branch`]), so nothing in *this*
/// description depends on it.
#[cfg(test)]
fn build_pipeline(source_fragment: &str) -> Result<CapturePipeline> {
    build_pipeline_in(source_fragment, CaptureDomain::System)
}

/// [`build_pipeline`] in an explicit [`CaptureDomain`].
fn build_pipeline_in(source_fragment: &str, domain: CaptureDomain) -> Result<CapturePipeline> {
    match domain {
        CaptureDomain::System => build_pipeline_with_orientation(source_fragment, orientation_fragment()),
        CaptureDomain::Gpu => build_pipeline_from_description(&gpu_pipeline_description(source_fragment), domain, ""),
    }
}

/// Name of the queue [`decoupled_source`] puts right behind the portal source.
const PORTAL_QUEUE_NAME: &str = "portal_queue";

/// `source_fragment` with a small `queue` right behind it when it is the
/// portal's `pipewiresrc`; any other source is returned unchanged.
///
/// `Why (task #52, stock xdg-desktop-portal-wlr 0.8.4 on sway 1.12):` the queue
/// is what stops the capture start from flapping the PipeWire stream. In
/// `pipewiresrc`'s `READY -> PAUSED` change the stream first goes `streaming`
/// (a stream starts active, that is how it gets linked and negotiated), and
/// only once the element's `start()` is done does it call
/// `pw_stream_set_active(false)` to hold the stream until `PLAYING`, which
/// then re-activates it. `start()` runs on the source's streaming thread
/// *after* it has pushed the negotiated caps downstream, and a caps event is
/// handled synchronously by every element behind the source. Behind a heavy
/// chain (here `vapostproc`, the tee and the preview branch's own `vapostproc`
/// and `videoconvert`: ~1.3 ms, `GST_DEBUG=pipewiresrc:6`) the portal has been
/// `streaming` that long when the pause arrives, i.e. it has already asked the
/// compositor for its first frame, and the pause + resume 100 us later makes
/// stock xdpw (ext-image-copy-capture backend, which creates a frame object on
/// every `streaming` without checking for an outstanding one) ask for a second
/// frame: `ext_image_copy_capture_session_v1#N: error 1: session already has a
/// frame object`, xdpw exits, the node is gone and the share fails.
/// A queue accepts the caps event and hands it to its own thread, so `start()`
/// follows the `streaming` state by ~100 us, the pause + resume then land
/// before the portal has requested anything and xdpw never sees the flap.
/// Measured with `portal-probe.py` and this exact tail against stock xdpw 0.8.4:
/// without the queue the stream flaps and xdpw dies, 4/4 runs with it do not
/// flap and deliver 239 fps. Any element order that makes the caps handling
/// behind the source cheap would do; a queue is the one that does not depend
/// on which elements follow.
///
/// Costs one thread hop per frame. 2 buffers is enough: the portal pool is 16
/// ([`PORTAL_MIN_BUFFERS`] only raises the floor) and a full queue just blocks
/// the source like any slow consumer would.
fn decoupled_source(source_fragment: &str) -> String {
    if source_fragment.starts_with("pipewiresrc") {
        format!("{source_fragment} ! queue name={PORTAL_QUEUE_NAME} max-size-buffers=2 max-size-bytes=0 max-size-time=0")
    } else {
        source_fragment.to_string()
    }
}

/// [`build_pipeline`] with the orientation stage ([`arm_orientation_stage`])
/// chosen by the caller, so the tests can run both implementations
/// ([`ORIENTATION_VAPOSTPROC`] and [`ORIENTATION_VIDEOFLIP`]) on any machine.
/// System domain only: the GPU domain's `vapostproc` is the orientation stage.
fn build_pipeline_with_orientation(source_fragment: &str, orientation: &str) -> Result<CapturePipeline> {
    let source_fragment = decoupled_source(source_fragment);
    let description = format!(
        "{source_fragment} \
         ! videoconvert \
         ! capsfilter name={ORIENT_IN_NAME} caps=video/x-raw,format={SOURCE_RAW_FORMAT} \
         ! capsfilter name={ORIENT_OUT_NAME} caps=video/x-raw,format={SOURCE_RAW_FORMAT} \
         ! tee name={TEE_NAME} \
         {TEE_NAME}. ! queue leaky=downstream max-size-buffers=1 \
            ! videorate drop-only=true \
            ! video/x-raw,framerate={PREVIEW_BRANCH_FRAMERATE}/1 \
            ! videoconvert \
            ! video/x-raw,format=RGB \
            ! appsink name=preview_sink emit-signals=false sync=false max-buffers=1 drop=true"
    );
    build_pipeline_from_description(&description, CaptureDomain::System, orientation)
}

/// The [`CaptureDomain::Gpu`] description: see [`CaptureDomain`] for why it is
/// shaped like this. The preview branch downloads from the GPU itself
/// (`vapostproc` to a system-memory BGRx, at 1 fps).
fn gpu_pipeline_description(source_fragment: &str) -> String {
    let source_fragment = decoupled_source(source_fragment);
    format!(
        "{source_fragment} \
         ! vapostproc name={GPU_STAGE_NAME} video-direction=auto \
         ! capsfilter caps={GPU_TEE_CAPS} \
         ! tee name={TEE_NAME} \
         {TEE_NAME}. ! queue leaky=downstream max-size-buffers=1 \
            ! videorate drop-only=true \
            ! vapostproc \
            ! video/x-raw,format={SOURCE_RAW_FORMAT},framerate={PREVIEW_BRANCH_FRAMERATE}/1 \
            ! videoconvert \
            ! video/x-raw,format=RGB \
            ! appsink name=preview_sink emit-signals=false sync=false max-buffers=1 drop=true"
    )
}

fn build_pipeline_from_description(description: &str, domain: CaptureDomain, orientation: &str) -> Result<CapturePipeline> {
    let element = gst::parse::launch(description)
        .with_context(|| format!("failed to construct capture pipeline: {description}"))?;
    let pipeline = element
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("gst::parse::launch did not return a top-level Pipeline"))?;

    let preview_sink = pipeline
        .by_name("preview_sink")
        .ok_or_else(|| anyhow!("pipeline has no element named 'preview_sink'"))?
        .downcast::<gst_app::AppSink>()
        .map_err(|_| anyhow!("'preview_sink' element is not an AppSink"))?;

    let tee = pipeline
        .by_name(TEE_NAME)
        .ok_or_else(|| anyhow!("pipeline has no element named '{TEE_NAME}'"))?;

    isolate_source_from_branch_renegotiation(&tee, domain)?;
    if domain == CaptureDomain::System {
        arm_orientation_stage(&pipeline, orientation)?;
    }

    // Installed before the pipeline ever leaves `Null`, so the very first
    // buffer cannot slip past it. Removes itself after firing: no per-buffer
    // cost for the life of the capture.
    let frame_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
    if let Some(sink_pad) = tee.static_pad("sink") {
        let seen = frame_seen.clone();
        sink_pad.add_probe(gst::PadProbeType::BUFFER, move |_pad, _info| {
            seen.store(true, Ordering::Release);
            gst::PadProbeReturn::Remove
        });
    }

    Ok(CapturePipeline { pipeline, tee, preview_sink, frame_seen })
}

/// Stops a branch being added to the live `tee` from asking everything
/// *upstream* of the tee to renegotiate, by dropping `RECONFIGURE` events at
/// the tee's sink pad.
///
/// # Why this is the difference between real screen sharing working and not
///
/// `Why:` linking a new branch onto a `tee` that is already `Playing` makes
/// GStreamer send one `GST_EVENT_RECONFIGURE` upstream, which is how a newly
/// added consumer normally asks the producer to renegotiate for it. Against
/// a real portal capture, `pipewiresrc` acts on it by renegotiating its
/// PipeWire stream with the compositor's producer — **and the producer never
/// sends another buffer**. Not an error, not an EOS, nothing on the bus: the
/// stream sits in PipeWire's `streaming` state forever with no data.
///
/// Measured directly against one live portal capture, sampling the raw frame
/// rate off the tee every 1.5s:
///
/// | | frames off the tee |
/// | --- | --- |
/// | before adding a branch | 44.7, 29.3, 65.3, 32.7, 28.7 fps |
/// | after `add_branch`, reconfigure allowed upstream | 0.7, 0.0, 0.0, 0.0, 0.0 fps |
/// | after `add_branch`, reconfigure dropped here | 28.7, 28.7, 28.7, 28.7, 28.7 fps |
///
/// This is precisely the regression the all-dynamic-branch restructuring
/// introduced, and precisely why the whole unit suite stayed green while real
/// sharing was broken. Before it, the encode branch was *fixed* — built into
/// the launch description, linked before the pipeline ever left `Null` — so
/// no pad was ever added to a live tee and no reconfigure was ever sent.
/// Now every destination is dynamic, so the **first** destination of every
/// share kills the very source it is trying to encode, and the only symptom
/// downstream is `add_branch_live` timing out on its first IDR. A
/// `videotestsrc` renegotiates instantly and silently, which is why no test
/// could see it (see
/// `capture::tests::live_portal_capture_feeds_a_destination_branch`).
///
/// `Why dropping the event is correct rather than a workaround:` a
/// reconfigure asks upstream to produce something different. Upstream of
/// this tee there is nothing to ask for — the chain is
/// `<source> ! videoconvert ! video/x-raw,format=BGRx` (see
/// [`SOURCE_RAW_FORMAT`]), whose output format is pinned by us and whose
/// frame size is dictated by the compositor, not negotiable by us. And no
/// branch needs it to change: every branch begins with its own
/// `videoconvert`/`videoscale`/`videorate` and converts from whatever the
/// tee carries to whatever that destination wants. So the event has no
/// legitimate work to do here, and forwarding it only exposes the source to
/// a renegotiation it cannot survive.
fn isolate_source_from_branch_renegotiation(tee: &gst::Element, domain: CaptureDomain) -> Result<()> {
    let sink = tee
        .static_pad("sink")
        .ok_or_else(|| anyhow!("tee '{TEE_NAME}' has no sink pad to guard against renegotiation"))?;

    sink.add_probe(gst::PadProbeType::EVENT_UPSTREAM, |_, info| {
        if let Some(gst::PadProbeData::Event(event)) = &info.data
            && event.type_() == gst::EventType::Reconfigure
        {
            tracing::debug!("dropping an upstream RECONFIGURE at the capture tee (see build_pipeline)");
            return gst::PadProbeReturn::Drop;
        }
        gst::PadProbeReturn::Ok
    })
    .ok_or_else(|| anyhow!("failed to install the renegotiation guard on tee '{TEE_NAME}'"))?;

    // Same idea for the CAPS *query*: the tee answers it with the union of its
    // branches' preferences, and any one of them may be a fixed size (a
    // destination's encode size, the room-preview branch's vapostproc). A
    // source-side element that can scale (`vapostproc`, see
    // [`orientation_fragment`]) takes such an answer as an instruction and
    // shrinks the whole capture to that size before it reaches any other
    // branch. This is not hypothetical: the old 320x180 self-preview branch
    // (now removed) leaked its size upstream exactly like this. `videoconvert`/
    // `videoflip` never scale, which is why nothing needed this before.
    // Answering "anything in the pinned format" keeps every branch's wishes
    // where they belong, inside the branch. Keep it even though that branch
    // is gone: both capture domains still rely on it.
    sink.add_probe(gst::PadProbeType::QUERY_DOWNSTREAM, move |_, info| {
        if let Some(gst::PadProbeData::Query(query)) = &mut info.data
            && let gst::QueryViewMut::Caps(q) = query.view_mut()
        {
            let any_size = match domain {
                CaptureDomain::System => gst::Caps::builder("video/x-raw").field("format", SOURCE_RAW_FORMAT).build(),
                CaptureDomain::Gpu => gst::Caps::builder("video/x-raw")
                    .features(["memory:VAMemory"])
                    .field("format", "NV12")
                    .build(),
            };
            let answer = match q.filter() {
                Some(filter) => filter.intersect_with_mode(&any_size, gst::CapsIntersectMode::First),
                None => any_size,
            };
            q.set_result(&answer);
            return gst::PadProbeReturn::Handled;
        }
        gst::PadProbeReturn::Ok
    })
    .ok_or_else(|| anyhow!("failed to install the caps-query guard on tee '{TEE_NAME}'"))?;

    Ok(())
}

/// Builds the capture pipeline and drives it to `Playing`, waiting for the
/// state change to actually settle (success or error) before returning.
/// Production code uses [`start_pipeline_bounded`] (same steps, bounded and
/// bus-aware); this synchronous form is kept for the tests.
///
/// On any failure (construction or state-change), tears the pipeline back
/// down to `Null` before returning `Err`, so a failed attempt never leaks a
/// half-started pipeline.
///
/// No encoder-backend fallback here any more, unlike the version that built
/// a fixed encoder branch: this pipeline contains no encoder at all, so
/// there is nothing VAAPI-specific that can fail at this stage. That
/// fallback now lives per destination, where it belongs — see
/// [`branch::add_branch_with_fallback`]/[`branch::add_branch_live`].
///
/// `Why this is a plain, synchronous fn, never called directly from async
/// code:` [`Element::state`]'s blocking wait below takes a `ClockTime`
/// timeout argument, but that timeout is **not reliable** under real-world
/// contention — measured directly while diagnosing the "repeated Stop→Share
/// restart cycles permanently wedge the backend" bug (history: the
/// Electron-era frontend notes, since removed from the docs): starting a new capture
/// immediately after the previous one's portal session/pipeline finished
/// tearing down can leave `pipewiresrc`'s state change stuck deep inside
/// GStreamer/PipeWire's own synchronization for far longer than the 5s
/// argument here promises — observed anywhere from several seconds to
/// indefinitely, in the same process, across otherwise-identical repeated
/// attempts. Every caller of [`start_pipeline`] therefore MUST run it via
/// `tokio::task::spawn_blocking` (see [`start_sharing_with_source`]) rather
/// than calling it inline from an `async fn` — a call that can genuinely
/// never return must never run on one of the Tokio runtime's own (small,
/// fixed-size) worker threads, or enough repeated stalls permanently starve
/// every other task sharing that pool, including `driver.rs`'s poll loop and
/// every other task that `block_on`s into the runtime — which is
/// exactly the "every subsequent command silently never resolves" symptom
/// reported. Moving the block onto `spawn_blocking`'s separate, elastic
/// thread pool doesn't fix GStreamer's own timeout unreliability, but it
/// confines the damage to one throwaway blocking-pool thread instead of
/// starving the whole app.
#[cfg(test)]
fn start_pipeline(source_fragment: &str, domain: CaptureDomain) -> Result<CapturePipeline> {
    let built = build_pipeline_in(source_fragment, domain)?;
    play_and_wait(&built.pipeline)?;
    Ok(built)
}

/// The blocking half of [`start_pipeline`]: `Playing`, then wait for the state
/// change to settle. On failure the pipeline is back at `Null` and the error
/// carries the text of the bus error that explains it (see [`start_failure`]).
fn play_and_wait(pipeline: &gst::Pipeline) -> Result<()> {
    if let Err(e) = pipeline.set_state(gst::State::Playing) {
        let detail = take_bus_error(pipeline);
        let _ = pipeline.set_state(gst::State::Null);
        return Err(start_failure(format!("failed to start capture pipeline: {e}"), detail));
    }

    // Wait for the pipeline to actually get moving (or fail) before telling
    // the room this stream exists — announcing a stream that then
    // immediately errors out would be a confusing "stream with no frames"
    // for other members. `state()` blocks until the async state change
    // settles (success or error) or the timeout elapses — see
    // [`start_pipeline`]'s `Why:` note for why that promise isn't trusted by
    // any caller.
    let (state_result, _current, _pending) = pipeline.state(gst::ClockTime::from_seconds(5));
    if let Err(e) = state_result {
        let detail = take_bus_error(pipeline);
        let _ = pipeline.set_state(gst::State::Null);
        return Err(start_failure(format!("capture pipeline failed to reach PLAYING: {e:?}"), detail));
    }
    Ok(())
}

/// How long one capture start attempt may take before it is given up. A
/// healthy start settles in 100-300 ms (3 s measured under a 36-thread CPU
/// stress); a dead portal node leaves `pipewiresrc`'s state change blocked for
/// 30 s (measured, killed xdpw, `Element failed to change its state`).
const START_TIMEOUT: Duration = Duration::from_secs(8);

/// The same for the CPU fallback attempt after a GPU start failed. Shorter: if
/// the GPU attempt died because the portal did (xdpw's flap crash reports as
/// `not-negotiated` without a `target not found`), this attempt can only run
/// into the timeout, and the user is waiting for it.
const FALLBACK_START_TIMEOUT: Duration = Duration::from_secs(5);

/// Why a capture start failed in a way a second attempt on the same portal
/// node cannot fix. Carried as the root cause of the error, see
/// [`start_is_hopeless`].
#[derive(Debug)]
struct StartFailed(String);

impl std::fmt::Display for StartFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StartFailed {}

/// Whether `e` is a [`StartFailed`]: the portal's stream is gone or never
/// answered, so the CPU fallback must not reconnect to the same node.
fn start_is_hopeless(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| cause.is::<StartFailed>())
}

/// Text `pipewiresrc` reports (`on_state_changed`, `stream error: ...`) when
/// the portal node is gone: the xdpw process died or the stream was closed.
const SOURCE_GONE_TEXT: &str = "target not found";

/// `what` + the bus error `detail`, as a [`StartFailed`] when the detail says
/// the portal node is gone and as a plain error otherwise.
fn start_failure(what: String, detail: Option<String>) -> anyhow::Error {
    match detail {
        Some(d) if d.contains(SOURCE_GONE_TEXT) => anyhow::Error::new(StartFailed(format!(
            "the screen-capture portal's stream is gone ({d}); the portal backend most likely exited"
        ))),
        Some(d) => anyhow!("{what}: {d}"),
        None => anyhow!("{what}"),
    }
}

/// The first error message on `pipeline`'s bus as `element: error (debug)`,
/// removing it. Other message types stay queued for the capture-level watcher.
///
/// Waits up to 300 ms for it: a failed state change returns before
/// `pipewiresrc`'s own error message has reached the bus (measured, #59:
/// `StateChangeError` with an empty bus, the `target not found` arrived
/// afterwards), and without the reason a dead portal looks like a GPU failure.
fn take_bus_error(pipeline: &gst::Pipeline) -> Option<String> {
    let msg = pipeline.bus()?.timed_pop_filtered(gst::ClockTime::from_mseconds(300), &[gst::MessageType::Error])?;
    Some(describe_bus_error(&msg))
}

fn describe_bus_error(msg: &gst::Message) -> String {
    match msg.view() {
        gst::MessageView::Error(err) => {
            let src = err.src().map(|s| s.path_string().to_string()).unwrap_or_else(|| "?".to_string());
            format!("{src}: {}{}", err.error(), err.debug().map(|d| format!(" ({d})")).unwrap_or_default())
        }
        _ => String::new(),
    }
}

/// [`start_pipeline`] for async callers, bounded by [`START_TIMEOUT`] and
/// ended early by the first bus error.
///
/// `Why (task #59):` the plain blocking start can sit in `pipewiresrc`'s state
/// change for 30 s when the portal's node dies (xdpw crash: the stock-xdpw
/// start-up flap, see [`decoupled_source`]), and then the CPU fallback sat in
/// another 30 s on the same dead node. The UI stayed on "Waiting for the
/// screen picker" for that long and the log said nothing. The bus carries the
/// real reason (`stream error: target not found`) almost at once, so the bus
/// is polled while the state change runs on a blocking thread; the first
/// error, or the timeout, ends the attempt. The blocked thread is not
/// abandoned silently: a task waits for it and then sets the pipeline to
/// `Null`, because the state change holds the pipeline's state lock and a
/// `set_state(Null)` from here would block behind it.
async fn start_pipeline_bounded(source_fragment: &str, domain: CaptureDomain, timeout: Duration) -> Result<CapturePipeline> {
    let fragment = source_fragment.to_string();
    let built = tokio::task::spawn_blocking(move || build_pipeline_in(&fragment, domain))
        .await
        .map_err(|e| anyhow!("capture pipeline build task panicked: {e}"))??;
    let pipeline = built.pipeline.clone();
    let bus = pipeline.bus().ok_or_else(|| anyhow!("capture pipeline has no bus"))?;
    let mut play = tokio::task::spawn_blocking({
        let pipeline = pipeline.clone();
        move || play_and_wait(&pipeline)
    });
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    let failure = loop {
        tokio::select! {
            result = &mut play => {
                return match result {
                    Ok(Ok(())) => Ok(built),
                    Ok(Err(e)) => Err(e),
                    Err(e) => Err(anyhow!("capture pipeline startup task panicked: {e}")),
                };
            }
            _ = tick.tick() => {
                if let Some(msg) = bus.pop_filtered(&[gst::MessageType::Error]) {
                    break start_failure(
                        "capture pipeline reported an error while starting".to_string(),
                        Some(describe_bus_error(&msg)),
                    );
                }
            }
            _ = &mut deadline => {
                break anyhow::Error::new(StartFailed(format!(
                    "the screen capture did not start within {} s (the portal's stream never answered)",
                    timeout.as_secs()
                )));
            }
        }
    };
    tokio::spawn(async move {
        let _ = play.await;
        let _ = tokio::task::spawn_blocking(move || pipeline.set_state(gst::State::Null)).await;
    });
    Err(failure)
}

/// Closes the portal session of a capture that never started, without letting
/// a dead portal block the caller (the D-Bus call is bounded to 3 s).
async fn close_portal_quietly(source: &CaptureSource) {
    #[cfg(unix)]
    match source {
        CaptureSource::Portal(portal) => match tokio::time::timeout(Duration::from_secs(3), portal.session.close()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!("failed to close the portal session of a capture that did not start: {e}"),
            Err(_) => tracing::warn!("closing the portal session of a capture that did not start timed out"),
        },
        #[cfg(any(test, feature = "test-source"))]
        CaptureSource::TestSource(_) => {}
    }
    #[cfg(not(unix))]
    let _ = source;
}

// ---------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------

/// A running screen-share capture started by [`start_sharing`]. Prefer
/// calling [`CaptureHandle::stop`] for graceful teardown (it also drains the
/// pump/preview tasks, closes the portal session, and notifies the relay);
/// if the handle is dropped without it, `Drop` still synchronously stops the
/// GStreamer pipeline as a backstop, so no element thread is left running —
/// it just skips the async portal-session-close step (see `Drop`'s doc
/// comment), and it just skips the graceful drain/notify steps.
/// Caller-chosen identity for one destination of a capture, opaque to this
/// module. `ui.rs` passes its own `DestinationId` through here so every
/// lookup on a [`CaptureHandle`] is unambiguous.
///
/// `Why not the `stream_id`:` a `stream_id` is only unique *within one
/// connection*, and the whole point of fan-out is that one capture feeds
/// several rooms — two of its destinations can legitimately carry the same
/// `stream_id` in different rooms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DestKey(pub u32);

/// One outgoing destination of a running capture: its own encode branch, its
/// own [`destination::DestinationRuntime`], and its own pump task
/// ([`destination_pump`]) encrypting and sending into *its* room.
///
/// Everything here is per destination, not per capture: the room's
/// `ScreenshareClient` (its key, nonce space and connection), the
/// `stream_id`, the encoder and its bitrate, the pause valve, the live
/// encode-rate gauge, and the lifecycle state machine. That is what makes
/// "one capture, N independently encoded and encrypted streams" literal
/// rather than aspirational.
pub struct DestinationHandle {
    /// The caller's own identity for this destination — see [`DestKey`].
    key: DestKey,
    /// This destination's `stream_id` *on its own room's connection*. Only
    /// unique within that connection, which is why it is never the key.
    stream_id: u8,
    /// Mailbox for [`destination_pump`], the one owner of this destination's
    /// encode branch — see [`DestCmd`].
    cmd_tx: mpsc::UnboundedSender<DestCmd>,
    runtime: destination::SharedDestination,
    encoder_fps: Arc<AtomicU32>,
    /// This destination's live *sent* rate — see [`SENT_FPS_WINDOW`]. Distinct
    /// from `encoder_fps`: this counts only access units that actually made
    /// it onto the wire (`send_media_payload` returned `Ok`), so the two can
    /// diverge on a send error.
    sent_fps: Arc<AtomicU32>,
    task: tokio::task::JoinHandle<()>,
}

impl DestinationHandle {
    /// Shared handle onto this destination's lifecycle state — see
    /// [`destination::DestinationRuntime`]. `ui.rs` clones this into its own
    /// `Destination` so the UI can read the state machine every repaint
    /// without going through the capture handle.
    pub fn runtime(&self) -> destination::SharedDestination {
        self.runtime.clone()
    }

    pub fn params(&self) -> destination::VideoParams {
        self.runtime.lock().unwrap().params()
    }

    /// This destination's live encode rate — see [`ENCODER_FPS_WINDOW`].
    pub fn encoder_fps(&self) -> u32 {
        self.encoder_fps.load(Ordering::Relaxed)
    }

    /// This destination's live sent rate — see [`SENT_FPS_WINDOW`].
    pub fn sent_fps(&self) -> u32 {
        self.sent_fps.load(Ordering::Relaxed)
    }

    /// Tier-1 change ([`destination::ChangeTier::Bitrate`]): a live encoder
    /// property write, fire-and-forget. This is the call the Absolute-Maximum
    /// bandwidth allocator drives (at most once a second, and only past the
    /// hysteresis threshold — see `ui.rs`'s `BandwidthController`).
    pub fn set_bitrate(&self, bitrate_kbps: u32) {
        let _ = self.cmd_tx.send(DestCmd::SetBitrate(bitrate_kbps));
    }

    /// Graceful teardown of just this destination: drain and remove its
    /// encode branch off the still-live tee, then tell its room the stream
    /// stopped. Every *other* destination of the same capture is mechanically
    /// untouched (its own `queue` isolates it — see [`branch::remove_branch`]).
    pub async fn stop(self) -> Result<()> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        if self.cmd_tx.send(DestCmd::Stop { reply: reply_tx }).is_err() {
            // Pump already gone; nothing left to drain.
            return Ok(());
        }
        let result = match tokio::time::timeout(DESTINATION_STOP_TIMEOUT, reply_rx).await {
            Ok(Ok(result)) => result.map_err(|e| anyhow!("{e}")),
            Ok(Err(_)) => Ok(()),
            Err(_) => Err(anyhow!("destination pump did not finish stopping within {DESTINATION_STOP_TIMEOUT:?}")),
        };
        // The pump returns right after replying; joining it is what
        // guarantees no task is still holding a branch when the pipeline goes
        // `Null` underneath it.
        let _ = tokio::time::timeout(DESTINATION_STOP_TIMEOUT, self.task).await;
        result
    }
}

/// How long [`DestinationHandle::stop`] waits for a destination's pump to
/// drain its branch. Comfortably above [`branch::remove_branch`]'s own 5s
/// internal drain bound, so this only ever fires if that one already gave up.
const DESTINATION_STOP_TIMEOUT: Duration = Duration::from_secs(7);

/// A running screen-share capture started by [`start_sharing`]: one real
/// screen grab, one GStreamer pipeline, and N [`DestinationHandle`]s hanging
/// off its `tee` — one per room this capture is being sent into.
///
/// Prefer calling [`CaptureHandle::stop`] for graceful teardown (it drains
/// every destination's branch, notifies each room, closes the portal session,
/// and joins the background tasks); if the handle is dropped without it,
/// `Drop` still synchronously stops the GStreamer pipeline as a backstop, so
/// no element thread is left running — it just skips the async
/// portal-session-close and per-room `StopStream` steps (see `Drop`'s doc
/// comment).
pub struct CaptureHandle {
    pipeline: gst::Pipeline,
    /// Where every destination's encode branch is added/removed.
    tee: gst::Element,
    capture_source: CaptureSource,
    /// Bus watcher (see [`capture_loop`]). Per capture, not per
    /// destination: the bus is not destination-scoped.
    capture_task: tokio::task::JoinHandle<()>,
    /// The first error the capture pipeline's bus reported (written by
    /// [`capture_loop`]); see [`CaptureErrors`].
    capture_errors: CaptureErrors,
    // `Option` (rather than a bare `PreviewHandle`) solely so `stop` can
    // `.take()` it out of `&mut self` to call `PreviewHandle::stop(self)`,
    // which consumes by value — required now that `CaptureHandle: Drop`
    // forbids moving fields out of `self` by value anywhere, not just in
    // `drop` itself. Always `Some` until `stop` runs; letting a `Some` drop
    // normally (bare-drop backstop path) is fine, per `PreviewHandle`'s own
    // doc comment (dropping `stop_tx` promptly stops its background task).
    preview_handle: Option<PreviewHandle>,
    /// The client the running [`Self::preview_handle`] sends through, kept so
    /// [`Self::ensure_preview_task`] can tell when that connection died (a
    /// reconnect hands the capture a *new* client) and move the preview task
    /// over to it.
    preview_client: Option<ScreenshareClient>,
    /// What this capture is considered to be producing, as the input to
    /// [`destination::classify_change`]/[`destination::clamp_to_source`]. The
    /// size half is GStreamer's real negotiated frame size, cached on first
    /// successful read; the framerate half is the rate the capture was
    /// started with (its `keepalive-time` floor), raised on demand by
    /// [`CaptureHandle::set_source_framerate`] — see that method for why the
    /// real source has no hard rate ceiling to speak of.
    source_state: Mutex<destination::SourceCaps>,
    /// One entry per room this capture feeds, in the order they were added.
    /// A `Vec` rather than a map: N is the number of rooms a user is
    /// simultaneously broadcasting into — a handful at most. Empty for a
    /// **staged** capture (`start_capture`/"Go live" flow, see
    /// `superdoc/features/stream-control.md`'s 2026-07-28 note): the tee is
    /// running with zero branches until the first destination is attached.
    destinations: Vec<DestinationHandle>,
    /// The tee's low-rate RGB branch, held here **unconsumed** until the
    /// first destination is attached (see [`Self::ensure_preview_task`]).
    /// Safe to leave unread for however long a staged capture waits for "Go
    /// live": the branch's appsink is `max-buffers=1 drop=true` (see
    /// `build_pipeline`), so it only ever holds the latest sample and never
    /// applies backpressure upstream. `None` once the preview task has
    /// started (or, for a not-yet-staged legacy build, never populated).
    preview_sink: Option<gst_app::AppSink>,
    /// See [`CaptureHandle::awaiting_first_frame`].
    frame_seen: Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for CaptureHandle {
    /// Synchronous backstop for when [`CaptureHandle::stop`] was never
    /// called (a panic, an app crash, or a caller that just drops the handle
    /// instead of awaiting graceful teardown). Without this, the GStreamer
    /// pipeline is left `Playing` — its element threads (`pipewiresrc`/
    /// `d3d11screencapturesrc`, every destination's `x264enc`/`vah264enc`, …)
    /// keep running indefinitely.
    ///
    /// This is a backstop, not a replacement for `stop()`'s graceful
    /// teardown: closing the ashpd portal session is an async D-Bus call and
    /// can't run in this synchronous `drop`, so on the Linux/portal path the
    /// OS recording indicator can stay lit until the OS itself notices this
    /// process's PipeWire fd close (or until `stop()` actually runs). Nor
    /// does it send any room a `StopStream`. This only guarantees the
    /// GStreamer pipeline itself stops.
    ///
    /// When `stop()` *did* run first, `self` is already fully torn down by
    /// the time this runs, so setting an already-`Null` pipeline to `Null`
    /// again is a harmless no-op (same established pattern as
    /// `AudioCaptureHandle`'s `Drop`).
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// Starts sharing this machine's screen into one room: acquires this
/// platform's capture source (Linux: runs the portal permission flow;
/// Windows: no consent flow needed, see [`acquire_capture_source`]), builds
/// and starts the capture pipeline, adds this capture's **first destination**
/// as a dynamic encode branch, and — once that branch has actually produced
/// its first IDR, i.e. capture is really flowing, not just "pipeline exists"
/// — calls [`ScreenshareClient::announce_stream`] so other room members see
/// the stream appear.
///
/// More rooms are added later with [`CaptureHandle::spawn_add_destination`];
/// nothing about this first one is structurally special (see
/// [`build_pipeline`]'s "Why no fixed encoder branch").
///
/// `key` is the caller's own identity for that first destination (see
/// [`DestKey`]). `bitrate_kbps` is plumbed straight into its encoder (see
/// [`DEFAULT_BITRATE_KBPS`]). `framerate` sets the *shared* source rate every
/// branch downstream of the tee sees (see [`build_pipeline`]) and scales each
/// branch's GOP/keyframe interval to match — see [`DEFAULT_FRAMERATE`]/
/// [`MAX_FRAMERATE`] for the selectable range.
///
/// Returns `Err` — never panics — if the capture source is unavailable (on
/// Linux: the portal is unreachable, or the user denies/cancels the
/// screencast permission dialog), or the GStreamer pipeline/first encode
/// branch fails to construct, start, or produce an IDR.
pub async fn start_sharing(
    client: &ScreenshareClient,
    stream_id: u8,
    key: DestKey,
    bitrate_kbps: u32,
    framerate: u32,
) -> Result<CaptureHandle> {
    ensure_gstreamer_initialized()?;

    let capture_source = acquire_capture_source().await?;
    start_sharing_with_source(client, stream_id, key, bitrate_kbps, framerate, capture_source).await
}

/// **Staged capture, no destination yet** — the "Go live" flow's first half
/// (see `superdoc/features/stream-control.md`'s 2026-07-28 note). Acquires
/// the capture source and starts the GStreamer pipeline exactly like
/// [`start_sharing`] does, but returns before adding any destination branch:
/// the resulting [`CaptureHandle::destinations`] is empty. Real
/// resolution/negotiated caps are already known (`source_caps()` is valid
/// immediately), and `AppState::go_live` (`session.rs`) later attaches the
/// first destination the same way fan-out attaches every subsequent one — via
/// [`CaptureHandle::spawn_add_destination`]. Nothing is sent to any room, and
/// no `AnnounceStream` fires, until that first destination's branch produces
/// its first IDR.
///
/// `Why this is architecturally sound and not a special case:` a running tee
/// with zero branches hanging off it is already exactly what a capture looks
/// like for the instant between "pipeline reaches Playing" and "first
/// destination's branch is added" in the existing `start_sharing_with_source`
/// path below — this function just stops there instead of immediately
/// chaining `spawn_destination`. The fan-out path (`add_destination` in
/// `session.rs`) has always added a branch to an *already-running* tee; this
/// is the same primitive, just used for the first branch instead of the
/// second.
pub async fn start_capture(framerate: u32) -> Result<CaptureHandle> {
    ensure_gstreamer_initialized()?;
    let capture_source = acquire_capture_source().await?;
    match start_capture_with_source(framerate, capture_source).await {
        // `Why one retry on a new portal session (task #59):` stock
        // xdg-desktop-portal-wlr exits when the `pipewiresrc` start flap lands
        // after its first frame request, and the odds of that grow with CPU
        // load (about 1 start in 4 at host load >= 20, none below 10; see
        // [`decoupled_source`]). Its node is gone with it, so nothing on this
        // session can recover, but the portal service restarts the backend on
        // the next request, which makes a fresh session cheap. Once, never in
        // a loop; a picker-based backend asks the user again, which beats a
        // dead end that needs the same click anyway.
        Err(e) if start_is_hopeless(&e) => {
            tracing::warn!("screen capture lost its portal while starting ({e:#}); requesting a new portal session once");
            // Lets a restarting backend (systemd `Restart=`, a compositor
            // autostart script) take its bus name again first.
            tokio::time::sleep(Duration::from_millis(500)).await;
            let capture_source = acquire_capture_source().await.map_err(|retry| {
                if is_picker_cancelled(&retry) {
                    retry
                } else {
                    retry.context(format!("could not get a new portal session after: {e:#}"))
                }
            })?;
            start_capture_with_source(framerate, capture_source).await
        }
        other => other,
    }
}

/// TEST-ONLY seam (see [`CaptureSource::TestSource`]): starts the exact same
/// tee→branch→encode→appsink→encrypt→fragment→send pipeline [`start_sharing`]
/// uses internally, sourced from a caller-supplied raw `gst-launch`-syntax
/// source fragment (e.g. `"videotestsrc is-live=true pattern=smpte ! \
/// video/x-raw,width=320,height=240,framerate=15/1"`) instead of the
/// interactive portal/pipewiresrc (or Windows `d3d11screencapturesrc`) flow.
///
/// Exists solely so an automated integration test can drive this module's
/// real capture → encrypt → fragment → send path end to end without a human
/// clicking the interactive Wayland portal consent dialog (which cannot be
/// automated). `#[cfg(test)]`-gated: this function does not exist at all in
/// a non-test build, so it changes nothing about [`start_sharing`]'s
/// production API or behavior.
#[cfg(any(test, feature = "test-source"))]
pub async fn start_sharing_from_test_source(
    client: &ScreenshareClient,
    stream_id: u8,
    bitrate_kbps: u32,
    framerate: u32,
    source_fragment: &str,
) -> Result<CaptureHandle> {
    ensure_gstreamer_initialized()?;
    start_sharing_with_source(
        client,
        stream_id,
        DestKey(0),
        bitrate_kbps,
        framerate,
        CaptureSource::TestSource(source_fragment.to_string()),
    )
    .await
}

/// TEST-ONLY seam, the zero-destination sibling of
/// [`start_sharing_from_test_source`]: starts a real `TestSource`-backed
/// capture pipeline (real GStreamer, real negotiated caps via
/// [`CaptureHandle::source_caps`]) with **no** destination attached — the
/// exact shape [`start_capture`] hands `AppState::stage_video_share`, minus
/// the interactive portal prompt. Exists so an automated test can reproduce
/// the "attach the first destination to an already-running, zero-destination
/// capture" path (`AppState::go_live`/`add_destination`'s no-sibling
/// fallback in `session.rs`) without a human clicking the portal picker.
#[cfg(any(test, feature = "test-source"))]
pub async fn start_capture_from_test_source(framerate: u32, source_fragment: &str) -> Result<CaptureHandle> {
    ensure_gstreamer_initialized()?;
    start_capture_with_source(framerate, CaptureSource::TestSource(source_fragment.to_string())).await
}

/// Shared body of [`start_sharing`] / [`start_sharing_from_test_source`]:
/// starts a staged capture ([`start_capture_with_source`]), then immediately
/// attaches `key`/`stream_id` as its first destination and waits for that
/// branch's first IDR before returning — the "share and go live in one call"
/// behaviour both of those public entry points still provide.
async fn start_sharing_with_source(
    client: &ScreenshareClient,
    stream_id: u8,
    key: DestKey,
    bitrate_kbps: u32,
    framerate: u32,
    capture_source: CaptureSource,
) -> Result<CaptureHandle> {
    let mut handle = start_capture_with_source(framerate, capture_source).await?;

    let source = handle.source_caps();
    let params = destination::VideoParams {
        width: source.width,
        height: source.height,
        framerate: source.framerate,
        bitrate_kbps,
        // Preferred backend for the *branch*; `branch::add_branch_live`
        // silently falls back to software per branch if this machine's VAAPI
        // can't actually run another concurrent encode session.
        backend: detect_encoder_backend(),
        // "Good mix" defaults, not this pipeline's old hardcoded low-latency
        // extreme — see `TuneMode`/`QualityLevel`/`RateControlMode`'s
        // `#[default]` doc comments.
        tune: TuneMode::default(),
        quality_level: QualityLevel::default(),
        rate_control: RateControlMode::default(),
        gop_millis: DEFAULT_GOP_MILLIS,
        codec: VideoCodec::default(),
    };

    // Everything above already owns the pipeline/portal session via `handle`
    // (its `Drop` is the backstop), so a failure from here on is torn down
    // with `handle.stop()` — the same graceful teardown any other capture
    // gets, not a special-cased abandon path.
    //
    // `Why this matters:` a bare `?` here was once a real, measured leak.
    // When the first destination failed to come up, `start_sharing` returned
    // `Err`, the UI reported "share failed" and showed nothing running —
    // while the process kept a live PipeWire screencast stream, its
    // GStreamer element threads and the portal session open **for the rest
    // of the process's life**, with no affordance to stop it: the user's
    // screen went on being captured after they were told the share had
    // failed.
    let started = spawn_destination(&handle.pipeline, &handle.tee, &handle.capture_errors, client, stream_id, key, params).await;
    match started {
        Ok(destination) => {
            handle.ensure_preview_task(client, stream_id);
            handle.destinations.push(destination);
            Ok(handle)
        }
        Err(e) => {
            if let Err(stop_err) = handle.stop().await {
                tracing::warn!("failed to stop capture after its first destination failed to start: {stop_err:#}");
            }
            Err(e)
        }
    }
}

/// Shared body of [`start_capture`] and [`start_sharing_with_source`]:
/// everything from "a [`CaptureSource`] has been acquired" onward that does
/// **not** depend on any destination — building and starting the pipeline,
/// and wiring up the capture-level bus watcher. Returns a
/// [`CaptureHandle`] with an empty `destinations` list; the tee's low-rate
/// preview branch is held (unconsumed — see [`CaptureHandle::preview_sink`])
/// rather than wired to a room, since which room gets the periodic thumbnail
/// isn't known until a destination exists.
async fn start_capture_with_source(framerate: u32, capture_source: CaptureSource) -> Result<CaptureHandle> {
    // Defensive clamp — see [`MAX_FRAMERATE`]'s doc comment. `ui.rs`'s
    // dropdown only ever offers values already inside this range, so this is
    // a backstop against a 0/absurd value reaching the `SourceCaps` this
    // capture reports (and through it every `classify_change`/
    // `clamp_to_source` decision made against it), not the primary
    // validation. It lives here because this is where the value is actually
    // *consumed*: `build_pipeline` no longer takes a framerate at all, and
    // `launch_fragment` does its own clamp for `keepalive-time`.
    let framerate = framerate.clamp(MIN_FRAMERATE, MAX_FRAMERATE);

    let source_fragment = capture_source.launch_fragment(framerate);
    // `Why spawn_blocking (inside `start_pipeline_bounded`):` the start synchronously blocks on
    // `Element::state()`, whose timeout argument is not reliable under real
    // contention (a capture started immediately after the previous one's
    // portal/pipeline teardown can leave GStreamer's state change stuck for
    // far longer than promised — see `start_pipeline`'s own `Why:` note).
    // Calling it inline here would block whichever Tokio runtime worker
    // thread happens to run this task for as long as that stall lasts —
    // observed anywhere from several seconds to indefinitely — and enough
    // repeated stalls permanently starve the whole shared worker pool this
    // process depends on for everything else (`driver.rs`'s poll loop, every
    // other command). Running it on `spawn_blocking`'s separate, elastic
    // thread pool instead confines a stuck state-change to one throwaway
    // thread there, no matter how long (or whether) it ever returns.
    //
    // `Why the GPU domain falls back to the CPU one on a start failure:` the
    // GPU path depends on the driver accepting DMA-BUF from the compositor and
    // on `vapostproc` negotiating with it. If that fails at start the share
    // must still come up, just slower, instead of failing outright. Not when
    // the portal's node itself is gone (`start_is_hopeless`), and each attempt
    // is bounded by `START_TIMEOUT`, so a failed start ends within seconds.
    let domain = CaptureDomain::for_source(&capture_source);
    let started = match start_pipeline_bounded(&source_fragment, domain, START_TIMEOUT).await {
        // `Why not when the failure is a [`StartFailed`]`: the portal's node is
        // gone or silent, a fresh `pipewiresrc` on it would only wait again
        // (and, if the portal is alive after all, flap it a second time).
        Err(e) if domain == CaptureDomain::Gpu && !start_is_hopeless(&e) => {
            tracing::warn!("GPU capture path failed to start ({e:#}); falling back to the CPU path");
            start_pipeline_bounded(&source_fragment, CaptureDomain::System, FALLBACK_START_TIMEOUT).await.map(|built| (built, CaptureDomain::System))
        }
        other => other.map(|built| (built, domain)),
    };
    let (built, domain) = match started {
        Ok(started) => started,
        Err(e) => {
            tracing::warn!("screen capture failed to start: {e:#}");
            close_portal_quietly(&capture_source).await;
            return Err(e);
        }
    };
    tracing::info!("capture started in the {domain:?} domain");
    let CapturePipeline { pipeline, tee, preview_sink, frame_seen } = built;

    // Size for the never-upscale clamp ([`destination::clamp_to_source`]) and
    // for whatever destination is added first: read off the tee's negotiated
    // sink caps now that the pipeline is playing, falling back to 0x0
    // ("unknown / no constraint") if caps haven't landed yet rather than
    // failing the whole call over a cosmetic field.
    let (width, height) = negotiated_size(&tee).unwrap_or((0, 0));
    let source = destination::SourceCaps { width: width as u32, height: height as u32, framerate };

    let bus = match pipeline.bus() {
        Some(bus) => bus,
        None => return Err(abandon_capture(pipeline, capture_source, anyhow!("pipeline has no bus")).await),
    };
    let capture_errors = CaptureErrors::default();
    let capture_task = tokio::spawn(capture_loop(bus.stream(), capture_errors.clone()));

    Ok(CaptureHandle {
        frame_seen,
        pipeline,
        tee,
        capture_source,
        capture_task,
        capture_errors,
        preview_handle: None,
        preview_client: None,
        preview_sink: Some(preview_sink),
        source_state: Mutex::new(source),
        destinations: Vec::new(),
    })
}

/// Tear down a capture that never became a [`CaptureHandle`]: stop the
/// GStreamer pipeline and close the portal session, then hand back the error
/// that caused the abandonment (with context, so the caller's message still
/// explains what actually failed).
///
/// `Why this exists as its own function:` `CaptureHandle`'s `Drop` impl is
/// what normally guarantees a capture can never outlive its owner, but it
/// only applies once the handle is *constructed*. Every fallible step between
/// starting the pipeline and constructing it is outside that guarantee, and
/// the consequence of getting it wrong is not a leaked allocation — it is a
/// screen that keeps being captured (recording indicator lit, PipeWire stream
/// live) after the user was told the share failed.
async fn abandon_capture(pipeline: gst::Pipeline, capture_source: CaptureSource, error: anyhow::Error) -> anyhow::Error {
    if let Err(e) = pipeline.set_state(gst::State::Null) {
        tracing::warn!("failed to stop the pipeline of an abandoned capture: {e}");
    }

    #[cfg(unix)]
    match &capture_source {
        CaptureSource::Portal(portal) => {
            if let Err(e) = portal.session.close().await {
                tracing::warn!("failed to close the portal session of an abandoned capture: {e}");
            }
        }
        #[cfg(any(test, feature = "test-source"))]
        CaptureSource::TestSource(_) => {}
    }
    drop(capture_source);

    error
}

/// Builds one destination end to end: a dynamic encode branch on `tee`
/// (waiting for its first IDR, with per-branch VAAPI→software fallback), a
/// fresh [`destination::DestinationRuntime`], a [`destination_pump`] task
/// encrypting and sending into `client`'s room, and the room's
/// `AnnounceStream`.
///
/// `Why announce here, after the first IDR, and not before:` announcing a
/// stream whose encoder then turns out not to work would show every other
/// member a stream that never produces a frame. `branch::add_branch_live`
/// only returns once this branch has proven itself, so this is the earliest
/// honest moment to announce — and the same moment the destination's own
/// state machine goes `Starting` → `Live`.
async fn spawn_destination(
    pipeline: &gst::Pipeline,
    tee: &gst::Element,
    capture_errors: &CaptureErrors,
    client: &ScreenshareClient,
    stream_id: u8,
    key: DestKey,
    params: destination::VideoParams,
) -> Result<DestinationHandle> {
    // The destination's wire clock is anchored *before* its branch exists, so
    // `on_pipeline_start` below only ever re-anchors the offset onto a
    // pipeline this destination's epoch already predates. See
    // [`destination::DestinationRuntime`]'s doc comment for why that split is
    // what makes a later source restart look like low framerate rather than a
    // glitch.
    let runtime = destination::DestinationRuntime::shared(Instant::now(), params);
    {
        let running_time_us = pipeline.current_running_time().map(|t| t.useconds()).unwrap_or(0);
        runtime.lock().unwrap().on_pipeline_start(running_time_us, Instant::now());
    }

    let (encode_branch, first_idr) =
        branch::add_branch_live_waiting(
            pipeline,
            tee,
            branch::BranchId(key.0),
            params.to_encode_settings(),
            branch::SourceWait::Indefinite,
        )
        .await
        .map_err(|e| blame_the_source(e.context(format!("failed to bring up the encode branch for stream {stream_id}")), capture_errors))?;

    // Feed the branch's proving IDR through the normal accounting so it is
    // cached for the stall protocol and drives `Starting` → `Live` exactly
    // once, in one place — and, critically, so it actually reaches the
    // viewer: `wait_for_first_idr` consumed it off the appsink, so nothing
    // else will ever see it.
    if let Some(buffer) = first_idr.buffer()
        && let Ok(map) = buffer.map_readable()
    {
        {
            let payload = map.as_slice();
            let buffer_pts_us = buffer.pts().map(|t| t.useconds()).unwrap_or(0);
            let (wire_pts_us, stalled) = {
                let mut runtime = runtime.lock().unwrap();
                runtime.note_encoded_frame(true, payload, Instant::now());
                (runtime.wire_pts_from_buffer(buffer_pts_us), runtime.is_stalled())
            };
            if let Err(e) = send_media_payload(client, stream_id, payload, wire_pts_us, true, stalled) {
                tracing::warn!("stream {stream_id}: could not send the branch's first IDR: {e:#}");
            }
        }
    }

    if let Err(e) = client
        .announce_stream(stream_id, StreamKind::Video, "Screen", params.width as u16, params.height as u16)
        .await
    {
        // Nothing is announced, so nothing may keep encoding: remove the
        // branch rather than leaking it onto the tee — the same
        // built-then-abandoned cleanup contract every failure path in
        // [`branch::add_branch_live`] follows.
        let _ = branch::remove_branch(pipeline, tee, encode_branch).await;
        return Err(e).context("failed to announce video stream to the relay");
    }

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let encoder_fps = Arc::new(AtomicU32::new(0));
    let sent_fps = Arc::new(AtomicU32::new(0));
    let task = tokio::spawn(destination_pump(
        client.clone(),
        stream_id,
        pipeline.clone(),
        tee.clone(),
        encode_branch,
        runtime.clone(),
        encoder_fps.clone(),
        sent_fps.clone(),
        cmd_rx,
    ));

    Ok(DestinationHandle { key, stream_id, cmd_tx, runtime, encoder_fps, sent_fps, task })
}

/// Bridges `preview_sink`'s raw-RGB samples (the tee's low-rate branch, see
/// this module's doc comment) into a [`FrameStream`] for
/// [`preview::spawn_preview_task`], extracting width/height from the
/// negotiated caps and stride-correcting the pixel data into a tightly
/// packed `width * height * 3`-byte buffer — the same [`gst_video::VideoFrameRef`]-based
/// pattern `decode.rs`'s `pull_color_image` uses on the receiving side for
/// the equivalent stride issue. Malformed samples are dropped (logged, not
/// propagated) rather than ending the stream.
fn preview_frame_stream(preview_sink: gst_app::AppSink) -> FrameStream {
    Box::pin(preview_sink.stream().filter_map(|sample| async move {
        match sample_to_raw_frame(&sample) {
            Ok(frame) => Some(frame),
            Err(e) => {
                tracing::warn!("dropping malformed preview capture frame: {e:#}");
                None
            }
        }
    }))
}

fn sample_to_raw_frame(sample: &gst::Sample) -> Result<RawFrame> {
    let buffer = sample.buffer().ok_or_else(|| anyhow!("preview sample had no buffer"))?;
    let caps = sample.caps().ok_or_else(|| anyhow!("preview sample had no caps"))?;
    let info = gst_video::VideoInfo::from_caps(caps).context("failed to parse preview capture caps")?;

    let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info)
        .map_err(|_| anyhow!("failed to map preview capture buffer readable"))?;

    let width = frame.width() as usize;
    let height = frame.height() as usize;
    let stride = frame.plane_stride()[0] as usize;
    let plane = frame
        .plane_data(0)
        .map_err(|_| anyhow!("preview capture frame had no plane 0 data"))?;

    // Tightly pack RGB rows for `preview::RawFrame`, stripping any stride
    // padding GStreamer added past `width * 3` (see decode.rs's
    // `pull_color_image` for the equivalent handling on the receive side).
    let row_bytes = width * 3;
    let mut packed = Vec::with_capacity(row_bytes * height);
    for row in 0..height {
        let start = row * stride;
        let end = start + row_bytes;
        let Some(row_slice) = plane.get(start..end) else {
            anyhow::bail!("preview capture frame plane shorter than expected (stride/height mismatch)");
        };
        packed.extend_from_slice(row_slice);
    }

    Ok(RawFrame {
        width: width as u32,
        height: height as u32,
        rgb: packed,
    })
}

/// Negotiated frame size on `element`'s sink pad. Used against the `tee` to
/// learn what the shared capture source is actually producing (the size half
/// of [`destination::SourceCaps`]) — `None` while caps haven't landed yet.
fn negotiated_size(element: &gst::Element) -> Option<(u16, u16)> {
    let pad = element.static_pad("sink")?;
    let caps = pad.current_caps()?;
    let s = caps.structure(0)?;
    let width: i32 = s.get("width").ok()?;
    let height: i32 = s.get("height").ok()?;
    Some((width.clamp(0, u16::MAX as i32) as u16, height.clamp(0, u16::MAX as i32) as u16))
}

/// What [`CaptureHandle::change_destination`] decided to do with a requested
/// settings change, so the caller can reflect it (an "applying…" spinner for
/// a swap, nothing at all for a bitrate write) without re-deriving the
/// classification itself.
pub struct ChangeRequest {
    /// Which tier [`destination::classify_change`] put this change in.
    pub tier: destination::ChangeTier,
    /// The parameters that were actually requested, after
    /// [`destination::clamp_to_source`]'s never-upscale clamp — which may
    /// differ from what the caller asked for.
    pub applied: destination::VideoParams,
    /// Resolves when a tier-2/tier-3 branch swap finishes. `None` for tiers
    /// that complete synchronously (`Unchanged`, `Bitrate`).
    pub done: Option<tokio::sync::oneshot::Receiver<Result<(), String>>>,
}

impl CaptureHandle {
    /// Tears the capture down cleanly: stops every destination (draining its
    /// encode branch and telling its room the stream is gone), stops the
    /// GStreamer pipeline, closes the portal session (Linux only — Windows
    /// has no session to close, see [`CaptureSource`]), and waits for the
    /// background tasks to finish draining.
    pub async fn stop(mut self) -> Result<()> {
        // Destinations first: each one drains and removes its own branch off
        // the still-live tee and sends its room a `StopStream`. Doing this
        // before the pipeline's EOS keeps every removal on the well-tested
        // live-pipeline path (`branch::remove_branch`) rather than racing a
        // pipeline that is already tearing down.
        for destination in std::mem::take(&mut self.destinations) {
            let stream_id = destination.stream_id;
            if let Err(e) = destination.stop().await {
                tracing::warn!("stopping destination (stream {stream_id}) did not finish cleanly: {e:#}");
            }
        }

        // Ask the pipeline to end gracefully by sending it a real EOS event
        // *before* forcing it to `Null`, and wait for the capture task to
        // notice.
        //
        // This isn't just tidiness: `gstreamer_app::AppSinkStream` (what
        // [`capture_loop`] awaits) only ever yields its terminal `None` once
        // the appsink's `is_eos()` flag is actually set, which only happens
        // once a real EOS event has flowed all the way through the pipeline
        // into it (see that type's `poll_next`). Jumping straight to
        // `set_state(Null)` never sends that event — it just tears the
        // elements down — so the appsink's stream would sit `Pending` forever
        // and the capture task would never complete, hanging this whole
        // function indefinitely. `Pipeline::send_event` on a
        // downstream/serialized event like EOS delegates to the bin's source
        // element(s), so it originates upstream of the `tee` exactly like a
        // natural end-of-stream would.
        //
        // The `timeout` is a backstop, not the expected path: a healthy
        // pipeline should drain and reach EOS in well under a second, but
        // this guarantees `stop()` itself can never hang the caller forever
        // if a source somehow never honors EOS (e.g. an unresponsive
        // hardware capture device).
        if !self.pipeline.send_event(gst::event::Eos::new()) {
            tracing::warn!("capture pipeline did not accept the EOS event; forcing a hard stop");
        }

        // `&mut` rather than moving `self.capture_task` out: `CaptureHandle`
        // implements `Drop` (see above), which forbids moving individual
        // fields out of `self` by value — `JoinHandle` is `Unpin`, so
        // awaiting through `&mut` works identically.
        match tokio::time::timeout(Duration::from_secs(5), &mut self.capture_task).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!("capture task did not shut down cleanly: {e}"),
            Err(_) => tracing::warn!("capture task did not observe EOS within 5s; forcing pipeline to Null anyway"),
        }

        if let Err(e) = self.pipeline.set_state(gst::State::Null) {
            tracing::warn!("error stopping capture pipeline: {e}");
        }

        // `.take()` rather than moving `self.preview_handle` out directly,
        // for the same reason as `capture_task` above: `PreviewHandle::stop`
        // consumes `self` by value, so it can't be called through a
        // reference.
        if let Some(preview_handle) = self.preview_handle.take() {
            preview_handle.stop().await;
        }

        #[cfg(unix)]
        {
            // Borrow rather than move `self.capture_source` — also required
            // now that `CaptureHandle: Drop`, though `Session::close` takes
            // `&self` anyway, so a borrow is all this needs regardless.
            // `CaptureSource` has exactly one variant on this platform in a
            // non-test build (see its `#[cfg(unix)]`/`#[cfg(windows)]`
            // split); a `match` is what lets the `#[cfg(test)]`-only
            // `TestSource` variant coexist here without an exhaustiveness
            // error in test builds.
            match &self.capture_source {
                CaptureSource::Portal(portal) => {
                    if let Err(e) = portal.session.close().await {
                        tracing::warn!("error closing screencast portal session: {e}");
                    }
                }
                #[cfg(any(test, feature = "test-source"))]
                CaptureSource::TestSource(_) => {}
            }
        }

        Ok(())
    }

    // -----------------------------------------------------------------
    // Destinations
    // -----------------------------------------------------------------

    fn destination(&self, key: DestKey) -> Option<&DestinationHandle> {
        self.destinations.iter().find(|d| d.key == key)
    }

    pub fn destination_runtime(&self, key: DestKey) -> Option<destination::SharedDestination> {
        self.destination(key).map(|d| d.runtime())
    }

    /// This destination's live encode rate — see [`ENCODER_FPS_WINDOW`]. `0`
    /// for an unknown key or a paused encoder.
    pub fn encoder_fps(&self, key: DestKey) -> u32 {
        self.destination(key).map(|d| d.encoder_fps()).unwrap_or(0)
    }

    /// This destination's live sent rate — see [`SENT_FPS_WINDOW`]. `0` for
    /// an unknown key or a destination that isn't successfully sending.
    pub fn sent_fps(&self, key: DestKey) -> u32 {
        self.destination(key).map(|d| d.sent_fps()).unwrap_or(0)
    }

    /// Fan-out / transfer: start sending this *same capture* into another
    /// room as a second, fully independent encoded+encrypted stream.
    ///
    /// Returns immediately with a receiver for the result: building a branch
    /// and waiting for its first IDR is async, but `ui.rs` drives everything
    /// from a synchronous `update()` and owns this handle by value, so it
    /// cannot `.await` a `&self` method. Everything the work needs
    /// (`gst::Pipeline`/`gst::Element` are `Send + Sync` refcounted handles,
    /// `ScreenshareClient` is cloneable) is cloned into the spawned task; the
    /// caller polls the receiver each frame and hands the resulting
    /// [`DestinationHandle`] back via [`CaptureHandle::register_destination`].
    ///
    /// `Why the caller registers it rather than this method:` the spawned
    /// task cannot touch `self` (it outlives the borrow), and a shared
    /// `Mutex<Vec<DestinationHandle>>` would buy nothing — the UI already has
    /// `&mut` access at exactly the moment it polls the result.
    pub fn spawn_add_destination(
        &self,
        client: &ScreenshareClient,
        stream_id: u8,
        key: DestKey,
        params: destination::VideoParams,
    ) -> tokio::sync::oneshot::Receiver<Result<DestinationHandle, String>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let pipeline = self.pipeline.clone();
        let tee = self.tee.clone();
        let client = client.clone();
        let params = destination::clamp_to_source(params, self.source_caps());
        let capture_errors = self.capture_errors.clone();
        tokio::spawn(async move {
            let result = spawn_destination(&pipeline, &tee, &capture_errors, &client, stream_id, key, params)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
        });
        rx
    }

    /// Adopt a destination built by [`CaptureHandle::spawn_add_destination`].
    /// `client` is only used to start the periodic room-preview task the
    /// *first* time this capture ever gains a destination (see
    /// [`Self::ensure_preview_task`]) — every later `register_destination`
    /// call (ordinary fan-out into a second/third room) is a no-op there.
    /// `None` (the destination's own room vanished before this call landed —
    /// an unlikely but possible race) just skips the preview task; the
    /// destination is still adopted either way, matching this method's
    /// pre-"Go live" behaviour of always adopting what it's handed.
    pub fn register_destination(&mut self, client: Option<&ScreenshareClient>, destination: DestinationHandle) {
        if let Some(client) = client {
            self.ensure_preview_task(client, destination.stream_id);
        }
        self.destinations.push(destination);
    }

    /// Starts this capture's periodic room-preview task (the tee's low-rate
    /// RGB branch → downscale/JPEG/encrypt/send, see `preview.rs`) the first
    /// time it gains a destination, using that destination's room/`stream_id`
    /// as the one this capture's thumbnail is sent to. A no-op if the task is
    /// already running (every later destination on this capture — ordinary
    /// fan-out) or if this build never staged a `preview_sink` to begin with.
    ///
    /// `Why "first destination" and not "the capture itself":` a staged
    /// capture (`start_capture`) has no room to send a thumbnail to yet — see
    /// [`CaptureHandle::preview_sink`]'s doc comment on why the appsink is
    /// safe to leave unconsumed until this runs.
    ///
    /// `Why it also restarts:` the task is bound to one client and one
    /// `stream_id`. After an automatic reconnect the session rebuilds this
    /// capture's destinations on a *new* client (no new picker), and the old
    /// task keeps failing on the dead connection, so peers would see no
    /// thumbnail of the share until the capture was restarted. So when the
    /// client the task was started with has a closed connection and the caller
    /// brings a live one, the old task is stopped and a new one started on the
    /// new client and stream id. This is why the `AppSink` is cloned, not
    /// taken: a fresh frame stream can be made from it as often as needed.
    fn ensure_preview_task(&mut self, client: &ScreenshareClient, stream_id: u8) {
        if self.preview_handle.is_some() {
            let dead = self.preview_client.as_ref().is_some_and(|c| c.connection().close_reason().is_some());
            if !dead || client.connection().close_reason().is_some() {
                return;
            }
            // Dropping the handle stops its task promptly (see `PreviewHandle`).
            self.preview_handle = None;
        }
        let Some(sink) = self.preview_sink.clone() else { return };
        let preview_frames: FrameStream = preview_frame_stream(sink);
        self.preview_handle = Some(preview::spawn_preview_task(client.clone(), stream_id, preview_frames));
        self.preview_client = Some(client.clone());
    }

    /// Stop sending into one room, leaving every other destination of this
    /// capture running. Same async-from-a-sync-caller shape as
    /// [`CaptureHandle::spawn_add_destination`]: the destination is removed
    /// from this handle immediately and torn down in a spawned task.
    ///
    /// A no-op (immediately `Ok`) for an unknown key.
    pub fn spawn_remove_destination(&mut self, key: DestKey) -> tokio::sync::oneshot::Receiver<Result<(), String>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let Some(index) = self.destinations.iter().position(|d| d.key == key) else {
            let _ = tx.send(Ok(()));
            return rx;
        };
        let destination = self.destinations.remove(index);
        tokio::spawn(async move {
            let _ = tx.send(destination.stop().await.map_err(|e| format!("{e:#}")));
        });
        rx
    }

    /// Forward a `ClientEvent::SubscriberCount { stream_id, count }` to the
    /// destination that owns `key`, which pauses (`count == 0`) or resumes
    /// (`count > 0`, forcing a keyframe so the next frame any new subscriber
    /// gets is decodable) *that destination's* encoder branch. Every other
    /// destination of this capture is unaffected — subscriber counts are per
    /// stream, and so is the valve that acts on them.
    ///
    /// A no-op if the key is unknown or its pump has already exited.
    pub fn notify_subscriber_count(&self, key: DestKey, count: u32) {
        if let Some(destination) = self.destination(key) {
            let _ = destination.cmd_tx.send(DestCmd::SubscriberCount(count));
        }
    }

    /// Forward a `ClientEvent::KeyframeRequest { stream_id }` to the
    /// destination that owns `key` — same per-destination scoping and
    /// already-stopped no-op behavior as
    /// [`CaptureHandle::notify_subscriber_count`].
    pub fn notify_keyframe_request(&self, key: DestKey) {
        if let Some(destination) = self.destination(key) {
            let _ = destination.cmd_tx.send(DestCmd::KeyframeRequest);
        }
    }

    /// Tier-1 change, direct: write one destination's encoder bitrate live.
    /// This is the allocator's write path (see `ui.rs`'s
    /// `BandwidthController`); a *user-driven* settings edit should go
    /// through [`CaptureHandle::change_destination`] instead, so it gets
    /// classified rather than assumed to be tier 1.
    pub fn set_destination_bitrate(&self, key: DestKey, bitrate_kbps: u32) {
        if let Some(destination) = self.destination(key) {
            destination.set_bitrate(bitrate_kbps);
        }
    }

    /// Apply a requested settings change to one destination, routed through
    /// [`destination::classify_change`] — the single entry point for *every*
    /// user-driven edit.
    ///
    /// `Why everything goes through the classifier:` the three tiers differ
    /// by orders of magnitude in cost and in what the viewer sees (a property
    /// write nobody notices / a make-before-break branch swap nobody notices
    /// / a shared source restart that stalls **every** destination on this
    /// capture at once). Deciding at a call site — "this is just a bitrate,
    /// I'll write it directly" — is exactly how a change silently takes the
    /// wrong tier, so the classification is not skippable here.
    pub fn change_destination(&self, key: DestKey, requested: destination::VideoParams) -> Result<ChangeRequest> {
        let source = self.source_caps();
        let target = destination::clamp_to_source(requested, source);
        let destination = self
            .destination(key)
            .ok_or_else(|| anyhow!("no such destination on this capture: {key:?}"))?;
        let current = destination.params();
        let tier = destination::classify_change(source, current, target);

        let done = match tier {
            destination::ChangeTier::Unchanged => None,
            destination::ChangeTier::Bitrate => {
                destination.set_bitrate(target.bitrate_kbps);
                None
            }
            destination::ChangeTier::BranchSwap => Some(self.request_swap(destination, target)),
            destination::ChangeTier::SourceRestart => {
                // Tier 3 first (the shared source has to be producing at the
                // new rate before this destination's branch can be capped to
                // it), then the tier-2 swap that actually re-caps the branch.
                self.set_source_framerate(target.framerate);
                Some(self.request_swap(destination, target))
            }
        };

        Ok(ChangeRequest { tier, applied: target, done })
    }

    fn request_swap(
        &self,
        destination: &DestinationHandle,
        params: destination::VideoParams,
    ) -> tokio::sync::oneshot::Receiver<Result<(), String>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if destination.cmd_tx.send(DestCmd::ApplyParams { params, reply: tx }).is_err() {
            // The pump is gone (the destination is being torn down); the
            // receiver resolves to a `RecvError` the caller already handles.
        }
        rx
    }

    // -----------------------------------------------------------------
    // The shared capture source
    // -----------------------------------------------------------------

    /// What the shared source is producing right now — the input to
    /// [`destination::classify_change`] and [`destination::clamp_to_source`].
    ///
    /// The size half is filled in lazily: a capture whose caps hadn't landed
    /// when it started reports `0x0` ("no constraint") until they do, at
    /// which point the real size is cached here for good.
    /// `true` while this capture has **never** produced a frame.
    ///
    /// On a portal/wlroots capture of a monitor where nothing is repainting,
    /// this stays `true` indefinitely — the compositor only answers a capture
    /// request once the output commits a new frame (see
    /// [`branch::wait_for_source_frame`]). Destinations attached in that
    /// state stay `Starting` rather than failing, and this is what lets a UI
    /// say *why* ("waiting for the shared screen to change") instead of
    /// showing an unexplained spinner.
    pub fn awaiting_first_frame(&self) -> bool {
        !self.frame_seen.load(Ordering::Acquire)
    }

    pub fn source_caps(&self) -> destination::SourceCaps {
        let mut state = self.source_state.lock().unwrap();
        if (state.width == 0 || state.height == 0)
            && let Some((width, height)) = negotiated_size(&self.tee)
        {
            state.width = width as u32;
            state.height = height as u32;
        }
        *state
    }

    /// Raise the rate this capture is considered to be running at, so a
    /// destination is allowed to encode above the rate the capture was
    /// started with ([`destination::ChangeTier::SourceRestart`]).
    ///
    /// `Why this does no pipeline surgery — and why that is not a cop-out:`
    /// the plan doc's tier 3 assumes the shared source has a hard rate
    /// ceiling that only a restart can raise. Measured against the real
    /// portal source, it doesn't:
    ///
    /// - The source is **damage-driven and variable-rate**
    ///   (`framerate=0/1`): it was observed delivering ~94 fps under motion
    ///   and ~30 fps on a static desktop with `keepalive-time` set from the
    ///   selected framerate. The selected rate is a *floor for the idle
    ///   case*, not a ceiling.
    /// - A destination asking for more than that floor is served by its own
    ///   branch's `videorate`, which duplicates the last frame to fill the
    ///   gap — the same mechanism that already delivers "a constant framerate
    ///   even on a completely static screen". Two branches off one real
    ///   portal capture were measured holding 31.5 fps and 5.5 fps at the
    ///   same time.
    ///
    /// So there is nothing to restart: nothing is being withheld from the
    /// destination that a source change would unlock. Both mechanisms that
    /// *could* have changed the live source's rate were measured and rejected
    /// — rewriting a pre-tee `videorate`'s capsfilter stalls the whole
    /// capture dead (see [`build_pipeline`]), and writing `keepalive-time` on
    /// a live `pipewiresrc` kills the stream outright (measured: every branch
    /// dropped to 0 fps and never recovered).
    ///
    /// What this therefore does is the honest minimum: record the higher rate
    /// so the capture's [`destination::SourceCaps`] stops classifying that
    /// destination's setting as tier 3, and let the caller's branch swap
    /// apply it. No stall is raised, because nothing actually stalls.
    ///
    /// A genuine source change — the user picking a different monitor, or the
    /// portal session dying — *does* need the stall protocol and a rebuilt
    /// pipeline; that is what [`CaptureHandle::begin_stall`],
    /// [`CaptureHandle::note_source_restarted`] and
    /// [`branch::rebuild_after_source_restart`] are for, and they stay tested
    /// against that day.
    pub fn set_source_framerate(&self, framerate: u32) {
        let framerate = framerate.clamp(MIN_FRAMERATE, MAX_FRAMERATE);
        let mut state = self.source_state.lock().unwrap();
        if framerate > state.framerate {
            state.framerate = framerate;
        }
    }

    /// Tier-3 entry point, first half: the shared capture source is going
    /// down or changing, so **every** destination fed by it stalls at the same
    /// instant — no `StopStream`, no teardown, just the cached-IDR keepalive
    /// at ~2fps until frames are flowing again (see [`send_stall_keepalive`]).
    /// Callers must pair this with [`CaptureHandle::note_source_restarted`];
    /// nothing clears a stall on its own except a real IDR arriving.
    pub fn begin_stall(&self, reason: destination::StallReason) {
        let runtimes: Vec<destination::SharedDestination> =
            self.destinations.iter().map(|d| d.runtime.clone()).collect();
        branch::stall_all(&runtimes, reason);
    }

    /// Tier-3, second half: the source is producing again — re-anchor every
    /// destination's wire clock onto the pipeline's current running time so
    /// PTS stays monotonic across the gap (see
    /// [`destination::DestinationRuntime::on_pipeline_start`]). The stall
    /// itself ends when each destination's next real IDR is sent, not here —
    /// "the source is back" and "the viewer has something decodable again"
    /// are different events and only the second one should clear the
    /// placeholder.
    pub fn note_source_restarted(&self) {
        let running_time_us = self.pipeline.current_running_time().map(|t| t.useconds()).unwrap_or(0);
        let now = Instant::now();
        for destination in &self.destinations {
            destination.runtime.lock().unwrap().on_pipeline_start(running_time_us, now);
        }
    }
}

// ---------------------------------------------------------------------
// Encode → encrypt → fragment → send pump
// ---------------------------------------------------------------------

/// Sends a `gst_video::UpstreamForceKeyUnitEvent` into the pipeline from
/// `encoder_sink_pad` (the encoded-output appsink's sink pad — see
/// [`start_sharing_with_source`]'s doc comment on why that specific pad,
/// not `pipeline.send_event()`). Pushing an upstream event on a sink pad
/// delivers it to the peer pad (the encoder's src pad), which is exactly
/// where `GstVideoEncoder` — the base class both `x264enc` and `vah264enc`
/// build on — listens for force-key-unit requests: the very next buffer the
/// encoder outputs will not carry the `DELTA_UNIT` flag.
fn force_keyframe(encoder_sink_pad: &gst::Pad) {
    let event = gst_video::UpstreamForceKeyUnitEvent::builder().all_headers(true).build();
    if !encoder_sink_pad.push_event(event) {
        tracing::warn!("force-keyframe event was not accepted by the capture pipeline");
    }
}

/// Sets [`ENCODER_VALVE_NAME`]'s `drop` property and immediately reads it
/// back, warning loudly if it didn't actually take — a `GObject` property
/// write is normally infallible/synchronous for a simple boolean like this,
/// so a mismatch here would mean something is very wrong (e.g. the element
/// handle outlived/detached from its pipeline), not a transient timing
/// issue. This exists purely as a diagnostic backstop for exactly the class
/// of bug this function's callers were written to guard against — pausing
/// or resuming silently *appearing* to work (the call site runs, the log
/// fires) while the valve's real state diverges from `pump_loop`'s `paused`
/// bool — so if that ever happens again, it shows up as a loud warning
/// instead of a silent 35-second-bandwidth-flatline mystery.
fn set_valve_drop(encoder_valve: &gst::Element, drop: bool, stream_id: u8) {
    encoder_valve.set_property("drop", drop);
    let actual: bool = encoder_valve.property("drop");
    if actual != drop {
        tracing::warn!(
            "stream {stream_id}: encoder_valve's 'drop' property is {actual} right after setting it to {drop} — \
             the valve did not actually change state; the encoder branch is now out of sync with pump_loop's \
             paused/resumed bookkeeping"
        );
    }
}

/// Sliding-window size for [`CaptureHandle::encoder_fps`]'s live rate —
/// wide enough to smooth over natural per-frame jitter (encode time,
/// scheduler noise) but narrow enough that the displayed number reacts
/// quickly to a real change (e.g. the encoder pausing at 0 subscribers, or
/// content going idle/busy).
const ENCODER_FPS_WINDOW: Duration = Duration::from_secs(2);

/// Records that one encoded access unit just reached the appsink (called
/// from [`pump_loop`]'s `appsink_stream` arm) and recomputes
/// [`CaptureHandle::encoder_fps`]'s published rate from the count of
/// timestamps still inside [`ENCODER_FPS_WINDOW`]. `times` is `pump_loop`'s
/// own local sliding window — plain `VecDeque` push-back/pop-front, no lock
/// needed since only `pump_loop`'s single task ever touches it; `fps` is the
/// `Arc<AtomicU32>` shared out to [`CaptureHandle::encoder_fps`] for
/// lock-free reads from `ui.rs`.
fn record_encoder_frame(times: &mut VecDeque<Instant>, fps: &AtomicU32) {
    let now = Instant::now();
    times.push_back(now);
    while let Some(&oldest) = times.front() {
        if now.duration_since(oldest) > ENCODER_FPS_WINDOW {
            times.pop_front();
        } else {
            break;
        }
    }
    let rate = times.len() as f64 / ENCODER_FPS_WINDOW.as_secs_f64();
    fps.store(rate.round() as u32, Ordering::Relaxed);
}

/// Sliding-window size for [`CaptureHandle::sent_fps`]'s live rate. Same
/// width as [`ENCODER_FPS_WINDOW`] so the two numbers are directly
/// comparable at a glance — a gap between them is a real signal (send
/// errors), not an artifact of different smoothing.
const SENT_FPS_WINDOW: Duration = ENCODER_FPS_WINDOW;

/// Records that one encoded access unit was just handed off to the QUIC
/// connection successfully (i.e. [`send_media_payload`] returned `Ok`, called
/// from [`destination_pump`]'s appsink-sample arm right after
/// [`record_encoder_frame`]) and recomputes [`CaptureHandle::sent_fps`]'s
/// published rate — same sliding-window shape as `record_encoder_frame`, see
/// its doc comment.
fn record_sent_frame(times: &mut VecDeque<Instant>, fps: &AtomicU32) {
    let now = Instant::now();
    times.push_back(now);
    while let Some(&oldest) = times.front() {
        if now.duration_since(oldest) > SENT_FPS_WINDOW {
            times.pop_front();
        } else {
            break;
        }
    }
    let rate = times.len() as f64 / SENT_FPS_WINDOW.as_secs_f64();
    fps.store(rate.round() as u32, Ordering::Relaxed);
}

/// Everything one destination's [`destination_pump`] can be asked to do.
///
/// `Why a command channel rather than shared state the UI writes directly:`
/// a destination's encode branch is owned by exactly one task, and every
/// interesting operation on it (pause, force a keyframe, swap it for a
/// differently-configured one, drain and remove it) either mutates it or
/// consumes it. Routing all of them through one mailbox is what makes
/// "exactly one owner, no locks around GStreamer objects" true, and it is
/// what lets a swap be `await`ed inside the pump without the UI thread ever
/// blocking.
enum DestCmd {
    /// New subscriber count for this destination's stream. `0` pauses its
    /// encoder branch (via the branch's own `valve`) to save CPU/bandwidth
    /// while nobody is watching *this* stream; a transition from `0` to
    /// non-zero resumes it and forces a keyframe (see [`force_keyframe`]) so
    /// the newly (re)subscribed viewer's decoder has something to start from.
    SubscriberCount(u32),
    /// A new subscriber joined (relay-debounced). Forces a keyframe — and,
    /// if this branch happened to be paused, resumes it too (a
    /// `KeyframeRequest` implies at least one subscriber exists, regardless
    /// of whether its paired `SubscriberCount` update has arrived yet).
    KeyframeRequest,
    /// Tier 1 ([`destination::ChangeTier::Bitrate`]): live encoder property
    /// write, no lifecycle consequence at all.
    SetBitrate(u32),
    /// Tier 2 ([`destination::ChangeTier::BranchSwap`]): replace this
    /// destination's branch with one built for `params`, make-before-break.
    ApplyParams { params: destination::VideoParams, reply: tokio::sync::oneshot::Sender<Result<(), String>> },
    /// Drain and remove this destination's branch, tell its room the stream
    /// stopped, and exit.
    Stop { reply: tokio::sync::oneshot::Sender<Result<(), String>> },
}

/// The first error a capture pipeline's bus reported, kept so a destination
/// that fails to come up afterwards can say *why* ([`blame_the_source`]).
///
/// `Why (task #76):` a source that dies right after its first frames (measured:
/// the bundled `pipewiresrc` 1.4.2 against `xdg-desktop-portal-hyprland` ends
/// with `stream error: unhandled format` after the first buffer) leaves the
/// encode branch with no input. The only symptom `add_branch_live_waiting`
/// can see is "produced no IDR", i.e. it blames the encoder, and the real
/// reason went to the log alone. [`capture_loop`] is the one consumer of the
/// bus, so it records the error here and the destination reads it back.
#[derive(Clone, Default)]
struct CaptureErrors(Arc<Mutex<Option<String>>>);

impl CaptureErrors {
    /// Keeps the first error only: later ones are usually its consequences.
    fn record(&self, text: String) {
        self.0.lock().unwrap().get_or_insert(text);
    }

    fn first(&self) -> Option<String> {
        self.0.lock().unwrap().clone()
    }
}

/// `error` (a destination that failed to come up), led by the capture
/// pipeline's own recorded error when it has one: the source failing is the
/// cause, whatever the encoder then reported about starving.
fn blame_the_source(error: anyhow::Error, errors: &CaptureErrors) -> anyhow::Error {
    match errors.first() {
        Some(source) => error.context(format!("screen capture failed: {source}")),
        None => error,
    }
}

/// Per-**capture** background loop: watches the pipeline bus for error/EOS.
///
/// Deliberately holds nothing destination-scoped: with fan-out there are N
/// destinations per capture but still exactly one bus, so this loop exists
/// once per capture and every send/encode concern lives in
/// [`destination_pump`] instead.
async fn capture_loop(mut bus_stream: gst::bus::BusStream, errors: CaptureErrors) {
    let mut bus_done = false;

    while !bus_done {
        match bus_stream.next().await {
            Some(msg) => handle_bus_message(&msg, &mut bus_done, &errors),
            None => bus_done = true,
        }
    }
}

/// Per-**destination** loop: drains encoded access units off *this
/// destination's* encode branch, encrypts/fragments/sends them into *this
/// destination's* room, keeps the stall protocol's cached-IDR keepalive
/// going, and services the [`DestCmd`]s above (including branch swaps, which
/// it owns because it owns the branch).
///
/// One of these per room a capture feeds. Two destinations of the same
/// capture share nothing but the raw frames coming off the `tee`: separate
/// encoders, separate bitrates, separate room keys and nonce sequences,
/// separate `stream_id`s, separate lifecycle state. That is the whole
/// "confirmed intentional bandwidth multiplication" model from the plan doc,
/// made literal.
#[allow(clippy::too_many_arguments)]
async fn destination_pump(
    client: ScreenshareClient,
    stream_id: u8,
    pipeline: gst::Pipeline,
    tee: gst::Element,
    branch: branch::EncodeBranch,
    runtime: destination::SharedDestination,
    encoder_fps: Arc<AtomicU32>,
    sent_fps: Arc<AtomicU32>,
    mut cmd_rx: mpsc::UnboundedReceiver<DestCmd>,
) {
    let start = Instant::now();
    // The stall protocol's clock. Ticks unconditionally (a `select!` arm
    // can't be conditionally scheduled without re-creating the interval), but
    // `send_stall_keepalive` is a cheap early-return whenever the destination
    // isn't stalled — so this costs one wakeup every `STALL_RESEND_INTERVAL`.
    let mut stall_ticker = tokio::time::interval(destination::STALL_RESEND_INTERVAL);
    stall_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Sliding window of the `Instant`s each encoded access unit reached the
    // appsink, used to compute this destination's live encode rate — see
    // [`ENCODER_FPS_WINDOW`].
    let mut encoded_frame_times: VecDeque<Instant> = VecDeque::new();
    // Sliding window of the `Instant`s each encoded access unit was
    // successfully handed off to the QUIC connection — see [`SENT_FPS_WINDOW`].
    let mut sent_frame_times: VecDeque<Instant> = VecDeque::new();
    // (when, `offered_frames()`) samples of the current branch over
    // [`ENCODER_FPS_WINDOW`], for the offered rate the encoder-limit hint
    // compares against (#46, [`destination::EncoderLimit`]). Keyed by branch
    // id: a swapped-in branch's counter starts again at 0.
    let mut offered_samples: VecDeque<(Instant, u64)> = VecDeque::new();
    let mut offered_branch: Option<branch::BranchId> = None;
    // Mirrors this branch's valve so a `SubscriberCount` transition from one
    // non-zero count to another (e.g. 2 -> 1) doesn't spuriously re-force a
    // keyframe — only an actual 0 -> non-zero (or an explicit
    // `KeyframeRequest`) does.
    let mut paused = false;
    // `Option` purely so a swap can take the branch by value (that ownership
    // transfer *is* the appsink-reader handoff — see `branch::EncodeBranch`'s
    // doc comment) and put its replacement back. Never `None` across a
    // `select!` poll except inside the swap arm itself.
    let mut branch = Some(branch);
    // Set once this branch's appsink has yielded its terminal `None`, so the
    // sample arm stops being scheduled instead of spinning on a stream that
    // will only ever return `None` again.
    let mut branch_ended = false;
    // Distinct `BranchId`s per swap, purely for greppable element names.
    let mut next_branch_id: u32 = 1;

    loop {
        tokio::select! {
            sample = async { branch.as_mut().expect("guarded by branch.is_some()").next_sample().await },
                if branch.is_some() && !branch_ended =>
            {
                match sample {
                    Some(sample) => {
                        record_encoder_frame(&mut encoded_frame_times, &encoder_fps);
                        // A frame arriving while stalled is only a delta
                        // unless it happens to be a GOP boundary, and a delta
                        // cannot end the stall (the viewer has nothing
                        // decodable to apply it to) — so ask for an IDR now
                        // instead of waiting out the rest of the GOP.
                        let was_stalled = runtime.lock().unwrap().is_stalled();
                        let result = send_encoded_sample(&client, stream_id, &sample, start, &runtime);
                        if was_stalled
                            && runtime.lock().unwrap().is_stalled()
                            && let Some(pad) = branch.as_ref().and_then(|b| b.keyframe_pad())
                        {
                            force_keyframe(&pad);
                        }
                        match result {
                            Ok(()) => record_sent_frame(&mut sent_frame_times, &sent_fps),
                            Err(e) => tracing::warn!("dropping encoded frame for stream {stream_id}: {e:#}"),
                        }
                    }
                    None => branch_ended = true,
                }
            }
            _ = stall_ticker.tick() => {
                let now = Instant::now();
                match branch.as_ref().filter(|_| !paused && !branch_ended) {
                    Some(b) => {
                        if offered_branch != Some(b.id) {
                            offered_branch = Some(b.id);
                            offered_samples.clear();
                        }
                        let offered_now = b.offered_frames();
                        offered_samples.push_back((now, offered_now));
                        while offered_samples.len() > 2
                            && offered_samples.front().is_some_and(|(t, _)| now.duration_since(*t) > ENCODER_FPS_WINDOW)
                        {
                            offered_samples.pop_front();
                        }
                        let (t0, n0) = offered_samples.front().copied().expect("just pushed");
                        let secs = now.duration_since(t0).as_secs_f64();
                        if secs >= 1.0 {
                            let offered_fps = ((offered_now - n0) as f64 / secs).round() as u32;
                            runtime.lock().unwrap().observe_encoder_rate(now, encoder_fps.load(Ordering::Relaxed), offered_fps);
                        }
                    }
                    None => {
                        offered_samples.clear();
                        runtime.lock().unwrap().reset_encoder_limit();
                    }
                }
                if !paused && branch.is_some() && !branch_ended {
                    let mut runtime = runtime.lock().unwrap();
                    let silence = destination::silence_threshold(runtime.params().framerate);
                    if runtime.stall_if_silent(Instant::now(), silence) {
                        tracing::warn!(
                            "stream {stream_id}: no encoded frame for {silence:?}; re-sending the cached IDR until the capture produces again"
                        );
                    }
                }
                send_stall_keepalive(&client, stream_id, &runtime);
            }
            cmd = cmd_rx.recv() => {
                match cmd {
                    // Every sender is gone: the `CaptureHandle` was dropped
                    // without `stop()` (the bare-drop backstop path), so the
                    // pipeline is being torn down under us. Nothing useful is
                    // left to drain.
                    None => return,
                    Some(DestCmd::SubscriberCount(0)) => {
                        if !paused && let Some(branch) = branch.as_ref() {
                            tracing::info!("stream {stream_id} has 0 subscribers; pausing its encoder to save CPU/bandwidth");
                            branch.set_paused(true, stream_id);
                            paused = true;
                            // No more encoded frames will arrive while
                            // paused, so `record_encoder_frame`'s window
                            // won't naturally decay on its own — reset the
                            // published rate to 0 immediately rather than
                            // leaving it stuck reporting its last pre-pause
                            // value.
                            encoded_frame_times.clear();
                            encoder_fps.store(0, Ordering::Relaxed);
                            sent_frame_times.clear();
                            sent_fps.store(0, Ordering::Relaxed);
                        }
                    }
                    Some(DestCmd::SubscriberCount(_)) => {
                        if paused && let Some(branch) = branch.as_ref() {
                            tracing::info!("stream {stream_id} has a subscriber again; resuming its encoder");
                            branch.set_paused(false, stream_id);
                            paused = false;
                            runtime.lock().unwrap().restart_silence_clock(Instant::now());
                            // Resuming after a pause means the first frame
                            // back out of the encoder must be a keyframe —
                            // nothing decodable was produced while paused.
                            if let Some(pad) = branch.keyframe_pad() {
                                force_keyframe(&pad);
                            }
                        }
                    }
                    Some(DestCmd::KeyframeRequest) => {
                        if let Some(branch) = branch.as_ref() {
                            if paused {
                                // A subscribe implies at least one subscriber
                                // exists even if the paired `SubscriberCount`
                                // update hasn't arrived yet — don't leave the
                                // encoder paused (and the new subscriber with
                                // nothing to decode) waiting on message order.
                                tracing::info!("stream {stream_id} got a keyframe request while paused; resuming its encoder");
                                branch.set_paused(false, stream_id);
                                paused = false;
                                runtime.lock().unwrap().restart_silence_clock(Instant::now());
                            }
                            if let Some(pad) = branch.keyframe_pad() {
                                force_keyframe(&pad);
                            }
                        }
                    }
                    Some(DestCmd::SetBitrate(bitrate_kbps)) => {
                        if let Some(branch) = branch.as_ref() {
                            branch.set_bitrate_kbps(bitrate_kbps);
                            runtime.lock().unwrap().set_bitrate(bitrate_kbps);
                        }
                    }
                    Some(DestCmd::ApplyParams { params, reply }) => {
                        let Some(old) = branch.take() else {
                            let _ = reply.send(Err("destination has no live encode branch".to_string()));
                            continue;
                        };
                        let id = branch::BranchId(next_branch_id);
                        next_branch_id = next_branch_id.wrapping_add(1);
                        // `Why the whole swap is awaited here, inside the
                        // pump's own loop:` the branch must be owned by
                        // exactly one place, and this is it. The cost is that
                        // this destination's already-encoded samples aren't
                        // drained while the replacement branch is coming up
                        // (its appsink is `drop=true`, so they're discarded,
                        // not queued) — normally a few hundred milliseconds,
                        // bounded by `branch::FIRST_IDR_TIMEOUT`. The
                        // destination reports `Rebuilding` throughout, which
                        // is exactly what that state is for. Every *other*
                        // destination on this capture keeps sending
                        // untouched, since each has its own pump.
                        match branch::swap_to_new_branch(&pipeline, &tee, &runtime, old, id, params).await {
                            branch::SwapOutcome::Swapped(new_branch) => {
                                // The new branch's own first IDR was consumed
                                // by the swap's readiness check, so ask for
                                // another one rather than leaving the viewer
                                // decoding deltas against a GOP it never got.
                                if let Some(pad) = new_branch.keyframe_pad() {
                                    force_keyframe(&pad);
                                }
                                if paused {
                                    // A replacement branch is born open; carry
                                    // the destination's paused state across.
                                    new_branch.set_paused(true, stream_id);
                                }
                                branch = Some(new_branch);
                                branch_ended = false;
                                runtime.lock().unwrap().restart_silence_clock(Instant::now());
                                let _ = reply.send(Ok(()));
                            }
                            branch::SwapOutcome::KeptOld(old, e) => {
                                branch = Some(old);
                                runtime.lock().unwrap().restart_silence_clock(Instant::now());
                                let _ = reply.send(Err(format!("{e:#}")));
                            }
                        }
                    }
                    Some(DestCmd::Stop { reply }) => {
                        let mut result = Ok(());
                        if let Some(branch) = branch.take()
                            && let Err(e) = branch::remove_branch(&pipeline, &tee, branch).await
                        {
                            result = Err(format!("{e:#}"));
                        }
                        if let Err(e) = client.stop_stream(stream_id).await {
                            tracing::warn!("error notifying relay that stream {stream_id} stopped: {e:#}");
                        }
                        let _ = reply.send(result);
                        return;
                    }
                }
            }
        }
    }
}

fn handle_bus_message(msg: &gst::Message, bus_done: &mut bool, errors: &CaptureErrors) {
    match msg.view() {
        gst::MessageView::Error(err) => {
            tracing::error!(
                "capture pipeline error from {}: {} ({})",
                msg.src().map(|s| s.path_string().to_string()).unwrap_or_else(|| "unknown".into()),
                err.error(),
                err.debug().unwrap_or_default(),
            );
            errors.record(format!(
                "{}: {}",
                msg.src().map(|s| s.name().to_string()).unwrap_or_else(|| "unknown".into()),
                err.error()
            ));
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
/// [`crate::net::NonceSequence`]), then split via
/// `fragment::fragment_frame_with_flags` using the frame's nonce counter as
/// `frame_id` — the receiver reconstructs the same nonce from
/// `(conn_id, frame_id)`, so these must match.
///
/// `wire_pts_us` must already have been through
/// [`destination::DestinationRuntime`]'s monotonic clamp — this function
/// deliberately takes a finished wire PTS rather than a raw buffer PTS, so
/// there is exactly one place in this file where a timestamp can reach the
/// wire and exactly one place the invariant can be broken.
fn send_media_payload(
    client: &ScreenshareClient,
    stream_id: u8,
    payload: &[u8],
    wire_pts_us: u64,
    keyframe: bool,
    stalled: bool,
) -> Result<()> {
    let nonce_seq = client.nonce_sequence();
    let (nonce, counter) = nonce_seq
        .next_nonce(stream_id)
        .map_err(|e| anyhow!("{e}"))
        .context("cannot send another frame")?;

    let room_key = client.room_key();
    let ciphertext =
        encrypt_frame(&room_key, stream_id, nonce, wire_pts_us, payload).context("failed to encrypt frame")?;

    let connection = client.connection();
    let max_datagram_size = connection
        .max_datagram_size()
        .ok_or_else(|| anyhow!("peer does not support QUIC datagrams"))?;

    // `Why:` the `stalled` bit rides the media header's cleartext `flags`
    // byte, which is **outside** the AEAD's AAD (`net.rs`'s `aead_encrypt`
    // authenticates only `[version, stream_id]`, because the relay must be
    // able to read and route the header without the room key — spec §6.2). It
    // is therefore advisory only: a UI hint for the viewer's
    // "waiting for capture…" placeholder, nothing more. Never let it drive
    // teardown, (un)subscribe, or any other decision with a security or
    // protocol consequence — see `protocol::MEDIA_FLAG_STALLED`. Note what
    // keeps that safe: a stalled destination keeps sending *real,
    // authenticated* frames and never sends `StopStream`, so the viewer's
    // actual media state never depends on this bit being truthful.
    let mut flags = 0u8;
    if keyframe {
        flags |= crate::protocol::MEDIA_FLAG_KEYFRAME;
    }
    if stalled {
        flags |= crate::protocol::MEDIA_FLAG_STALLED;
    }

    let datagrams =
        fragment_frame_with_flags(client.conn_id(), stream_id, counter, flags, &ciphertext, max_datagram_size)
            .context("failed to fragment encrypted frame")?;

    for datagram in datagrams {
        connection
            .send_datagram(datagram.into())
            .map_err(|e| anyhow!("failed to send media datagram: {e}"))?;
    }

    Ok(())
}

/// Takes one encoded access unit off the appsink, maps it onto this
/// destination's wire clock, records it in the [`destination::DestinationRuntime`]
/// (caching IDRs for the stall protocol and driving the first-IDR
/// `Starting`/`Stalled` → `Live` transitions), and sends it.
fn send_encoded_sample(
    client: &ScreenshareClient,
    stream_id: u8,
    sample: &gst::Sample,
    start: Instant,
    runtime: &destination::SharedDestination,
) -> Result<()> {
    let buffer = sample.buffer().ok_or_else(|| anyhow!("sample had no buffer"))?;

    // Presentation timestamp in microseconds, per protocol-spec §6.1's
    // 8-byte PTS prefix. `do-timestamp=true` on pipewiresrc gives buffers a
    // pipeline running-time PTS; fall back to wall-clock-since-start on the
    // rare buffer that arrives without one rather than dropping the frame.
    let buffer_pts_us = buffer
        .pts()
        .map(|t| t.useconds())
        .unwrap_or_else(|| start.elapsed().as_micros() as u64);

    let keyframe = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);

    let map = buffer.map_readable().context("failed to map encoded H.264 access unit")?;
    let payload = map.as_slice();

    let (wire_pts_us, stalled) = {
        let mut runtime = runtime.lock().unwrap();
        // Note the frame *before* reading `is_stalled`: a real frame arriving
        // is precisely what ends a stall, so a frame that itself ends the
        // stall must not still carry the advisory "stalled" bit.
        runtime.note_encoded_frame(keyframe, payload, Instant::now());
        (runtime.wire_pts_from_buffer(buffer_pts_us), runtime.is_stalled())
    };

    send_media_payload(client, stream_id, payload, wire_pts_us, keyframe, stalled)
}

/// The stall protocol's send half: while a destination is
/// [`destination::DestinationState::Stalled`], re-send its cached IDR at
/// [`destination::STALL_RESEND_INTERVAL`], freshly re-encrypted (new nonce,
/// per protocol §5 — a nonce is never reused, not even for identical bytes)
/// and with a wire PTS that keeps advancing in real time.
///
/// `Why re-send at all, instead of just going quiet:` going quiet is
/// indistinguishable from a crashed sender. Re-sending a decodable IDR keeps
/// the viewer's decoder fed and its stream subscription alive, keeps
/// `decode.rs`'s no-frames-in-3s backstop from firing, and — because the PTS
/// keeps advancing on the destination's own unbroken clock — makes the outage
/// render as a stretch of very low framerate rather than a discontinuity the
/// viewer's pacer has to hard-reset through. Nothing is torn down: no
/// `StopStream` is sent, so the viewer never sees a disconnect/resubscribe.
fn send_stall_keepalive(client: &ScreenshareClient, stream_id: u8, runtime: &destination::SharedDestination) {
    let now = Instant::now();
    let Some((payload, wire_pts_us)) = ({
        let mut runtime = runtime.lock().unwrap();
        runtime.stall_resend_due(now).map(|idr| {
            let pts = runtime.wire_pts_from_epoch(now);
            (idr, pts)
        })
    }) else {
        return;
    };

    if let Err(e) = send_media_payload(client, stream_id, &payload, wire_pts_us, true, true) {
        tracing::warn!("stream {stream_id}: failed to re-send the cached IDR while stalled: {e:#}");
    }
}

// ---------------------------------------------------------------------
// Per-destination lifecycle: state machine, wire-PTS continuity, stall
// protocol, and the three-tier change model
// ---------------------------------------------------------------------

/// Everything that is genuinely *per outgoing destination* rather than per
/// capture pipeline: the [`DestinationState`] machine, the monotonic wire-PTS
/// invariant that survives a capture restart, the cached-IDR stall protocol,
/// and the pure functions classifying a settings change into one of the
/// plan doc's three tiers.
///
/// Deliberately free of GStreamer types (only [`EncoderBackend`] leaks in,
/// and that's a plain enum): everything here is ordinary Rust state that can
/// be unit-tested against injected `Instant`s with no pipeline, no portal, and
/// no clock of its own. The GStreamer half — building/swapping/removing the
/// branch this state describes — lives in [`branch`], and the two are joined
/// by [`branch::swap_to_new_branch`]/[`branch::rebuild_after_source_restart`].
///
/// See `superdoc/architecture/overview.md`'s "Per-destination lifecycle"
/// section and `superdoc/brainstorm/ui-redesign-multiroom-streaming.md`.
pub mod destination {
    use super::*;

    /// How often a `Stalled` destination re-sends its cached IDR (~2fps, per
    /// the plan doc). Slow enough to cost almost nothing while a capture is
    /// down, frequent enough that the viewer's decoder keeps receiving
    /// decodable frames and never times its stream out.
    pub const STALL_RESEND_INTERVAL: Duration = Duration::from_millis(500);

    /// How long a `Live` destination's branch may go without producing an
    /// access unit before it counts as stalled (see
    /// [`DestinationRuntime::stall_if_silent`]). Floored at three frame
    /// periods so a deliberately low framerate is never mistaken for silence.
    pub fn silence_threshold(framerate: u32) -> Duration {
        let three_frames = Duration::from_millis(3000 / u64::from(framerate.max(1)));
        three_frames.max(Duration::from_secs(3))
    }

    /// Why a destination is in [`DestinationState::Stalled`]. Only ever a
    /// *capture-source-level* cause: per the plan doc's three-tier model,
    /// nothing a single destination can ask for on its own is allowed to reach
    /// this state (see [`ChangeTier`]).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum StallReason {
        /// The user switched which monitor/window is captured, or raised a
        /// destination's fps requirement above the source's current rate — a
        /// deliberate, expected restart of the shared capture source.
        SourceChanged,
        /// The capture source died on its own (portal session revoked, device
        /// unplugged, pipeline error) and is being brought back.
        SourceLost,
    }

    /// Lifecycle of one outgoing destination. Exactly one variant —
    /// [`DestinationState::Stalled`] — is visible to the viewer; every other
    /// transition is designed to be invisible (see the plan doc's three-tier
    /// change model, and [`ChangeTier`] below).
    #[derive(Debug, Clone, PartialEq)]
    pub enum DestinationState {
        /// The branch has been built but hasn't emitted its first IDR yet, so
        /// nothing decodable has reached the viewer. Entered exactly once, at
        /// destination creation.
        Starting,
        /// Producing frames normally.
        Live,
        /// A branch swap is in flight (new resolution/fps/encoder backend for
        /// *this* destination only). The **old** branch is still authoritative
        /// and still sending, so this is invisible to the viewer — it exists
        /// so the UI can show "applying…" and so a swap that never completes
        /// is diagnosable. Never becomes `Stalled`: a failed swap keeps the
        /// old branch (see [`branch::SwapOutcome`]).
        Rebuilding { since: Instant },
        /// The shared capture source is down. The only viewer-visible state.
        /// While here the stream is *never* torn down (no `StopStream`); the
        /// last cached IDR is re-sent at [`STALL_RESEND_INTERVAL`] with
        /// [`crate::protocol::MEDIA_FLAG_STALLED`] set, so the viewer sees a
        /// frozen frame at a low framerate rather than a disconnect.
        Stalled { since: Instant, reason: StallReason },
        /// Unrecoverable: this destination will not produce frames again
        /// without user action.
        Failed(String),
    }

    impl DestinationState {
        /// The one state the viewer can tell apart from healthy streaming.
        pub fn is_stalled(&self) -> bool {
            matches!(self, DestinationState::Stalled { .. })
        }

        /// Short, stable identifier for logs/tests (and, later, UI copy keys).
        pub fn label(&self) -> &'static str {
            match self {
                DestinationState::Starting => "starting",
                DestinationState::Live => "live",
                DestinationState::Rebuilding { .. } => "rebuilding",
                DestinationState::Stalled { .. } => "stalled",
                DestinationState::Failed(_) => "failed",
            }
        }
    }

    /// One destination's encode settings. Independent of every other
    /// destination fed by the same capture — that independence is exactly what
    /// [`branch::add_branch`]'s per-branch `videoscale`/`videorate`/capsfilter
    /// chain buys.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct VideoParams {
        pub width: u32,
        pub height: u32,
        pub framerate: u32,
        pub bitrate_kbps: u32,
        pub backend: EncoderBackend,
        /// Low-latency vs. best-quality — [`super::TuneMode`]. Software-backend
        /// only; a no-op while [`Self::backend`] is [`EncoderBackend::Vaapi`].
        pub tune: TuneMode,
        /// Performance vs. quality encode speed — [`super::QualityLevel`].
        /// Real on both backends.
        pub quality_level: QualityLevel,
        /// CQP/QVBR rate control — [`super::RateControlMode`]. Native on
        /// [`EncoderBackend::Vaapi`]; see that type's doc comment for how
        /// each variant maps on [`EncoderBackend::Software`].
        pub rate_control: RateControlMode,
        /// Keyframe/GOP interval in milliseconds — [`super::DEFAULT_GOP_MILLIS`]
        /// for every preset except Gaming, which uses a shorter interval for
        /// faster recovery from motion artifacts (see that constant's doc
        /// comment).
        pub gop_millis: u32,
        /// H.264/H.265/AV1 — [`super::VideoCodec`]. Only meaningful on
        /// [`EncoderBackend::Vaapi`]; [`EncoderBackend::Software`] silently
        /// always produces H.264 — see [`super::EncoderBackend::actual_codec`].
        pub codec: VideoCodec,
    }

    impl VideoParams {
        pub fn to_encode_settings(self) -> branch::EncodeSettings {
            branch::EncodeSettings {
                width: self.width,
                height: self.height,
                framerate: self.framerate,
                bitrate_kbps: self.bitrate_kbps,
                backend: self.backend,
                tune: self.tune,
                quality_level: self.quality_level,
                rate_control: self.rate_control,
                gop_millis: self.gop_millis,
                codec: self.codec,
            }
        }
    }

    /// What the shared capture source is actually producing right now: its
    /// native frame size and the rate the source element is running at (which
    /// is `max(fps)` across all its destinations — see
    /// [`required_source_framerate`]).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct SourceCaps {
        pub width: u32,
        pub height: u32,
        pub framerate: u32,
    }

    /// The plan doc's three-tier change model, plus a no-op tier. Returned by
    /// [`classify_change`]; determines how much machinery a settings change
    /// costs and — critically — whether the viewer can tell.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ChangeTier {
        /// Nothing to do.
        Unchanged,
        /// Tier 1 — bitrate only: a live property write on the running
        /// encoder. No branch, no state change, zero viewer impact.
        Bitrate,
        /// Tier 2 — resolution/fps/encoder-backend for *one* destination:
        /// build a new branch, wait for its first IDR, flip, drop the old one
        /// ([`branch::swap_to_new_branch`]). `Rebuilding` → `Live`; the viewer
        /// sees nothing. Other destinations on the same capture are mechanically
        /// untouched (their own `queue`s isolate them).
        BranchSwap,
        /// Tier 3 — the capture **source** must change (different monitor/window,
        /// or an fps requirement above what the source currently runs at). The
        /// only tier that stalls, and it stalls **every** destination sharing
        /// that capture at once — accepted per the plan doc, on the condition
        /// that it looks like a framerate dip, not a broken stream (which is
        /// what the stall protocol + PTS invariant below deliver).
        SourceRestart,
    }

    /// Which tier a requested settings change falls into, given what the
    /// shared source is currently producing.
    ///
    /// `Why:` the fps rule is the non-obvious one and is checked *first*.
    /// Anything at or below `source.framerate` is servable entirely inside
    /// this destination's own branch, because that branch already has its own
    /// `videorate` doing the downconversion ([`branch::add_branch`]) — so it's
    /// a tier-2 swap. Going *above* it cannot be: `videorate` can only
    /// duplicate frames the source actually delivered, so a higher rate has to
    /// come from the source element itself, which means restarting it — and
    /// that restart is shared, so it hits every co-located destination. This
    /// asymmetry (down is free, up is expensive) is the whole reason the
    /// capture runs at `max(fps)` across its destinations rather than at any
    /// single destination's rate.
    pub fn classify_change(source: SourceCaps, from: VideoParams, to: VideoParams) -> ChangeTier {
        if to.framerate > source.framerate {
            return ChangeTier::SourceRestart;
        }
        if to.width != from.width
            || to.height != from.height
            || to.framerate != from.framerate
            || to.backend != from.backend
            // `tune`/`quality_level`/`rate_control` are never live-writable
            // (see [`super::TuneMode`]/[`super::QualityLevel`]/
            // [`super::RateControlMode`]'s doc comments) — even though
            // vah264enc's `target-usage`/`rate-control` technically are,
            // routing every backend/control through the same `BranchSwap`
            // path keeps behavior uniform and testable across both. A fast
            // follow could special-case VAAPI to a true live write later.
            || to.tune != from.tune
            || to.quality_level != from.quality_level
            || to.rate_control != from.rate_control
            // `gop_millis` (`key-int-max`) is likewise not a live-writable
            // encoder property — a preset switch to/from Gaming's shorter
            // GOP is a `BranchSwap` like every other tuning knob above.
            || to.gop_millis != from.gop_millis
            // A codec change swaps the encoder *element* itself (`vah264enc`
            // -> `vah265enc`, etc.) — there is no live property that could
            // ever make that a tier-1 write, so this is unconditionally a
            // `BranchSwap` on both backends.
            || to.codec != from.codec
        {
            return ChangeTier::BranchSwap;
        }
        if to.bitrate_kbps != from.bitrate_kbps {
            return ChangeTier::Bitrate;
        }
        ChangeTier::Unchanged
    }

    /// **Never upscale**: clamp a requested resolution down to at most the
    /// capture's native size, and the framerate into the pipeline's supported
    /// range. Framerate is deliberately *not* clamped to the source's rate —
    /// asking for more than the source runs at is legal, it's just a
    /// [`ChangeTier::SourceRestart`] rather than a swap.
    ///
    /// `Why:` (clamping proportionally rather than per-axis) upscaling wastes
    /// encode budget on invented pixels and never adds detail, so it's simply
    /// disallowed — but clamping each axis independently would silently change
    /// the aspect ratio when only one axis overshoots (e.g. 3840x1080 requested
    /// off a 1920x1080 capture would become 1920x1080, stretching the image).
    /// Scaling both axes by the single tightest overshoot ratio keeps the
    /// picture geometrically correct, which matters because the branch's
    /// `videoscale` will happily distort otherwise.
    pub fn clamp_to_source(params: VideoParams, source: SourceCaps) -> VideoParams {
        let mut out = VideoParams {
            framerate: params.framerate.clamp(MIN_FRAMERATE, MAX_FRAMERATE),
            ..params
        };

        if source.width == 0 || source.height == 0 || out.width == 0 || out.height == 0 {
            // Source size not negotiated yet (or a caller asking for
            // "whatever the source is"): nothing to clamp against.
            return out;
        }

        if out.width > source.width || out.height > source.height {
            // Rational arithmetic in u64 rather than floats: exact, and no
            // rounding surprise that could leave a dimension one pixel over.
            let (w, h) = (out.width as u64, out.height as u64);
            let (sw, sh) = (source.width as u64, source.height as u64);
            // Pick the axis that overshoots most: scale = min(sw/w, sh/h).
            let (num, den) = if sw * h <= sh * w { (sw, w) } else { (sh, h) };
            out.width = ((w * num) / den).max(1) as u32;
            out.height = ((h * num) / den).max(1) as u32;
        }

        out
    }

    /// The rate the shared capture source must run at to serve every
    /// destination: the maximum any of them needs. Everything below that is
    /// downconverted per-branch by `videorate`, costing nothing extra at the
    /// source.
    pub fn required_source_framerate(dest_framerates: impl IntoIterator<Item = u32>) -> u32 {
        dest_framerates
            .into_iter()
            .max()
            .unwrap_or(DEFAULT_FRAMERATE)
            .clamp(MIN_FRAMERATE, MAX_FRAMERATE)
    }

    /// How long the encoder must stay below (or back above) the bar before
    /// [`EncoderLimit`] changes its verdict (#46: "for more than 3 s").
    pub const ENCODER_LIMIT_HOLD: Duration = Duration::from_secs(3);

    /// The encoder counts as limiting when it produces less than this
    /// percentage of the frames it is offered (#46: "more than 10% below").
    pub const ENCODER_LIMIT_PERCENT: u32 = 90;

    /// A destination's encoder is the bottleneck: it encodes `encoding_fps`
    /// of the `target_fps` the destination asks for. See [`EncoderLimit`].
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct EncoderLimited {
        pub encoding_fps: u32,
        pub target_fps: u32,
    }

    /// Sustained "the encoder can't keep up" detector (#46), measured at run
    /// time, no per-GPU tables.
    ///
    /// Compares the encoder's output rate against what it was *offered*:
    /// `min(frames reaching the branch, target fps)`. `Why` not the target
    /// alone: the portal only sends a frame when the screen changes, so an
    /// idle or 60 Hz screen under a 240 fps target would read as an encoder
    /// limit. Frames reaching the branch are counted before its leaky queue,
    /// i.e. before an overloaded encoder makes that queue drop them.
    ///
    /// The verdict flips only after [`ENCODER_LIMIT_HOLD`] of continuous
    /// evidence in either direction, so a rate hovering at the 90 % line
    /// doesn't flicker the hint.
    #[derive(Debug, Clone, Default)]
    pub struct EncoderLimit {
        limited: bool,
        /// Since when the evidence has contradicted `limited`.
        flip_since: Option<Instant>,
        encoding_fps: u32,
        target_fps: u32,
    }

    impl EncoderLimit {
        /// One observation: the encoder produced `encoder_fps` while
        /// `offered_fps` frames/s reached the branch, against `target_fps`.
        pub fn observe(&mut self, now: Instant, encoder_fps: u32, offered_fps: u32, target_fps: u32) {
            let expected = offered_fps.min(target_fps);
            let below = expected > 0 && u64::from(encoder_fps) * 100 < u64::from(expected) * u64::from(ENCODER_LIMIT_PERCENT);
            self.encoding_fps = encoder_fps;
            self.target_fps = target_fps;
            if below == self.limited {
                self.flip_since = None;
                return;
            }
            let since = *self.flip_since.get_or_insert(now);
            if now.duration_since(since) >= ENCODER_LIMIT_HOLD {
                self.limited = below;
                self.flip_since = None;
            }
        }

        /// Forget everything (paused, rebuilt, not Live): a new encoder or a
        /// paused one says nothing about the old one's speed.
        pub fn reset(&mut self) {
            *self = Self::default();
        }

        pub fn verdict(&self) -> Option<EncoderLimited> {
            self.limited.then_some(EncoderLimited { encoding_fps: self.encoding_fps, target_fps: self.target_fps })
        }
    }

    /// Shared handle onto one destination's [`DestinationRuntime`]. The pump
    /// task ([`super::pump_loop`]) writes it as frames flow; `ui.rs` reads the
    /// state to render; the swap/restart orchestrators in [`branch`] drive its
    /// transitions. A plain `Mutex` (not a channel): this is last-value state,
    /// and every writer holds it for a few field assignments at most.
    pub type SharedDestination = Arc<Mutex<DestinationRuntime>>;

    /// Per-destination lifecycle state: the [`DestinationState`] machine, the
    /// monotonic wire-PTS clock, and the stall protocol's cached IDR.
    ///
    /// # The wire-PTS invariant
    ///
    /// `Why:` a GStreamer pipeline's running time restarts at 0 every time the
    /// pipeline is (re)built, so sending buffer PTS straight onto the wire
    /// makes a capture restart look like the stream jumped *backwards in time*
    /// — which a viewer-side pacer (`decode.rs`'s `PacingState`) can only read
    /// as a discontinuity to hard-reset on, i.e. a visible glitch. Making wire
    /// PTS a property of the **destination** instead fixes that: [`Self::pts_epoch`]
    /// is set once, at destination creation, and never moves; [`Self::pts_offset_us`]
    /// is recomputed on every pipeline (re)start so that the new pipeline's
    /// running time maps back onto that same unbroken timeline. A restart then
    /// reads to the viewer as *a period of low framerate* (few frames arrived
    /// during the gap) rather than a break — exactly the illusion the stall
    /// protocol is built to sustain.
    ///
    /// The `max(…, last_sent + 1)` floor in [`Self::wire_pts_from_buffer`] is a
    /// **hard clamp, not a formula**: it guarantees monotonicity even if the
    /// offset arithmetic is wrong, if a buffer arrives with a stale PTS from
    /// before a restart, or if a stall re-send interleaves with a real frame.
    /// Nothing else in this file may hand a PTS to the wire without going
    /// through it.
    pub struct DestinationRuntime {
        state: DestinationState,
        /// Fixed at creation; the zero point of this destination's wire clock.
        /// Never reassigned — reassigning it is exactly the bug this design
        /// exists to prevent.
        pts_epoch: Instant,
        /// Recomputed on every pipeline/branch (re)start, per
        /// [`Self::on_pipeline_start`].
        pts_offset_us: u64,
        /// The last PTS actually put on the wire, if any. `None` before the
        /// first send so the very first frame is free to be PTS 0.
        last_sent_pts_us: Option<u64>,
        /// Last IDR seen from the authoritative branch, re-sent while stalled.
        last_idr: Option<Vec<u8>>,
        last_stall_resend: Option<Instant>,
        /// When the authoritative branch last produced an access unit; the
        /// input to [`Self::stall_if_silent`].
        last_frame_at: Option<Instant>,
        params: VideoParams,
        /// Fed by [`super::destination_pump`]; read by the UI (#46).
        encoder_limit: EncoderLimit,
    }

    impl DestinationRuntime {
        /// A brand-new destination: [`DestinationState::Starting`] until its
        /// first IDR, with its wire clock's epoch anchored at `now`.
        pub fn new(now: Instant, params: VideoParams) -> Self {
            Self {
                state: DestinationState::Starting,
                pts_epoch: now,
                pts_offset_us: 0,
                last_sent_pts_us: None,
                last_idr: None,
                last_stall_resend: None,
                last_frame_at: None,
                params,
                encoder_limit: EncoderLimit::default(),
            }
        }

        /// `Some` while this `Live` destination's encoder has been the
        /// bottleneck for a while — see [`EncoderLimit`]. The UI's
        /// "Encoding N of M fps" hint.
        pub fn encoder_limited(&self) -> Option<EncoderLimited> {
            if self.state != DestinationState::Live {
                return None;
            }
            self.encoder_limit.verdict()
        }

        /// Feed [`EncoderLimit`] one rate observation (the pump does, every
        /// [`STALL_RESEND_INTERVAL`]). Outside `Live` the tracker resets.
        pub fn observe_encoder_rate(&mut self, now: Instant, encoder_fps: u32, offered_fps: u32) {
            if self.state != DestinationState::Live {
                self.encoder_limit.reset();
                return;
            }
            let target = self.params.framerate;
            self.encoder_limit.observe(now, encoder_fps, offered_fps, target);
        }

        /// See [`EncoderLimit::reset`].
        pub fn reset_encoder_limit(&mut self) {
            self.encoder_limit.reset();
        }

        pub fn shared(now: Instant, params: VideoParams) -> SharedDestination {
            Arc::new(Mutex::new(Self::new(now, params)))
        }

        pub fn state(&self) -> DestinationState {
            self.state.clone()
        }

        pub fn is_stalled(&self) -> bool {
            self.state.is_stalled()
        }

        pub fn params(&self) -> VideoParams {
            self.params
        }

        /// Tier-1 change ([`ChangeTier::Bitrate`]): record the new bitrate.
        /// Deliberately does **not** touch [`Self::state`] — a live encoder
        /// property write has no lifecycle consequence at all, which is the
        /// entire point of it being its own tier.
        pub fn set_bitrate(&mut self, bitrate_kbps: u32) {
            self.params.bitrate_kbps = bitrate_kbps;
        }

        /// Record the params a completed branch swap actually applied.
        pub fn set_params(&mut self, params: VideoParams) {
            self.params = params;
        }

        pub fn pts_offset_us(&self) -> u64 {
            self.pts_offset_us
        }

        pub fn last_sent_pts_us(&self) -> Option<u64> {
            self.last_sent_pts_us
        }

        /// Re-anchor the wire clock onto a freshly (re)started pipeline:
        /// `pts_offset_us = pts_epoch.elapsed() - pipeline_running_time`.
        /// Call this **before** any buffer from the new pipeline reaches
        /// [`Self::wire_pts_from_buffer`].
        ///
        /// `Why saturating_sub`: the subtraction is conceptually signed (a
        /// pipeline whose running time already exceeds this destination's own
        /// age — possible when a destination is added to a capture that has
        /// been running for a while — yields a negative offset). Rather than
        /// carry a signed offset through every arithmetic path, this saturates
        /// to 0 and lets the monotonic floor in [`Self::wire_pts_from_buffer`]
        /// absorb the difference: PTS then advances at +1µs per frame for as
        /// long as the raw values lag, which is ugly but *monotonic*, and
        /// recovers by itself. Correctness here rests on the clamp, never on
        /// this arithmetic — see the type's doc comment.
        pub fn on_pipeline_start(&mut self, pipeline_running_time_us: u64, now: Instant) {
            let epoch_elapsed_us = now.saturating_duration_since(self.pts_epoch).as_micros() as u64;
            self.pts_offset_us = epoch_elapsed_us.saturating_sub(pipeline_running_time_us);
        }

        /// Re-anchor the wire clock onto a **replacement encode branch's** own
        /// timeline, given the PTS of the first buffer that branch produced.
        /// Call this at the swap's flip, before any buffer from the new branch
        /// can reach [`Self::wire_pts_from_buffer`] —
        /// [`branch::swap_to_new_branch`] does.
        ///
        /// `Why a swap needs this at all — the second half of the "changing
        /// the preset ... sometimes crashes the stream" bug:` a branch added
        /// to an already-running pipeline does **not** start its output
        /// timeline at the pipeline's current running time. Its `videorate`
        /// anchors on the inherited segment instead, so the replacement branch
        /// begins emitting PTS from roughly where the *capture* started —
        /// measured on this project's own real desktop capture as a first
        /// buffer 10.1s behind the running time on a capture that had been up
        /// 10.1s (`applying_a_preset_never_bursts_a_branch_above_its_configured_framerate`'s
        /// instrumentation), i.e. the lag is exactly the share's age.
        ///
        /// Without re-anchoring, every one of those buffers maps to a wire PTS
        /// below the last one already sent, so [`Self::advance_pts`]'s
        /// monotonic floor takes over and the stream advances **+1µs per
        /// frame** until the branch's raw PTS climbs back past where the old
        /// branch left off — i.e. for the entire age of the share. On a
        /// minutes-old share that is minutes of frames arriving effectively
        /// simultaneously as far as the viewer's pacer is concerned. The
        /// clamp is doing its job (nothing ever goes backwards), but "ugly and
        /// recovers by itself" only holds for a lag of milliseconds, which is
        /// what [`Self::on_pipeline_start`] was written for.
        ///
        /// `Why the branch's first buffer PTS and not the pipeline's running
        /// time:` the whole point is that those two are *not* the same for a
        /// mid-stream branch. Anchoring on what the branch actually produced
        /// works on either timeline.
        ///
        /// `Why this corrects the offset only when the seam actually goes
        /// backwards, instead of recomputing it unconditionally like
        /// [`Self::on_pipeline_start`]:` a replacement branch whose timeline
        /// *does* line up with the wire clock needs no correction at all, and
        /// recomputing anyway would move the offset — forwards, silently
        /// jumping the viewer's clock, for no reason. The only thing broken
        /// here is a seam that lands in the past, so that is the only thing
        /// this touches.
        ///
        /// `Why `last_sent_pts_us` is the anchor rather than the epoch:` the
        /// epoch form ([`Self::on_pipeline_start`]) subtracts the branch's
        /// timestamp from this destination's age, which needs the branch's
        /// numbers to be small — and a capture source is free to hand out
        /// enormous PTS values (this project's own test source stamps buffers
        /// ~41 days in), where that subtraction saturates to zero and silently
        /// does nothing. Anchoring on the last PTS actually put on the wire is
        /// arithmetic on two values from the same scale, so it can't saturate
        /// away, and it is also exactly the quantity the seam has to be
        /// continuous with.
        pub fn on_branch_swapped(&mut self, first_buffer_pts_us: u64) {
            let Some(last_sent_us) = self.last_sent_pts_us else {
                // Nothing has reached the wire yet, so there is no seam to
                // bridge — whatever the new branch emits first is free to be
                // this destination's first PTS.
                return;
            };
            if first_buffer_pts_us.saturating_add(self.pts_offset_us) < last_sent_us {
                self.pts_offset_us = last_sent_us.saturating_sub(first_buffer_pts_us);
            }
        }

        /// Wire PTS for a real captured buffer:
        /// `max(buffer_pts + offset, last_sent + 1)`.
        pub fn wire_pts_from_buffer(&mut self, buffer_pts_us: u64) -> u64 {
            self.advance_pts(buffer_pts_us.saturating_add(self.pts_offset_us))
        }

        /// Wire PTS for a synthesized send that has no buffer of its own —
        /// i.e. a stall re-send of the cached IDR. Derived from the same
        /// epoch, so a stalled stream's PTS keeps advancing in real time and
        /// the gap reads as low framerate rather than a frozen clock.
        pub fn wire_pts_from_epoch(&mut self, now: Instant) -> u64 {
            let elapsed_us = now.saturating_duration_since(self.pts_epoch).as_micros() as u64;
            self.advance_pts(elapsed_us)
        }

        /// The single monotonicity gate — see the type's doc comment.
        fn advance_pts(&mut self, candidate_us: u64) -> u64 {
            let pts = match self.last_sent_pts_us {
                Some(last) => candidate_us.max(last.saturating_add(1)),
                None => candidate_us,
            };
            self.last_sent_pts_us = Some(pts);
            pts
        }

        /// One encoded access unit reached the wire from the **authoritative**
        /// branch. Caches IDRs for the stall protocol and drives the
        /// first-IDR transitions.
        ///
        /// `Why:` this only transitions out of `Starting`/`Stalled`, never out
        /// of `Rebuilding`. During a swap the *old* branch is still
        /// authoritative and still emitting its own periodic IDRs (2s GOP) —
        /// letting those end the rebuild would declare the swap complete
        /// before the new branch had produced anything, defeating
        /// make-before-break. `Rebuilding` is exited only by
        /// [`Self::finish_rebuild`], which the swap orchestrator calls once it
        /// has actually seen the *new* branch's first IDR.
        pub fn note_encoded_frame(&mut self, keyframe: bool, payload: &[u8], now: Instant) {
            self.last_frame_at = Some(now);
            if keyframe {
                self.last_idr = Some(payload.to_vec());
            }
            match (&self.state, keyframe) {
                (DestinationState::Starting, true) | (DestinationState::Stalled { .. }, true) => {
                    self.state = DestinationState::Live;
                    self.last_stall_resend = None;
                    let _ = now;
                }
                _ => {}
            }
        }

        /// Tier-2: a branch swap is starting. Invisible to the viewer.
        ///
        /// `Why` this refuses to start from `Stalled` as well as from
        /// `Failed`: `Rebuilding` asserts "the old branch is still
        /// authoritative and still sending", and [`Self::abandon_rebuild`]
        /// resolves back to `Live` on that assumption. While the shared
        /// capture source is down that assumption is false — nothing is being
        /// produced at all — so overwriting a `Stalled` with `Rebuilding`
        /// would (a) silence the stall protocol's cached-IDR keepalive (which
        /// only runs while `is_stalled()`), and (b) let a failed swap report
        /// `Live` while the source is still gone, i.e. hide a real outage
        /// behind a healthy-looking state. A swap requested during an outage
        /// is simply not startable; the caller keeps its old branch and can
        /// retry once the source is back (`note_encoded_frame`'s first real
        /// IDR returns the destination to `Live`).
        pub fn begin_rebuild(&mut self, now: Instant) {
            if matches!(self.state, DestinationState::Failed(_)) || self.state.is_stalled() {
                return;
            }
            self.state = DestinationState::Rebuilding { since: now };
        }

        /// Tier-2 completed: the new branch produced its first IDR and is now
        /// authoritative.
        pub fn finish_rebuild(&mut self) {
            if matches!(self.state, DestinationState::Rebuilding { .. }) {
                self.state = DestinationState::Live;
            }
        }

        /// Tier-2 failed: the old branch is still authoritative and still
        /// live, so go back to `Live` rather than stalling — a failed swap
        /// must never be worse for the viewer than not having tried.
        pub fn abandon_rebuild(&mut self) {
            if matches!(self.state, DestinationState::Rebuilding { .. }) {
                self.state = DestinationState::Live;
            }
        }

        /// Tier-3: the shared capture source went away. Enters the one
        /// viewer-visible state; the stream itself is *not* torn down.
        pub fn begin_stall(&mut self, reason: StallReason, now: Instant) {
            if matches!(self.state, DestinationState::Failed(_)) {
                return;
            }
            if !self.state.is_stalled() {
                self.state = DestinationState::Stalled { since: now, reason };
                // Re-send immediately on the next poll rather than waiting a
                // full interval, so the advisory flag reaches the viewer as
                // promptly as possible.
                self.last_stall_resend = None;
            }
        }

        /// The silence watchdog: a `Live` destination whose branch has
        /// produced nothing for `silence` enters [`StallReason::SourceLost`],
        /// which starts the cached-IDR keepalive. Returns whether it did.
        ///
        /// `Why:` the stall protocol's whole purpose is that a viewer's stream
        /// survives a capture that stops delivering — but until now only a
        /// *deliberate* source restart ([`branch::stall_all`]) ever entered
        /// it. A source that simply went quiet (the compositor not answering,
        /// a portal hiccup) left the viewer with no frames and no sign why,
        /// until its own no-frames-in-3s backstop fired. With `keepalive-time`
        /// a healthy static screen never trips this; it is the net underneath
        /// that.
        ///
        /// Only `Live` stalls: `Starting` has no IDR to resend, `Rebuilding`
        /// has its own bounded lifetime, and the caller must not call this
        /// for a deliberately paused (valve-closed) branch.
        pub fn stall_if_silent(&mut self, now: Instant, silence: Duration) -> bool {
            let Some(last) = self.last_frame_at else { return false };
            if self.state != DestinationState::Live || now.saturating_duration_since(last) < silence {
                return false;
            }
            self.begin_stall(StallReason::SourceLost, now);
            self.is_stalled()
        }

        /// Count silence from `now` rather than from the last frame: called
        /// when the branch was deliberately not producing (resumed from a
        /// pause, or the pump was busy swapping), so that gap is not mistaken
        /// for a dead source by [`Self::stall_if_silent`].
        pub fn restart_silence_clock(&mut self, now: Instant) {
            if self.last_frame_at.is_some() {
                self.last_frame_at = Some(now);
            }
        }

        pub fn fail(&mut self, message: impl Into<String>) {
            self.state = DestinationState::Failed(message.into());
        }

        pub fn cached_idr(&self) -> Option<&[u8]> {
            self.last_idr.as_deref()
        }

        /// The stall protocol's pacing gate: `Some(idr)` when this stalled
        /// destination is due to re-send its cached IDR (~2fps), `None`
        /// otherwise (not stalled, nothing cached yet, or too soon).
        pub fn stall_resend_due(&mut self, now: Instant) -> Option<Vec<u8>> {
            if !self.state.is_stalled() {
                return None;
            }
            if let Some(last) = self.last_stall_resend {
                if now.saturating_duration_since(last) < STALL_RESEND_INTERVAL {
                    return None;
                }
            }
            let idr = self.last_idr.clone()?;
            self.last_stall_resend = Some(now);
            Some(idr)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Everything here drives `Instant`s in by hand rather than sleeping —
        /// the same discipline `decode.rs`'s `pacing_*` tests use, and the
        /// reason this whole module was kept free of clocks and GStreamer.
        fn t0() -> Instant {
            Instant::now()
        }

        fn params(framerate: u32) -> VideoParams {
            VideoParams {
                width: 1920,
                height: 1080,
                framerate,
                bitrate_kbps: 8000,
                backend: EncoderBackend::Software,
                tune: TuneMode::default(),
                quality_level: QualityLevel::default(),
                rate_control: RateControlMode::default(),
                gop_millis: DEFAULT_GOP_MILLIS,
                codec: VideoCodec::default(),
            }
        }

        // -------------------------------------------------------------
        // Encoder limit (#46)
        // -------------------------------------------------------------

        /// Feeds `runtime` one observation every 500 ms for `secs` seconds,
        /// starting at `from`; returns the time after the last one.
        fn feed(runtime: &mut DestinationRuntime, from: Instant, secs: u64, enc: u32, offered: u32) -> Instant {
            let mut t = from;
            for _ in 0..secs * 2 {
                t += Duration::from_millis(500);
                runtime.observe_encoder_rate(t, enc, offered);
            }
            t
        }

        fn live(framerate: u32) -> (DestinationRuntime, Instant) {
            let now = t0();
            let mut runtime = DestinationRuntime::new(now, params(framerate));
            runtime.note_encoded_frame(true, b"idr", now);
            assert_eq!(runtime.state(), DestinationState::Live);
            (runtime, now)
        }

        #[test]
        fn encoder_limit_needs_more_than_three_seconds_below_ninety_percent() {
            let (mut runtime, now) = live(240);
            // 168 of 240 offered: below 90 %, but not for long enough yet.
            let t = feed(&mut runtime, now, 2, 168, 280);
            assert_eq!(runtime.encoder_limited(), None);
            feed(&mut runtime, t, 2, 168, 280);
            assert_eq!(runtime.encoder_limited(), Some(EncoderLimited { encoding_fps: 168, target_fps: 240 }));
        }

        #[test]
        fn a_source_that_offers_few_frames_is_not_an_encoder_limit() {
            // Idle screen: the portal offers 20 fps, the encoder makes all 20.
            let (mut runtime, now) = live(240);
            feed(&mut runtime, now, 10, 20, 20);
            assert_eq!(runtime.encoder_limited(), None);
            // 230 of 240 is within 10 %.
            feed(&mut runtime, now, 10, 230, 280);
            assert_eq!(runtime.encoder_limited(), None);
        }

        #[test]
        fn encoder_limit_clears_only_after_three_seconds_of_keeping_up_and_outside_live() {
            let (mut runtime, now) = live(240);
            let t = feed(&mut runtime, now, 4, 168, 280);
            assert!(runtime.encoder_limited().is_some());
            let t = feed(&mut runtime, t, 2, 240, 280);
            assert!(runtime.encoder_limited().is_some(), "one good second doesn't clear it");
            let t = feed(&mut runtime, t, 2, 240, 280);
            assert_eq!(runtime.encoder_limited(), None);

            let t = feed(&mut runtime, t, 4, 168, 280);
            assert!(runtime.encoder_limited().is_some());
            // A rebuild (e.g. "Use H.265") forgets the old encoder's verdict.
            runtime.begin_rebuild(t);
            assert_eq!(runtime.encoder_limited(), None);
            runtime.observe_encoder_rate(t, 168, 280);
            runtime.finish_rebuild();
            assert_eq!(runtime.encoder_limited(), None);
        }

        // -------------------------------------------------------------
        // State machine
        // -------------------------------------------------------------

        #[test]
        fn a_new_destination_starts_in_starting_and_goes_live_on_its_first_idr() {
            let now = t0();
            let mut runtime = DestinationRuntime::new(now, params(30));
            assert_eq!(runtime.state(), DestinationState::Starting);

            // A delta frame is not enough — nothing decodable has reached the
            // viewer yet.
            runtime.note_encoded_frame(false, b"delta", now + Duration::from_millis(10));
            assert_eq!(runtime.state(), DestinationState::Starting);

            runtime.note_encoded_frame(true, b"idr", now + Duration::from_millis(20));
            assert_eq!(runtime.state(), DestinationState::Live);
        }

        /// The subtle one: while a swap is in flight the **old** branch is
        /// still authoritative and still emitting its own periodic IDRs. Those
        /// must not end the rebuild — only the new branch actually producing
        /// does, via `finish_rebuild`.
        #[test]
        fn the_old_branchs_idrs_do_not_end_a_rebuild() {
            let now = t0();
            let mut runtime = DestinationRuntime::new(now, params(30));
            runtime.note_encoded_frame(true, b"idr", now);
            runtime.begin_rebuild(now + Duration::from_millis(100));

            for i in 0..5 {
                runtime.note_encoded_frame(true, b"old-branch-idr", now + Duration::from_millis(200 + i * 50));
                assert!(
                    matches!(runtime.state(), DestinationState::Rebuilding { .. }),
                    "an IDR from the still-authoritative old branch must not complete the swap"
                );
            }

            runtime.finish_rebuild();
            assert_eq!(runtime.state(), DestinationState::Live);
        }

        /// A swap that can't be applied must leave the destination exactly as
        /// it was — still `Live` on its old branch, never `Stalled`.
        #[test]
        fn an_abandoned_rebuild_returns_to_live_never_stalled() {
            let now = t0();
            let mut runtime = DestinationRuntime::new(now, params(30));
            runtime.note_encoded_frame(true, b"idr", now);
            runtime.begin_rebuild(now + Duration::from_millis(10));
            runtime.abandon_rebuild();
            assert_eq!(runtime.state(), DestinationState::Live);
        }

        /// A rebuild is not startable from `Stalled`, and a failed one
        /// therefore cannot resolve a real outage away.
        ///
        /// Without the guard, `begin_rebuild` overwrote `Stalled` with
        /// `Rebuilding` and `abandon_rebuild` then resolved that to `Live` —
        /// so a branch swap attempted while the shared capture source was
        /// down would report a healthy, live destination while nothing was
        /// being produced at all, and would silence the cached-IDR keepalive
        /// (which only runs while `is_stalled()`) for as long as the "rebuild"
        /// lasted.
        #[test]
        fn a_rebuild_cannot_be_started_from_stalled_and_cannot_resolve_the_stall() {
            let now = t0();
            let mut runtime = DestinationRuntime::new(now, params(30));
            runtime.note_encoded_frame(true, b"idr", now);
            runtime.begin_stall(StallReason::SourceChanged, now + Duration::from_millis(10));

            runtime.begin_rebuild(now + Duration::from_millis(20));
            assert!(runtime.is_stalled(), "a stalled destination must not enter Rebuilding");

            runtime.abandon_rebuild();
            assert!(runtime.is_stalled(), "abandoning must not turn a real outage into Live");
            runtime.finish_rebuild();
            assert!(runtime.is_stalled(), "finishing a rebuild that never started must not clear the stall either");

            // The one thing that does clear it: a real frame.
            runtime.note_encoded_frame(true, b"idr2", now + Duration::from_millis(30));
            assert_eq!(runtime.state(), DestinationState::Live);
        }

        #[test]
        fn a_stall_ends_on_the_first_idr_after_the_source_is_back() {
            let now = t0();
            let mut runtime = DestinationRuntime::new(now, params(30));
            runtime.note_encoded_frame(true, b"idr", now);

            runtime.begin_stall(StallReason::SourceChanged, now + Duration::from_secs(1));
            assert!(runtime.is_stalled());

            // A delta frame can't end it: the viewer has nothing decodable.
            runtime.note_encoded_frame(false, b"delta", now + Duration::from_secs(2));
            assert!(runtime.is_stalled());

            runtime.note_encoded_frame(true, b"new-idr", now + Duration::from_secs(3));
            assert_eq!(runtime.state(), DestinationState::Live);
        }

        #[test]
        fn a_live_destination_that_goes_silent_stalls_and_a_real_idr_revives_it() {
            let now = t0();
            let mut runtime = DestinationRuntime::new(now, params(30));
            let silence = silence_threshold(30);
            runtime.note_encoded_frame(true, b"idr", now);

            assert!(!runtime.stall_if_silent(now + silence - Duration::from_millis(1), silence));
            assert!(runtime.stall_if_silent(now + silence, silence));
            assert!(matches!(runtime.state(), DestinationState::Stalled { reason: StallReason::SourceLost, .. }));
            // The cached IDR is what the stall protocol re-sends.
            assert_eq!(runtime.stall_resend_due(now + silence).as_deref(), Some(&b"idr"[..]));
            // Already stalled: no second transition.
            assert!(!runtime.stall_if_silent(now + silence * 2, silence));

            runtime.note_encoded_frame(true, b"idr2", now + silence * 2);
            assert_eq!(runtime.state(), DestinationState::Live);
        }

        #[test]
        fn silence_never_stalls_a_destination_that_has_nothing_to_resend_or_is_not_live() {
            let now = t0();
            let silence = silence_threshold(30);
            let mut starting = DestinationRuntime::new(now, params(30));
            assert!(!starting.stall_if_silent(now + silence * 10, silence), "Starting has no frame yet");

            let mut rebuilding = DestinationRuntime::new(now, params(30));
            rebuilding.note_encoded_frame(true, b"idr", now);
            rebuilding.begin_rebuild(now);
            assert!(!rebuilding.stall_if_silent(now + silence * 10, silence), "a rebuild has its own bound");
        }

        #[test]
        fn a_pause_or_busy_pump_does_not_count_as_silence() {
            let now = t0();
            let silence = silence_threshold(30);
            let mut runtime = DestinationRuntime::new(now, params(30));
            runtime.note_encoded_frame(true, b"idr", now);
            // Paused for a minute, then resumed.
            runtime.restart_silence_clock(now + Duration::from_secs(60));
            assert!(!runtime.stall_if_silent(now + Duration::from_secs(60) + Duration::from_millis(500), silence));
        }

        #[test]
        fn the_silence_threshold_scales_with_a_low_framerate() {
            assert_eq!(silence_threshold(30), Duration::from_secs(3));
            assert_eq!(silence_threshold(144), Duration::from_secs(3));
            // At 1 fps a frame every second is normal; 3 frame periods = 3s,
            // at 0 (defensive) it must not divide by zero.
            assert!(silence_threshold(1) >= Duration::from_secs(3));
            assert!(silence_threshold(0) >= Duration::from_secs(3));
        }

        #[test]
        fn begin_stall_does_not_restart_an_existing_stalls_clock() {
            let now = t0();
            let mut runtime = DestinationRuntime::new(now, params(30));
            runtime.begin_stall(StallReason::SourceLost, now + Duration::from_secs(1));
            let first = runtime.state();
            runtime.begin_stall(StallReason::SourceLost, now + Duration::from_secs(5));
            assert_eq!(runtime.state(), first, "a repeated stall notification must not reset `since`");
        }

        #[test]
        fn a_failed_destination_is_terminal_against_stall_and_rebuild() {
            let now = t0();
            let mut runtime = DestinationRuntime::new(now, params(30));
            runtime.fail("encoder exploded");
            runtime.begin_stall(StallReason::SourceLost, now);
            runtime.begin_rebuild(now);
            assert!(matches!(runtime.state(), DestinationState::Failed(_)));
        }

        // -------------------------------------------------------------
        // Stall protocol pacing
        // -------------------------------------------------------------

        #[test]
        fn stall_resend_paces_at_the_configured_interval_and_only_while_stalled() {
            let now = t0();
            let mut runtime = DestinationRuntime::new(now, params(30));
            runtime.note_encoded_frame(true, b"cached-idr", now);

            // Not stalled: nothing to re-send, however often it's polled.
            assert!(runtime.stall_resend_due(now).is_none());

            runtime.begin_stall(StallReason::SourceChanged, now);
            // First poll after entering the stall fires immediately, so the
            // advisory flag reaches the viewer promptly.
            assert_eq!(runtime.stall_resend_due(now).as_deref(), Some(&b"cached-idr"[..]));
            // Too soon.
            assert!(runtime.stall_resend_due(now + STALL_RESEND_INTERVAL / 2).is_none());
            // Due again.
            assert_eq!(
                runtime.stall_resend_due(now + STALL_RESEND_INTERVAL).as_deref(),
                Some(&b"cached-idr"[..])
            );

            // Recovery stops the re-sends.
            runtime.note_encoded_frame(true, b"fresh", now + Duration::from_secs(9));
            assert!(runtime.stall_resend_due(now + Duration::from_secs(20)).is_none());
        }

        #[test]
        fn a_stall_before_any_idr_was_ever_cached_sends_nothing() {
            let now = t0();
            let mut runtime = DestinationRuntime::new(now, params(30));
            runtime.begin_stall(StallReason::SourceLost, now);
            assert!(
                runtime.stall_resend_due(now).is_none(),
                "with no cached IDR there is nothing decodable to re-send — must not send garbage"
            );
        }

        // -------------------------------------------------------------
        // The wire-PTS invariant
        // -------------------------------------------------------------

        /// The invariant, stated directly: wire PTS must never go backwards,
        /// including across a pipeline restart that resets buffer PTS to 0.
        #[test]
        fn wire_pts_never_goes_backwards_across_a_simulated_restart() {
            let epoch = t0();
            let mut runtime = DestinationRuntime::new(epoch, params(30));
            runtime.on_pipeline_start(0, epoch);

            let mut sent = Vec::new();

            // 3 seconds of normal capture at 30fps: buffer PTS climbing from 0.
            for frame in 0..90u64 {
                sent.push(runtime.wire_pts_from_buffer(frame * 33_333));
            }
            let before_restart = *sent.last().unwrap();

            // The capture source is switched: 2s of downtime during which the
            // stall protocol keeps re-sending on the epoch clock.
            let restart_at = epoch + Duration::from_secs(5);
            for tick in 0..4u64 {
                sent.push(runtime.wire_pts_from_epoch(epoch + Duration::from_millis(3_000 + tick * 500)));
            }

            // New pipeline: its running time restarts at 0. This is the exact
            // condition that, without `pts_offset_us`, would send the wire
            // clock crashing back to zero.
            runtime.on_pipeline_start(0, restart_at);
            for frame in 0..90u64 {
                sent.push(runtime.wire_pts_from_buffer(frame * 33_333));
            }

            for pair in sent.windows(2) {
                assert!(
                    pair[1] > pair[0],
                    "wire PTS went backwards or stalled: {} -> {} (the monotonic clamp is the whole point)",
                    pair[0],
                    pair[1]
                );
            }

            // Not just monotonic — the post-restart timeline must actually pick
            // up near where real time is, so the gap reads as "few frames
            // arrived", not as a 5-second replay of the past.
            let first_after_restart = sent[94];
            assert!(
                first_after_restart >= before_restart,
                "the first post-restart frame must not predate the last pre-restart one"
            );
            assert!(
                first_after_restart >= 4_500_000,
                "post-restart PTS ({first_after_restart}µs) should land near the destination's real elapsed time \
                 (~5s), not back near zero — that mapping is what makes a restart look like low framerate"
            );
        }

        /// A branch swap hands the destination a replacement branch whose
        /// timeline starts back near the *capture's* start rather than at the
        /// current running time (see [`DestinationRuntime::on_branch_swapped`]
        /// for the measurement). Monotonicity alone is not enough here: the
        /// clamp would happily emit the whole post-swap stream at +1µs per
        /// frame, which is a technically-legal timeline the viewer's pacer
        /// cannot render. The frames have to keep their real spacing.
        #[test]
        fn a_branch_swap_keeps_real_frame_spacing_when_the_new_branch_restarts_its_timeline() {
            let epoch = t0();
            let mut runtime = DestinationRuntime::new(epoch, params(30));
            runtime.on_pipeline_start(0, epoch);

            // 10 seconds of streaming at 30fps.
            let mut last_before = 0;
            for frame in 0..300u64 {
                last_before = runtime.wire_pts_from_buffer(frame * 33_333);
            }

            // The preset click: the replacement branch's own timeline starts
            // one frame period in, i.e. ten seconds behind where the old
            // branch left off.
            runtime.on_branch_swapped(33_333);

            let mut sent = Vec::new();
            for frame in 1..=90u64 {
                sent.push(runtime.wire_pts_from_buffer(frame * 33_333));
            }

            assert!(sent[0] >= last_before, "the seam must not go backwards: {last_before} -> {}", sent[0]);
            for pair in sent.windows(2) {
                let delta = pair[1] - pair[0];
                assert!(
                    (30_000..40_000).contains(&delta),
                    "post-swap frames are {delta}µs apart instead of one 30fps frame period — the wire clock \
                     is running through the monotonic clamp rather than carrying real presentation times"
                );
            }
        }

        /// The other side of the same coin: a swap whose replacement branch
        /// *does* continue the timeline must not be "corrected" into a forward
        /// jump. Re-anchoring repairs a broken seam; it is not a toll every
        /// swap pays.
        #[test]
        fn a_branch_swap_that_continues_the_timeline_leaves_the_wire_clock_alone() {
            let epoch = t0();
            let mut runtime = DestinationRuntime::new(epoch, params(30));
            runtime.on_pipeline_start(0, epoch);

            for frame in 0..300u64 {
                runtime.wire_pts_from_buffer(frame * 33_333);
            }
            let offset_before = runtime.pts_offset_us();

            // The replacement branch picks up exactly where the old one left
            // off — nothing to repair.
            runtime.on_branch_swapped(300 * 33_333);
            assert_eq!(
                runtime.pts_offset_us(),
                offset_before,
                "a seam that is already continuous must leave the offset untouched"
            );
        }

        /// The clamp is a hard guarantee, not a consequence of the offset
        /// arithmetic being right: even fed strictly *decreasing* raw values it
        /// must still emit a strictly increasing sequence.
        #[test]
        fn the_monotonic_clamp_holds_even_against_decreasing_buffer_timestamps() {
            let epoch = t0();
            let mut runtime = DestinationRuntime::new(epoch, params(30));
            runtime.on_pipeline_start(0, epoch);

            let mut last = runtime.wire_pts_from_buffer(1_000_000);
            for step in (0..20u64).rev() {
                let pts = runtime.wire_pts_from_buffer(step * 50_000);
                assert!(pts > last, "clamp failed: {last} -> {pts}");
                last = pts;
            }
        }

        #[test]
        fn the_first_frame_may_be_pts_zero() {
            let epoch = t0();
            let mut runtime = DestinationRuntime::new(epoch, params(30));
            runtime.on_pipeline_start(0, epoch);
            assert_eq!(runtime.wire_pts_from_buffer(0), 0, "the very first frame must not be forced off zero");
            assert!(runtime.wire_pts_from_buffer(0) > 0, "but the second one must advance");
        }

        #[test]
        fn on_pipeline_start_maps_the_new_running_time_onto_the_original_epoch() {
            let epoch = t0();
            let mut runtime = DestinationRuntime::new(epoch, params(30));
            runtime.on_pipeline_start(0, epoch + Duration::from_secs(10));
            assert_eq!(runtime.pts_offset_us(), 10_000_000);

            // A pipeline that had already been running for 4s when this
            // destination attached: the offset absorbs the difference.
            runtime.on_pipeline_start(4_000_000, epoch + Duration::from_secs(10));
            assert_eq!(runtime.pts_offset_us(), 6_000_000);
        }

        // -------------------------------------------------------------
        // The three-tier change model
        // -------------------------------------------------------------

        #[test]
        fn bitrate_only_changes_are_tier_one() {
            let source = SourceCaps { width: 1920, height: 1080, framerate: 60 };
            let from = params(30);
            let to = VideoParams { bitrate_kbps: 4000, ..from };
            assert_eq!(classify_change(source, from, to), ChangeTier::Bitrate);
            assert_eq!(classify_change(source, from, from), ChangeTier::Unchanged);
        }

        /// The load-bearing asymmetry: at or below the source's rate is a
        /// branch swap (the branch's own `videorate` downconverts); above it
        /// needs the source itself to run faster, which is a restart.
        #[test]
        fn fps_below_the_source_cap_swaps_and_above_it_restarts() {
            let source = SourceCaps { width: 1920, height: 1080, framerate: 60 };
            let from = params(60);

            for lower in [1u32, 15, 30, 59, 60] {
                assert_eq!(
                    classify_change(source, from, VideoParams { framerate: lower, ..from }),
                    if lower == 60 { ChangeTier::Unchanged } else { ChangeTier::BranchSwap },
                    "{lower}fps is at or below the source's 60fps and must be servable by this branch alone"
                );
            }

            for higher in [61u32, 90, 120, 240] {
                assert_eq!(
                    classify_change(source, from, VideoParams { framerate: higher, ..from }),
                    ChangeTier::SourceRestart,
                    "{higher}fps exceeds the source's 60fps — videorate cannot invent frames, so the source must restart"
                );
            }
        }

        #[test]
        fn resolution_and_backend_changes_are_tier_two() {
            let source = SourceCaps { width: 1920, height: 1080, framerate: 60 };
            let from = params(30);
            assert_eq!(
                classify_change(source, from, VideoParams { width: 1280, height: 720, ..from }),
                ChangeTier::BranchSwap
            );
            assert_eq!(
                classify_change(source, from, VideoParams { backend: EncoderBackend::Vaapi, ..from }),
                ChangeTier::BranchSwap
            );
        }

        /// The encoder tuning knobs (`tune`/`quality_level`/`rate_control`,
        /// plus `gop_millis`) are never tier-1 live-writable even though
        /// vah264enc's `target-usage`/`rate-control` technically are — see
        /// [`EncoderBackend::launch_fragment`]'s `Why:` note. All of them
        /// route through the same `BranchSwap` path
        /// as resolution/fps/backend changes.
        #[test]
        fn tuning_knob_changes_are_tier_two() {
            let source = SourceCaps { width: 1920, height: 1080, framerate: 60 };
            let from = params(30);
            assert_eq!(
                classify_change(source, from, VideoParams { tune: TuneMode::LowLatency, ..from }),
                ChangeTier::BranchSwap
            );
            assert_eq!(
                classify_change(source, from, VideoParams { quality_level: QualityLevel::Quality, ..from }),
                ChangeTier::BranchSwap
            );
            // `from`'s rate_control is `RateControlMode::default()` (`Qvbr`)
            // — flip it to the other variant, `Cqp`.
            assert_eq!(
                classify_change(source, from, VideoParams { rate_control: RateControlMode::Cqp, ..from }),
                ChangeTier::BranchSwap
            );
            assert_eq!(
                classify_change(source, from, VideoParams { gop_millis: 750, ..from }),
                ChangeTier::BranchSwap
            );
            // No change at all in any of the four: still `Unchanged`.
            assert_eq!(classify_change(source, from, from), ChangeTier::Unchanged);
        }

        /// A codec change (H.264 -> H.265/AV1) is also tier 2 — see
        /// `classify_change`'s `Why:` note on `to.codec != from.codec`.
        #[test]
        fn codec_changes_are_tier_two() {
            let source = SourceCaps { width: 1920, height: 1080, framerate: 60 };
            let from = params(30);
            assert_eq!(
                classify_change(source, from, VideoParams { codec: VideoCodec::H265, ..from }),
                ChangeTier::BranchSwap
            );
            assert_eq!(
                classify_change(source, from, VideoParams { codec: VideoCodec::Av1, ..from }),
                ChangeTier::BranchSwap
            );
        }

        /// A resolution change that *also* raises fps past the source cap is
        /// tier 3, not tier 2 — the fps check has to win.
        #[test]
        fn an_fps_increase_dominates_a_simultaneous_resolution_change() {
            let source = SourceCaps { width: 1920, height: 1080, framerate: 30 };
            let from = params(30);
            let to = VideoParams { width: 1280, height: 720, framerate: 120, ..from };
            assert_eq!(classify_change(source, from, to), ChangeTier::SourceRestart);
        }

        // -------------------------------------------------------------
        // Never upscale
        // -------------------------------------------------------------

        #[test]
        fn resolution_is_clamped_down_to_the_source_and_never_up() {
            let source = SourceCaps { width: 1920, height: 1080, framerate: 60 };

            // Below native: untouched.
            let down = clamp_to_source(VideoParams { width: 1280, height: 720, ..params(30) }, source);
            assert_eq!((down.width, down.height), (1280, 720));

            // Exactly native: untouched.
            let exact = clamp_to_source(VideoParams { width: 1920, height: 1080, ..params(30) }, source);
            assert_eq!((exact.width, exact.height), (1920, 1080));

            // Above native on both axes: clamped to native.
            let up = clamp_to_source(VideoParams { width: 3840, height: 2160, ..params(30) }, source);
            assert_eq!((up.width, up.height), (1920, 1080), "must never upscale");
        }

        /// Clamping proportionally, not per-axis — a per-axis `min` would
        /// silently stretch the picture when only one axis overshoots.
        #[test]
        fn clamping_preserves_aspect_ratio_when_only_one_axis_overshoots() {
            let source = SourceCaps { width: 1920, height: 1080, framerate: 60 };
            let clamped = clamp_to_source(VideoParams { width: 3840, height: 1080, ..params(30) }, source);
            assert_eq!((clamped.width, clamped.height), (1920, 540), "32:9 input must stay 32:9, not become 16:9");
            assert!(clamped.width <= source.width && clamped.height <= source.height);
        }

        #[test]
        fn clamping_leaves_framerate_to_the_tier_model_but_enforces_pipeline_bounds() {
            let source = SourceCaps { width: 1920, height: 1080, framerate: 30 };
            // Above the *source's* rate is legal here — it's a SourceRestart,
            // not something to clamp away.
            assert_eq!(clamp_to_source(params(120), source).framerate, 120);
            // Above the *pipeline's* hard ceiling is not.
            assert_eq!(clamp_to_source(params(10_000), source).framerate, MAX_FRAMERATE);
            assert_eq!(clamp_to_source(params(0), source).framerate, MIN_FRAMERATE);
        }

        #[test]
        fn an_unnegotiated_source_size_clamps_nothing() {
            let source = SourceCaps { width: 0, height: 0, framerate: 30 };
            let out = clamp_to_source(VideoParams { width: 3840, height: 2160, ..params(30) }, source);
            assert_eq!((out.width, out.height), (3840, 2160));
        }

        #[test]
        fn the_source_runs_at_the_max_fps_any_destination_needs() {
            assert_eq!(required_source_framerate([30, 60, 15]), 60);
            assert_eq!(required_source_framerate([30]), 30);
            assert_eq!(required_source_framerate([]), DEFAULT_FRAMERATE);
            assert_eq!(required_source_framerate([1_000]), MAX_FRAMERATE);
        }
    }
}

// ---------------------------------------------------------------------
// Dynamic tee branches (multi-destination fan-out)
// ---------------------------------------------------------------------

/// Dynamically adds/removes/swaps encode branches on a live capture
/// pipeline's shared `tee` at runtime — see this module's doc comment's
/// "Dynamic tee branches" section for how this fits the multi-room plan.
///
/// This module is deliberately narrow: it knows how to attach/detach a
/// self-contained encode chain to an already-built [`gst::Pipeline`]'s `tee`
/// element, and nothing about *why* a branch is being added/removed/swapped
/// or which room/`Destination` it belongs to — that's app-level state a
/// later stage owns. It intentionally does not depend on `ui.rs`:
/// [`BranchId`] is this module's own opaque identifier, not `ui.rs`'s
/// `DestinationId` — a caller wiring this up against real destinations
/// passes whatever `u32` it likes (e.g. a `DestinationId`'s inner value).
pub mod branch {
    use super::*;

    /// Opaque per-branch identifier, meaningful only to this module's caller
    /// (see this module's doc comment) — used purely to give each branch's
    /// GStreamer elements distinct, greppable names (`GST_DEBUG` dumps,
    /// `pipeline.by_name(...)` while debugging), never looked up or
    /// interpreted by this module itself.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct BranchId(pub u32);

    /// What one dynamically-added encode branch should produce — independent
    /// of every other branch on the same tee, since each branch gets its own
    /// `queue`/`videoconvert`/`videoscale`/`videorate`/encoder/`appsink`
    /// chain (see [`add_branch`]).
    #[derive(Debug, Clone, Copy)]
    pub struct EncodeSettings {
        pub width: u32,
        pub height: u32,
        pub framerate: u32,
        pub bitrate_kbps: u32,
        pub backend: EncoderBackend,
        /// See [`super::TuneMode`].
        pub tune: TuneMode,
        /// See [`super::QualityLevel`].
        pub quality_level: QualityLevel,
        /// See [`super::RateControlMode`].
        pub rate_control: RateControlMode,
        /// See [`super::destination::VideoParams::gop_millis`].
        pub gop_millis: u32,
        /// See [`super::VideoCodec`].
        pub codec: VideoCodec,
    }

    /// A live encode branch built by [`add_branch`]: a `gst::Bin` already
    /// added to its pipeline and linked to a tee request pad, plus
    /// everything [`remove_branch`] needs to tear it back down.
    ///
    /// # Reading this branch's encoded output
    ///
    /// Use [`EncodeBranch::next_sample`] (async) or, for a synchronous poll,
    /// `appsink.try_pull_sample(..)`. **Do not call `appsink.stream()`
    /// yourself.**
    ///
    /// `Why:` `gst_app::AppSink::stream()` is not a view onto the appsink —
    /// it *installs* callbacks via `gst_app_sink_set_callbacks`, which
    /// **replaces** whatever callback set was there before, and its `Drop`
    /// impl (on the `v1_18`-feature-off build this crate pins) resets the
    /// appsink's callbacks to an empty set. So two `stream()` calls on one
    /// appsink silently break each other: the first stream's waker is
    /// unregistered by the second, and dropping either one leaves the
    /// survivor parked indefinitely — it only ever wakes again by luck, because
    /// `AppSinkStream::poll_next` opportunistically `try_pull_sample`s before
    /// parking. That is exactly the collision that would happen here, because
    /// [`remove_branch`] must itself drain the appsink to EOS while a
    /// production reader is already consuming it. The fix is structural
    /// rather than a rule to remember: this field is the branch's **one and
    /// only** `AppSinkStream`, created lazily on first use and reused by
    /// everyone — including [`remove_branch`], which receives it by owning the
    /// whole `EncodeBranch`. Handing the branch to `remove_branch` *is* the
    /// reader's handoff; there is no window in which two live streams exist.
    pub struct EncodeBranch {
        pub id: BranchId,
        pub appsink: gst_app::AppSink,
        bin: gst::Bin,
        tee_pad: gst::Pad,
        /// This branch's own encoder element, kept so its `bitrate` property
        /// can be written live — the plan doc's tier-1 change (see
        /// [`super::destination::ChangeTier::Bitrate`]) and what the
        /// Absolute-Maximum bandwidth allocator drives once per second.
        encoder: gst::Element,
        /// This branch's own `valve`, gating everything downstream of the
        /// tee's `queue`. Per-branch rather than one shared valve on the
        /// capture: subscriber counts are per *stream*, so one destination
        /// dropping to zero subscribers must pause only its own encoder, not
        /// every destination fed by the same capture.
        valve: gst::Element,
        /// See the type's doc comment — created at most once per branch.
        stream: Option<gst_app::app_sink::AppSinkStream>,
        /// Frames that reached this branch (counted on its sink pad, before
        /// the leaky queue): the "offered" side of
        /// [`super::destination::EncoderLimit`].
        offered: Arc<AtomicU64>,
    }

    impl EncodeBranch {
        /// Tier-1 change: write this branch's encoder bitrate live. Both
        /// `x264enc` and `vah264enc` take `bitrate` in kbit/s and accept the
        /// write while `Playing` (see [`super::EncoderBackend::launch_fragment`]).
        pub fn set_bitrate_kbps(&self, bitrate_kbps: u32) {
            self.encoder.set_property("bitrate", bitrate_kbps);
        }

        /// Frames offered to this branch so far — see the `offered` field.
        pub fn offered_frames(&self) -> u64 {
            self.offered.load(Ordering::Relaxed)
        }

        /// Current value of the encoder's `bitrate` property, read back off
        /// the live element — the ground truth a bitrate write is checked
        /// against (used by this module's tests, and useful for diagnosing a
        /// write that silently didn't take).
        pub fn bitrate_kbps(&self) -> u32 {
            self.encoder.property::<u32>("bitrate")
        }

        /// Pause (`true`) or resume (`false`) this branch's encoder by
        /// dropping raw buffers at its `valve`, before they ever reach the
        /// encoder. See [`super::set_valve_drop`] for the read-back
        /// diagnostic this delegates to.
        pub fn set_paused(&self, paused: bool, stream_id: u8) {
            super::set_valve_drop(&self.valve, paused, stream_id);
        }

        /// The pad to push an upstream force-key-unit event from for *this*
        /// branch (see [`super::force_keyframe`]): its appsink's sink pad, so
        /// the event travels upstream through exactly this branch's encoder
        /// and no other branch's.
        pub fn keyframe_pad(&self) -> Option<gst::Pad> {
            self.appsink.static_pad("sink")
        }
        /// This branch's single `AppSinkStream`, created on first use. See the
        /// type's doc comment for why there must never be a second one.
        fn sample_stream(&mut self) -> &mut gst_app::app_sink::AppSinkStream {
            if self.stream.is_none() {
                self.stream = Some(self.appsink.stream());
            }
            self.stream.as_mut().expect("just populated above")
        }

        /// Next encoded access unit from this branch, or `None` once the
        /// branch has reached EOS. Cancel-safe in a `select!` exactly like the
        /// underlying `AppSinkStream::next`.
        pub async fn next_sample(&mut self) -> Option<gst::Sample> {
            self.sample_stream().next().await
        }

        /// Awaits this branch's first **IDR** (a buffer without
        /// `DELTA_UNIT`), bounded by `timeout`.
        ///
        /// `Why:` "the branch produced a sample" is not the same as "the
        /// viewer can decode it". A destination is only `Live` once something
        /// decodable from *this* branch is on the wire, and the whole
        /// make-before-break flip hinges on that distinction — flipping on the
        /// first arbitrary sample could hand the viewer a delta frame
        /// referencing a GOP it never received. In practice both `x264enc` and
        /// `vah264enc` emit an IDR first, so this normally returns on the very
        /// first sample; the loop is what makes that a checked property rather
        /// than an assumption.
        pub async fn wait_for_first_idr(&mut self, timeout: Duration) -> Result<gst::Sample> {
            tokio::time::timeout(timeout, async {
                while let Some(sample) = self.next_sample().await {
                    let is_idr = sample
                        .buffer()
                        .map(|b| !b.flags().contains(gst::BufferFlags::DELTA_UNIT))
                        .unwrap_or(false);
                    if is_idr {
                        return Some(sample);
                    }
                }
                None
            })
            .await
            .map_err(|_| anyhow!("encode branch produced no IDR within {timeout:?}"))?
            .ok_or_else(|| anyhow!("encode branch ended before producing an IDR"))
        }
    }

    /// Process-wide counter for generating element/bin names that are
    /// distinct across every branch this process ever creates, regardless of
    /// [`BranchId`] (which a caller could reuse or which could collide
    /// across independent captures/pipelines).
    ///
    /// `Why:` `GstBin::by_name` searches recursively into child bins, so two
    /// branches sharing the literal name `"sink"` for their `appsink` (the
    /// same name the pipeline's *fixed* encoder branch already uses — see
    /// [`build_pipeline`]) would make `pipeline.by_name("sink")` ambiguous
    /// the moment more than one branch coexists on the same pipeline. A
    /// monotonic, never-reused counter rules that out entirely, at the cost
    /// of names that don't reveal which `BranchId`/destination they belong
    /// to on their own — acceptable since this module never looks elements
    /// up by name itself; it keeps direct `gst::Bin`/`gst::Pad`/`AppSink`
    /// handles instead (see [`EncodeBranch`]).
    static NEXT_BRANCH_INDEX: AtomicU32 = AtomicU32::new(0);

    fn next_branch_index() -> u32 {
        NEXT_BRANCH_INDEX.fetch_add(1, Ordering::Relaxed)
    }

    /// A frame-rate **cap** for one encode branch: admits buffers so that at
    /// most `fps` per second get through, never inventing any, and never
    /// bursting to "catch up" after the branch was blocked or paused.
    ///
    /// `Why this instead of `videorate drop-only=true` (measured, #39):`
    /// videorate's drop-only mode keeps an output grid (`next_ts`, advanced by
    /// one frame period per frame it passes) and passes every frame whose
    /// timestamp is at or past the grid. If the branch is blocked for a while
    /// (a saturated encoder pushing back, or a paused `valve`), the grid falls
    /// behind real time, and afterwards *every* frame passes until it has
    /// caught up: a 30 fps cap on a 60 fps source ran at 66, 60 and 54 fps in
    /// the three half-seconds after a 1.5 s block. That is the "fps shoots up
    /// to 300+" class of bug this module has fixed before. This type is the
    /// same grid, plus one rule: a frame that arrives more than one period
    /// behind the grid resynchronises it instead of being chased.
    ///
    /// `Why half a period of tolerance:` a frame that arrives slightly early
    /// still counts for the grid slot it is nearest to (round to nearest,
    /// like videorate), so a 280 fps source into a 240 fps cap yields 240, not
    /// the 140 that "pass only if a full period has elapsed since the last
    /// one" would. The grid advances by exactly one period per admitted frame,
    /// so the long-run rate is `min(source rate, fps)`.
    ///
    /// The first frame ever admitted anchors the grid at its own timestamp, so a
    /// branch added to a long-running capture is not judged against the
    /// capture's (much older) segment start. `videorate` needed `skip-to-first`
    /// for that; without it a 30 fps branch burst to 76 fps (3 s into a capture)
    /// and 181 fps (10 s) after a preset change.
    #[derive(Debug, Clone)]
    pub struct RateCap {
        period_ns: u64,
        next_ns: Option<u64>,
    }

    impl RateCap {
        /// A buffer further ahead of the grid than this is a timeline restart
        /// (a new segment), not a fast source: resynchronise on it.
        const RESYNC_AHEAD_NS: u64 = 1_000_000_000;

        pub fn new(fps: u32) -> Self {
            RateCap { period_ns: 1_000_000_000 / u64::from(fps.max(1)), next_ns: None }
        }

        /// Whether the buffer stamped `pts_ns` is admitted.
        pub fn admit(&mut self, pts_ns: u64) -> bool {
            let Some(next) = self.next_ns else {
                self.next_ns = Some(pts_ns + self.period_ns);
                return true;
            };
            if next > pts_ns + Self::RESYNC_AHEAD_NS {
                self.next_ns = Some(pts_ns + self.period_ns);
                return true;
            }
            if pts_ns + self.period_ns / 2 < next {
                return false;
            }
            self.next_ns = Some(if pts_ns > next + self.period_ns { pts_ns + self.period_ns } else { next + self.period_ns });
            true
        }
    }

    /// Requests a new pad from `tee` and builds+links a fresh, independent
    /// encode branch onto it: `queue leaky=downstream max-size-buffers=2 !
    /// videoconvert ! videoscale ! videorate ! capsfilter(format,w,h,fps) !
    /// <encoder> ! capsfilter(h264) ! appsink`, added to `pipeline` and
    /// sync'd to its current state *before* being linked to the tee — so a
    /// branch added to an already-`Playing` pipeline starts flowing
    /// immediately instead of sitting `Null` while the tee tries to push
    /// buffers into it. On any failure after the tee pad has been requested
    /// and/or the bin added, this rolls back what it already did (removes
    /// the bin, nulls it, releases the tee pad) rather than leaking a
    /// half-wired branch into the pipeline.
    ///
    /// `id` is purely for naming (see [`BranchId`]'s doc comment); it plays
    /// no other role here.
    ///
    /// # Why `leaky=downstream max-size-buffers=2` is not optional
    ///
    /// `Why:` this is the exact fix that resolved this pipeline's own
    /// "freezes solid under sustained motion" bug on the *original* fixed
    /// encoder branch (see [`ENCODER_QUEUE_MAX_BUFFERS`]'s doc comment and
    /// [`build_pipeline`]'s doc comment for the full incident writeup): a
    /// plain `queue` defaults to non-leaky with a 10MB byte cap, which a real
    /// capture source's variable, motion-driven raw-frame rate can fill in a
    /// fraction of a second — once full, a non-leaky queue *blocks* rather
    /// than drops, and that block propagates upstream through the shared
    /// `tee` to the capture source itself, stalling every other branch on
    /// the same tee too, not just this one. Every dynamically-added branch
    /// shares that same upstream `tee` and capture source, so it is exactly
    /// as exposed to that failure mode as the original branch was — using
    /// the identical `leaky=downstream`/bounded-buffers discipline here is
    /// what keeps one slow or misbehaving destination's branch from ever
    /// being able to stall every other destination sharing this capture.
    /// This is non-negotiable for any *new* branch this module (or a future
    /// caller) ever builds.
    ///
    /// # Why a `videoconvert` here, beyond the plan doc's literal chain
    ///
    /// The pipeline's single `tee` carries one negotiated raw-video format
    /// shared by every branch already on it (whichever branch's requested
    /// caps "win" the negotiation through the shared `videoconvert` upstream
    /// of the tee — see this module's parent doc comment). A
    /// dynamically-added branch's [`EncodeSettings::backend`] can differ
    /// from whatever format is already flowing through the tee (e.g. a
    /// per-destination VAAPI-vs-software choice, expected in a later stage
    /// per the plan doc) — without its own `videoconvert`, a branch
    /// requesting a format the tee isn't already carrying would simply fail
    /// to negotiate. The existing preview branch already
    /// establishes this exact pattern for the same reason (it needs RGB, not
    /// whatever the tee happens to carry) — this branch follows it too,
    /// rather than assuming its [`EncodeSettings::backend`] always happens
    /// to match whichever branch won the tee's format negotiation first.
    pub fn add_branch(
        pipeline: &gst::Pipeline,
        tee: &gst::Element,
        id: BranchId,
        settings: EncodeSettings,
    ) -> Result<EncodeBranch> {
        let index = next_branch_index();
        // Defensive clamp, mirroring `build_pipeline`'s own — see
        // `MAX_FRAMERATE`'s doc comment for why this can never be skipped.
        let framerate = settings.framerate.clamp(MIN_FRAMERATE, MAX_FRAMERATE);
        // `settings.gop_millis` rather than the removed `GOP_SECONDS`
        // constant directly — see [`destination::VideoParams::gop_millis`].
        // Rounds rather than truncates so a short GOP (e.g. the Gaming
        // preset's sub-second interval) doesn't get silently floored to 0
        // frames at low framerates; `.max(1)` is the hard floor `key-int-max`
        // itself needs (0 would mean "never insert a keyframe past the
        // first" — see [`MAX_FRAMERATE`]'s doc comment on the same hazard).
        let gop_frames = (((settings.gop_millis as u64) * (framerate as u64) + 500) / 1000).max(1) as u32;
        let encoder = settings.backend.launch_fragment(
            settings.bitrate_kbps,
            gop_frames,
            settings.tune,
            settings.quality_level,
            settings.rate_control,
            settings.codec,
        );
        let raw_format = settings.backend.raw_format();
        // What the encoder element selected above will *actually* emit —
        // not `settings.codec` itself, since [`EncoderBackend::Software`]
        // silently always produces H.264 regardless of what was requested
        // (see [`EncoderBackend::actual_codec`]). This is what picks the
        // matching downstream caps immediately below.
        let actual_codec = settings.backend.actual_codec(settings.codec);
        // Output caps per codec — confirmed via `gst-inspect-1.0`'s SRC pad
        // template on each element (see [`VAAPI_H265_ENCODER_ELEMENT`]/
        // [`VAAPI_AV1_ENCODER_ELEMENT`]'s doc comments): `vah264enc`/
        // `vah265enc` both emit Annex-B (`stream-format=byte-stream,
        // alignment=au`), matching `x264enc`'s own output caps — only the
        // media type name differs. `vaav1enc` is fundamentally different:
        // AV1 has no Annex-B start-code framing at all, so its caps are
        // `stream-format=obu-stream,alignment=tu` (raw OBUs, one temporal
        // unit per buffer) — `decode/codec.rs` relies on exactly this
        // Annex-B-vs-OBU-stream distinction to tell an AV1 access unit apart
        // from an H.264/H.265 one with no other hint.
        let output_caps = match actual_codec {
            VideoCodec::H264 => "video/x-h264,stream-format=byte-stream,alignment=au",
            VideoCodec::H265 => "video/x-h265,stream-format=byte-stream,alignment=au",
            VideoCodec::Av1 => "video/x-av1,stream-format=obu-stream,alignment=tu",
        };
        // `Why` a size-less capsfilter when either axis is 0: 0 is this
        // codebase's "whatever the source is producing" sentinel (see
        // [`super::destination::clamp_to_source`], which treats it as "nothing
        // to clamp against"), and `width=0` in a capsfilter is not that — it's
        // an impossible constraint that fails negotiation outright. Leaving
        // both fields out entirely is what actually expresses "don't
        // downscale", letting `videoscale` pass the source resolution through.
        let scale_caps = if settings.width == 0 || settings.height == 0 {
            String::new()
        } else {
            format!(",width={},height={}", settings.width, settings.height)
        };

        let appsink_name = format!("branch_appsink_{index}");
        let bin_name = format!("branch_{index}");
        let encoder_name = format!("branch_encoder_{index}");
        let valve_name = format!("branch_valve_{index}");

        // `valve ... drop-mode=forward-sticky-events` right after the queue,
        // for exactly the reasons the pipeline's original fixed encoder branch
        // used one (see [`super::ENCODER_VALVE_NAME`]'s and
        // [`super::build_pipeline`]'s doc comments): pausing a destination
        // with no subscribers must stop the *encoder* burning CPU without
        // touching the shared tee or the pipeline state, and the mode must
        // forward sticky events so [`remove_branch`]'s injected EOS still
        // drains through a paused branch instead of hanging its 5s timeout.
        //
        // The target fps is a CAP, never a floor ([`RateCap`], a pad probe on
        // the valve's src pad). `Why (the #39 "3 unique fps behind a 169 fps
        // stream" bug):` the branch used to *pad* up to the target by
        // duplicating frames with `videorate drop-only=false`. When the
        // encoder cannot sustain the target (VA: H.264 ~165 fps, AV1 ~113 fps
        // at 1080p) the queue upstream drops frames, `videorate` answers each
        // gap with a run of duplicates, the encoder spends its time encoding
        // those, and the queue drops yet more real frames: a feedback loop that
        // left 3 real frames per second inside a "169 fps" stream. A cap has no
        // such loop: a slow encoder just gets fewer (all real) frames, and a
        // static screen is kept alive by `pipewiresrc`'s `keepalive-time` plus
        // the stall protocol's cached IDR, not by padding here. It also
        // removes the old pause-replay hazard (`max-duplication-time`), since
        // nothing is ever synthesised.
        //
        // The cap sits right after the valve, before the scale/convert stage,
        // so frames that will be dropped never cost a GPU/CPU pass. The
        // `capssetter` after the convert stage only stamps the cap's
        // `framerate=N/1` onto the caps: the encoder needs the real target
        // for its rate control (a variable-rate `0/1` makes `va*enc` assume
        // 30 fps and mis-size every frame), while nothing converts the rate.
        //
        // The scale/convert stage follows the capture domain
        // ([`CaptureDomain`]): on the GPU domain one `vapostproc` scales and
        // keeps the frame in VAMemory NV12 for the VA encoder (no download);
        // a software encoder branch on that tee downloads to I420 instead.
        // On the system domain it is the CPU `videoconvert ! videoscale`.
        let domain = CaptureDomain::of(pipeline);
        let convert = match (domain, settings.backend) {
            (CaptureDomain::Gpu, EncoderBackend::Vaapi) => {
                format!("! vapostproc ! video/x-raw(memory:VAMemory),format=NV12{scale_caps}")
            }
            (CaptureDomain::Gpu, EncoderBackend::Software) => {
                format!("! vapostproc ! video/x-raw,format={raw_format}{scale_caps}")
            }
            (CaptureDomain::System, _) => {
                format!("! videoconvert ! videoscale ! video/x-raw,format={raw_format}{scale_caps}")
            }
        };
        // The encoded-side appsink must never drop: it used to be
        // `drop=true`, which discards the *oldest* queued access unit, i.e.
        // possibly the IDR a new viewer needs or a reference frame the next
        // P-frames depend on. All load shedding happens in the raw-frame
        // `queue leaky=downstream` above (which drops the oldest *raw* frame);
        // if the consumer ever stalls, the encoder blocks and that queue
        // absorbs it.
        let description = format!(
            "queue leaky=downstream max-size-buffers={ENCODER_QUEUE_MAX_BUFFERS} \
             ! valve name={valve_name} drop=false drop-mode=forward-sticky-events \
             {convert} \
             ! capssetter caps=video/x-raw,framerate={framerate}/1 join=true replace=false \
             ! {encoder} name={encoder_name} \
             ! {output_caps} \
             ! appsink name={appsink_name} emit-signals=false sync=false max-buffers=8 drop=false"
        );

        // `ghost_unlinked_pads=true` auto-creates a ghost pad on the bin for
        // the one pad left unconnected inside it (the `queue`'s sink pad),
        // named after its target — `"sink"` — exactly what we need to link
        // the tee's new request pad into below.
        let bin = gst::parse::bin_from_description_with_name(&description, true, &bin_name)
            .with_context(|| format!("failed to construct encode branch bin: {description}"))?;

        let appsink = bin
            .by_name(&appsink_name)
            .ok_or_else(|| anyhow!("branch bin has no element named '{appsink_name}'"))?
            .downcast::<gst_app::AppSink>()
            .map_err(|_| anyhow!("'{appsink_name}' element is not an AppSink"))?;

        let encoder_element = bin
            .by_name(&encoder_name)
            .ok_or_else(|| anyhow!("branch bin has no element named '{encoder_name}'"))?;
        let valve = bin
            .by_name(&valve_name)
            .ok_or_else(|| anyhow!("branch bin has no element named '{valve_name}'"))?;

        let cap = Mutex::new(RateCap::new(framerate));
        valve
            .static_pad("src")
            .ok_or_else(|| anyhow!("valve '{valve_name}' has no src pad"))?
            .add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
                let Some(gst::PadProbeData::Buffer(buffer)) = &info.data else { return gst::PadProbeReturn::Ok };
                // A buffer without a timestamp cannot be paced; let it through.
                let Some(pts) = buffer.pts() else { return gst::PadProbeReturn::Ok };
                if cap.lock().unwrap().admit(pts.nseconds()) {
                    gst::PadProbeReturn::Ok
                } else {
                    gst::PadProbeReturn::Drop
                }
            })
            .ok_or_else(|| anyhow!("failed to install the frame-rate cap on '{valve_name}'"))?;

        let branch_sink_pad = bin.static_pad("sink").ok_or_else(|| {
            anyhow!("branch bin has no ghost sink pad (the queue's sink pad should have been auto-ghosted)")
        })?;

        let offered = Arc::new(AtomicU64::new(0));
        let counter = offered.clone();
        branch_sink_pad
            .add_probe(gst::PadProbeType::BUFFER, move |_pad, _info| {
                counter.fetch_add(1, Ordering::Relaxed);
                gst::PadProbeReturn::Ok
            })
            .ok_or_else(|| anyhow!("failed to install the offered-frame counter on branch {id:?}"))?;

        let tee_pad = tee
            .request_pad_simple("src_%u")
            .ok_or_else(|| anyhow!("tee refused to provide a new request pad (src_%u)"))?;

        if let Err(e) = pipeline.add(&bin) {
            tee.release_request_pad(&tee_pad);
            return Err(anyhow!("failed to add encode branch bin to the pipeline: {e}"));
        }

        if let Err(e) = bin.sync_state_with_parent() {
            // Roll back rather than leaving a half-wired bin (added to the
            // pipeline, not Playing, tee pad requested but unlinked) behind.
            let _ = pipeline.remove(&bin);
            let _ = bin.set_state(gst::State::Null);
            tee.release_request_pad(&tee_pad);
            return Err(anyhow!("encode branch bin failed to sync state with its parent pipeline: {e}"));
        }

        if let Err(e) = tee_pad.link(&branch_sink_pad) {
            let _ = pipeline.remove(&bin);
            let _ = bin.set_state(gst::State::Null);
            tee.release_request_pad(&tee_pad);
            return Err(anyhow!("failed to link tee's new request pad to the encode branch: {e:?}"));
        }

        Ok(EncodeBranch { id, appsink, bin, tee_pad, encoder: encoder_element, valve, stream: None, offered })
    }

    /// [`add_branch`] with the same per-branch VAAPI→software fallback
    /// [`super::start_sharing_with_source`] applies to the whole pipeline:
    /// if a [`EncoderBackend::Vaapi`] branch fails to construct or link, retry
    /// once on [`EncoderBackend::Software`] rather than failing the
    /// destination.
    ///
    /// `Why per branch, not once per process:` the existing whole-pipeline
    /// fallback only ever ran while there was exactly one encoder in flight.
    /// With fan-out there are N concurrent encode sessions off one capture,
    /// and VAAPI's concurrent-session limit is small (commonly 2–4) — so the
    /// *third* destination can fail on a machine where the first two succeeded
    /// and where [`detect_encoder_backend`]'s process-wide probe still
    /// correctly reports VAAPI as available. That failure is per-branch and
    /// transient, exactly the shape a per-branch fallback handles and a
    /// process-wide probe cannot.
    pub fn add_branch_with_fallback(
        pipeline: &gst::Pipeline,
        tee: &gst::Element,
        id: BranchId,
        settings: EncodeSettings,
    ) -> Result<EncodeBranch> {
        match add_branch(pipeline, tee, id, settings) {
            Ok(branch) => Ok(branch),
            Err(e) if settings.backend == EncoderBackend::Vaapi => {
                tracing::warn!(
                    "VAAPI encode branch {id:?} failed to build ({e:#}); falling back to software for this branch only"
                );
                add_branch(pipeline, tee, id, EncodeSettings { backend: EncoderBackend::Software, ..settings })
                    .context("software fallback encode branch also failed to build")
            }
            Err(e) => Err(e),
        }
    }

    /// How long a freshly-added branch gets to emit its first IDR before it's
    /// declared non-functional. Generous relative to a 2s GOP: the very first
    /// encoded frame is always an IDR, so anything approaching this means the
    /// branch is genuinely not encoding (a VAAPI session that constructed but
    /// can't run, a starved source), not merely slow.
    pub const FIRST_IDR_TIMEOUT: Duration = Duration::from_secs(5);

    /// Bound on [`remove_branch`]'s final `pipeline.remove`/
    /// `bin.set_state(Null)`/`release_request_pad` tail — see that call
    /// site's `Why:` note. Generous (well above anything measured on real
    /// hardware) because this is a last-resort backstop, not a normal-path
    /// timing budget.
    pub const BRANCH_TEARDOWN_TIMEOUT: Duration = Duration::from_secs(10);

    /// Bound on [`construct_branch_bounded`]'s off-thread
    /// `add_branch_with_fallback` call — see that function's `Why:` note for
    /// the real, reproduced hang this closes. 20s: generous relative to
    /// everything measured on real hardware (worst case seen was ~5s, for a
    /// cold `QualityLevel::Quality` construction at a very low framerate —
    /// see `exploratory_lookahead_timing_across_framerates`), while still
    /// being a hard backstop against the class of stuck
    /// GStreamer/driver-internal state change this was written for.
    pub const BRANCH_CONSTRUCTION_TIMEOUT: Duration = Duration::from_secs(20);

    /// Runs `add_branch_with_fallback` (a synchronous, blocking-GStreamer
    /// call — `bin_from_description_with_name`/`pipeline.add`/
    /// `bin.sync_state_with_parent`/`tee_pad.link`) on a `spawn_blocking`
    /// thread, bounded by [`BRANCH_CONSTRUCTION_TIMEOUT`].
    ///
    /// `Why this exists, mirroring [`remove_branch`]'s teardown tail:` this
    /// call was the one remaining *unbounded* blocking GStreamer call in the
    /// whole swap path, made directly on whatever task/thread
    /// [`add_branch_live`] runs on (in production, [`destination_pump`]'s own
    /// task). Real-hardware reproduction (see
    /// `capture::tests::exploratory_three_sequential_swaps_heartbeat` and
    /// `end_to_end_test::late_watch_after_all_three_tuning_controls_toggled_gets_a_real_keyframe_every_time`,
    /// both `#[ignore]`d) caught this call genuinely hanging — multiple
    /// minutes, no progress — for the third of three rapid sequential
    /// `VideoParams` swaps (tune, then quality_level, then rate_control) on
    /// VAAPI with `RateControlMode::Cqp` (`rate-control=cqp`) +
    /// `QualityLevel::Quality` (`target-usage=1`), even though the exact same
    /// settings applied as a single, isolated `add_branch` call were fast
    /// (tens of milliseconds) — i.e. this is triggered by cumulative
    /// VAAPI-session churn (a new hardware encode session being requested
    /// while the previous swap's session is still being torn down), not by
    /// these settings alone. A `gdb` backtrace could not be captured (this
    /// dev sandbox's `ptrace_scope` blocks attaching, even to a child of the
    /// tracer, and there is no passwordless `sudo` to lower it), but `/proc`
    /// showed the unmistakable shape of a genuine block: the tokio runtime's
    /// own thread parked in `futex_wait` while a *different* thread (a
    /// GStreamer/VAAPI-driver-internal worker, not owned by this process's
    /// async runtime) sat at 100% CPU — i.e. `sync_state_with_parent` really
    /// was waiting synchronously for something with no timeout protecting
    /// it, exactly as this project's own established pattern (every *other*
    /// blocking wait in this module — the IDR wait, the EOS drain, now the
    /// teardown tail — already has a bound) says it should never be able to.
    ///
    /// This does not "fix" whatever is slow/stuck inside GStreamer or the
    /// VAAPI driver on cumulative session churn — that's outside this
    /// module's reach. What it fixes is the blast radius: before this, that
    /// stall blocked the *entire* async runtime the destination's pump task
    /// shares with everything else (other destinations, the bus watcher,
    /// the reply channel back to the UI) for as long as it lasted — which in
    /// the reproduction above was still climbing past four minutes with no
    /// sign of ending when the process was killed. After this, the stall is
    /// confined to one `spawn_blocking` thread, the destination's own
    /// `ApplyParams` reply arrives (as a timeout error) within
    /// [`BRANCH_CONSTRUCTION_TIMEOUT`], and every other task on the runtime
    /// keeps running throughout.
    async fn construct_branch_bounded(
        pipeline: &gst::Pipeline,
        tee: &gst::Element,
        id: BranchId,
        settings: EncodeSettings,
    ) -> Result<EncodeBranch> {
        let pipeline = pipeline.clone();
        let tee = tee.clone();
        match tokio::time::timeout(
            BRANCH_CONSTRUCTION_TIMEOUT,
            tokio::task::spawn_blocking(move || add_branch_with_fallback(&pipeline, &tee, id, settings)),
        )
        .await
        {
            Ok(Ok(result)) => result,
            Ok(Err(join_err)) => Err(anyhow!("encode branch construction task panicked: {join_err}")),
            Err(_) => Err(anyhow!(
                "encode branch construction did not complete within {BRANCH_CONSTRUCTION_TIMEOUT:?} — a \
                 GStreamer/encoder state change appears stuck; giving up rather than blocking the caller \
                 (and everything sharing its async runtime) forever. The stuck construction thread is left \
                 to finish on its own (best-effort); it will not touch a valid tee pad the caller can see \
                 again since this branch was never returned to it"
            )),
        }
    }

    /// [`add_branch_with_fallback`], then actually wait for the branch to
    /// prove itself by producing its first IDR — falling back to software (and
    /// tearing the dud branch back down) if a VAAPI branch constructs fine but
    /// never encodes anything.
    ///
    /// `Why the second, runtime fallback:` construction success is a weak
    /// signal for hardware encode. `vah264enc` can be built and even reach
    /// `Playing` and *then* fail to produce output when the driver refuses
    /// another concurrent session — the same reason
    /// [`super::start_sharing_with_source`] doesn't trust
    /// [`probe_vaapi_encoder`] alone and re-checks against the real pipeline.
    /// This is that check, per branch.
    pub async fn add_branch_live(
        pipeline: &gst::Pipeline,
        tee: &gst::Element,
        id: BranchId,
        settings: EncodeSettings,
    ) -> Result<(EncodeBranch, gst::Sample)> {
        add_branch_live_waiting(pipeline, tee, id, settings, SourceWait::Bounded(SOURCE_FRAME_TIMEOUT)).await
    }

    /// How long [`add_branch_live`] gives an *already-running* capture to hand
    /// the tee its next buffer before giving up. With `keepalive-time` that
    /// is one frame period, so anything near this means the source is dead
    /// (not idle), and a branch swap should fail back to its old branch
    /// rather than hold the destination's pump forever.
    pub const SOURCE_FRAME_TIMEOUT: Duration = Duration::from_secs(10);

    /// How long a capture may stay silent before [`wait_for_source_frame`]
    /// starts saying so in the log.
    const SOURCE_SILENCE_WARN_AFTER: Duration = Duration::from_secs(3);

    /// How [`add_branch_live_waiting`] waits for the capture source's next
    /// buffer.
    #[derive(Debug, Clone, Copy)]
    pub enum SourceWait {
        /// Fail with an error after this long.
        Bounded(Duration),
        /// Wait as long as the capture is alive. For a destination's *first*
        /// branch and for rebuilding after a source restart, where "the
        /// screen has not changed yet" is a normal state, not a failure.
        Indefinite,
    }

    /// Resolves once the capture source has handed the `tee` a buffer —
    /// **before** the encode branch is built, so the encoder's IDR deadline
    /// ([`FIRST_IDR_TIMEOUT`]) only ever measures the encoder.
    ///
    /// # Why this exists — "sharing an idle monitor dies"
    ///
    /// `Why:` a portal `ScreenCast` stream on wlroots (`xdg-desktop-portal-wlr`
    /// over `wlr-screencopy`'s `copy_with_damage`) is **damage-driven in the
    /// strictest sense**: the compositor answers a capture request only when
    /// the output commits a new frame. On a monitor where nothing repaints it
    /// does not even answer the *first* request, so `pipewiresrc` never
    /// receives a single buffer. Measured on this machine against the real
    /// portal, DP-1 idle: `GST_DEBUG=pipewiresrc:6` shows the stream reach
    /// `streaming`, two buffers allocated, and then `popped buffer (nil)`
    /// forever; 0.0 fps at the tee for 20s. One pointer movement onto the
    /// monitor produced a frame, and from then on `keepalive-time` held a
    /// steady ~29 fps with the pointer gone.
    ///
    /// So `keepalive-time` is correct and necessary, but it can only *repeat*
    /// a buffer the element already has; it cannot conjure the first. Before
    /// this wait existed, the first destination's branch was built against a
    /// silent tee, burned its 5s IDR deadline on the hardware encoder, burned
    /// another 5s on the software retry, and the share died with "encode
    /// branch produced no IDR" — blaming the encoder for a screen that simply
    /// had not changed.
    ///
    /// Waiting is the honest behaviour: the destination stays "starting" and
    /// the share comes up the moment anything on that monitor changes. It
    /// returns an error only if `wait` is bounded and expires, or the capture
    /// pipeline is torn down underneath it (so a stopped capture never leaves
    /// this task parked forever).
    ///
    /// Implemented as a one-shot `BUFFER` probe on the tee's sink pad. In the
    /// steady state (a capture that already has a last buffer) the keepalive
    /// makes it fire within one frame period.
    pub async fn wait_for_source_frame(pipeline: &gst::Pipeline, tee: &gst::Element, wait: SourceWait) -> Result<()> {
        let pad = tee.static_pad("sink").ok_or_else(|| anyhow!("tee has no sink pad"))?;
        let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();
        let tx = Mutex::new(Some(tx));
        let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let probe_fired = fired.clone();
        let probe_id = pad.add_probe(gst::PadProbeType::BUFFER, move |_pad, _info| {
            if let Some(tx) = tx.lock().unwrap().take() {
                probe_fired.store(true, Ordering::Release);
                let _ = tx.send(());
            }
            gst::PadProbeReturn::Remove
        });
        // Dropping this future (the caller gave up) must not leave a probe
        // behind on a pad that outlives it.
        struct ProbeGuard {
            pad: gst::Pad,
            id: Option<gst::PadProbeId>,
            fired: Arc<std::sync::atomic::AtomicBool>,
        }
        impl Drop for ProbeGuard {
            fn drop(&mut self) {
                if let Some(id) = self.id.take()
                    && !self.fired.load(Ordering::Acquire)
                {
                    self.pad.remove_probe(id);
                }
            }
        }
        let _guard = ProbeGuard { pad, id: probe_id, fired };

        let started = Instant::now();
        let mut warned = false;
        loop {
            tokio::select! {
                _ = &mut rx => return Ok(()),
                _ = tokio::time::sleep(Duration::from_millis(250)) => {
                    if pipeline.current_state() == gst::State::Null {
                        return Err(anyhow!("the capture was stopped before its source produced a frame"));
                    }
                    let waited = started.elapsed();
                    if let SourceWait::Bounded(limit) = wait
                        && waited >= limit
                    {
                        return Err(anyhow!("the capture source produced no frame within {limit:?}"));
                    }
                    if !warned && waited >= SOURCE_SILENCE_WARN_AFTER {
                        warned = true;
                        tracing::warn!(
                            "the capture source has produced no frame in {waited:?}: an idle monitor only \
                             delivers a frame once something on it changes; waiting rather than failing"
                        );
                    }
                }
            }
        }
    }

    /// [`add_branch_live`] with an explicit policy for how long to wait for the
    /// capture source's next buffer first — see [`wait_for_source_frame`].
    pub async fn add_branch_live_waiting(
        pipeline: &gst::Pipeline,
        tee: &gst::Element,
        id: BranchId,
        settings: EncodeSettings,
        source_wait: SourceWait,
    ) -> Result<(EncodeBranch, gst::Sample)> {
        wait_for_source_frame(pipeline, tee, source_wait).await?;
        let mut branch = construct_branch_bounded(pipeline, tee, id, settings).await?;
        match branch.wait_for_first_idr(FIRST_IDR_TIMEOUT).await {
            Ok(sample) => Ok((branch, sample)),
            Err(e) if settings.backend == EncoderBackend::Vaapi => {
                tracing::warn!("VAAPI encode branch {id:?} produced no IDR ({e:#}); retrying on software");
                // Drain/remove the dud before building its replacement, so a
                // failed attempt never leaves an orphan branch on the tee.
                let _ = remove_branch(pipeline, tee, branch).await;
                let mut fallback =
                    construct_branch_bounded(pipeline, tee, id, EncodeSettings { backend: EncoderBackend::Software, ..settings })
                        .await
                        .context("software fallback encode branch failed to build")?;
                // Same cleanup contract as the two sibling failure paths: a
                // branch that was built and linked but is being abandoned
                // must be removed, never left orphaned on the tee.
                //
                // `Why:` without this, a fallback branch that also fails to
                // produce an IDR stays linked to a live `tee`, `Playing`,
                // encoding into an appsink nobody reads — forever, since the
                // caller never receives it and therefore can never remove it.
                // Every future `remove_branch`/`swap` on that capture leaves
                // it behind too, so a handful of failed adds silently pile up
                // parallel encoders on one capture. Mirrors the `let _ =
                // remove_branch(...)` in the VAAPI-dud arm above and the
                // generic `Err(e)` arm below.
                match fallback.wait_for_first_idr(FIRST_IDR_TIMEOUT).await {
                    Ok(sample) => Ok((fallback, sample)),
                    Err(e) => {
                        let _ = remove_branch(pipeline, tee, fallback).await;
                        Err(e).context("software fallback encode branch produced no IDR either")
                    }
                }
            }
            Err(e) => {
                let _ = remove_branch(pipeline, tee, branch).await;
                Err(e)
            }
        }
    }

    /// Safely removes `branch` from `pipeline`'s `tee`: blocks the tee's src
    /// pad feeding this branch, injects a real EOS event directly into the
    /// branch (bypassing the tee, which stays untouched and keeps serving
    /// every other branch), waits for that EOS to actually drain all the way
    /// through the branch to its `appsink` (bounded by a timeout so a
    /// misbehaving branch can never hang the caller forever), then unlinks,
    /// removes, nulls, and releases the tee's request pad — in that order,
    /// matching GStreamer's own documented idiom for dynamically removing a
    /// tee branch from a live pipeline.
    ///
    /// # Why draining to the appsink matters as much as the leaky queue does
    ///
    /// `Why:` skipping straight to `pipeline.remove()` + `set_state(Null)`
    /// (as if this were the original whole-pipeline teardown) is exactly the
    /// class of bug this project has already been bitten by once, in a
    /// different spot: [`CaptureHandle::stop`]'s doc comment records that
    /// jumping straight to `Null` on a pipeline never sends EOS, so anything
    /// downstream still expecting to see it can hang. Here the risk is
    /// narrower in blast radius (only this one branch, not the whole
    /// pipeline) but identical in kind: an encoder (`x264enc`/`vah264enc`)
    /// holds internal lookahead/reorder state that must be flushed on EOS to
    /// emit its last few encoded frames cleanly, and — more importantly —
    /// tearing the branch's elements down via `Null` while a buffer could
    /// still be in flight to it races the tee: if we unlink/release the tee
    /// pad while a buffer is mid-delivery, that pad could hand a buffer to
    /// now-`Null` elements. Blocking the tee's src pad first (so no further
    /// buffer for this branch can possibly arrive), only then injecting EOS,
    /// and only then waiting to actually observe the appsink go EOS before
    /// touching pipeline structure at all, is what makes branch removal safe
    /// to do repeatedly, live, next to other branches that must keep running
    /// untouched — reintroducing this exact "torn down without draining"
    /// shortcut here would resurrect the sustained-motion-freeze class of
    /// bug in a new spot, just as surely as skipping the leaky queue would.
    /// See this module's stress test
    /// (`repeated_add_remove_cycles_do_not_stall_the_pipeline`) for the
    /// guarantee this buys: dozens of add/remove cycles against a live
    /// pipeline with no stall and no leaked elements.
    pub async fn remove_branch(pipeline: &gst::Pipeline, tee: &gst::Element, mut branch: EncodeBranch) -> Result<()> {
        // Take over the branch's *existing* reader rather than installing a
        // second one — see [`EncodeBranch`]'s doc comment for why calling
        // `appsink.stream()` here would silently break whatever was already
        // reading this branch. `sample_stream()` creates one only if nobody
        // ever read this branch at all, so the "at most one AppSinkStream per
        // AppSink, ever" invariant holds on both paths. Taking the branch by
        // value is what makes this safe by construction: the production reader
        // cannot still be holding it.
        let _ = branch.sample_stream();
        let EncodeBranch { id: _, appsink: _, bin, tee_pad, encoder: _, valve: _, stream, offered: _ } = branch;
        let mut appsink_stream = stream.expect("sample_stream() populated it immediately above");

        let branch_sink_pad = bin
            .static_pad("sink")
            .ok_or_else(|| anyhow!("encode branch bin unexpectedly lost its ghost sink pad"))?;

        // Block the tee's src pad feeding this branch, then — from inside
        // that block, so no further buffer for this branch can possibly be
        // in flight past this point — inject a real EOS event directly into
        // the branch's own entry pad, and `Drop` the buffer/event that
        // triggered the block instead of letting it continue.
        //
        // `Why Drop, not Remove`: returning `Remove` here (tried first) lets
        // the item that triggered the block continue flowing to the branch
        // right after — but our synthetic EOS, injected synchronously inside
        // this same callback, has *already* been fully processed by the
        // branch's entry element by the time that happens (`send_event` on a
        // sink pad runs its event function inline). The formerly-blocked
        // buffer then arrives at an element that has already seen EOS, which
        // makes it return `GST_FLOW_EOS` (`gst_pad_chain_data_unchecked`
        // refuses a buffer on a pad that has already seen EOS). That flow
        // return then propagates *upstream*, and that is where the real
        // damage happens: `tee` hands it back out of its chain function, and
        // the capture source — `basesrc`, i.e. the real screen-capture
        // source in production and `videotestsrc` in this module's tests —
        // treats a `FLOW_EOS` from its push as "the stream is over", stops
        // producing, and pushes a genuine EOS *event* downstream. That event
        // lands as a sticky event on the **tee's sink pad**, and the tee
        // forwards its sticky events onto every src pad it has, plus every
        // src pad requested from it afterwards.
        //
        // So the blast radius is not "this one branch" and not "the next
        // branch to be added": it is the entire capture. Every fixed branch
        // (the real encoder, the preview) dies with it,
        // and every future `request_pad_simple` hands back a pad that is
        // already EOS-flagged, silently black-holing that branch's data with
        // no error ever posted to the bus. Measured directly, not inferred:
        // with `Remove`, the tee's *sink* pad reads `EOS_FLAG=true` /
        // `sticky_eos=true` immediately after the first removal, which can
        // only mean the EOS came down from upstream.
        //
        // `Drop` avoids all of it at the root: the triggering item never
        // reaches the branch, so nothing is ever pushed into an already-EOS'd
        // branch entry, no `FLOW_EOS` is ever returned upstream, and the
        // source keeps running. `Drop` is also what keeps the *other*
        // branches flowing while this one drains: a `tee` pushes to all of
        // its src pads sequentially from a single streaming thread, so
        // genuinely *blocking* this pad (returning `Ok` from a BLOCK probe)
        // would stall the tee — and therefore every sibling branch — for as
        // long as the drain below takes. `Drop` returns immediately and only
        // starves this one doomed pad. See
        // `removing_a_dynamic_branch_leaves_the_fixed_branches_producing`
        // for the regression guard; it asserts on the tee sink pad's EOS
        // flag precisely because that is the signal that flips first.
        //
        // We don't remove the probe afterward: it keeps dropping everything
        // aimed at this pad until the pad is unlinked and released moments
        // later, which is exactly what we want (the branch must stay starved
        // for the whole drain window).
        //
        // The `sent_eos` one-shot guard, however, *is* needed: this callback
        // fires once per dropped buffer for the entire drain window (tens of
        // times at 30fps), and only the first firing has any effect — EOS into
        // an already-EOS'd branch is a no-op. Re-sending it every time was
        // merely wasteful when nothing downstream cared; now that a
        // `DestinationState` machine and a stall protocol hang off branch
        // removal, keeping a per-buffer `send_event` (which runs the branch's
        // whole event chain inline on the tee's streaming thread) out of the
        // hot path is worth the one atomic. `Relaxed` is sufficient: the flag
        // guards an idempotent operation, not the publication of any data.
        let sent_eos = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let send_eos_once = {
            let sink_pad = branch_sink_pad.clone();
            let sent_eos = sent_eos.clone();
            move || {
                if !sent_eos.swap(true, Ordering::Relaxed) && !sink_pad.send_event(gst::event::Eos::new()) {
                    tracing::warn!("encode branch removal: EOS was not accepted by the branch's entry pad");
                }
            }
        };
        let send_eos_in_block = send_eos_once.clone();
        tee_pad.add_probe(gst::PadProbeType::BLOCK_DOWNSTREAM, move |_pad, _info| {
            send_eos_in_block();
            gst::PadProbeReturn::Drop
        });
        // `Why a second, IDLE probe:` the BLOCK_DOWNSTREAM probe above only
        // fires when *something* is pushed through the tee pad. On a silent
        // source (an idle desktop — the portal only delivers a buffer on
        // damage) nothing ever is, so the EOS would never be injected and the
        // drain below would always burn its full 5s timeout ("did not drain to
        // EOS within 5s"), leaving the branch's encoder undrained. An IDLE
        // probe fires immediately when the pad has nothing in flight, and
        // `send_eos_once`'s shared flag keeps the two from both sending.
        tee_pad.add_probe(gst::PadProbeType::IDLE, move |_pad, _info| {
            send_eos_once();
            gst::PadProbeReturn::Remove
        });

        // Wait for that EOS to actually reach the branch's appsink — not
        // just "we sent it" — by draining `appsink`'s sample stream until it
        // ends (mirrors `CaptureHandle::stop`'s use of the same
        // `AppSinkStream` "yields `None` only once real EOS arrives"
        // contract). Bounded so a branch that somehow never drains (a stuck
        // encoder, a GStreamer bug) fails loudly via a log line instead of
        // hanging every future `remove_branch` caller forever; pipeline
        // structure gets torn down regardless of whether this actually
        // observed EOS, matching `CaptureHandle::stop`'s established
        // "best-effort wait, then force it anyway" shape.
        let drained = tokio::time::timeout(Duration::from_secs(5), async {
            while appsink_stream.next().await.is_some() {}
        })
        .await
        .is_ok();
        if !drained {
            tracing::warn!("encode branch did not drain to EOS within 5s; removing it anyway");
        }

        let _ = tee_pad.unlink(&branch_sink_pad);

        // `Why this tail is wrapped in a bounded `spawn_blocking` + timeout,
        // unlike a plain sequential call:` every other blocking wait in this
        // function (the appsink drain just above) is already bounded — this
        // was the one exception: `pipeline.remove`/`bin.set_state(Null)` are
        // genuinely blocking GStreamer calls with no timeout at all around
        // them, on the same async task [`destination_pump`] awaits a branch
        // swap from. `set_state(Null)` in particular has to synchronously
        // tear down every element in `bin` — including the encoder, which
        // for a branch that's still deep in its own start-up (a slow
        // `speed-preset`/lookahead configuration on a low framerate can take
        // several real seconds just to reach its first output — see
        // `capture::tests::exploratory_lookahead_timing_across_framerates`,
        // which measured this getting close to [`FIRST_IDR_TIMEOUT`] on this
        // project's own reference hardware) is exactly the state a dud
        // branch gets torn down in ([`add_branch_live`]'s failure path calls
        // this function on precisely that branch). Real-hardware stress
        // testing (hundreds of swaps across the full framerate/preset
        // matrix, both encoder backends, warm and cold branches) never
        // caught this call actually taking long, let alone hanging — but it
        // was also the only blocking call in this whole module without a
        // bound, which is reason enough to close the gap: if a GStreamer
        // element ever *does* stick here (a known class of upstream
        // encoder-teardown bug), this turns a permanent hang of the
        // destination's pump task — which stops that destination from ever
        // processing another command again, `Stop` included — into a loud,
        // bounded failure instead. The blocking thread is deliberately left
        // to finish on its own past the timeout (there is no way to cancel a
        // blocked native call); that's an acceptable one-thread leak against
        // the alternative of hanging the pump forever.
        let pipeline = pipeline.clone();
        let tee = tee.clone();
        let teardown = tokio::task::spawn_blocking(move || {
            if let Err(e) = pipeline.remove(&bin) {
                tracing::warn!("failed to remove encode branch bin from the pipeline: {e}");
            }
            if let Err(e) = bin.set_state(gst::State::Null) {
                tracing::warn!("failed to stop encode branch bin: {e}");
            }
            tee.release_request_pad(&tee_pad);
        });
        if tokio::time::timeout(BRANCH_TEARDOWN_TIMEOUT, teardown).await.is_err() {
            tracing::error!(
                "encode branch teardown (pipeline.remove/bin.set_state(Null)/release_request_pad) did not \
                 complete within {BRANCH_TEARDOWN_TIMEOUT:?} — a GStreamer element's teardown appears stuck; \
                 giving up on it rather than hanging this destination's pump task forever"
            );
        }

        Ok(())
    }

    /// Make-before-break: builds and links a brand-new branch for
    /// `new_settings` via [`add_branch`], returning it already producing
    /// output alongside `old`'s still-live branch — this function never
    /// touches `old`; it isn't even a parameter. This is the plan doc's
    /// "branch-swap" mechanism (see this module's parent doc comment): the
    /// caller keeps reading `old`'s `appsink` as authoritative until the
    /// newly-returned branch emits its first IDR, then atomically flips
    /// which appsink it reads from and calls [`remove_branch`] on `old` —
    /// that Rust-side "wait for IDR, then flip" bookkeeping is app-level
    /// state this module doesn't own (see this module's parent doc
    /// comment), so it isn't implemented here.
    ///
    /// Exists as its own named function (rather than every swap call site
    /// just calling [`add_branch`] directly) purely to name the intent at
    /// the call site — a swap and a fresh additional destination are the
    /// same GStreamer mechanics today, but documenting them as distinct
    /// callers keeps the door open for the two to diverge later (e.g. a
    /// swap wanting to carry over the old branch's bitrate/rate-control
    /// history for a smoother ramp-up) without every call site needing to be
    /// re-audited to tell which kind of `add_branch` call it originally was.
    pub fn swap_branch(
        pipeline: &gst::Pipeline,
        tee: &gst::Element,
        id: BranchId,
        new_settings: EncodeSettings,
    ) -> Result<EncodeBranch> {
        add_branch(pipeline, tee, id, new_settings)
    }

    // -----------------------------------------------------------------
    // Orchestrators: the GStreamer mechanics above driven by the
    // `DestinationState` machine in `super::destination`
    // -----------------------------------------------------------------

    /// Result of [`swap_to_new_branch`]. Not a plain `Result<EncodeBranch>`
    /// on purpose.
    ///
    /// `Why:` a swap consumes the old branch by value (that ownership transfer
    /// *is* the appsink-reader handoff — see [`EncodeBranch`]'s doc comment),
    /// so a plain `Err` would destroy the caller's only handle to a branch
    /// that is still linked, still `Playing`, and still the destination's sole
    /// source of frames. Make-before-break is only make-before-break if the
    /// failure path gives the old branch back: a settings change that can't be
    /// applied must leave the viewer exactly as they were, never worse.
    pub enum SwapOutcome {
        /// The new branch is authoritative and the old one has been removed.
        Swapped(EncodeBranch),
        /// The swap failed; the returned branch is the **original**, untouched
        /// and still live. The destination has been returned to
        /// [`super::destination::DestinationState::Live`].
        KeptOld(EncodeBranch, anyhow::Error),
    }

    /// Tier-2 ([`destination::ChangeTier::BranchSwap`]) end to end:
    /// mark the destination `Rebuilding`, build the replacement branch
    /// alongside the still-running original, wait for the **new** branch's
    /// first IDR, only then flip authority and tear the original down.
    ///
    /// The viewer never sees anything: the old branch keeps producing for the
    /// entire window, and this path can never enter
    /// [`super::destination::DestinationState::Stalled`] — by construction,
    /// since there is always exactly one live branch at every instant.
    ///
    /// `new_settings` is applied as given; callers wanting the never-upscale
    /// policy should pass it through
    /// [`super::destination::clamp_to_source`] first.
    pub async fn swap_to_new_branch(
        pipeline: &gst::Pipeline,
        tee: &gst::Element,
        runtime: &destination::SharedDestination,
        old: EncodeBranch,
        new_id: BranchId,
        new_params: destination::VideoParams,
    ) -> SwapOutcome {
        runtime.lock().unwrap().begin_rebuild(Instant::now());

        let built = add_branch_live(pipeline, tee, new_id, new_params.to_encode_settings()).await;
        let (new_branch, first_idr) = match built {
            Ok(v) => v,
            Err(e) => {
                runtime.lock().unwrap().abandon_rebuild();
                return SwapOutcome::KeptOld(old, e);
            }
        };

        // The flip itself: from here on the new branch is authoritative. This
        // is a plain Rust state change (which appsink the pump reads), not a
        // GStreamer operation — which is exactly why it can be atomic and
        // therefore invisible.
        {
            // The replacement branch brings its own timeline with it (see
            // `on_branch_swapped`) — re-anchor the wire clock onto it here,
            // inside the same lock as the flip, so there is no instant at
            // which the new branch is authoritative while the wire clock is
            // still anchored on the old one's.
            let first_pts_us = first_idr.buffer().and_then(|b| b.pts()).map(|t| t.useconds()).unwrap_or(0);
            let mut runtime = runtime.lock().unwrap();
            runtime.set_params(new_params);
            runtime.on_branch_swapped(first_pts_us);
            runtime.finish_rebuild();
        }

        if let Err(e) = remove_branch(pipeline, tee, old).await {
            // The swap already succeeded; a messy teardown of the *old* branch
            // is a leak to log, not a reason to fail the destination.
            tracing::warn!("encode branch swap completed but removing the old branch failed: {e:#}");
        }

        SwapOutcome::Swapped(new_branch)
    }

    /// Tier-3 ([`super::destination::ChangeTier::SourceRestart`]), first half:
    /// the shared capture source is going away, so **every** destination fed
    /// by it stalls at the same instant.
    ///
    /// `Why all of them, unconditionally:` they share one real screen grab, so
    /// there is no way to keep any of them fed while it's down. The plan doc
    /// records the user explicitly accepting this ("as long as it doesn't look
    /// broken") — which is what the stall protocol delivers: no `StopStream`,
    /// the cached IDR keeps being re-sent at ~2fps, and wire PTS keeps
    /// advancing, so each viewer sees a low-framerate pause rather than a
    /// stream that disappeared.
    pub fn stall_all(runtimes: &[destination::SharedDestination], reason: destination::StallReason) {
        let now = Instant::now();
        for runtime in runtimes {
            runtime.lock().unwrap().begin_stall(reason, now);
        }
    }

    /// Tier-3, second half: bring one stalled destination back on a
    /// freshly-restarted capture. Re-anchors the wire clock onto the new
    /// pipeline's running time *before* any of its buffers can be sent (see
    /// [`super::destination::DestinationRuntime::on_pipeline_start`]), builds
    /// the branch, and waits for its first IDR — which is what returns the
    /// destination to `Live`.
    pub async fn rebuild_after_source_restart(
        pipeline: &gst::Pipeline,
        tee: &gst::Element,
        runtime: &destination::SharedDestination,
        id: BranchId,
        params: destination::VideoParams,
    ) -> Result<EncodeBranch> {
        // Order matters: re-anchor first. A buffer from the new pipeline that
        // reached `wire_pts_from_buffer` before this ran would be timestamped
        // against the *old* offset — i.e. back near zero — and the monotonic
        // clamp would then have to drag every subsequent frame forward at
        // +1µs, pinning the stream's apparent clock for as long as it took to
        // catch up.
        let running_time_us = pipeline.current_running_time().map(|t| t.useconds()).unwrap_or(0);
        runtime.lock().unwrap().on_pipeline_start(running_time_us, Instant::now());

        // Indefinite wait: a freshly restarted capture on an idle monitor has
        // no frame to give until something changes (see
        // [`wait_for_source_frame`]), and the destination is `Stalled` — its
        // viewers are being fed the cached IDR meanwhile — which is exactly
        // the state this protocol exists for. Failing here would turn "the
        // screen is idle" into a dead share.
        let (branch, first_idr) = add_branch_live_waiting(
            pipeline,
            tee,
            id,
            params.to_encode_settings(),
            SourceWait::Indefinite,
        )
        .await
        .inspect_err(|e| runtime.lock().unwrap().fail(format!("capture restart failed: {e:#}")))?;

        // The first IDR is what actually ends the stall — feed it through the
        // same accounting a normal frame takes, so the cached IDR and the
        // `Stalled` → `Live` transition happen exactly once, in one place.
        // If that one sample happens to be unmappable, the destination simply
        // stays `Stalled` until the pump sees the branch's next IDR (at most a
        // GOP away) — degraded, never wedged.
        if let Some(payload) = first_idr.buffer().and_then(|b| b.map_readable().ok()) {
            runtime.lock().unwrap().note_encoded_frame(true, payload.as_slice(), Instant::now());
        }

        Ok(branch)
    }

    #[cfg(test)]
    pub(super) mod tests {
        use super::*;

        /// Builds a minimal `videotestsrc ! videoconvert ! tee name=t`
        /// pipeline for these tests to add/remove/swap branches against —
        /// the same "swap `videotestsrc` in for the real capture source"
        /// pattern this file's other tests already use (see
        /// `run_backend_and_expect_encoded_buffer`'s doc comment in the
        /// parent `tests` module), just without the fixed encode/preview
        /// branches [`super::build_pipeline`] also adds, since these tests
        /// only care about *dynamic* branches on the tee.
        ///
        /// `Why:` includes one permanent `t. ! fakesink` branch even though
        /// no test here reads from it: matches production, where the real
        /// pipeline's `tee` always has its three fixed branches already
        /// linked (see [`super::build_pipeline`]) before any dynamic branch
        /// is ever added. A `tee` with *zero* linked src pads makes a live
        /// source's every push return `NOT_LINKED`, which `basesrc` treats
        /// as a fatal streaming error (posts an `Error` on the bus and stops
        /// pushing) rather than something to route around — with no fixed
        /// branch here, the source would error out before the first
        /// dynamically-added branch's own probe/link ever got a chance to
        /// run, an artifact of this synthetic test pipeline that a real
        /// capture pipeline can never hit.
        fn build_test_pipeline() -> (gst::Pipeline, gst::Element) {
            let description = "videotestsrc is-live=true pattern=smpte \
                 ! video/x-raw,width=64,height=64,framerate=30/1 \
                 ! videoconvert \
                 ! tee name=t \
                 t. ! queue leaky=downstream max-size-buffers=1 ! fakesink";
            let element = gst::parse::launch(description).expect("test pipeline should parse and link");
            let pipeline = element.downcast::<gst::Pipeline>().expect("top-level element should be a Pipeline");
            let tee = pipeline.by_name("t").expect("tee element should exist");
            (pipeline, tee)
        }

        /// Feeds `RateCap` a steady `source_fps` stream for `secs` seconds and
        /// returns how many frames it admits per second.
        fn cap_rate(cap_fps: u32, source_fps: u32, secs: u64) -> f64 {
            let mut cap = RateCap::new(cap_fps);
            let n = u64::from(source_fps) * secs;
            let admitted = (0..n).filter(|i| cap.admit(i * 1_000_000_000 / u64::from(source_fps))).count();
            admitted as f64 / secs as f64
        }

        #[test]
        fn rate_cap_holds_the_target_for_a_faster_source() {
            // 280 Hz portal into the 240 fps cap: 240, not the 140 that
            // "a full period since the last frame" would give.
            assert!((cap_rate(240, 280, 10) - 240.0).abs() < 2.0, "{}", cap_rate(240, 280, 10));
            assert!((cap_rate(30, 60, 10) - 30.0).abs() < 1.0);
            assert!((cap_rate(120, 280, 10) - 120.0).abs() < 2.0);
        }

        #[test]
        fn rate_cap_never_invents_or_pads_for_a_slower_source() {
            assert_eq!(cap_rate(240, 60, 5), 60.0);
            assert_eq!(cap_rate(60, 60, 5), 60.0);
            // 10 fps of "static screen" keepalive stays 10 fps.
            assert_eq!(cap_rate(240, 10, 5), 10.0);
        }

        #[test]
        fn rate_cap_does_not_burst_after_a_gap() {
            // 60 fps source, 30 fps cap, 1.5 s of nothing (a blocked encoder or
            // a paused valve), then the source resumes: the 0.5 s right after the
            // gap must still be ~30 fps. videorate drop-only passed 66, 60 and
            // 54 fps in the three half-seconds after such a gap.
            let mut cap = RateCap::new(30);
            let step = 1_000_000_000u64 / 60;
            for i in 0..120u64 {
                cap.admit(i * step);
            }
            let resume = 120 * step + 1_500_000_000;
            let admitted = (0..30u64).filter(|i| cap.admit(resume + i * step)).count();
            assert!((14..=16).contains(&admitted), "30 source frames after a gap admitted {admitted}, want ~15");
        }

        #[test]
        fn rate_cap_resynchronises_on_a_timeline_restart() {
            let mut cap = RateCap::new(30);
            for i in 0..60u64 {
                cap.admit(100_000_000_000 + i * 16_666_667);
            }
            // The segment restarted at 0: the very next frames must flow again
            // instead of being dropped until the old timeline is reached.
            assert!(cap.admit(0));
            assert!(cap.admit(33_333_334));
        }

        /// **The GPU capture domain** (VAMemory NV12 on the tee, `vapostproc`
        /// in every branch): with a `videotestsrc` standing in for the portal
        /// (it cannot hand out DMA-BUF, so `vapostproc` uploads), every codec
        /// the machine can encode comes up on it, a software branch comes up
        /// on the same tee (the per-branch fallback), the branches obey their
        /// rate caps, and both preview branches deliver RGB.
        ///
        /// Skips (passes) on machines without a working VA stack, where the
        /// GPU domain is never used.
        #[tokio::test]
        async fn gpu_domain_serves_every_codec_a_software_fallback_and_the_previews() {
            if gst::init().is_err() || !super::super::gpu_capture_available() {
                eprintln!("skipping: no VA stack here, the GPU capture domain is not used");
                return;
            }
            let built = super::super::build_pipeline_in(
                "videotestsrc is-live=true pattern=ball ! video/x-raw,width=640,height=360,framerate=120/1",
                super::super::CaptureDomain::Gpu,
            )
            .expect("GPU pipeline should construct");
            built.pipeline.set_state(gst::State::Playing).expect("pipeline should reach PLAYING");
            assert!(wait_until_playing(&built.pipeline, Duration::from_secs(30)), "pipeline never reached PLAYING");
            assert_eq!(super::super::CaptureDomain::of(&built.pipeline), super::super::CaptureDomain::Gpu);

            let base = EncodeSettings {
                width: 320,
                height: 180,
                framerate: 30,
                bitrate_kbps: 2000,
                backend: EncoderBackend::Vaapi,
                ..small_settings()
            };
            let mut branches = Vec::new();
            for (i, (backend, codec)) in [
                (EncoderBackend::Vaapi, VideoCodec::H264),
                (EncoderBackend::Vaapi, VideoCodec::H265),
                (EncoderBackend::Vaapi, VideoCodec::Av1),
                (EncoderBackend::Software, VideoCodec::H264),
            ]
            .into_iter()
            .enumerate()
            {
                let branch = add_branch(
                    &built.pipeline,
                    &built.tee,
                    BranchId(i as u32),
                    EncodeSettings { backend, codec, ..base },
                )
                .unwrap_or_else(|e| panic!("{backend:?}/{codec:?} branch must build on the GPU tee: {e:#}"));
                assert!(
                    wait_for_sample(&branch.appsink, Duration::from_secs(20)),
                    "{backend:?}/{codec:?} branch produced nothing on the GPU tee"
                );
                branches.push(branch);
            }

            // 120 fps source, 30 fps caps: ~30 per branch, never more.
            for branch in &branches {
                while branch.appsink.try_pull_sample(gst::ClockTime::ZERO).is_some() {}
                let window = Duration::from_secs(2);
                let deadline = Instant::now() + window;
                let mut n = 0u32;
                while Instant::now() < deadline {
                    if branch.appsink.try_pull_sample(gst::ClockTime::from_mseconds(20)).is_some() {
                        n += 1;
                    }
                }
                let fps = f64::from(n) / window.as_secs_f64();
                assert!((15.0..=36.0).contains(&fps), "a 30 fps cap on a 120 fps GPU tee produced {fps:.1} fps");
            }

            assert!(
                wait_for_sample(&built.preview_sink, Duration::from_secs(10)),
                "the preview branch produced nothing on the GPU tee"
            );

            // Removing a branch from the live GPU tee leaves the siblings running.
            let removed = branches.remove(0);
            remove_branch(&built.pipeline, &built.tee, removed).await.expect("removal should succeed");
            for branch in &branches {
                while branch.appsink.try_pull_sample(gst::ClockTime::ZERO).is_some() {}
                assert!(wait_for_sample(&branch.appsink, Duration::from_secs(5)), "a sibling stopped after a removal");
            }

            // A tier-2 swap (codec + size + rate) on the live GPU tee works too.
            let runtime = destination::DestinationRuntime::shared(Instant::now(), small_params());
            runtime.lock().unwrap().note_encoded_frame(true, b"idr", Instant::now());
            let first = branches.remove(0);
            let swapped = swap_to_new_branch(
                &built.pipeline,
                &built.tee,
                &runtime,
                first,
                BranchId(10),
                VideoParams {
                    width: 160,
                    height: 90,
                    framerate: 20,
                    bitrate_kbps: 1000,
                    backend: EncoderBackend::Vaapi,
                    codec: VideoCodec::H265,
                    ..small_params()
                },
            )
            .await;
            let swapped = match swapped {
                SwapOutcome::Swapped(branch) => branch,
                SwapOutcome::KeptOld(_, e) => panic!("the GPU-tee swap should have succeeded: {e:#}"),
            };
            assert!(wait_for_sample(&swapped.appsink, Duration::from_secs(5)), "the swapped branch is silent");
            built.pipeline.set_state(gst::State::Null).expect("pipeline should stop cleanly");
        }

        fn small_settings() -> EncodeSettings {
            EncodeSettings {
                width: 64,
                height: 64,
                framerate: 15,
                bitrate_kbps: 500,
                backend: EncoderBackend::Software,
                // Deliberately not `TuneMode`/`QualityLevel::default()`:
                // these tests are exercising branch/pause/resume
                // *mechanics*, not the tuning knobs, and need the encoder to
                // reliably keep up in real time on a plain CI machine.
                // `BestQuality` (no `tune=zerolatency`) lets x264enc hold
                // frames in its lookahead/B-frame buffer before emitting any
                // output at all, and `Balanced`'s `medium` preset is
                // measurably slower than `ultrafast` — both were observed to
                // starve these short real-time pipelines of frames within
                // the test's time budget. Pin the fast/low-latency end
                // explicitly so this stays deterministic regardless of what
                // `TuneMode`/`QualityLevel`'s defaults are.
                tune: TuneMode::LowLatency,
                quality_level: QualityLevel::Performance,
                rate_control: RateControlMode::default(),
                gop_millis: DEFAULT_GOP_MILLIS,
                codec: VideoCodec::default(),
            }
        }

        /// Taps the tee with a plain `queue ! appsink` — **no `videorate`** —
        /// and measures how many raw frames actually come off it per second.
        ///
        /// `Why not measure at an encode branch's appsink:` every encode
        /// branch has its own `videorate drop-only=false`, which *duplicates*
        /// frames up to its configured rate. A branch therefore reports its
        /// own rate no matter what the source is doing, which is exactly the
        /// point of per-destination framerates — and exactly why it cannot be
        /// used to observe the shared source rate. This tap has nothing
        /// between the tee and the sink, so what it counts is what the tee is
        /// really carrying.
        pub(in crate::capture) fn tap_tee_rate(pipeline: &gst::Pipeline, tee: &gst::Element, window: Duration) -> f64 {
            let bin = gst::parse::bin_from_description(
                "queue leaky=downstream max-size-buffers=2 ! appsink name=tap emit-signals=false sync=false max-buffers=8 drop=true",
                true,
            )
            .expect("tap bin should parse");
            let appsink = bin
                .by_name("tap")
                .expect("tap appsink should exist")
                .downcast::<gst_app::AppSink>()
                .expect("tap should be an AppSink");
            let tee_pad = tee.request_pad_simple("src_%u").expect("tee should give a pad");
            pipeline.add(&bin).expect("tap bin should add");
            bin.sync_state_with_parent().expect("tap bin should sync state");
            tee_pad.link(&bin.static_pad("sink").expect("ghost pad")).expect("tap should link");

            // Discard whatever was queued while linking, then count.
            while appsink.try_pull_sample(gst::ClockTime::ZERO).is_some() {}
            let deadline = Instant::now() + window;
            let mut count = 0u32;
            while Instant::now() < deadline {
                if appsink.try_pull_sample(gst::ClockTime::from_mseconds(50)).is_some() {
                    count += 1;
                }
            }

            let _ = tee_pad.unlink(&bin.static_pad("sink").expect("ghost pad"));
            let _ = pipeline.remove(&bin);
            let _ = bin.set_state(gst::State::Null);
            tee.release_request_pad(&tee_pad);
            count as f64 / window.as_secs_f64()
        }

        /// Blocks until `pipeline` has actually reached `Playing`, or the
        /// timeout elapses.
        ///
        /// `Why not just `pipeline.state(timeout)`:` that returns
        /// `Ok(StateChangeSuccess::Async)` when the change is still in
        /// flight, which reads as success and lets a test carry on against a
        /// pipeline that is still `Paused`. Harmless when a test runs alone
        /// (the change completes in milliseconds); under `cargo test`'s full
        /// parallelism, a dozen live GStreamer pipelines coming up at once
        /// pushed this past 5s and produced a "the branch produced nothing"
        /// failure whose real cause was "the pipeline was never playing".
        fn wait_until_playing(pipeline: &gst::Pipeline, timeout: Duration) -> bool {
            let deadline = Instant::now() + timeout;
            while Instant::now() < deadline {
                let (_result, current, _pending) = pipeline.state(gst::ClockTime::from_mseconds(200));
                if current == gst::State::Playing {
                    return true;
                }
            }
            false
        }

        /// Polls `appsink` for up to `timeout` and reports whether at least
        /// one sample arrived — the same bounded polling shape this file's
        /// other tests use (e.g. `run_backend_and_expect_encoded_buffer`),
        /// just factored out since every test below needs it at least once.
        fn wait_for_sample(appsink: &gst_app::AppSink, timeout: Duration) -> bool {
            let deadline = Instant::now() + timeout;
            while Instant::now() < deadline {
                if appsink.try_pull_sample(gst::ClockTime::from_mseconds(50)).is_some() {
                    return true;
                }
            }
            false
        }

        /// [`add_branch`] on an already-`Playing` pipeline must actually
        /// produce encoded H.264 output — the basic "the mechanics work at
        /// all" proof every other test here builds on.
        #[test]
        fn add_branch_on_playing_pipeline_produces_encoded_output() {
            if gst::init().is_err() {
                eprintln!("skipping: gstreamer not available in this environment");
                return;
            }

            let (pipeline, tee) = build_test_pipeline();
            pipeline.set_state(gst::State::Playing).expect("pipeline should reach PLAYING");
            let (state_result, ..) = pipeline.state(gst::ClockTime::from_seconds(5));
            state_result.expect("pipeline should settle into PLAYING");

            let branch =
                add_branch(&pipeline, &tee, BranchId(1), small_settings()).expect("add_branch should succeed");
            assert_eq!(branch.id, BranchId(1), "returned branch should carry the id it was created with");

            assert!(
                wait_for_sample(&branch.appsink, Duration::from_secs(5)),
                "a branch added to a live pipeline should produce at least one encoded sample within 5s"
            );

            pipeline.set_state(gst::State::Null).expect("pipeline should stop cleanly");
        }

        /// The core "other destinations aren't affected" guarantee the whole
        /// design depends on: removing one branch must not disturb a sibling
        /// branch on the same tee, which must keep producing output
        /// afterward exactly as if nothing happened.
        #[tokio::test]
        async fn removing_one_branch_leaves_the_other_branch_producing_output() {
            if gst::init().is_err() {
                eprintln!("skipping: gstreamer not available in this environment");
                return;
            }

            let (pipeline, tee) = build_test_pipeline();
            pipeline.set_state(gst::State::Playing).expect("pipeline should reach PLAYING");
            let (state_result, ..) = pipeline.state(gst::ClockTime::from_seconds(5));
            state_result.expect("pipeline should settle into PLAYING");

            let branch_a =
                add_branch(&pipeline, &tee, BranchId(1), small_settings()).expect("add_branch(a) should succeed");
            let branch_b =
                add_branch(&pipeline, &tee, BranchId(2), small_settings()).expect("add_branch(b) should succeed");

            assert!(
                wait_for_sample(&branch_a.appsink, Duration::from_secs(5)),
                "branch a should produce output before being removed"
            );
            assert!(
                wait_for_sample(&branch_b.appsink, Duration::from_secs(5)),
                "branch b should produce output before branch a is removed"
            );

            tokio::time::timeout(Duration::from_secs(10), remove_branch(&pipeline, &tee, branch_a))
                .await
                .expect("remove_branch(a) should not hang")
                .expect("remove_branch(a) should succeed");

            let (state_result, current, _pending) = pipeline.state(gst::ClockTime::from_seconds(5));
            state_result.expect("pipeline should remain in a valid state after removing branch a");
            assert_eq!(current, gst::State::Playing, "pipeline should still be Playing after removing branch a");

            assert!(
                wait_for_sample(&branch_b.appsink, Duration::from_secs(5)),
                "branch b must keep producing output after branch a was removed — other destinations must not be affected"
            );

            pipeline.set_state(gst::State::Null).expect("pipeline should stop cleanly");
        }

        /// Builds a test pipeline whose tee carries *fixed* branches shaped
        /// like the real [`super::build_pipeline`]'s — an encoder branch and
        /// an RGB preview branch, each ending in its own readable `appsink`
        /// — so a test can assert that dynamic add/remove leaves the
        /// **pre-existing** branches producing, not merely that one dynamic
        /// branch doesn't disturb another dynamic branch.
        ///
        /// `Why:` this is deliberately a second, heavier fixture rather than
        /// an extension of [`build_test_pipeline`]: the failure mode it
        /// exists to catch (see
        /// `removing_a_dynamic_branch_leaves_the_fixed_branches_producing`)
        /// is *upstream* EOS propagation, which is invisible to a fixture
        /// whose only fixed branch is an unread `fakesink` — nothing asserts
        /// a `fakesink` is still being fed, so the source could have shut
        /// down completely and every dynamic-vs-dynamic test would still
        /// pass.
        fn build_fixed_branch_test_pipeline() -> (gst::Pipeline, gst::Element, gst_app::AppSink, gst_app::AppSink) {
            let encoder = EncoderBackend::Software.launch_fragment(
                500,
                30,
                TuneMode::default(),
                QualityLevel::default(),
                RateControlMode::default(),
                VideoCodec::default(),
            );
            let raw_format = EncoderBackend::Software.raw_format();
            let description = format!(
                "videotestsrc is-live=true pattern=smpte \
                 ! video/x-raw,width=64,height=64,framerate=30/1 \
                 ! videoconvert \
                 ! tee name=t \
                 t. ! queue leaky=downstream max-size-buffers={ENCODER_QUEUE_MAX_BUFFERS} \
                    ! videorate drop-only=false max-duplication-time=0 \
                    ! video/x-raw,format={raw_format},framerate=15/1 \
                    ! {encoder} \
                    ! video/x-h264,stream-format=byte-stream,alignment=au \
                    ! appsink name=fixed_encoder_sink emit-signals=false sync=false max-buffers=4 drop=true \
                 t. ! queue leaky=downstream max-size-buffers=1 \
                    ! videorate \
                    ! video/x-raw,framerate=5/1 \
                    ! videoconvert \
                    ! video/x-raw,format=RGB \
                    ! appsink name=fixed_preview_sink emit-signals=false sync=false max-buffers=1 drop=true"
            );
            let element = gst::parse::launch(&description).expect("fixed-branch test pipeline should parse and link");
            let pipeline = element.downcast::<gst::Pipeline>().expect("top-level element should be a Pipeline");
            let tee = pipeline.by_name("t").expect("tee element should exist");
            let encoder_sink = pipeline
                .by_name("fixed_encoder_sink")
                .expect("fixed encoder sink should exist")
                .downcast::<gst_app::AppSink>()
                .expect("fixed_encoder_sink should be an AppSink");
            let preview_sink = pipeline
                .by_name("fixed_preview_sink")
                .expect("fixed preview sink should exist")
                .downcast::<gst_app::AppSink>()
                .expect("fixed_preview_sink should be an AppSink");
            (pipeline, tee, encoder_sink, preview_sink)
        }

        /// Adding and removing *dynamic* branches must leave the pipeline's
        /// pre-existing **fixed** branches — the real encoder branch and the
        /// RGB preview branch that exist before any dynamic branch is ever
        /// added (see [`super::build_pipeline`]) — producing output exactly
        /// as before.
        ///
        /// `Why:` this is the regression guard for the specific, silent
        /// failure mode that the `Drop`-not-`Remove` pad-probe return exists
        /// to prevent (see [`remove_branch`]'s probe comment). Letting the
        /// blocked buffer through to an already-EOS'd branch makes that
        /// branch return `GST_FLOW_EOS`, which propagates *upstream* — the
        /// `videotestsrc`/real capture source treats it as end-of-stream and
        /// pushes a real EOS event down through the shared tee, killing
        /// **every** branch on it and poisoning every future request pad via
        /// sticky-event forwarding. That blast radius is the whole capture,
        /// not just dynamic branches, so it has to be asserted against the
        /// fixed branches specifically: the dynamic-vs-dynamic tests above
        /// cannot see it, because they tear down and rebuild their dynamic
        /// branches on every cycle anyway.
        ///
        /// `Why:` the load-bearing assertion here is the tee **sink pad**'s
        /// EOS-flag check, not the "did a sample arrive" checks. Verified by
        /// flipping the probe back to `PadProbeReturn::Remove`: the sample
        /// assertions still pass on the first cycle, because each fixed
        /// `appsink` is holding already-buffered samples (`max-buffers=4
        /// drop=true`) that keep arriving for a moment after the source has
        /// actually died — starvation only becomes observable once that
        /// backlog drains. The EOS flag on the tee's sink pad flips
        /// *immediately* and unambiguously, so it catches the regression on
        /// the very first cycle instead of depending on buffer-drain timing.
        /// Don't drop it in favour of the sample checks alone.
        #[tokio::test]
        async fn removing_a_dynamic_branch_leaves_the_fixed_branches_producing() {
            if gst::init().is_err() {
                eprintln!("skipping: gstreamer not available in this environment");
                return;
            }

            let (pipeline, tee, fixed_encoder_sink, fixed_preview_sink) = build_fixed_branch_test_pipeline();
            pipeline.set_state(gst::State::Playing).expect("pipeline should reach PLAYING");
            let (state_result, ..) = pipeline.state(gst::ClockTime::from_seconds(5));
            state_result.expect("pipeline should settle into PLAYING");

            assert!(
                wait_for_sample(&fixed_encoder_sink, Duration::from_secs(5)),
                "fixed encoder branch should produce output before any dynamic branch exists"
            );
            assert!(
                wait_for_sample(&fixed_preview_sink, Duration::from_secs(5)),
                "fixed preview branch should produce output before any dynamic branch exists"
            );

            // Several cycles, since the poisoning this guards against only
            // becomes visible after the first remove.
            for cycle in 0..5u32 {
                let branch = add_branch(&pipeline, &tee, BranchId(cycle), small_settings())
                    .unwrap_or_else(|e| panic!("cycle {cycle}: add_branch failed: {e:#}"));
                assert!(
                    wait_for_sample(&branch.appsink, Duration::from_secs(5)),
                    "cycle {cycle}: dynamic branch produced no output"
                );

                tokio::time::timeout(Duration::from_secs(10), remove_branch(&pipeline, &tee, branch))
                    .await
                    .unwrap_or_else(|_| panic!("cycle {cycle}: remove_branch hung past its timeout"))
                    .unwrap_or_else(|e| panic!("cycle {cycle}: remove_branch failed: {e:#}"));

                assert!(
                    wait_for_sample(&fixed_encoder_sink, Duration::from_secs(5)),
                    "cycle {cycle}: fixed encoder branch stopped producing after a dynamic branch was removed"
                );
                assert!(
                    wait_for_sample(&fixed_preview_sink, Duration::from_secs(5)),
                    "cycle {cycle}: fixed preview branch stopped producing after a dynamic branch was removed"
                );

                let tee_sink = tee.static_pad("sink").expect("tee should have a sink pad");
                assert!(
                    !tee_sink.pad_flags().contains(gst::PadFlags::EOS),
                    "cycle {cycle}: the tee's sink pad went EOS — branch removal leaked EOS upstream into the shared \
                     capture source, which kills every branch on this tee"
                );
            }

            pipeline.set_state(gst::State::Null).expect("pipeline should stop cleanly");
        }

        /// The highest-risk piece of this whole mechanism per the
        /// architecture agent's own assessment: repeated add/remove cycles
        /// against a live pipeline must never stall it and must never leak
        /// elements. ~50 cycles, each bounded by its own timeout so a real
        /// hang fails this test loudly instead of wedging CI.
        #[tokio::test]
        async fn repeated_add_remove_cycles_do_not_stall_the_pipeline() {
            if gst::init().is_err() {
                eprintln!("skipping: gstreamer not available in this environment");
                return;
            }

            let (pipeline, tee) = build_test_pipeline();
            pipeline.set_state(gst::State::Playing).expect("pipeline should reach PLAYING");
            let (state_result, ..) = pipeline.state(gst::ClockTime::from_seconds(5));
            state_result.expect("pipeline should settle into PLAYING");

            let baseline_children = pipeline.children().len();

            for cycle in 0..50u32 {
                // `add_branch` is synchronous (no waiting on any GStreamer
                // event) — it either finishes near-instantly or errors, so
                // unlike `remove_branch` below it needs no timeout wrapper
                // of its own to guard against a hang.
                let branch = add_branch(&pipeline, &tee, BranchId(cycle), small_settings())
                    .unwrap_or_else(|e| panic!("cycle {cycle}: add_branch failed: {e:#}"));
                assert_eq!(branch.id, BranchId(cycle), "cycle {cycle}: returned branch should carry the id it was created with");

                assert!(
                    wait_for_sample(&branch.appsink, Duration::from_secs(5)),
                    "cycle {cycle}: branch produced no output"
                );

                tokio::time::timeout(Duration::from_secs(10), remove_branch(&pipeline, &tee, branch))
                    .await
                    .unwrap_or_else(|_| panic!("cycle {cycle}: remove_branch hung past its timeout"))
                    .unwrap_or_else(|e| panic!("cycle {cycle}: remove_branch failed: {e:#}"));

                let (state_result, current, _pending) = pipeline.state(gst::ClockTime::from_seconds(5));
                state_result.unwrap_or_else(|e| panic!("cycle {cycle}: pipeline state query failed: {e:?}"));
                assert_eq!(current, gst::State::Playing, "cycle {cycle}: pipeline dropped out of Playing");

                assert_eq!(
                    pipeline.children().len(),
                    baseline_children,
                    "cycle {cycle}: pipeline element count did not return to baseline after removal — a leaked branch"
                );
            }

            pipeline.set_state(gst::State::Null).expect("pipeline should stop cleanly");
        }

        /// [`swap_branch`]'s make-before-break contract: right after it
        /// returns, both the new branch and the still-not-yet-removed old
        /// branch must be live and producing output simultaneously — the
        /// caller (a later stage) is what eventually calls [`remove_branch`]
        /// on the old one, never `swap_branch` itself.
        #[tokio::test]
        async fn swap_branch_keeps_old_branch_alive_while_new_branch_produces_output() {
            if gst::init().is_err() {
                eprintln!("skipping: gstreamer not available in this environment");
                return;
            }

            let (pipeline, tee) = build_test_pipeline();
            pipeline.set_state(gst::State::Playing).expect("pipeline should reach PLAYING");
            let (state_result, ..) = pipeline.state(gst::ClockTime::from_seconds(5));
            state_result.expect("pipeline should settle into PLAYING");

            let old = add_branch(&pipeline, &tee, BranchId(1), small_settings()).expect("add_branch should succeed");
            assert!(
                wait_for_sample(&old.appsink, Duration::from_secs(5)),
                "old branch should produce output before the swap"
            );

            let new_settings = EncodeSettings { bitrate_kbps: 900, ..small_settings() };
            let new = swap_branch(&pipeline, &tee, BranchId(2), new_settings).expect("swap_branch should succeed");

            // Both branches must coexist and produce output right away —
            // make-before-break means the old branch is never disturbed by
            // the swap call itself.
            assert!(
                wait_for_sample(&new.appsink, Duration::from_secs(5)),
                "new branch should produce output immediately after swap_branch returns"
            );
            assert!(
                wait_for_sample(&old.appsink, Duration::from_secs(5)),
                "old branch should still be producing output after the swap — swap_branch must not touch it"
            );

            tokio::time::timeout(Duration::from_secs(10), remove_branch(&pipeline, &tee, old))
                .await
                .expect("remove_branch(old) should not hang")
                .expect("remove_branch(old) should succeed");
            tokio::time::timeout(Duration::from_secs(10), remove_branch(&pipeline, &tee, new))
                .await
                .expect("remove_branch(new) should not hang")
                .expect("remove_branch(new) should succeed");

            pipeline.set_state(gst::State::Null).expect("pipeline should stop cleanly");
        }

        // -------------------------------------------------------------
        // Orchestrators: branch mechanics driven by the DestinationState
        // machine (see `super::super::destination`)
        // -------------------------------------------------------------

        use super::super::destination::{
            DestinationRuntime, DestinationState, SharedDestination, StallReason, VideoParams,
        };

        fn small_params() -> VideoParams {
            VideoParams {
                width: 64,
                height: 64,
                framerate: 15,
                bitrate_kbps: 500,
                backend: EncoderBackend::Software,
                tune: TuneMode::default(),
                quality_level: QualityLevel::default(),
                rate_control: RateControlMode::default(),
                gop_millis: DEFAULT_GOP_MILLIS,
                codec: VideoCodec::default(),
            }
        }

        /// Adds a branch, waits for its first IDR, and feeds that IDR through
        /// the runtime — i.e. brings a brand-new destination from `Starting`
        /// all the way to `Live`, the same way `pump_loop` does in production.
        async fn start_live_destination(
            pipeline: &gst::Pipeline,
            tee: &gst::Element,
            id: BranchId,
            params: VideoParams,
        ) -> (EncodeBranch, SharedDestination) {
            let runtime = DestinationRuntime::shared(Instant::now(), params);
            runtime.lock().unwrap().on_pipeline_start(0, Instant::now());

            let (branch, idr) = add_branch_live(pipeline, tee, id, params.to_encode_settings())
                .await
                .unwrap_or_else(|e| panic!("{id:?}: add_branch_live failed: {e:#}"));
            let payload = idr
                .buffer()
                .and_then(|b| b.map_readable().ok())
                .expect("the first IDR sample should be mappable");
            runtime.lock().unwrap().note_encoded_frame(true, payload.as_slice(), Instant::now());
            assert_eq!(runtime.lock().unwrap().state(), DestinationState::Live);
            (branch, runtime)
        }

        /// The tier-2 guarantee, end to end against a live pipeline: a branch
        /// swap goes `Live` → `Rebuilding` → `Live` and **never** passes
        /// through `Stalled` — no viewer-visible interruption at any instant.
        ///
        /// `Why the polling watcher rather than checking the state before and
        /// after:` "ended up Live" is trivially true of a stall that recovered.
        /// The whole claim being tested is about the *interval*, so the test
        /// has to sample the interval. The watcher runs on its own worker
        /// thread (hence `multi_thread`) so it keeps sampling even while the
        /// swap is inside a blocking GStreamer call rather than an await point.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_branch_swap_goes_rebuilding_then_live_and_never_stalls() {
            if gst::init().is_err() {
                eprintln!("skipping: gstreamer not available in this environment");
                return;
            }

            let (pipeline, tee) = build_test_pipeline();
            pipeline.set_state(gst::State::Playing).expect("pipeline should reach PLAYING");
            let (state_result, ..) = pipeline.state(gst::ClockTime::from_seconds(5));
            state_result.expect("pipeline should settle into PLAYING");

            let (branch, runtime) = start_live_destination(&pipeline, &tee, BranchId(1), small_params()).await;

            let observed: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let watcher = {
                let (runtime, observed, stop) = (runtime.clone(), observed.clone(), stop.clone());
                tokio::spawn(async move {
                    while !stop.load(Ordering::Relaxed) {
                        {
                            let label = runtime.lock().unwrap().state().label();
                            let mut seen = observed.lock().unwrap();
                            if seen.last() != Some(&label) {
                                seen.push(label);
                            }
                        }
                        tokio::time::sleep(Duration::from_millis(2)).await;
                    }
                })
            };

            // A resolution + bitrate change: tier 2, one destination only.
            let new_params = VideoParams { width: 32, height: 32, bitrate_kbps: 900, ..small_params() };
            let outcome =
                swap_to_new_branch(&pipeline, &tee, &runtime, branch, BranchId(2), new_params).await;

            stop.store(true, Ordering::Relaxed);
            watcher.await.expect("watcher task should not panic");

            let new_branch = match outcome {
                SwapOutcome::Swapped(b) => b,
                SwapOutcome::KeptOld(_, e) => panic!("swap should have succeeded: {e:#}"),
            };

            let seen = observed.lock().unwrap().clone();
            assert!(
                !seen.contains(&"stalled"),
                "a branch swap must never stall the destination — observed transitions: {seen:?}"
            );
            assert!(
                seen.contains(&"rebuilding"),
                "the swap should have been observable as `Rebuilding` — observed transitions: {seen:?}"
            );
            assert_eq!(runtime.lock().unwrap().state(), DestinationState::Live);
            assert_eq!(
                runtime.lock().unwrap().params(),
                new_params,
                "a completed swap should record the params it actually applied"
            );

            // The new branch is real and producing, and the old one is gone
            // (element count back to one dynamic branch's worth).
            assert!(
                wait_for_sample(&new_branch.appsink, Duration::from_secs(5)),
                "the post-swap branch should be producing encoded output"
            );

            tokio::time::timeout(Duration::from_secs(10), remove_branch(&pipeline, &tee, new_branch))
                .await
                .expect("cleanup remove_branch should not hang")
                .expect("cleanup remove_branch should succeed");
            pipeline.set_state(gst::State::Null).expect("pipeline should stop cleanly");
        }

        /// A failed swap must give the old branch back, still live, and leave
        /// the destination `Live` — never `Stalled`, never branchless.
        #[tokio::test]
        async fn a_failed_swap_keeps_the_old_branch_and_never_stalls() {
            if gst::init().is_err() {
                eprintln!("skipping: gstreamer not available in this environment");
                return;
            }

            let (pipeline, tee) = build_test_pipeline();
            pipeline.set_state(gst::State::Playing).expect("pipeline should reach PLAYING");
            let (state_result, ..) = pipeline.state(gst::ClockTime::from_seconds(5));
            state_result.expect("pipeline should settle into PLAYING");

            let (branch, runtime) = start_live_destination(&pipeline, &tee, BranchId(1), small_params()).await;

            // 1x1 is a size `x264enc` accepts in its caps template but cannot
            // actually initialise an encoder for (verified: it fails in
            // `gst_x264_enc_init_encoder`, so the branch never prerolls), so
            // the replacement branch fails to come up — a stand-in for any
            // real "these settings don't work on this machine" failure.
            //
            // Not `width: 0`: that is this codebase's "whatever the source is"
            // sentinel, which `branch::add_branch` deliberately turns into a
            // *size-less* capsfilter (see its `Why:` note) and therefore
            // negotiates perfectly well.
            let impossible = VideoParams { width: 1, height: 1, ..small_params() };
            let outcome = swap_to_new_branch(&pipeline, &tee, &runtime, branch, BranchId(2), impossible).await;

            let old = match outcome {
                SwapOutcome::KeptOld(old, _) => old,
                SwapOutcome::Swapped(_) => panic!("a branch with unnegotiable caps should not have swapped in"),
            };
            assert_eq!(
                runtime.lock().unwrap().state(),
                DestinationState::Live,
                "a failed swap must leave the destination exactly as it was, never stalled"
            );
            assert_eq!(old.id, BranchId(1), "the branch handed back must be the *original*, not the failed replacement");
            // `Why this checks the branch is still live in the pipeline rather
            // than still producing samples:` the 1x1 negotiation failure this
            // test injects doesn't only fail the replacement branch — the
            // `NOT_NEGOTIATED` it returns travels back up through the shared
            // `tee` and kills `videotestsrc` outright ("streaming stopped,
            // reason not-negotiated", visible under `GST_DEBUG=2`). That is
            // the same upstream-propagation hazard [`remove_branch`] documents
            // at length, here provoked deliberately by the failure injection,
            // and it means *no* branch on this pipeline can produce anything
            // afterwards. The older form of this assertion pulled one sample
            // and passed, but only because the pre-`skip-to-first` catch-up
            // burst (see [`super::branch::RateCap`]) happened to leave a
            // straggler queued in the appsink — a coincidence, not the
            // property being tested. What `KeptOld` actually promises, and
            // what is checkable here, is that the caller gets its original
            // branch back intact and still wired into a `Playing` pipeline
            // rather than a torn-down husk.
            assert!(
                old.appsink.has_as_ancestor(&pipeline),
                "the original branch must still be wired into the pipeline after a failed swap, not torn down"
            );
            let (_, branch_state, _) = old.appsink.state(gst::ClockTime::ZERO);
            assert_ne!(
                branch_state,
                gst::State::Null,
                "the original branch must not have been nulled out from under the caller"
            );

            tokio::time::timeout(Duration::from_secs(10), remove_branch(&pipeline, &tee, old))
                .await
                .expect("cleanup remove_branch should not hang")
                .expect("cleanup remove_branch should succeed");
            pipeline.set_state(gst::State::Null).expect("pipeline should stop cleanly");
        }

        /// The tier-3 path, end to end: a capture-source restart stalls
        /// **every** destination fed by that capture simultaneously, each keeps
        /// its stream alive with its cached IDR while down, and all of them
        /// come back to `Live` on the new pipeline — with wire PTS strictly
        /// increasing straight through the restart, which is what makes the
        /// outage look like low framerate rather than a glitch.
        #[tokio::test]
        async fn a_source_restart_stalls_every_co_located_destination_and_recovers_them_all() {
            if gst::init().is_err() {
                eprintln!("skipping: gstreamer not available in this environment");
                return;
            }

            // Three destinations off one capture, at deliberately different
            // settings — the per-destination independence the branch chain's
            // own videoscale/videorate buys.
            let settings = [
                VideoParams { width: 64, height: 64, framerate: 15, ..small_params() },
                VideoParams { width: 32, height: 32, framerate: 10, ..small_params() },
                VideoParams { width: 48, height: 48, framerate: 5, ..small_params() },
            ];

            let (old_pipeline, old_tee) = build_test_pipeline();
            old_pipeline.set_state(gst::State::Playing).expect("pipeline should reach PLAYING");
            let (state_result, ..) = old_pipeline.state(gst::ClockTime::from_seconds(5));
            state_result.expect("pipeline should settle into PLAYING");

            let mut branches = Vec::new();
            let mut runtimes: Vec<SharedDestination> = Vec::new();
            for (i, params) in settings.iter().enumerate() {
                let (branch, runtime) =
                    start_live_destination(&old_pipeline, &old_tee, BranchId(i as u32), *params).await;
                branches.push(branch);
                runtimes.push(runtime);
            }

            // Send a run of real frames through each destination's wire clock,
            // recording every PTS that would have gone out.
            let mut sent: Vec<Vec<u64>> = vec![Vec::new(); runtimes.len()];
            for (i, branch) in branches.iter_mut().enumerate() {
                for _ in 0..5 {
                    let Some(sample) = branch.appsink.try_pull_sample(gst::ClockTime::from_mseconds(500)) else {
                        break;
                    };
                    let pts = sample.buffer().and_then(|b| b.pts()).map(|t| t.useconds()).unwrap_or(0);
                    sent[i].push(runtimes[i].lock().unwrap().wire_pts_from_buffer(pts));
                }
            }

            // --- the source goes down ---
            stall_all(&runtimes, StallReason::SourceChanged);
            for (i, runtime) in runtimes.iter().enumerate() {
                assert!(
                    runtime.lock().unwrap().state().is_stalled(),
                    "destination {i} sharing the restarted capture must stall too"
                );
            }

            // While stalled, each destination still has something decodable to
            // re-send — this is what keeps the viewer from seeing a disconnect.
            let stall_start = Instant::now();
            for (i, runtime) in runtimes.iter().enumerate() {
                let mut runtime = runtime.lock().unwrap();
                for tick in 0..4u32 {
                    let now = stall_start + super::super::destination::STALL_RESEND_INTERVAL * tick;
                    let idr = runtime
                        .stall_resend_due(now)
                        .unwrap_or_else(|| panic!("destination {i} had no cached IDR to re-send at tick {tick}"));
                    assert!(!idr.is_empty(), "the cached IDR must be real encoded bytes");
                    sent[i].push(runtime.wire_pts_from_epoch(now));
                }
            }

            for branch in branches.drain(..) {
                tokio::time::timeout(Duration::from_secs(10), remove_branch(&old_pipeline, &old_tee, branch))
                    .await
                    .expect("tearing down the old capture's branches should not hang")
                    .expect("tearing down the old capture's branches should succeed");
            }
            old_pipeline.set_state(gst::State::Null).expect("old pipeline should stop cleanly");

            // --- the source comes back as a brand-new pipeline, whose running
            // time restarts at zero ---
            let (new_pipeline, new_tee) = build_test_pipeline();
            new_pipeline.set_state(gst::State::Playing).expect("new pipeline should reach PLAYING");
            let (state_result, ..) = new_pipeline.state(gst::ClockTime::from_seconds(5));
            state_result.expect("new pipeline should settle into PLAYING");

            for (i, params) in settings.iter().enumerate() {
                let branch = rebuild_after_source_restart(
                    &new_pipeline,
                    &new_tee,
                    &runtimes[i],
                    BranchId(100 + i as u32),
                    *params,
                )
                .await
                .unwrap_or_else(|e| panic!("destination {i} failed to rebuild after the restart: {e:#}"));
                branches.push(branch);
            }

            for (i, runtime) in runtimes.iter().enumerate() {
                assert_eq!(
                    runtime.lock().unwrap().state(),
                    DestinationState::Live,
                    "destination {i} should be back to Live once its rebuilt branch produced an IDR"
                );
            }

            for (i, branch) in branches.iter_mut().enumerate() {
                for _ in 0..5 {
                    let Some(sample) = branch.appsink.try_pull_sample(gst::ClockTime::from_mseconds(500)) else {
                        break;
                    };
                    let pts = sample.buffer().and_then(|b| b.pts()).map(|t| t.useconds()).unwrap_or(0);
                    sent[i].push(runtimes[i].lock().unwrap().wire_pts_from_buffer(pts));
                }
            }

            for (i, ptss) in sent.iter().enumerate() {
                assert!(
                    ptss.len() > 8,
                    "destination {i} should have sent frames on both sides of the restart, got {}",
                    ptss.len()
                );
                for pair in ptss.windows(2) {
                    assert!(
                        pair[1] > pair[0],
                        "destination {i}'s wire PTS went backwards across the capture restart: {} -> {} \
                         (full sequence: {ptss:?})",
                        pair[0],
                        pair[1]
                    );
                }
            }

            for branch in branches.drain(..) {
                tokio::time::timeout(Duration::from_secs(10), remove_branch(&new_pipeline, &new_tee, branch))
                    .await
                    .expect("cleanup should not hang")
                    .expect("cleanup should succeed");
            }
            new_pipeline.set_state(gst::State::Null).expect("new pipeline should stop cleanly");
        }

        /// Regression guard for the `appsink.stream()` handoff hazard (see
        /// [`EncodeBranch`]'s doc comment): a branch that a *production reader*
        /// has already been consuming via [`EncodeBranch::next_sample`] must
        /// still remove promptly.
        ///
        /// `Why the timing assertion is the real check:` `remove_branch`
        /// installing its own second `AppSinkStream` would not fail loudly — it
        /// would clobber the reader's callbacks, and whichever of the two
        /// streams got dropped first would reset the appsink's callbacks to an
        /// empty set, leaving the survivor's waker unregistered. The drain
        /// would then only complete by luck (`poll_next` opportunistically
        /// `try_pull_sample`s before parking), and in the unlucky case it burns
        /// the full internal 5s drain timeout and logs "did not drain to EOS"
        /// while still returning `Ok`. Asserting that removal finishes well
        /// under that timeout is what distinguishes "drained properly" from
        /// "timed out and forced it anyway".
        #[tokio::test]
        async fn a_branch_already_being_read_removes_promptly_without_a_second_appsink_stream() {
            if gst::init().is_err() {
                eprintln!("skipping: gstreamer not available in this environment");
                return;
            }

            let (pipeline, tee) = build_test_pipeline();
            pipeline.set_state(gst::State::Playing).expect("pipeline should reach PLAYING");
            let (state_result, ..) = pipeline.state(gst::ClockTime::from_seconds(5));
            state_result.expect("pipeline should settle into PLAYING");

            let mut reader_branch =
                add_branch(&pipeline, &tee, BranchId(1), small_settings()).expect("add_branch should succeed");
            let sibling =
                add_branch(&pipeline, &tee, BranchId(2), small_settings()).expect("add_branch should succeed");

            // Consume through the branch's own reader — this is what installs
            // its single `AppSinkStream`.
            for _ in 0..3 {
                let sample = tokio::time::timeout(Duration::from_secs(5), reader_branch.next_sample())
                    .await
                    .expect("reading the branch should not hang")
                    .expect("the branch should produce samples");
                assert!(sample.buffer().is_some());
            }

            let started = Instant::now();
            tokio::time::timeout(Duration::from_secs(10), remove_branch(&pipeline, &tee, reader_branch))
                .await
                .expect("remove_branch should not hang")
                .expect("remove_branch should succeed");
            let elapsed = started.elapsed();
            assert!(
                elapsed < Duration::from_secs(3),
                "removing an already-read branch took {elapsed:?} — that is the internal drain timeout expiring, \
                 i.e. the appsink's callbacks were clobbered by a second AppSinkStream"
            );

            assert!(
                wait_for_sample(&sibling.appsink, Duration::from_secs(5)),
                "the sibling branch must be unaffected by removing a branch that was being read"
            );

            tokio::time::timeout(Duration::from_secs(10), remove_branch(&pipeline, &tee, sibling))
                .await
                .expect("cleanup should not hang")
                .expect("cleanup should succeed");
            pipeline.set_state(gst::State::Null).expect("pipeline should stop cleanly");
        }

        /// Per-destination independence: two branches off the *same* tee,
        /// asked for different sizes and framerates, must each honour their own
        /// — that is the whole point of the per-branch
        /// videoscale/videorate/capsfilter chain.
        #[tokio::test]
        async fn two_branches_on_one_capture_honour_their_own_resolutions() {
            if gst::init().is_err() {
                eprintln!("skipping: gstreamer not available in this environment");
                return;
            }

            let (pipeline, tee) = build_test_pipeline();
            pipeline.set_state(gst::State::Playing).expect("pipeline should reach PLAYING");
            let (state_result, ..) = pipeline.state(gst::ClockTime::from_seconds(5));
            state_result.expect("pipeline should settle into PLAYING");

            // The source is 64x64 (see `build_test_pipeline`); both requests
            // are downscales, per the never-upscale policy.
            let big = EncodeSettings { width: 64, height: 64, framerate: 15, ..small_settings() };
            let small = EncodeSettings { width: 32, height: 32, framerate: 5, ..small_settings() };

            let branch_big = add_branch(&pipeline, &tee, BranchId(1), big).expect("add_branch(big) should succeed");
            let branch_small =
                add_branch(&pipeline, &tee, BranchId(2), small).expect("add_branch(small) should succeed");

            assert!(wait_for_sample(&branch_big.appsink, Duration::from_secs(5)), "big branch produced nothing");
            assert!(wait_for_sample(&branch_small.appsink, Duration::from_secs(5)), "small branch produced nothing");

            let negotiated = |appsink: &gst_app::AppSink| -> (i32, i32) {
                let caps = appsink
                    .static_pad("sink")
                    .and_then(|p| p.current_caps())
                    .expect("branch appsink should have negotiated caps");
                let s = caps.structure(0).expect("caps should have a structure");
                (s.get("width").expect("width"), s.get("height").expect("height"))
            };

            assert_eq!(negotiated(&branch_big.appsink), (64, 64));
            assert_eq!(
                negotiated(&branch_small.appsink),
                (32, 32),
                "each branch must encode at its own resolution off the one shared capture"
            );

            for branch in [branch_big, branch_small] {
                tokio::time::timeout(Duration::from_secs(10), remove_branch(&pipeline, &tee, branch))
                    .await
                    .expect("cleanup should not hang")
                    .expect("cleanup should succeed");
            }
            pipeline.set_state(gst::State::Null).expect("pipeline should stop cleanly");
        }

        /// The bandwidth allocator's actual write path: a tier-1 bitrate
        /// change must land on the *live* encoder element of the branch it
        /// targets, and on no other branch's.
        ///
        /// `Why this is worth a real-GStreamer test rather than trusting the
        /// property write:` "the UI shows a number that never reaches the
        /// pipeline" is a bug shape this module has already been bitten by
        /// twice (see `build_pipeline_framerate_parameter_actually_caps_
        /// encoder_output_rate`'s doc comment), and Absolute-Maximum mode
        /// rests entirely on these writes landing. Reading the property back
        /// off the element proves the write reached the encoder that is
        /// actually encoding, not a detached handle.
        #[tokio::test]
        async fn setting_one_branchs_bitrate_writes_through_to_that_encoder_only() {
            if gst::init().is_err() {
                eprintln!("skipping: gstreamer not available in this environment");
                return;
            }

            let (pipeline, tee) = build_test_pipeline();
            pipeline.set_state(gst::State::Playing).expect("pipeline should reach PLAYING");
            let (state_result, ..) = pipeline.state(gst::ClockTime::from_seconds(5));
            state_result.expect("pipeline should settle into PLAYING");

            let first = add_branch(&pipeline, &tee, BranchId(1), EncodeSettings { bitrate_kbps: 500, ..small_settings() })
                .expect("add_branch should succeed");
            let second =
                add_branch(&pipeline, &tee, BranchId(2), EncodeSettings { bitrate_kbps: 900, ..small_settings() })
                    .expect("add_branch should succeed");

            // Each branch starts at the rate it was *built* with — proof the
            // per-destination bitrate isn't shared state.
            assert_eq!(first.bitrate_kbps(), 500);
            assert_eq!(second.bitrate_kbps(), 900);
            assert!(wait_for_sample(&first.appsink, Duration::from_secs(5)), "first branch produced nothing");

            // A live write while both branches are encoding.
            first.set_bitrate_kbps(2_500);
            assert_eq!(first.bitrate_kbps(), 2_500, "the bitrate write did not reach the live encoder");
            assert_eq!(second.bitrate_kbps(), 900, "one destination's bitrate write must not touch another's encoder");

            // …and the branch keeps producing across it (a property write is
            // not supposed to disturb the stream at all — that is exactly why
            // it is its own tier).
            assert!(
                wait_for_sample(&first.appsink, Duration::from_secs(5)),
                "the branch stopped producing after a live bitrate write"
            );

            for branch in [first, second] {
                tokio::time::timeout(Duration::from_secs(10), remove_branch(&pipeline, &tee, branch))
                    .await
                    .expect("cleanup should not hang")
                    .expect("cleanup should succeed");
            }
            pipeline.set_state(gst::State::Null).expect("pipeline should stop cleanly");
        }

        /// Per-destination pause: closing one branch's own valve stops that
        /// branch's encoder without touching a sibling branch on the same tee.
        /// Subscriber counts are per stream, so the pause has to be per branch
        /// — a shared valve would take every fanned-out destination down with
        /// the one nobody happens to be watching.
        #[tokio::test]
        async fn pausing_one_branch_does_not_pause_its_sibling() {
            if gst::init().is_err() {
                eprintln!("skipping: gstreamer not available in this environment");
                return;
            }

            let (pipeline, tee) = build_test_pipeline();
            pipeline.set_state(gst::State::Playing).expect("pipeline should reach PLAYING");
            let (state_result, ..) = pipeline.state(gst::ClockTime::from_seconds(5));
            state_result.expect("pipeline should settle into PLAYING");

            let paused_branch = add_branch(&pipeline, &tee, BranchId(1), small_settings()).expect("add_branch");
            let live_branch = add_branch(&pipeline, &tee, BranchId(2), small_settings()).expect("add_branch");

            assert!(wait_for_sample(&paused_branch.appsink, Duration::from_secs(5)), "branch 1 produced nothing");
            assert!(wait_for_sample(&live_branch.appsink, Duration::from_secs(5)), "branch 2 produced nothing");

            paused_branch.set_paused(true, 0);
            // Drain whatever was already encoded and in flight before the
            // valve closed, then confirm nothing new arrives.
            while paused_branch.appsink.try_pull_sample(gst::ClockTime::from_mseconds(50)).is_some() {}
            assert!(
                !wait_for_sample(&paused_branch.appsink, Duration::from_millis(700)),
                "a paused branch must stop producing encoded output"
            );
            assert!(
                wait_for_sample(&live_branch.appsink, Duration::from_secs(5)),
                "pausing one destination must not pause another destination of the same capture"
            );

            paused_branch.set_paused(false, 0);
            assert!(
                wait_for_sample(&paused_branch.appsink, Duration::from_secs(5)),
                "a resumed branch must start producing again"
            );

            for branch in [paused_branch, live_branch] {
                tokio::time::timeout(Duration::from_secs(10), remove_branch(&pipeline, &tee, branch))
                    .await
                    .expect("cleanup should not hang")
                    .expect("cleanup should succeed");
            }
            pipeline.set_state(gst::State::Null).expect("pipeline should stop cleanly");
        }

        /// **Regression ("resuming a stream doesn't work"):** a branch that
        /// was paused for a long time must resume *at the live edge* — not
        /// replay the whole pause as duplicated stale frames.
        ///
        /// The bug this pins: the branch's `videorate drop-only=false` had
        /// `max-duplication-time=0`, i.e. *no limit* on how large a gap it
        /// would fill by duplicating the last frame it saw. The branch's
        /// `valve` sits directly upstream of that `videorate`, so a pause is
        /// indistinguishable from "the source went quiet" — and the instant
        /// the valve reopened, `videorate` synthesised one duplicate of the
        /// frozen pre-pause frame for **every frame period of the entire
        /// pause** before letting the first live frame through. Measured at a
        /// 35s pause: ~1000 stale duplicates, all encoded, all encrypted, all
        /// sent. The viewer's `decode::PacingState` then paced that backlog
        /// out by its (perfectly valid, evenly spaced) PTS — a real-time
        /// replay of the whole pause showing one frozen image, which is
        /// exactly the reported "resuming streams doesn't work, the streamer
        /// has to restart" symptom.
        ///
        /// Asserted two ways, because either alone is weak: the resumed
        /// branch's PTS must *jump across* the pause (not creep one frame
        /// past where it stopped), and the first second after resume must
        /// carry roughly one second's worth of frames rather than a burst.
        #[tokio::test]
        async fn a_long_pause_resumes_at_the_live_edge_instead_of_replaying_the_gap() {
            if gst::init().is_err() {
                eprintln!("skipping: gstreamer not available in this environment");
                return;
            }

            let (pipeline, tee) = build_test_pipeline();
            pipeline.set_state(gst::State::Playing).expect("pipeline should reach PLAYING");
            assert!(wait_until_playing(&pipeline, Duration::from_secs(30)), "pipeline never reached PLAYING");

            let framerate = 15u32;
            let branch = add_branch(&pipeline, &tee, BranchId(1), EncodeSettings { framerate, ..small_settings() })
                .expect("add_branch");
            assert!(wait_for_sample(&branch.appsink, Duration::from_secs(20)), "branch produced nothing to begin with");

            // Last PTS the branch emitted before the pause. Sampled from a
            // short window of *live* output first, then topped up with
            // whatever was still in flight when the valve closed.
            //
            // `Why the live window rather than only draining after pausing:`
            // closing the valve stops raw frames at the branch's entry, so
            // what is left downstream afterwards is at most a buffer or two
            // and can legitimately be nothing at all (`videorate` holds its
            // current frame until the next one arrives to compare against,
            // and with [`super::branch::RateCap`] there is no start-up
            // burst padding the appsink either). Reading a real PTS off the
            // running branch first makes this independent of that race; the
            // drain below still runs, so the value is the *last* one emitted.
            let mut last_pts_before = None;
            let warmup = Instant::now() + Duration::from_millis(500);
            while Instant::now() < warmup {
                if let Some(sample) = branch.appsink.try_pull_sample(gst::ClockTime::from_mseconds(100)) {
                    last_pts_before = sample.buffer().and_then(|b| b.pts()).or(last_pts_before);
                }
            }
            branch.set_paused(true, 0);
            while let Some(sample) = branch.appsink.try_pull_sample(gst::ClockTime::from_mseconds(100)) {
                last_pts_before = sample.buffer().and_then(|b| b.pts()).or(last_pts_before);
            }
            let last_pts_before = last_pts_before.expect("a pre-pause sample should carry a PTS");

            // Long enough that a full replay is unmistakable (at 15fps this
            // is 90 duplicate frames with the bug) while keeping the test
            // quick. The real report was ~35s; the mechanism is linear in
            // the pause, so 6s proves it just as well.
            let pause = Duration::from_secs(6);
            tokio::time::sleep(pause).await;
            branch.set_paused(false, 0);

            let window = Duration::from_secs(1);
            let deadline = Instant::now() + window;
            let mut first_pts_after = None;
            let mut count = 0u32;
            while Instant::now() < deadline {
                if let Some(sample) = branch.appsink.try_pull_sample(gst::ClockTime::from_mseconds(50)) {
                    count += 1;
                    if first_pts_after.is_none() {
                        first_pts_after = sample.buffer().and_then(|b| b.pts());
                    }
                }
            }
            let first_pts_after = first_pts_after.expect("the branch must produce something after resuming");
            let pts_jump = first_pts_after.saturating_sub(last_pts_before);
            eprintln!(
                "[resume] pts {last_pts_before} -> {first_pts_after} (jump {pts_jump}), \
                 {count} frames in the first second after a {pause:?} pause"
            );

            // The first frame after the pause must be a *live* one. With the
            // bug it is a duplicate of the pre-pause frame, one frame period
            // later; the floor here is half the pause, far above that and far
            // below the pause itself (so a little drain latency can't flake it).
            assert!(
                pts_jump > gst::ClockTime::from_mseconds(pause.as_millis() as u64 / 2),
                "resuming replayed the pause instead of jumping to the live edge \
                 (PTS advanced only {pts_jump} across a {pause:?} pause)"
            );
            // …and no catch-up burst behind it. Generous headroom (4x) so a
            // loaded parallel test run can't flake this, while a full replay
            // (which delivers the whole 6s backlog as fast as the encoder
            // manages) still lands far above it.
            assert!(
                count < framerate * 4,
                "resuming produced a catch-up burst: {count} frames in one second at {framerate}fps"
            );

            tokio::time::timeout(Duration::from_secs(10), remove_branch(&pipeline, &tee, branch))
                .await
                .expect("cleanup should not hang")
                .expect("cleanup should succeed");
            pipeline.set_state(gst::State::Null).expect("pipeline should stop cleanly");
        }

        /// A live **PipeWire** video source standing in for the portal's, at a
        /// realistic screen size and pixel format — the closest thing to the
        /// real capture path that can run unattended.
        ///
        /// `Why this exists at all:` every other test in this file feeds the
        /// pipeline from `videotestsrc`, which is a *self-paced, in-process,
        /// system-memory, tiny-frame* source. The real capture path is none of
        /// those things: `pipewiresrc` receives buffers from another process
        /// across a PipeWire link, at full screen resolution (megabytes per
        /// frame) in a packed 4-byte format, at whatever irregular rate the
        /// compositor delivers damage. A whole class of bug — anything to do
        /// with buffer size, negotiation against a live remote producer, or
        /// timing — can only show up on this path, and one did: the pre-tee
        /// rate conversion that stalled a real source dead while every
        /// `videotestsrc` test stayed green (see [`super::build_pipeline`]'s
        /// "Why there is no rate conversion before the `tee`").
        ///
        /// Returns `None` (test skips, doesn't fail) when PipeWire isn't usable
        /// here — no daemon, no `pw-dump`, or `pipewiresink` not installed —
        /// mirroring how the VAAPI tests skip on machines without a GPU.
        struct PipeWireProducer {
            pipeline: gst::Pipeline,
            node_id: u32,
            /// Present only for [`PipeWireProducer::start_static`]: the gate
            /// that goes silent after the first few frames.
            gate: Option<gst::Element>,
        }

        impl Drop for PipeWireProducer {
            fn drop(&mut self) {
                let _ = self.pipeline.set_state(gst::State::Null);
            }
        }

        impl PipeWireProducer {
            fn start(width: u32, height: u32) -> Option<Self> {
                Self::start_inner(width, height, false)
            }

            /// A **damage-driven** producer on an idle desktop: it delivers a
            /// handful of frames while the stream is being set up and then
            /// goes completely silent, exactly like a compositor with nothing
            /// to repaint. Call [`Self::go_silent`] once the consumer is
            /// connected to stop it (or leave it: the gate closes by itself
            /// after the first buffer).
            fn start_static(width: u32, height: u32) -> Option<Self> {
                Self::start_inner(width, height, true)
            }

            fn go_silent(&self) {
                if let Some(gate) = &self.gate {
                    gate.set_property("drop", true);
                }
            }

            fn start_inner(width: u32, height: u32, with_gate: bool) -> Option<Self> {
                static PRODUCER_SEQ: AtomicU32 = AtomicU32::new(0);
                let node_name = format!(
                    "sss-regression-source-{}-{}",
                    std::process::id(),
                    PRODUCER_SEQ.fetch_add(1, Ordering::Relaxed)
                );
                // `stream-properties` takes a `GstStructure` in
                // launch syntax — unquoted, or the quotes end up inside the
                // property value and the node keeps its default name.
                let gate = if with_gate { "valve name=gate drop=false ! " } else { "" };
                let description = format!(
                    "videotestsrc is-live=true pattern=ball \
                     ! video/x-raw,format=BGRx,width={width},height={height},framerate=60/1 \
                     ! {gate}pipewiresink mode=provide stream-properties=props,node.name={node_name}"
                );
                let Ok(element) = gst::parse::launch(&description) else {
                    return None;
                };
                let pipeline = element.downcast::<gst::Pipeline>().ok()?;
                if pipeline.set_state(gst::State::Playing).is_err() {
                    let _ = pipeline.set_state(gst::State::Null);
                    return None;
                }

                // The node only appears in the graph once the sink is actually
                // playing; poll rather than sleeping a fixed guess.
                let deadline = Instant::now() + Duration::from_secs(5);
                let node_id = loop {
                    if let Some(node_id) = Self::find_node(&node_name) {
                        break Some(node_id);
                    }
                    if Instant::now() >= deadline {
                        break None;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                };
                match node_id {
                    Some(node_id) => {
                        let gate = pipeline.by_name("gate");
                        Some(Self { pipeline, node_id, gate })
                    }
                    None => {
                        let _ = pipeline.set_state(gst::State::Null);
                        None
                    }
                }
            }

            /// `pw-dump`'s JSON graph, filtered to the node this producer
            /// created — the same shell-out-and-parse approach `audio.rs` uses
            /// for source discovery.
            fn find_node(node_name: &str) -> Option<u32> {
                let output = std::process::Command::new("pw-dump").output().ok()?;
                if !output.status.success() {
                    return None;
                }
                let objects: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
                // Every `?` here is confined to one object's own lookup chain:
                // applying `?` to a per-object field inside the loop body
                // would abandon the whole scan at the first object that
                // happens to lack it (most `pw-dump` objects have no
                // `info.props` at all).
                objects.as_array()?.iter().find_map(|object| {
                    let props = object.get("info")?.get("props")?;
                    let name = props.get("node.name")?.as_str()?;
                    if name != node_name {
                        return None;
                    }
                    Some(object.get("id")?.as_u64()? as u32)
                })
            }
        }

        /// **Regression (real-source path):** a destination branch must come up
        /// against a live `pipewiresrc` delivering *full-screen-sized* frames.
        ///
        /// The failure shape this pins is the one real sharing actually hit:
        /// the pipeline reaches `Playing`, the bus stays silent, the preview
        /// branches run — and the encode branch never sees a single frame, so
        /// `add_branch_live` times out waiting for its first IDR and sharing
        /// fails with a bare "encode branch produced no IDR within 5s". Every
        /// `videotestsrc`-fed test in this file stayed green throughout,
        /// because none of them involves a live cross-process producer or a
        /// full-screen-sized buffer.
        ///
        /// The resolution here is deliberately large (a 3840x2160 BGRx frame
        /// is ~33MB, well past `GstQueue`'s 10MB default byte limit, which
        /// each branch's entry `queue` leaves at its default). Keep it that
        /// way: this test is only worth its runtime while it exercises the
        /// buffer sizes a real screen produces.
        #[tokio::test]
        #[ignore = "touches the user's real PipeWire session; software 4K x264 with a 5 s budget flakes under host load — run with --ignored on a quiet machine"]
        async fn a_destination_branch_comes_up_against_a_real_pipewire_source() {
            if gst::init().is_err() {
                eprintln!("skipping: gstreamer not available in this environment");
                return;
            }
            let Some(producer) = PipeWireProducer::start(3840, 2160) else {
                eprintln!("skipping: no usable PipeWire daemon/pipewiresink/pw-dump in this environment");
                return;
            };

            let built = super::super::build_pipeline(&format!(
                "pipewiresrc path={} do-timestamp=true keepalive-time=33",
                producer.node_id
            ))
            .expect("pipeline should construct");
            built.pipeline.set_state(gst::State::Playing).expect("pipeline should reach PLAYING");
            assert!(
                wait_until_playing(&built.pipeline, Duration::from_secs(30)),
                "pipeline never reached PLAYING"
            );

            // Exactly what `spawn_destination` does for the first destination:
            // encode at whatever the source turned out to be.
            let (width, height) = super::super::negotiated_size(&built.tee).unwrap_or((0, 0));
            assert!(width > 0 && height > 0, "the tee should have negotiated a real frame size");
            let settings = EncodeSettings {
                width: width as u32,
                height: height as u32,
                framerate: 30,
                bitrate_kbps: 4_000,
                backend: EncoderBackend::Software,
                tune: TuneMode::default(),
                quality_level: QualityLevel::default(),
                rate_control: RateControlMode::default(),
                gop_millis: DEFAULT_GOP_MILLIS,
                codec: VideoCodec::default(),
            };

            let result = add_branch_live(&built.pipeline, &built.tee, BranchId(1), settings).await;
            let (branch, _first_idr) = result.expect(
                "a destination branch must come up against a real, full-screen-sized PipeWire source \
                 — this is the exact failure real screen sharing hit",
            );
            // …and keep producing, not just emit the one proving IDR.
            assert!(
                wait_for_sample(&branch.appsink, Duration::from_secs(5)),
                "the branch stopped producing after its first IDR"
            );

            tokio::time::timeout(Duration::from_secs(10), remove_branch(&built.pipeline, &built.tee, branch))
                .await
                .expect("cleanup should not hang")
                .expect("cleanup should succeed");
            built.pipeline.set_state(gst::State::Null).expect("pipeline should stop cleanly");
        }

        /// **Regression (static desktop):** a destination branch must come up,
        /// and keep producing, against a *damage-driven* PipeWire producer that
        /// has gone completely silent — the shape of an idle monitor.
        ///
        /// `Why:` sharing an idle monitor through the real portal ended with
        /// "encode branch produced no IDR within 5s" while a repainting monitor
        /// worked. [`a_destination_branch_comes_up_against_a_real_pipewire_source`]
        /// cannot see that, because its producer repaints at 60 fps forever.
        #[tokio::test]
        async fn a_destination_branch_comes_up_against_a_silent_pipewire_source() {
            if gst::init().is_err() {
                eprintln!("skipping: gstreamer not available in this environment");
                return;
            }
            let Some(producer) = PipeWireProducer::start_static(1920, 1080) else {
                eprintln!("skipping: no usable PipeWire daemon/pipewiresink/pw-dump in this environment");
                return;
            };

            let built = super::super::build_pipeline(&format!(
                "pipewiresrc path={} do-timestamp=true keepalive-time=33",
                producer.node_id
            ))
            .expect("pipeline should construct");
            built.pipeline.set_state(gst::State::Playing).expect("pipeline should reach PLAYING");
            assert!(
                wait_until_playing(&built.pipeline, Duration::from_secs(30)),
                "pipeline never reached PLAYING"
            );
            // The compositor delivered what it had; from now on nothing changes.
            tokio::time::sleep(Duration::from_millis(300)).await;
            producer.go_silent();
            tokio::time::sleep(Duration::from_millis(700)).await;
            eprintln!(
                "[static] raw frames off the tee while the producer is silent: {:.1} fps",
                tap_tee_rate(&built.pipeline, &built.tee, Duration::from_millis(1500))
            );

            let (width, height) = super::super::negotiated_size(&built.tee).unwrap_or((0, 0));
            assert!(width > 0 && height > 0, "the tee should have negotiated a real frame size");
            let settings = EncodeSettings {
                width: width as u32,
                height: height as u32,
                framerate: 30,
                bitrate_kbps: 4_000,
                backend: EncoderBackend::Software,
                tune: TuneMode::default(),
                quality_level: QualityLevel::default(),
                rate_control: RateControlMode::default(),
                gop_millis: DEFAULT_GOP_MILLIS,
                codec: VideoCodec::default(),
            };
            let started = Instant::now();
            let result = add_branch_live(&built.pipeline, &built.tee, BranchId(1), settings).await;
            eprintln!("[static] first IDR after {:?}", started.elapsed());
            let (branch, _first_idr) =
                result.expect("a destination branch must come up against a silent (static-desktop) source");
            let measured = Instant::now();
            let mut count = 0u32;
            while measured.elapsed() < Duration::from_secs(3) {
                if branch.appsink.try_pull_sample(gst::ClockTime::from_mseconds(50)).is_some() {
                    count += 1;
                }
            }
            eprintln!("[static] {count} encoded access units in 3s after the first IDR");
            assert!(count >= 20, "a static source must keep producing at roughly the branch rate, got {count} in 3s");

            tokio::time::timeout(Duration::from_secs(10), remove_branch(&built.pipeline, &built.tee, branch))
                .await
                .expect("cleanup should not hang")
                .expect("cleanup should succeed");
            built.pipeline.set_state(gst::State::Null).expect("pipeline should stop cleanly");
        }

        /// A running capture whose source can be silenced: a `videotestsrc`
        /// behind a `valve`, standing in for a damage-driven portal stream on
        /// an idle monitor. Close the gate with [`set_gate_closed`].
        ///
        /// `Why the gate starts open and is closed after PLAYING:` the fixed
        /// preview branches' appsinks need a first buffer to preroll, so a
        /// test pipeline whose source never delivers cannot reach `Playing`.
        /// The real portal pipeline does (its source is a live element that
        /// reports no-preroll); what these tests need is "the tee currently
        /// gets nothing", which is the same thing from the waiter's side.
        fn gated_capture() -> super::super::CapturePipeline {
            let built = super::super::build_pipeline(
                "videotestsrc is-live=true pattern=smpte \
                 ! video/x-raw,width=64,height=64,framerate=30/1 \
                 ! valve name=gate drop=false",
            )
            .expect("pipeline should construct");
            built.pipeline.set_state(gst::State::Playing).expect("pipeline should reach PLAYING");
            assert!(wait_until_playing(&built.pipeline, Duration::from_secs(30)), "pipeline never reached PLAYING");
            built
        }

        fn set_gate_closed(built: &super::super::CapturePipeline, closed: bool) {
            built.pipeline.by_name("gate").expect("gate").set_property("drop", closed);
        }

        /// **Regression (idle monitor):** a destination added to a capture
        /// that has not produced a frame yet must *wait*, not fail — and come
        /// up as soon as the first frame arrives.
        ///
        /// The wait is deliberately longer than twice [`FIRST_IDR_TIMEOUT`]:
        /// before the fix, a silent source burned the hardware encoder's 5s
        /// deadline, then the software retry's, and the share died with
        /// "encode branch produced no IDR".
        #[tokio::test]
        async fn a_destination_waits_for_the_first_frame_of_an_idle_source_instead_of_failing() {
            if gst::init().is_err() {
                eprintln!("skipping: gstreamer not available in this environment");
                return;
            }
            let built = gated_capture();
            set_gate_closed(&built, true);
            tokio::time::sleep(Duration::from_millis(300)).await;

            let pipeline = built.pipeline.clone();
            let tee = built.tee.clone();
            let task = tokio::spawn(async move {
                add_branch_live_waiting(&pipeline, &tee, BranchId(1), small_settings(), SourceWait::Indefinite).await
            });

            tokio::time::sleep(FIRST_IDR_TIMEOUT + Duration::from_millis(500)).await;
            assert!(!task.is_finished(), "the destination must keep waiting for an idle source, not fail");

            // The screen finally changes.
            set_gate_closed(&built, false);
            let result = tokio::time::timeout(Duration::from_secs(20), task)
                .await
                .expect("the branch must come up promptly once a frame arrives")
                .expect("task must not panic");
            let (branch, _idr) = result.expect("branch should come up once the source produces");

            tokio::time::timeout(Duration::from_secs(10), remove_branch(&built.pipeline, &built.tee, branch))
                .await
                .expect("cleanup should not hang")
                .expect("cleanup should succeed");
            built.pipeline.set_state(gst::State::Null).expect("pipeline should stop cleanly");
        }

        /// The bounded flavour (used by branch swaps) still fails on a dead
        /// source, and a stopped capture releases a waiter instead of leaving
        /// it parked forever.
        #[tokio::test]
        async fn waiting_for_a_source_frame_is_bounded_when_asked_and_ends_when_the_capture_stops() {
            if gst::init().is_err() {
                eprintln!("skipping: gstreamer not available in this environment");
                return;
            }
            let built = gated_capture();
            set_gate_closed(&built, true);
            tokio::time::sleep(Duration::from_millis(300)).await;

            let err = wait_for_source_frame(&built.pipeline, &built.tee, SourceWait::Bounded(Duration::from_millis(600)))
                .await
                .expect_err("a bounded wait on a dead source must fail");
            assert!(format!("{err:#}").contains("no frame"), "{err:#}");

            let pipeline = built.pipeline.clone();
            let tee = built.tee.clone();
            let waiter =
                tokio::spawn(async move { wait_for_source_frame(&pipeline, &tee, SourceWait::Indefinite).await });
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert!(!waiter.is_finished());
            built.pipeline.set_state(gst::State::Null).expect("pipeline should stop cleanly");
            let err = tokio::time::timeout(Duration::from_secs(5), waiter)
                .await
                .expect("stopping the capture must release the waiter")
                .expect("task must not panic")
                .expect_err("a stopped capture is an error for the waiter");
            assert!(format!("{err:#}").contains("stopped"), "{err:#}");
        }

        /// `awaiting_first_frame`'s flag is false until a buffer reaches the
        /// tee, and true after.
        #[tokio::test]
        async fn the_capture_reports_when_it_has_produced_its_first_frame() {
            if gst::init().is_err() {
                eprintln!("skipping: gstreamer not available in this environment");
                return;
            }
            let built = super::super::build_pipeline(
                "videotestsrc is-live=true pattern=smpte ! video/x-raw,width=64,height=64,framerate=30/1",
            )
            .expect("pipeline should construct");
            assert!(!built.frame_seen.load(Ordering::Acquire), "nothing has run yet");
            built.pipeline.set_state(gst::State::Playing).expect("pipeline should reach PLAYING");
            assert!(wait_until_playing(&built.pipeline, Duration::from_secs(30)), "pipeline never reached PLAYING");
            assert!(built.frame_seen.load(Ordering::Acquire), "a running source must have flipped the flag");
            built.pipeline.set_state(gst::State::Null).expect("pipeline should stop cleanly");
        }

        /// Removing a branch from a capture that has gone silent must drain
        /// promptly. The EOS used to be injected only from a probe that fires
        /// on the next *buffer*, so on a silent source every removal burned its
        /// full 5s drain timeout ("did not drain to EOS within 5s") — which is
        /// what a failed share on an idle monitor logged right after the IDR
        /// error.
        #[tokio::test]
        async fn removing_a_branch_from_a_silent_capture_does_not_wait_out_the_drain_timeout() {
            if gst::init().is_err() {
                eprintln!("skipping: gstreamer not available in this environment");
                return;
            }
            let built = gated_capture();
            let branch = add_branch(&built.pipeline, &built.tee, BranchId(1), small_settings())
                .expect("branch should build");
            assert!(wait_for_sample(&branch.appsink, Duration::from_secs(20)), "branch produced nothing");

            // The screen goes idle for good (no keepalive in this test).
            set_gate_closed(&built, true);
            tokio::time::sleep(Duration::from_millis(500)).await;

            let started = Instant::now();
            remove_branch(&built.pipeline, &built.tee, branch).await.expect("removal should succeed");
            let took = started.elapsed();
            assert!(took < Duration::from_secs(3), "removal from a silent capture took {took:?}; it waited out the drain timeout");
            built.pipeline.set_state(gst::State::Null).expect("pipeline should stop cleanly");
        }

        /// **Sibling isolation:** two destinations off one capture each hold
        /// their *own* framerate, and everything that happens to one of them —
        /// a settings change (branch swap), a pause, being removed entirely —
        /// leaves the other's rate undisturbed.
        ///
        /// `Why measured rates rather than "it still produces something":` the
        /// risk fan-out introduces is not that a sibling dies outright (that
        /// would be obvious), it is that one destination's churn quietly drags
        /// another's rate around — a shared `videorate` renegotiating, a tee
        /// stalling while one branch reconfigures, a swap draining buffers a
        /// sibling needed. Only a before/after *rate* comparison catches that.
        #[tokio::test]
        async fn sibling_destinations_keep_their_own_rate_through_a_swap_and_a_removal() {
            if gst::init().is_err() {
                eprintln!("skipping: gstreamer not available in this environment");
                return;
            }

            // A 60fps source so both a 30fps and a 10fps destination are
            // genuine down-conversions the branch has to perform itself.
            let built = super::super::build_pipeline(
                "videotestsrc is-live=true pattern=smpte ! video/x-raw,width=64,height=64,framerate=60/1",
            )
            .expect("pipeline should construct");
            built.pipeline.set_state(gst::State::Playing).expect("pipeline should reach PLAYING");
            assert!(
                wait_until_playing(&built.pipeline, Duration::from_secs(30)),
                "pipeline never reached PLAYING"
            );

            let settings = |framerate: u32| EncodeSettings { framerate, ..small_settings() };
            let observer = add_branch(&built.pipeline, &built.tee, BranchId(1), settings(30))
                .expect("observer branch should build");
            let subject = add_branch(&built.pipeline, &built.tee, BranchId(2), settings(10))
                .expect("subject branch should build");
            // Generous first-sample waits: `cargo test` runs this file's
            // GStreamer tests in parallel, and a dozen live pipelines
            // competing for CPU can push a fresh branch's first encoded frame
            // well past the few hundred milliseconds it takes when run alone.
            assert!(wait_for_sample(&observer.appsink, Duration::from_secs(20)), "observer produced nothing");
            assert!(wait_for_sample(&subject.appsink, Duration::from_secs(20)), "subject produced nothing");

            let measure = |branch: &EncodeBranch| -> f64 {
                // The branch's appsink never drops (a backlog from the time
                // this branch was not being read would inflate the window),
                // so start from an empty queue.
                while branch.appsink.try_pull_sample(gst::ClockTime::ZERO).is_some() {}
                let window = Duration::from_millis(1500);
                let deadline = Instant::now() + window;
                let mut count = 0u32;
                while Instant::now() < deadline {
                    if branch.appsink.try_pull_sample(gst::ClockTime::from_mseconds(50)).is_some() {
                        count += 1;
                    }
                }
                count as f64 / window.as_secs_f64()
            };

            // Each destination really does run at its own rate off the one
            // shared capture — the premise everything below rests on.
            let observer_before = measure(&observer);
            let subject_before = measure(&subject);
            eprintln!("[siblings] baseline: observer(30fps)={observer_before:.1} subject(10fps)={subject_before:.1}");
            // Deliberately a *relative* claim: under parallel test load every
            // absolute rate sags together, so "the 30fps one runs
            // meaningfully faster than the 10fps one" is the load-robust way
            // to state "each destination holds its own rate". Every assertion
            // below is likewise relative to this same run's own baseline.
            assert!(
                observer_before > subject_before * 1.5,
                "the two destinations should run at their own different rates, measured                  observer(30fps)={observer_before:.1} subject(10fps)={subject_before:.1}"
            );
            assert!(observer_before > 5.0, "the pipeline appears not to be running at all");

            // 1. Pause the subject: the observer must not notice.
            subject.set_paused(true, 0);
            let observer_paused = measure(&observer);
            subject.set_paused(false, 0);
            assert!(
                (observer_paused - observer_before).abs() < observer_before * 0.4,
                "pausing a sibling destination changed this one's rate ({observer_before:.1} -> {observer_paused:.1})"
            );

            // 2. Swap the subject to a different resolution *and* rate — a
            //    real tier-2 branch swap on the live tee.
            let runtime = destination::DestinationRuntime::shared(Instant::now(), small_params());
            runtime.lock().unwrap().note_encoded_frame(true, b"idr", Instant::now());
            let swapped = swap_to_new_branch(
                &built.pipeline,
                &built.tee,
                &runtime,
                subject,
                BranchId(3),
                VideoParams { width: 32, height: 32, framerate: 5, ..small_params() },
            )
            .await;
            let new_subject = match swapped {
                SwapOutcome::Swapped(branch) => branch,
                SwapOutcome::KeptOld(_, e) => panic!("the swap should have succeeded: {e:#}"),
            };
            let observer_after_swap = measure(&observer);
            eprintln!("[siblings] after sibling swap: observer={observer_after_swap:.1}");
            assert!(
                (observer_after_swap - observer_before).abs() < observer_before * 0.4,
                "swapping a sibling's branch changed this one's rate ({observer_before:.1} -> {observer_after_swap:.1})"
            );

            // 3. Remove the subject entirely: the observer keeps going at its
            //    own rate (the classic "removing one branch EOS'd the whole
            //    tee" failure would show up here as a rate of 0).
            tokio::time::timeout(Duration::from_secs(10), remove_branch(&built.pipeline, &built.tee, new_subject))
                .await
                .expect("removal should not hang")
                .expect("removal should succeed");
            let observer_after_removal = measure(&observer);
            eprintln!("[siblings] after sibling removal: observer={observer_after_removal:.1}");
            assert!(
                (observer_after_removal - observer_before).abs() < observer_before * 0.4,
                "removing a sibling destination changed this one's rate ({observer_before:.1} -> {observer_after_removal:.1})"
            );

            tokio::time::timeout(Duration::from_secs(10), remove_branch(&built.pipeline, &built.tee, observer))
                .await
                .expect("cleanup should not hang")
                .expect("cleanup should succeed");
            built.pipeline.set_state(gst::State::Null).expect("pipeline should stop cleanly");
        }

    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Task #52: the portal source gets the decoupling queue (see
    /// [`decoupled_source`]) in both domains' descriptions, and any other
    /// source is left alone.
    #[test]
    fn only_the_portal_source_gets_the_decoupling_queue() {
        let portal = "pipewiresrc fd=7 path=42 do-timestamp=true";
        let queued = decoupled_source(portal);
        assert!(queued.starts_with(portal) && queued.contains(&format!("queue name={PORTAL_QUEUE_NAME}")), "{queued}");
        assert!(gpu_pipeline_description(portal).contains(&format!("{PORTAL_QUEUE_NAME} max-size-buffers=2 ")));
        assert_eq!(decoupled_source("videotestsrc is-live=true"), "videotestsrc is-live=true");
        // The queue must sit directly behind the source, in front of the first real element.
        let desc = gpu_pipeline_description(portal);
        let q = desc.find(PORTAL_QUEUE_NAME).unwrap();
        assert!(q < desc.find("vapostproc").unwrap());
    }

    /// Task #59: a bus error naming a vanished portal node is a [`StartFailed`]
    /// (no CPU fallback on the same node); any other start error stays plain.
    #[test]
    fn a_vanished_portal_node_is_a_hopeless_start() {
        let gone = start_failure(
            "failed to start".to_string(),
            Some("pipewiresrc0: stream error: target not found".to_string()),
        );
        assert!(start_is_hopeless(&gone), "{gone:#}");
        assert!(format!("{gone:#}").contains("target not found"));
        let other = start_failure("failed to start".to_string(), Some("vapostproc0: not-negotiated".to_string()));
        assert!(!start_is_hopeless(&other), "{other:#}");
        assert!(!start_is_hopeless(&start_failure("failed to start".to_string(), None)));
    }

    /// Task #76: an error the capture pipeline's bus reported is recorded by
    /// [`capture_loop`] and leads the message of a destination that then fails
    /// to come up ("produced no IDR") instead of being blamed on the encoder.
    #[tokio::test]
    async fn a_source_error_on_the_bus_is_blamed_when_the_branch_fails() {
        if gst::init().is_err() {
            eprintln!("skipping: gstreamer not available in this environment");
            return;
        }
        let pipeline = gst::Pipeline::new();
        let errors = CaptureErrors::default();
        let task = tokio::spawn(capture_loop(pipeline.bus().unwrap().stream(), errors.clone()));
        let encoder_symptom = || anyhow!("encode branch produced no IDR within 5s").context("software fallback encode branch produced no IDR either");

        // Nothing recorded: the error is returned untouched.
        let plain = blame_the_source(encoder_symptom(), &errors);
        assert!(format!("{plain:#}").starts_with("software fallback"), "{plain:#}");

        // What pipewiresrc 1.4.2 reports against xdg-desktop-portal-hyprland.
        let source = gst::ElementFactory::make("identity").name("pipewiresrc17").build().unwrap();
        pipeline.add(&source).unwrap();
        let _ = pipeline.post_message(
            gst::message::Error::builder(gst::StreamError::Failed, "stream error: unhandled format").src(&source).build(),
        );
        // The bus ends the loop on its first error, so waiting for the task is the sync point.
        tokio::time::timeout(Duration::from_secs(5), task).await.expect("capture_loop ends on a bus error").unwrap();

        let blamed = blame_the_source(encoder_symptom(), &errors);
        let text = format!("{blamed:#}");
        assert!(text.starts_with("screen capture failed: pipewiresrc17: stream error: unhandled format"), "{text}");
        assert!(text.contains("produced no IDR"), "the encoder's own message stays in the chain: {text}");
        // Only the first error is kept: later ones are consequences of it.
        errors.record("second".to_string());
        assert_eq!(errors.first().as_deref(), Some("pipewiresrc17: stream error: unhandled format"));
    }

    /// Task #59: a start that errors on the bus ends promptly with that error
    /// instead of waiting out a state change.
    #[tokio::test]
    async fn a_failing_source_ends_the_start_promptly_with_its_error() {
        if gst::init().is_err() {
            eprintln!("skipping: gstreamer not available in this environment");
            return;
        }
        let started = Instant::now();
        let result = start_pipeline_bounded("filesrc location=/nonexistent/sss-59", CaptureDomain::System, START_TIMEOUT).await;
        let err = result.err().expect("a missing file cannot start a capture");
        assert!(started.elapsed() < Duration::from_secs(3), "took {:?}", started.elapsed());
        assert!(format!("{err:#}").contains("sss-59"), "{err:#}");
    }

    /// Shared by [`pipeline_elements_link_and_produce_encoded_output`] and
    /// [`vaapi_pipeline_links_and_produces_encoded_output`] (and their H.265/
    /// AV1 analogues): builds a `videotestsrc ! videoconvert !
    /// video/x-raw,format=<backend's format> ! <backend's encoder> !
    /// <codec's output caps> ! appsink` pipeline (the same chain
    /// [`build_pipeline`]/[`branch::add_branch`] produce, with `videotestsrc`
    /// standing in for `pipewiresrc fd=… path=…` since a real portal fd needs
    /// an interactive permission grant this test environment can't provide),
    /// runs it, and returns whether at least one encoded access unit came
    /// out. `codec` picks both the encoder element (on
    /// [`EncoderBackend::Vaapi`] — see [`EncoderBackend::launch_fragment`])
    /// and the matching output caps (see [`branch::add_branch`]'s output-caps
    /// selection, mirrored here rather than shared code since this is a
    /// standalone `gst::parse::launch` pipeline, not a `branch::add_branch`
    /// call).
    fn run_backend_and_expect_encoded_buffer(backend: EncoderBackend, codec: VideoCodec) -> bool {
        let encoder = backend.launch_fragment(
            DEFAULT_BITRATE_KBPS,
            (DEFAULT_GOP_MILLIS * DEFAULT_FRAMERATE) / 1000,
            TuneMode::default(),
            QualityLevel::default(),
            RateControlMode::default(),
            codec,
        );
        let raw_format = backend.raw_format();
        let output_caps = match backend.actual_codec(codec) {
            VideoCodec::H264 => "video/x-h264,stream-format=byte-stream,alignment=au",
            VideoCodec::H265 => "video/x-h265,stream-format=byte-stream,alignment=au",
            VideoCodec::Av1 => "video/x-av1,stream-format=obu-stream,alignment=tu",
        };
        let description = format!(
            "videotestsrc num-buffers=10 \
             ! videoconvert \
             ! video/x-raw,format={raw_format} \
             ! {encoder} \
             ! {output_caps} \
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
            run_backend_and_expect_encoded_buffer(EncoderBackend::Software, VideoCodec::H264),
            "expected at least one encoded H.264 access unit from the software encoder test pipeline"
        );
    }

    /// Builds just the encoder element (no surrounding pipeline) from
    /// [`EncoderBackend::launch_fragment`]'s output, so a test can read back
    /// the real GStreamer properties the tuning knobs are supposed to set —
    /// the same "construct via `gst::parse::launch`, downcast, read
    /// `.property(...)`" pattern [`EncodeBranch::bitrate_kbps`] and its
    /// tests already use, just without a whole pipeline around it.
    fn build_encoder_element(
        backend: EncoderBackend,
        tune: TuneMode,
        quality_level: QualityLevel,
        rate_control: RateControlMode,
    ) -> gst::Element {
        build_encoder_element_for_codec(backend, tune, quality_level, rate_control, VideoCodec::H264)
    }

    /// [`build_encoder_element`], with an explicit [`VideoCodec`] — used by
    /// the H.265/AV1 property-mapping tests below, which need a non-default
    /// codec's element (`vah265enc`/`vaav1enc`) rather than always
    /// `vah264enc`.
    fn build_encoder_element_for_codec(
        backend: EncoderBackend,
        tune: TuneMode,
        quality_level: QualityLevel,
        rate_control: RateControlMode,
        codec: VideoCodec,
    ) -> gst::Element {
        let fragment = backend.launch_fragment(4000, 60, tune, quality_level, rate_control, codec);
        gst::parse::launch(&fragment).unwrap_or_else(|e| panic!("encoder fragment '{fragment}' should parse: {e}"))
    }

    /// [`TuneMode`] only has a real effect on the software backend — see that
    /// type's doc comment. `LowLatency` sets x264enc's `tune` bitmask to
    /// include `zerolatency` (`0x4`); `BestQuality` leaves it unset (`0x0`,
    /// x264enc's default) since there is no explicit "best quality" tune
    /// value to set, only the absence of the low-latency one.
    #[test]
    fn tune_mode_maps_to_x264enc_tune_property() {
        if gst::init().is_err() {
            eprintln!("skipping: gstreamer not available in this environment");
            return;
        }

        let low_latency = build_encoder_element(
            EncoderBackend::Software,
            TuneMode::LowLatency,
            QualityLevel::default(),
            RateControlMode::default(),
        );
        let low_latency_tune = low_latency.property_value("tune").serialize().unwrap();
        assert!(
            low_latency_tune.to_lowercase().contains("zero"),
            "TuneMode::LowLatency should set x264enc's `tune` flags to include zerolatency, got '{low_latency_tune}'"
        );

        let best_quality = build_encoder_element(
            EncoderBackend::Software,
            TuneMode::BestQuality,
            QualityLevel::default(),
            RateControlMode::default(),
        );
        let best_quality_tune = best_quality.property_value("tune").serialize().unwrap();
        assert!(
            !best_quality_tune.to_lowercase().contains("zero"),
            "TuneMode::BestQuality should leave x264enc's `tune` property unset, got '{best_quality_tune}'"
        );
    }

    /// [`QualityLevel`] has a real, direct analogue on both backends:
    /// x264enc's `speed-preset` and vah264enc's `target-usage`.
    #[test]
    fn quality_level_maps_to_speed_preset_and_target_usage() {
        if gst::init().is_err() {
            eprintln!("skipping: gstreamer not available in this environment");
            return;
        }

        let performance = build_encoder_element(
            EncoderBackend::Software,
            TuneMode::default(),
            QualityLevel::Performance,
            RateControlMode::default(),
        );
        // x264enc's `speed-preset` is an enum GEnum; comparing its nick
        // avoids hardcoding the underlying integer value.
        assert_eq!(performance.property_value("speed-preset").serialize().unwrap(), "ultrafast");

        let balanced = build_encoder_element(
            EncoderBackend::Software,
            TuneMode::default(),
            QualityLevel::Balanced,
            RateControlMode::default(),
        );
        assert_eq!(balanced.property_value("speed-preset").serialize().unwrap(), "medium");

        let quality = build_encoder_element(
            EncoderBackend::Software,
            TuneMode::default(),
            QualityLevel::Quality,
            RateControlMode::default(),
        );
        assert_eq!(quality.property_value("speed-preset").serialize().unwrap(), "veryslow");

        if gst::ElementFactory::make(VAAPI_ENCODER_ELEMENT).build().is_err() {
            eprintln!("skipping VAAPI half: '{VAAPI_ENCODER_ELEMENT}' is not available in this environment");
            return;
        }

        let vaapi_performance = build_encoder_element(
            EncoderBackend::Vaapi,
            TuneMode::default(),
            QualityLevel::Performance,
            RateControlMode::default(),
        );
        assert_eq!(vaapi_performance.property::<u32>("target-usage"), 2);

        let vaapi_balanced = build_encoder_element(
            EncoderBackend::Vaapi,
            TuneMode::default(),
            QualityLevel::Balanced,
            RateControlMode::default(),
        );
        assert_eq!(vaapi_balanced.property::<u32>("target-usage"), 4);

        let vaapi_quality = build_encoder_element(
            EncoderBackend::Vaapi,
            TuneMode::default(),
            QualityLevel::Quality,
            RateControlMode::default(),
        );
        assert_eq!(vaapi_quality.property::<u32>("target-usage"), 1);
    }

    /// [`RateControlMode`] is a clean, native mapping onto vah264enc's own
    /// `rate-control` enum (`cqp`/`vbr` — `Qvbr` maps to `vbr`, not literal
    /// `qvbr`, see that variant's doc comment); on x264enc, `Qvbr` maps to
    /// plain untouched `bitrate=` (no `pass` override, its closest available
    /// analogue — see [`EncoderBackend::launch_fragment`]'s `Why:` note)
    /// while `Cqp` switches to `pass=qual`. Also confirms `Qvbr`'s VAAPI-only
    /// `target-percentage`/`cpb-size` are genuinely applied and genuinely
    /// different from `Cqp`'s untouched element defaults — not silently
    /// falling back to the same behavior.
    #[test]
    fn rate_control_mode_maps_per_backend() {
        if gst::init().is_err() {
            eprintln!("skipping: gstreamer not available in this environment");
            return;
        }

        let x264_qvbr = build_encoder_element(
            EncoderBackend::Software,
            TuneMode::default(),
            QualityLevel::default(),
            RateControlMode::Qvbr,
        );
        // No `pass` override at all: x264enc's own plain-`bitrate=` default,
        // read back as its unset/default nick.
        assert_eq!(x264_qvbr.property_value("pass").serialize().unwrap(), "cbr");

        let x264_cqp = build_encoder_element(
            EncoderBackend::Software,
            TuneMode::default(),
            QualityLevel::default(),
            RateControlMode::Cqp,
        );
        assert_eq!(x264_cqp.property_value("pass").serialize().unwrap(), "qual");

        if gst::ElementFactory::make(VAAPI_ENCODER_ELEMENT).build().is_err() {
            eprintln!("skipping VAAPI half: '{VAAPI_ENCODER_ELEMENT}' is not available in this environment");
            return;
        }

        let vaapi_cqp = build_encoder_element(
            EncoderBackend::Vaapi,
            TuneMode::default(),
            QualityLevel::default(),
            RateControlMode::Cqp,
        );
        assert_eq!(vaapi_cqp.property_value("rate-control").serialize().unwrap(), "cqp");
        // `Cqp` leaves `qpi` at the element's own default (26) and
        // `target-percentage` unset (66%, VBR-only anyway) — untouched.
        assert_eq!(vaapi_cqp.property::<u32>("qpi"), 26);
        assert_eq!(vaapi_cqp.property::<u32>("target-percentage"), 66);

        let vaapi_qvbr = build_encoder_element(
            EncoderBackend::Vaapi,
            TuneMode::default(),
            QualityLevel::default(),
            RateControlMode::Qvbr,
        );
        // Despite the variant's name, this is `vbr`, not literal `qvbr` — see
        // [`RateControlMode::Qvbr`]'s doc comment for why.
        assert_eq!(vaapi_qvbr.property_value("rate-control").serialize().unwrap(), "vbr");
        // `Qvbr` tunes `target-percentage` well above `Cqp`'s untouched
        // default (66%, itself moot under `cqp` since that property is
        // VBR-only), so the real encode target sits close to the configured
        // ceiling instead of swinging down toward two-thirds of it — see
        // [`VBR_TARGET_PERCENTAGE`]'s doc comment for the exact value/why.
        assert_eq!(vaapi_qvbr.property::<u32>("target-percentage"), VBR_TARGET_PERCENTAGE);
        assert!(
            vaapi_qvbr.property::<u32>("target-percentage") > vaapi_cqp.property::<u32>("target-percentage"),
            "Qvbr's target-percentage must be higher than the untuned 66% default"
        );
        // `cpb-size` set to a deliberately tight fraction of the configured
        // bitrate (4000 kbps, per [`build_encoder_element`]'s hardcoded test
        // bitrate) — not left at the element's own `0` ("auto-calculate")
        // default, and meaningfully smaller than a naive 1x-bitrate buffer.
        assert_eq!(
            vaapi_qvbr.property::<u32>("cpb-size"),
            4000 * VBR_CPB_SIZE_NUMERATOR / VBR_CPB_SIZE_DENOMINATOR
        );
        assert_eq!(vaapi_cqp.property::<u32>("cpb-size"), 0, "Cqp must not set cpb-size at all");
    }

    /// Exercises the two mechanisms `pump_loop` relies on for
    /// pause-at-zero-subscribers/keyframe-on-new-subscriber (see this
    /// module's doc comment and [`CaptureControlEvent`]) directly against a
    /// real GStreamer pipeline shaped like the encoder branch [`build_pipeline`]
    /// produces (`valve name=v drop=false ! ... ! x264enc ! ... ! appsink`),
    /// with `videotestsrc` standing in for the real capture source per
    /// [`run_backend_and_expect_encoded_buffer`]'s doc comment:
    ///
    /// 1. With a large `key-int-max` (so only the very first frame is
    ///    naturally a keyframe), confirms encoded output actually contains
    ///    both a keyframe and a later `DELTA_UNIT` (non-keyframe) buffer —
    ///    establishing the delta-frame baseline the rest of the test
    ///    depends on.
    /// 2. Sets the valve's `drop` property to `true` and confirms no further
    ///    encoded buffers reach the appsink while it's closed.
    /// 3. Re-opens the valve, pushes a real
    ///    [`gst_video::UpstreamForceKeyUnitEvent`] on the appsink's sink pad
    ///    (exactly what [`force_keyframe`] does), and confirms the next
    ///    buffer that comes out does *not* carry `DELTA_UNIT` — i.e. is a
    ///    real keyframe, not just whatever the encoder would have produced
    ///    next regardless.
    #[test]
    fn encoder_valve_gates_output_and_force_key_unit_event_forces_a_keyframe() {
        if gst::init().is_err() {
            eprintln!("skipping: gstreamer not available in this environment");
            return;
        }

        let backend = EncoderBackend::Software;
        // A huge GOP so nothing but the very first frame is a keyframe
        // within this test's short run, isolating the force-key-unit
        // event's effect from x264enc's own periodic keyframing.
        //
        // `TuneMode::LowLatency`/`QualityLevel::Performance`, not
        // `::default()`: this is a real-time pipeline mechanics test (valve
        // gating / forced keyframes), not a tuning-knob test — it needs the
        // encoder to emit output immediately and keep up in real time, which
        // `BestQuality`'s lookahead/B-frame buffering and `Balanced`'s
        // `medium` preset were both observed to not reliably do on a plain
        // CI machine (see `small_settings`'s `Why:` note above).
        let encoder = backend.launch_fragment(
            DEFAULT_BITRATE_KBPS,
            10_000,
            TuneMode::LowLatency,
            QualityLevel::Performance,
            RateControlMode::default(),
            VideoCodec::default(),
        );
        let raw_format = backend.raw_format();
        let description = format!(
            "videotestsrc is-live=true \
             ! videoconvert \
             ! valve name=v drop=false \
             ! video/x-raw,format={raw_format} \
             ! {encoder} \
             ! video/x-h264,stream-format=byte-stream,alignment=au \
             ! appsink name=sink emit-signals=false sync=false max-buffers=4 drop=true"
        );

        let element = gst::parse::launch(&description).expect("pipeline should parse and link");
        let pipeline = element.downcast::<gst::Pipeline>().expect("top-level element should be a Pipeline");
        let valve = pipeline.by_name("v").expect("valve element should exist");
        let sink = pipeline
            .by_name("sink")
            .expect("sink element should exist")
            .downcast::<gst_app::AppSink>()
            .expect("sink should be an AppSink");
        let sink_pad = sink.static_pad("sink").expect("appsink should have a sink pad");

        pipeline.set_state(gst::State::Playing).expect("pipeline should reach PLAYING");

        // (1) Baseline: the first buffer is a keyframe, a later one is a
        // plain delta frame (proving the huge key-int-max is actually
        // suppressing automatic re-keyframing, so step 3 below is a real
        // test of the force-key-unit event and not a fluke of timing).
        let mut got_keyframe = false;
        let mut got_delta = false;
        for _ in 0..100 {
            if got_keyframe && got_delta {
                break;
            }
            if let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_mseconds(200)) {
                let buffer = sample.buffer().expect("sample should have a buffer");
                if buffer.flags().contains(gst::BufferFlags::DELTA_UNIT) {
                    got_delta = true;
                } else {
                    got_keyframe = true;
                }
            }
        }
        assert!(got_keyframe, "expected the first encoded buffer to be a keyframe");
        assert!(got_delta, "expected a later buffer to be a non-keyframe delta frame with a large key-int-max");

        // (2) Closing the valve should stop new encoded buffers from
        // reaching the appsink. Drain anything already in flight first, then
        // confirm nothing further shows up.
        valve.set_property("drop", true);
        while sink.try_pull_sample(gst::ClockTime::from_mseconds(50)).is_some() {}
        assert!(
            sink.try_pull_sample(gst::ClockTime::from_mseconds(700)).is_none(),
            "closing the valve should stop new encoded buffers from reaching the appsink"
        );

        // (3) Re-opening the valve and forcing a keyframe (the same
        // `UpstreamForceKeyUnitEvent`-on-the-appsink's-sink-pad mechanism
        // `force_keyframe` uses) must make the very next buffer a keyframe.
        valve.set_property("drop", false);
        let event = gst_video::UpstreamForceKeyUnitEvent::builder().all_headers(true).build();
        assert!(sink_pad.push_event(event), "force-keyframe event should be accepted by the pipeline");

        let mut resumed_keyframe = None;
        for _ in 0..100 {
            if let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_mseconds(200)) {
                let buffer = sample.buffer().expect("sample should have a buffer");
                resumed_keyframe = Some(!buffer.flags().contains(gst::BufferFlags::DELTA_UNIT));
                break;
            }
        }

        pipeline.set_state(gst::State::Null).expect("pipeline should stop cleanly");

        assert_eq!(
            resumed_keyframe,
            Some(true),
            "the first encoded buffer after resuming + a forced keyframe request should not carry DELTA_UNIT"
        );
    }

    /// Sanity-checks the tee's second (preview) branch this module's doc
    /// comment describes: `... ! videoconvert ! tee name=t !` with a
    /// `t. ! queue ... ! videorate ! video/x-raw,framerate=1/1 !
    /// videoconvert ! video/x-raw,format=RGB ! appsink` branch alongside the
    /// main encoder branch — the same structure [`build_pipeline`] produces,
    /// with `videotestsrc` standing in for `pipewiresrc fd=… path=…` per
    /// [`run_backend_and_expect_encoded_buffer`]'s doc comment. Asserts the
    /// preview branch's `appsink` yields a sample that, once stride-corrected
    /// via [`sample_to_raw_frame`] exactly like the real pipeline does, is a
    /// tightly-packed `width * height * 3`-byte RGB8 frame of the source's
    /// negotiated dimensions — i.e. a frame [`preview::spawn_preview_task`]
    /// could actually consume.
    #[test]
    fn pipeline_tee_branch_produces_rgb_preview_frames() {
        if gst::init().is_err() {
            eprintln!("skipping: gstreamer not available in this environment");
            return;
        }

        let backend = EncoderBackend::Software;
        let encoder = backend.launch_fragment(
            DEFAULT_BITRATE_KBPS,
            (DEFAULT_GOP_MILLIS * DEFAULT_FRAMERATE) / 1000,
            TuneMode::default(),
            QualityLevel::default(),
            RateControlMode::default(),
            VideoCodec::default(),
        );
        let raw_format = backend.raw_format();
        let description = format!(
            "videotestsrc num-buffers=40 \
             ! video/x-raw,width=640,height=480 \
             ! videoconvert \
             ! tee name=t \
             t. ! queue \
                ! video/x-raw,format={raw_format} \
                ! {encoder} \
                ! video/x-h264,stream-format=byte-stream,alignment=au \
                ! appsink name=sink emit-signals=false sync=false \
             t. ! queue leaky=downstream max-size-buffers=1 \
                ! videorate \
                ! video/x-raw,framerate={PREVIEW_BRANCH_FRAMERATE}/1 \
                ! videoconvert \
                ! video/x-raw,format=RGB \
                ! appsink name=preview_sink emit-signals=false sync=false max-buffers=1 drop=true"
        );

        let element = gst::parse::launch(&description).expect("tee pipeline should parse and link");
        let pipeline = element.downcast::<gst::Pipeline>().expect("top-level element should be a Pipeline");
        let preview_sink = pipeline
            .by_name("preview_sink")
            .expect("preview_sink element should exist")
            .downcast::<gst_app::AppSink>()
            .expect("preview_sink should be an AppSink");

        pipeline.set_state(gst::State::Playing).expect("pipeline should reach PLAYING");

        let mut raw_frame = None;
        let bus = pipeline.bus().unwrap();
        for msg in bus.iter_timed(gst::ClockTime::from_seconds(10)) {
            match msg.view() {
                gst::MessageView::Eos(_) => break,
                gst::MessageView::Error(e) => panic!("pipeline error: {} ({:?})", e.error(), e.debug()),
                _ => {}
            }
            if let Some(sample) = preview_sink.try_pull_sample(gst::ClockTime::from_mseconds(50)) {
                raw_frame = Some(sample_to_raw_frame(&sample).expect("preview sample should convert to a RawFrame"));
                break;
            }
        }

        pipeline.set_state(gst::State::Null).expect("pipeline should stop cleanly");

        let frame = raw_frame.expect("expected at least one RGB preview frame from the tee branch");
        assert_eq!(frame.width, 640);
        assert_eq!(frame.height, 480);
        assert_eq!(
            frame.rgb.len(),
            640 * 480 * 3,
            "preview frame RGB buffer should be tightly packed with no stride padding"
        );
    }

    /// **Regression (leak on a failed share):** abandoning a capture that
    /// never became a [`CaptureHandle`] must actually stop its pipeline.
    ///
    /// `Why this is worth a test of its own:` `CaptureHandle`'s `Drop` is
    /// what normally guarantees a capture can't outlive its owner, and it
    /// only exists once the handle is *constructed*. When the first
    /// destination failed to come up, `start_sharing_with_source` used to
    /// return `Err` from between those two points — so nothing ever stopped
    /// the pipeline, and the process went on capturing the user's screen
    /// (measured: 38 live threads, ~3.7% CPU, PipeWire stream still open)
    /// for the rest of its life, with no UI affordance to stop something the
    /// user had just been told had failed.
    #[tokio::test]
    async fn abandoning_a_capture_stops_its_pipeline() {
        if gst::init().is_err() {
            eprintln!("skipping: gstreamer not available in this environment");
            return;
        }

        let built = build_pipeline(
            "videotestsrc is-live=true pattern=smpte ! video/x-raw,width=64,height=64,framerate=30/1",
        )
        .expect("pipeline should construct");
        let pipeline = built.pipeline.clone();
        pipeline.set_state(gst::State::Playing).expect("pipeline should reach PLAYING");
        let (state_result, ..) = pipeline.state(gst::ClockTime::from_seconds(5));
        state_result.expect("pipeline should settle into PLAYING");

        let error = abandon_capture(
            built.pipeline,
            CaptureSource::TestSource("videotestsrc".to_string()),
            anyhow!("the first destination failed"),
        )
        .await;

        // The original error survives, so the caller's message still explains
        // what actually went wrong rather than reporting the cleanup.
        assert!(format!("{error:#}").contains("the first destination failed"));

        let (_result, current, _pending) = pipeline.state(gst::ClockTime::from_seconds(5));
        assert_eq!(
            current,
            gst::State::Null,
            "an abandoned capture's pipeline must be stopped, not left running with no owner"
        );
    }

    /// **Regression (the guard itself):** adding a destination branch to a
    /// live `tee` must not send a `RECONFIGURE` event upstream of that tee.
    ///
    /// `Why this asserts on the event rather than on frames:` the damage a
    /// stray reconfigure does is entirely source-specific — it kills a real
    /// portal capture stone dead while a `videotestsrc` renegotiates
    /// instantly and silently (see
    /// [`super::isolate_source_from_branch_renegotiation`] for the measured
    /// before/after). So no `videotestsrc`-fed test can ever observe the
    /// *consequence*, and an assertion on frames would pass with the bug
    /// fully present — which is exactly what happened. What a synthetic
    /// source *can* prove is the mechanism: that the event never gets past
    /// the tee. That is unattended coverage for the one thing the
    /// `#[ignore]`d portal test would otherwise be the only guard for.
    #[test]
    fn adding_a_branch_never_renegotiates_upstream_of_the_tee() {
        if gst::init().is_err() {
            eprintln!("skipping: gstreamer not available in this environment");
            return;
        }

        let built = build_pipeline(
            "videotestsrc is-live=true pattern=smpte ! video/x-raw,width=64,height=64,framerate=30/1",
        )
        .expect("pipeline should construct");

        // Count on the *upstream* side of the guard: the pad feeding the
        // tee's sink pad, i.e. the first thing a forwarded reconfigure would
        // reach on its way to the source.
        let upstream = built
            .tee
            .static_pad("sink")
            .and_then(|sink| sink.peer())
            .expect("the tee's sink pad should be linked to the source chain");
        let seen = Arc::new(AtomicU32::new(0));
        let counter = seen.clone();
        upstream
            .add_probe(gst::PadProbeType::EVENT_UPSTREAM, move |_, info| {
                if let Some(gst::PadProbeData::Event(event)) = &info.data
                    && event.type_() == gst::EventType::Reconfigure
                {
                    counter.fetch_add(1, Ordering::Relaxed);
                }
                gst::PadProbeReturn::Ok
            })
            .expect("probe should install");

        built.pipeline.set_state(gst::State::Playing).expect("pipeline should reach PLAYING");
        let (state_result, ..) = built.pipeline.state(gst::ClockTime::from_seconds(5));
        state_result.expect("pipeline should settle into PLAYING");

        // Two branches, because the first one added to a live tee and every
        // one after it are different code paths as far as pad activation is
        // concerned, and both must stay silent.
        let first = branch::add_branch(&built.pipeline, &built.tee, branch::BranchId(1), branch::EncodeSettings {
            width: 64,
            height: 64,
            framerate: 15,
            bitrate_kbps: 500,
            backend: EncoderBackend::Software,
            tune: TuneMode::default(),
            quality_level: QualityLevel::default(),
            rate_control: RateControlMode::default(),
            gop_millis: DEFAULT_GOP_MILLIS,
            codec: VideoCodec::default(),
        })
        .expect("first branch should build");
        let second = branch::add_branch(&built.pipeline, &built.tee, branch::BranchId(2), branch::EncodeSettings {
            width: 32,
            height: 32,
            framerate: 10,
            bitrate_kbps: 500,
            backend: EncoderBackend::Software,
            tune: TuneMode::default(),
            quality_level: QualityLevel::default(),
            rate_control: RateControlMode::default(),
            gop_millis: DEFAULT_GOP_MILLIS,
            codec: VideoCodec::default(),
        })
        .expect("second branch should build");

        let count = seen.load(Ordering::Relaxed);
        drop(first);
        drop(second);
        built.pipeline.set_state(gst::State::Null).expect("pipeline should stop cleanly");

        assert_eq!(
            count, 0,
            "adding a destination branch let {count} RECONFIGURE event(s) reach upstream of the tee — \
             against a real portal source that is what stops the capture producing frames entirely"
        );
    }

    /// Real, measured proof that a destination's configured framerate
    /// actually reaches its encoder's output rate — not just that the
    /// pipeline parses/links (that alone would not have caught either of the
    /// prior bugs in this exact area: the original bitrate-starvation bug
    /// where the `videorate` cap was simply missing, and the general "looks
    /// configured in the UI but isn't actually applied" bug shape).
    ///
    /// The rate is measured **at the destination's own appsink**, which is
    /// where it is now enforced: nothing converts the rate before the tee
    /// (see [`build_pipeline`]'s "Why there is no rate conversion before the
    /// `tee`"), so the branch's own `videorate` is the only thing standing
    /// between a 60fps source and the encoder.
    #[test]
    fn a_destinations_framerate_actually_caps_its_encoder_output_rate() {
        if gst::init().is_err() {
            eprintln!("skipping: gstreamer not available in this environment");
            return;
        }

        // Small resolution and software encoder keep this fast/deterministic
        // on any machine (no VAAPI dependency), while `is-live=true` and a
        // fixed 60/1 source framerate (comfortably above every configured
        // rate under test) makes the branch's `videorate` — not the source's
        // own delivery rate — the thing actually being measured, mirroring
        // how a real portal source can deliver well above the configured rate
        // under motion. `TuneMode::LowLatency`/`QualityLevel::Performance`
        // are pinned explicitly below (not `::default()`) for the same
        // reason: this test measures real-time output rate, and
        // `BestQuality`'s lookahead buffering / `Balanced`'s `medium` preset
        // were observed to starve it of frames on a plain CI machine (see
        // `branch::tests::small_settings`'s `Why:` note).
        let source = "videotestsrc is-live=true pattern=smpte \
             ! video/x-raw,width=64,height=64,framerate=60/1";

        fn measure_output_fps(source: &str, configured_fps: u32) -> f64 {
            let built = build_pipeline(source).expect("pipeline should construct");
            built.pipeline.set_state(gst::State::Playing).expect("pipeline should reach PLAYING");
            let (state_result, ..) = built.pipeline.state(gst::ClockTime::from_seconds(5));
            state_result.expect("pipeline should settle into PLAYING");

            let branch = branch::add_branch(
                &built.pipeline,
                &built.tee,
                branch::BranchId(0),
                branch::EncodeSettings {
                    width: 0,
                    height: 0,
                    framerate: configured_fps,
                    bitrate_kbps: DEFAULT_BITRATE_KBPS,
                    backend: EncoderBackend::Software,
                    tune: TuneMode::LowLatency,
                    quality_level: QualityLevel::Performance,
                    rate_control: RateControlMode::default(),
                    gop_millis: DEFAULT_GOP_MILLIS,
                    codec: VideoCodec::default(),
                },
            )
            .expect("encode branch should build on the live pipeline");

            let window = Duration::from_secs(2);
            let deadline = Instant::now() + window;
            let mut count = 0u32;
            while Instant::now() < deadline {
                if branch.appsink.try_pull_sample(gst::ClockTime::from_mseconds(100)).is_some() {
                    count += 1;
                }
            }

            drop(branch);
            built.pipeline.set_state(gst::State::Null).expect("pipeline should stop cleanly");
            count as f64 / window.as_secs_f64()
        }

        let low_configured = 10u32;
        let high_configured = 25u32;
        let low_measured = measure_output_fps(source, low_configured);
        let high_measured = measure_output_fps(source, high_configured);

        eprintln!(
            "measured encoder output rate: configured {low_configured}fps -> {low_measured:.1}fps measured; \
             configured {high_configured}fps -> {high_measured:.1}fps measured"
        );

        // Generous tolerance (±40%) — this is real wall-clock GStreamer
        // timing on a shared CI/dev machine, not a deterministic simulation —
        // but tight enough that "ignoring the parameter and just passing
        // through whatever the 60fps source delivers" (which would measure
        // close to 60fps at both settings) or "always emitting at some other
        // fixed rate regardless of the parameter" both fail it.
        assert!(
            (low_measured - low_configured as f64).abs() < low_configured as f64 * 0.4,
            "configured {low_configured}fps produced a measured rate of {low_measured:.1}fps, \
             too far off to say the framerate parameter is actually being applied"
        );
        assert!(
            (high_measured - high_configured as f64).abs() < high_configured as f64 * 0.4,
            "configured {high_configured}fps produced a measured rate of {high_measured:.1}fps, \
             too far off to say the framerate parameter is actually being applied"
        );
        assert!(
            high_measured > low_measured * 1.3,
            "changing the configured framerate from {low_configured} to {high_configured} did not \
             meaningfully change the measured output rate ({low_measured:.1}fps vs {high_measured:.1}fps) — \
             looks like the parameter isn't actually reaching the pipeline, just like the bug this test guards against"
        );
    }

    /// A real desktop-capture source fragment for the real-hardware tests
    /// below, or `None` on a machine where `ximagesrc` can't grab a screen.
    ///
    /// `Why `ximagesrc` and not the real portal source:` the portal path
    /// ([`CaptureSource::Portal`]) needs an interactive consent dialog no
    /// automated test can click — but the thing under test here is what the
    /// *branch* does with real, damage-driven, full-resolution desktop frames,
    /// which `ximagesrc` delivers just as genuinely (real pixels, real
    /// 1920x1080 encode cost, real variable arrival rate) while
    /// [`CaptureSource::TestSource`] keeps the rest of the pipeline identical.
    ///
    /// **Region is pinned to the first monitor's 1920x1080 origin block on
    /// purpose** — never a full multi-head X screen grab — so this test can
    /// never capture whatever is on another output.
    #[cfg(unix)]
    fn real_desktop_source() -> Option<String> {
        if gst::ElementFactory::find("ximagesrc").is_none() {
            return None;
        }
        // `Why no `do-timestamp=true` here, unlike the production
        // `pipewiresrc` fragment:` on `ximagesrc` that property stamps buffers
        // off the raw clock instead of pipeline running time, which would make
        // this test's PTS arithmetic wildly unrepresentative of production.
        // Plain `basesrc` timestamping gives running-time PTS starting at ~0 —
        // exactly what `pipewiresrc do-timestamp=true` produces (see
        // [`send_encoded_sample`]).
        Some(
            "ximagesrc startx=0 starty=0 endx=1919 endy=1079 use-damage=false \
             ! video/x-raw,framerate=60/1"
                .to_string(),
        )
    }

    /// What [`drain_branch_like_the_pump`] observed over its window.
    struct DrainStats {
        /// Peak sliding-window encode rate, computed with exactly
        /// [`record_encoder_frame`]'s window and arithmetic — so this is the
        /// same number the destination card's "Captured: N fps" readout shows.
        peak_fps: f64,
        total: u32,
        /// How many frames got a wire PTS only 1µs past the previous one, i.e.
        /// how many fell through [`destination::DestinationRuntime`]'s
        /// monotonic clamp instead of carrying a real timestamp.
        clamped_pts: u32,
    }

    /// Drains `branch` for `window` exactly the way [`destination_pump`]'s
    /// sample arm does — including running every buffer's PTS through the
    /// destination's real wire clock ([`send_encoded_sample`]'s
    /// `wire_pts_from_buffer` call), which is what makes the PTS half of the
    /// swap observable here.
    async fn drain_branch_like_the_pump(
        branch: &mut branch::EncodeBranch,
        runtime: &destination::SharedDestination,
        window: Duration,
    ) -> DrainStats {
        let mut times: VecDeque<Instant> = VecDeque::new();
        let mut stats = DrainStats { peak_fps: 0.0, total: 0, clamped_pts: 0 };
        let mut last_wire_pts: Option<u64> = None;
        let deadline = Instant::now() + window;
        while Instant::now() < deadline {
            let Ok(Some(sample)) = tokio::time::timeout(Duration::from_millis(250), branch.next_sample()).await else {
                continue;
            };
            stats.total += 1;

            let buffer_pts_us = sample.buffer().and_then(|b| b.pts()).map(|t| t.useconds()).unwrap_or(0);
            let wire_pts_us = runtime.lock().unwrap().wire_pts_from_buffer(buffer_pts_us);
            if last_wire_pts.is_some_and(|last| wire_pts_us.saturating_sub(last) <= 1) {
                stats.clamped_pts += 1;
            }
            last_wire_pts = Some(wire_pts_us);

            let now = Instant::now();
            times.push_back(now);
            while let Some(&oldest) = times.front() {
                if now.duration_since(oldest) > ENCODER_FPS_WINDOW {
                    times.pop_front();
                } else {
                    break;
                }
            }
            stats.peak_fps = stats.peak_fps.max(times.len() as f64 / ENCODER_FPS_WINDOW.as_secs_f64());
        }
        stats
    }

    /// Regression test for the reported **"changing the preset makes the FPS
    /// shoot up to 300+ and sometimes crashes the stream"** bug.
    ///
    /// Applying a preset from the Streams tab's edit panel changes `tune`,
    /// `quality_level` *and* `rate_control` in one `edit_destination` call,
    /// which [`destination::classify_change`] turns into a single
    /// [`destination::ChangeTier::BranchSwap`]. The swap is `await`ed inside
    /// [`destination_pump`]'s own loop, so for its entire duration **nothing
    /// drains this destination's appsink** — this test reproduces that exactly
    /// (the `swap_to_new_branch().await` below is the only thing running while
    /// it's in flight, same as in the pump).
    ///
    /// What must hold afterwards: the destination keeps producing at its own
    /// configured framerate. A burst well above it is not cosmetic — every one
    /// of those access units is encrypted, fragmented and pushed at the relay
    /// (see [`destination_pump`]'s send arm), which is what took the live
    /// stream down.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn applying_a_preset_never_bursts_a_branch_above_its_configured_framerate() {
        if gst::init().is_err() {
            eprintln!("skipping: gstreamer not available in this environment");
            return;
        }
        let Some(source) = real_desktop_source() else {
            eprintln!("skipping: no ximagesrc on this machine, can't grab a real desktop");
            return;
        };

        const CONFIGURED_FPS: u32 = 30;

        let built = build_pipeline(&source).expect("pipeline should construct");
        built.pipeline.set_state(gst::State::Playing).expect("pipeline should reach PLAYING");
        let (state_result, ..) = built.pipeline.state(gst::ClockTime::from_seconds(10));
        if state_result.is_err() {
            built.pipeline.set_state(gst::State::Null).ok();
            eprintln!("skipping: real desktop capture pipeline never reached PLAYING here");
            return;
        }

        // The destination as the user had it before touching anything: the
        // "Low-Latency" preset's fields.
        let before = destination::VideoParams {
            width: 1920,
            height: 1080,
            framerate: CONFIGURED_FPS,
            bitrate_kbps: DEFAULT_BITRATE_KBPS,
            // Whatever this machine would really pick, so the test exercises
            // the branch the owner's app actually runs.
            backend: detect_encoder_backend(),
            tune: TuneMode::LowLatency,
            quality_level: QualityLevel::Performance,
            // Low-Latency wants predictable, non-quality-seeking behavior.
            rate_control: RateControlMode::Cqp,
            gop_millis: DEFAULT_GOP_MILLIS,
            codec: VideoCodec::default(),
        };
        // Exactly what clicking "Balanced" in the edit panel sends: three
        // fields at once, nothing else changed.
        let after = destination::VideoParams {
            tune: TuneMode::BestQuality,
            quality_level: QualityLevel::Balanced,
            ..before
        };
        assert_eq!(
            destination::classify_change(
                destination::SourceCaps { width: 1920, height: 1080, framerate: CONFIGURED_FPS },
                before,
                after
            ),
            destination::ChangeTier::BranchSwap,
            "a preset click must be a tier-2 branch swap — otherwise this test isn't exercising the reported path"
        );

        let runtime = destination::DestinationRuntime::shared(Instant::now(), before);
        let mut branch = branch::add_branch(&built.pipeline, &built.tee, branch::BranchId(0), before.to_encode_settings())
            .expect("encode branch should build on the live pipeline");

        // `Why ten seconds and not one:` the burst this guards against is
        // proportional to how long the capture has been running when the
        // preset is clicked (see [`branch::RateCap`]) — a share that just
        // started has nothing to replay and would pass even while broken.
        let before_stats = drain_branch_like_the_pump(&mut branch, &runtime, Duration::from_secs(10)).await;
        eprintln!(
            "before the preset change: peak {:.1}fps over {} access units, {} clamped PTS",
            before_stats.peak_fps, before_stats.total, before_stats.clamped_pts
        );

        let mut branch = match branch::swap_to_new_branch(&built.pipeline, &built.tee, &runtime, branch, branch::BranchId(1), after).await
        {
            branch::SwapOutcome::Swapped(b) => b,
            branch::SwapOutcome::KeptOld(b, e) => {
                eprintln!("preset swap failed on this machine ({e:#}); nothing to measure");
                drop(b);
                built.pipeline.set_state(gst::State::Null).ok();
                return;
            }
        };

        let after_stats = drain_branch_like_the_pump(&mut branch, &runtime, Duration::from_secs(6)).await;
        eprintln!(
            "after the preset change: peak {:.1}fps over {} access units, {} clamped PTS",
            after_stats.peak_fps, after_stats.total, after_stats.clamped_pts
        );

        drop(branch);
        built.pipeline.set_state(gst::State::Null).expect("pipeline should stop cleanly");

        // 2x the configured rate is already far beyond any legitimate
        // scheduling jitter; the reported bug was ten times the cap.
        assert!(
            after_stats.peak_fps <= CONFIGURED_FPS as f64 * 2.0,
            "after applying a preset the branch burst to {:.1}fps, way above its configured \
             {CONFIGURED_FPS}fps (it was {:.1}fps before the change) — this is the runaway-fps \
             bug: every one of those frames is encrypted, fragmented and pushed at the relay",
            after_stats.peak_fps,
            before_stats.peak_fps
        );

        // The other half of the same bug: the replacement branch brings its
        // own (capture-age-behind) timeline, so without the re-anchor in
        // `swap_to_new_branch` every post-swap frame falls through the
        // monotonic clamp at +1µs and the viewer's pacer gets a stream of
        // effectively simultaneous frames. A handful right at the seam is
        // fine; hundreds is the bug.
        assert!(
            after_stats.clamped_pts <= 2,
            "{} of the {} frames after the preset change got a wire PTS only 1µs past the previous one — \
             the replacement branch's timeline was never re-anchored, so the whole stream is timestamped \
             through DestinationRuntime's monotonic clamp instead of carrying real presentation times",
            after_stats.clamped_pts,
            after_stats.total
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
            run_backend_and_expect_encoded_buffer(EncoderBackend::Vaapi, VideoCodec::H264),
            "VAAPI probe reported available, but the real vah264enc pipeline produced no encoded output"
        );
    }

    /// [`vaapi_pipeline_links_and_produces_encoded_output`]'s H.265 analogue:
    /// real-hardware proof the `videoconvert ! NV12 ! vah265enc !
    /// video/x-h265,stream-format=byte-stream,alignment=au ! appsink` chain
    /// actually links and produces output on this machine — not merely that
    /// `gst-inspect-1.0 vah265enc` lists the element. Gated on
    /// [`probe_vaapi_encoder`] (which only probes `vah264enc`, the baseline
    /// "is VAAPI usable at all" check — see [`supported_codecs`]'s doc
    /// comment on why H.265/AV1 don't get their own separate runtime probe)
    /// rather than skipped independently, since a machine without a working
    /// VAAPI device can't run any of the three VAAPI encoders.
    #[test]
    fn vaapi_h265_pipeline_links_and_produces_encoded_output() {
        if gst::init().is_err() {
            eprintln!("skipping: gstreamer not available in this environment");
            return;
        }

        if !probe_vaapi_encoder() {
            eprintln!("skipping: VAAPI is not available/functional on this machine");
            return;
        }

        if gst::ElementFactory::make(VAAPI_H265_ENCODER_ELEMENT).build().is_err() {
            eprintln!(
                "skipping: '{VAAPI_H265_ENCODER_ELEMENT}' is not available on this machine even though vah264enc is"
            );
            return;
        }

        assert!(
            run_backend_and_expect_encoded_buffer(EncoderBackend::Vaapi, VideoCodec::H265),
            "vah265enc is available, but the real H.265 VAAPI pipeline produced no encoded output"
        );
    }

    /// [`vaapi_pipeline_links_and_produces_encoded_output`]'s AV1 analogue —
    /// see that test's and [`vaapi_h265_pipeline_links_and_produces_encoded_output`]'s
    /// doc comments. This is also real proof that
    /// [`run_backend_and_expect_encoded_buffer`]'s AV1 output caps
    /// (`stream-format=obu-stream,alignment=tu` — no Annex-B start codes,
    /// unlike H.264/H.265) actually negotiate against `vaav1enc`'s real SRC
    /// caps, not just that they're a plausible guess.
    #[test]
    fn vaapi_av1_pipeline_links_and_produces_encoded_output() {
        if gst::init().is_err() {
            eprintln!("skipping: gstreamer not available in this environment");
            return;
        }

        if !probe_vaapi_encoder() {
            eprintln!("skipping: VAAPI is not available/functional on this machine");
            return;
        }

        if gst::ElementFactory::make(VAAPI_AV1_ENCODER_ELEMENT).build().is_err() {
            eprintln!(
                "skipping: '{VAAPI_AV1_ENCODER_ELEMENT}' is not available on this machine even though vah264enc is"
            );
            return;
        }

        assert!(
            run_backend_and_expect_encoded_buffer(EncoderBackend::Vaapi, VideoCodec::Av1),
            "vaav1enc is available, but the real AV1 VAAPI pipeline produced no encoded output"
        );
    }

    /// [`quality_level_maps_to_speed_preset_and_target_usage`]'s H.265/AV1
    /// analogue: `target-usage` maps identically to `vah264enc`'s on both
    /// (confirmed via `gst-inspect-1.0` — same property name, same 1-7
    /// range, same semantics), so this only needs to check one representative
    /// value per codec rather than repeat the full Performance/Balanced/
    /// Quality sweep already covered for `vah264enc`.
    #[test]
    fn quality_level_maps_to_target_usage_on_h265_and_av1() {
        if gst::init().is_err() {
            eprintln!("skipping: gstreamer not available in this environment");
            return;
        }
        if !probe_vaapi_encoder() {
            eprintln!("skipping: VAAPI is not available/functional on this machine");
            return;
        }

        if gst::ElementFactory::make(VAAPI_H265_ENCODER_ELEMENT).build().is_ok() {
            let h265_quality = build_encoder_element_for_codec(
                EncoderBackend::Vaapi,
                TuneMode::default(),
                QualityLevel::Quality,
                RateControlMode::default(),
                VideoCodec::H265,
            );
            assert_eq!(h265_quality.property::<u32>("target-usage"), 1);
        } else {
            eprintln!("skipping H.265 half: '{VAAPI_H265_ENCODER_ELEMENT}' is not available in this environment");
        }

        if gst::ElementFactory::make(VAAPI_AV1_ENCODER_ELEMENT).build().is_ok() {
            let av1_performance = build_encoder_element_for_codec(
                EncoderBackend::Vaapi,
                TuneMode::default(),
                QualityLevel::Performance,
                RateControlMode::default(),
                VideoCodec::Av1,
            );
            assert_eq!(av1_performance.property::<u32>("target-usage"), 2);
        } else {
            eprintln!("skipping AV1 half: '{VAAPI_AV1_ENCODER_ELEMENT}' is not available in this environment");
        }
    }

    /// [`rate_control_mode_maps_per_backend`]'s H.265/AV1 analogue: both
    /// `vah265enc`/`vaav1enc` expose the same `cbr`/`vbr`/`cqp` `rate-control`
    /// enum as `vah264enc` (confirmed via `gst-inspect-1.0`).
    #[test]
    fn rate_control_mode_maps_on_h265_and_av1() {
        if gst::init().is_err() {
            eprintln!("skipping: gstreamer not available in this environment");
            return;
        }
        if !probe_vaapi_encoder() {
            eprintln!("skipping: VAAPI is not available/functional on this machine");
            return;
        }

        if gst::ElementFactory::make(VAAPI_H265_ENCODER_ELEMENT).build().is_ok() {
            let h265_quality = build_encoder_element_for_codec(
                EncoderBackend::Vaapi,
                TuneMode::default(),
                QualityLevel::default(),
                RateControlMode::Cqp,
                VideoCodec::H265,
            );
            assert_eq!(h265_quality.property_value("rate-control").serialize().unwrap(), "cqp");
        } else {
            eprintln!("skipping H.265 half: '{VAAPI_H265_ENCODER_ELEMENT}' is not available in this environment");
        }

        if gst::ElementFactory::make(VAAPI_AV1_ENCODER_ELEMENT).build().is_ok() {
            let av1_adaptive = build_encoder_element_for_codec(
                EncoderBackend::Vaapi,
                TuneMode::default(),
                QualityLevel::default(),
                RateControlMode::Qvbr,
                VideoCodec::Av1,
            );
            assert_eq!(av1_adaptive.property_value("rate-control").serialize().unwrap(), "vbr");
        } else {
            eprintln!("skipping AV1 half: '{VAAPI_AV1_ENCODER_ELEMENT}' is not available in this environment");
        }
    }

    /// [`EncoderBackend::Software`] only ever produces H.264 — see
    /// [`EncoderBackend::actual_codec`] — regardless of the requested
    /// [`VideoCodec`]. [`supported_codecs`] is what a caller should consult
    /// to avoid ever making this request in the first place; this test locks
    /// in the silent-downgrade behavior for the case something upstream
    /// doesn't.
    #[test]
    fn software_backend_always_produces_h264_regardless_of_requested_codec() {
        assert_eq!(EncoderBackend::Software.actual_codec(VideoCodec::H264), VideoCodec::H264);
        assert_eq!(EncoderBackend::Software.actual_codec(VideoCodec::H265), VideoCodec::H264);
        assert_eq!(EncoderBackend::Software.actual_codec(VideoCodec::Av1), VideoCodec::H264);
        assert_eq!(EncoderBackend::Vaapi.actual_codec(VideoCodec::H265), VideoCodec::H265);
    }

    #[test]
    fn supported_codecs_gates_h265_and_av1_to_vaapi_only() {
        assert_eq!(supported_codecs(EncoderBackend::Software), &[VideoCodec::H264]);
        assert_eq!(
            supported_codecs(EncoderBackend::Vaapi),
            &[VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1]
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

    /// Verifies the exact backstop `impl Drop for CaptureHandle` performs —
    /// `set_state(Null)` on a still-`Playing` pipeline — actually stops it,
    /// for the case a `CaptureHandle` is dropped bare (crash/panic/a
    /// disconnect path that never awaits `.stop()`). Constructing a full
    /// `CaptureHandle` here isn't practical (it needs a live
    /// `ScreenshareClient`/portal session), so this instead wraps a
    /// `videotestsrc`-based pipeline (per this module's other tests) in a
    /// minimal struct whose `Drop` impl is structurally identical to
    /// `CaptureHandle`'s, and checks — via a cloned `gst::Pipeline` handle
    /// (cheap: it's a ref-counted GObject wrapper, so cloning doesn't clone
    /// the underlying element graph) kept alive past the wrapper's scope —
    /// that the pipeline is `Null` after the wrapper drops without any
    /// explicit teardown call.
    #[test]
    fn drop_backstop_stops_a_playing_pipeline() {
        if gst::init().is_err() {
            eprintln!("skipping: gstreamer not available in this environment");
            return;
        }

        struct DropOnlyPipeline(gst::Pipeline);
        impl Drop for DropOnlyPipeline {
            fn drop(&mut self) {
                let _ = self.0.set_state(gst::State::Null);
            }
        }

        let element = gst::parse::launch("videotestsrc is-live=true ! fakesink").expect("pipeline should parse and link");
        let pipeline = element.downcast::<gst::Pipeline>().expect("top-level element should be a Pipeline");

        pipeline.set_state(gst::State::Playing).expect("pipeline should start reaching PLAYING");
        let (state_result, _current, _pending) = pipeline.state(gst::ClockTime::from_seconds(5));
        state_result.expect("pipeline should settle into PLAYING before the drop-backstop check");

        let pipeline_for_check = pipeline.clone();
        {
            let _wrapper = DropOnlyPipeline(pipeline);
            // `_wrapper` drops here with no `.stop()`-equivalent call ever
            // made — mirrors a bare-dropped `CaptureHandle`.
        }

        let (state_result, current, _pending) = pipeline_for_check.state(gst::ClockTime::from_seconds(5));
        state_result.expect("pipeline should have settled after Drop's set_state(Null) backstop");
        assert_eq!(
            current,
            gst::State::Null,
            "bare-dropping the handle should leave the pipeline Null, not still Playing"
        );
    }

    /// **The real capture path, end to end**: opens a genuine portal
    /// screencast session, builds this module's real pipeline against the
    /// resulting `pipewiresrc`, and brings a destination branch up on it —
    /// i.e. everything `start_sharing` does except the relay/announce half.
    ///
    /// `Why this needs to exist alongside the `videotestsrc` suite:` the whole
    /// unit suite can be green while real screen sharing is broken. It has
    /// been: the restructuring that made every destination a dynamic branch
    /// passed every test and then failed against the real portal, because a
    /// real source differs in ways `videotestsrc` cannot express (a live
    /// remote producer, damage-driven irregular delivery, full-screen-sized
    /// buffers, and caps this pipeline has to renegotiate rather than fix up
    /// front). This is the smallest test that would have caught it.
    ///
    /// The regression it actually caught is documented in full on
    /// [`isolate_source_from_branch_renegotiation`]: adding the first
    /// destination's branch to the live `tee` sent a `RECONFIGURE` upstream
    /// that killed the portal source dead. Measured here as 1 pass in 5 runs
    /// before the guard and 6 in 6 after it.
    ///
    /// `#[ignore]`d: it needs a real desktop session with a portal backend,
    /// and on a backend that prompts it needs a human to approve the dialog.
    /// Run with:
    /// `cargo test capture::tests::live_portal_capture_feeds_a_destination_branch -- --ignored --nocapture`
    #[cfg(unix)]
    #[tokio::test]
    #[ignore]
    async fn live_portal_capture_feeds_a_destination_branch() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        ensure_gstreamer_initialized().expect("gstreamer init");

        let portal = match open_portal_capture().await {
            Ok(portal) => portal,
            Err(e) => {
                eprintln!("skipping: no portal screencast session available here ({e:#})");
                return;
            }
        };
        let source = CaptureSource::Portal(portal);
        let fragment = source.launch_fragment(DEFAULT_FRAMERATE);
        eprintln!("[portal] source fragment: {fragment}");
        if let CaptureSource::Portal(p) = &source {
            eprintln!("[portal] node_id={} position={:?}", p.node_id, p.position);
        }

        let built = start_pipeline(&fragment, CaptureDomain::for_source(&source)).expect("real capture pipeline should start");
        let (width, height) = negotiated_size(&built.tee).unwrap_or((0, 0));
        eprintln!("[portal] negotiated size at the tee: {width}x{height}");

        // Diagnostics, not assertions — when this test fails it fails as
        // "no IDR within 5s" with nothing to distinguish "the source never
        // produced a frame" from "frames arrive but the branch can't
        // negotiate against them", and these two lines are what tell those
        // apart without a GST_DEBUG rerun.
        if let Some(caps) = built.tee.static_pad("sink").and_then(|p| p.current_caps()) {
            eprintln!("[portal] full caps at the tee: {caps}");
        }
        eprintln!(
            "[portal] raw frames off the tee: {:.1} fps",
            branch::tests::tap_tee_rate(&built.pipeline, &built.tee, Duration::from_millis(1500))
        );

        let settings = branch::EncodeSettings {
            width: width as u32,
            height: height as u32,
            framerate: DEFAULT_FRAMERATE,
            bitrate_kbps: DEFAULT_BITRATE_KBPS,
            backend: detect_encoder_backend(),
            tune: TuneMode::default(),
            quality_level: QualityLevel::default(),
            rate_control: RateControlMode::default(),
            gop_millis: DEFAULT_GOP_MILLIS,
            codec: VideoCodec::default(),
        };
        let idr_started = Instant::now();
        let result = branch::add_branch_live(&built.pipeline, &built.tee, branch::BranchId(0), settings).await;
        eprintln!("[portal] add_branch_live returned after {:?} (ok={})", idr_started.elapsed(), result.is_ok());
        let (branch, _idr) = match result {
            Ok(v) => v,
            Err(e) => {
                let _ = built.pipeline.set_state(gst::State::Null);
                panic!("a destination branch must come up against a real portal capture: {e:#}");
            }
        };
        eprintln!("[portal] first IDR received; branch is live");

        // Keep it running long enough to prove it is not a one-frame fluke.
        // `SSS_LIVE_SECONDS` lengthens the soak (e.g. 60 for the idle-monitor
        // check: the branch must keep producing at its configured rate for the
        // whole time, not just emit the one proving IDR).
        let seconds: u64 = std::env::var("SSS_LIVE_SECONDS").ok().and_then(|v| v.parse().ok()).unwrap_or(3);
        let deadline = Instant::now() + Duration::from_secs(seconds);
        let mut samples = 0u32;
        let mut last_report = Instant::now();
        let mut since_report = 0u32;
        while Instant::now() < deadline {
            if branch.appsink.try_pull_sample(gst::ClockTime::from_mseconds(100)).is_some() {
                samples += 1;
                since_report += 1;
            }
            if last_report.elapsed() >= Duration::from_secs(5) {
                eprintln!("[portal] {:.1} encoded fps over the last 5s", since_report as f64 / 5.0);
                since_report = 0;
                last_report = Instant::now();
            }
        }
        eprintln!("[portal] {samples} encoded access units in {seconds}s");
        // Deliberately a loose floor, not a rate check: a genuinely static
        // desktop produces very little damage, so the honest guarantee here is
        // "frames keep coming after the first IDR", which is what the failure
        // this test exists for (zero, forever) violates. Measured on an
        // ordinarily-active desktop this lands around 90 (~30fps).
        assert!(
            samples as u64 >= seconds.max(3) * 5 / 3,
            "a real capture should keep producing encoded frames, got {samples} in {seconds}s"
        );

        let _ = branch::remove_branch(&built.pipeline, &built.tee, branch).await;
        built.pipeline.set_state(gst::State::Null).expect("pipeline should stop cleanly");
    }

    #[cfg(unix)]
    mod picker_cancel {
        use super::super::*;
        use ashpd::zbus;

        /// The exact error the real portal returned when the picker was
        /// SIGTERMed (captured from `live_portal_probe`).
        fn invalid_session_method_error() -> zbus::Error {
            let call = zbus::Message::method_call("/org/freedesktop/portal/desktop", "Start")
                .unwrap()
                .build(&())
                .unwrap();
            let reply = zbus::Message::error(&call.header(), "org.freedesktop.DBus.Error.AccessDenied")
                .unwrap()
                .build(&"Invalid session")
                .unwrap();
            zbus::Error::MethodError(
                "org.freedesktop.DBus.Error.AccessDenied".try_into().unwrap(),
                Some("Invalid session".to_string()),
                reply,
            )
        }

        #[test]
        fn the_real_dismissed_picker_error_is_a_cancel() {
            let e = ashpd::Error::Portal(ashpd::PortalError::ZBus(invalid_session_method_error()));
            assert!(is_portal_user_cancel(&e));
        }

        #[test]
        fn the_fdo_mapped_form_of_it_is_a_cancel_too() {
            let fdo = zbus::fdo::Error::AccessDenied("Invalid session".to_string());
            let e = ashpd::Error::Portal(ashpd::PortalError::ZBus(zbus::Error::FDO(Box::new(fdo))));
            assert!(is_portal_user_cancel(&e));
        }

        #[test]
        fn the_standard_portal_cancel_codes_are_cancels() {
            assert!(is_portal_user_cancel(&ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled)));
            assert!(is_portal_user_cancel(&ashpd::Error::Portal(ashpd::PortalError::Cancelled("x".into()))));
        }

        #[test]
        fn real_failures_are_not_cancels() {
            assert!(!is_portal_user_cancel(&ashpd::Error::Response(ashpd::desktop::ResponseError::Other)));
            assert!(!is_portal_user_cancel(&ashpd::Error::Portal(ashpd::PortalError::Failed("x".into()))));
            assert!(!is_portal_user_cancel(&ashpd::Error::Portal(ashpd::PortalError::NotAllowed("x".into()))));
            assert!(!is_portal_user_cancel(&ashpd::Error::NoResponse));
            // AccessDenied for some *other* reason is a real denial.
            let other = zbus::fdo::Error::AccessDenied("not on the allow-list".to_string());
            assert!(!is_portal_user_cancel(&ashpd::Error::Portal(ashpd::PortalError::ZBus(zbus::Error::FDO(
                Box::new(other)
            )))));
        }

        #[test]
        fn a_cancel_is_recognised_through_anyhow_context() {
            let e = anyhow::Error::new(PickerCancelled).context("starting capture");
            assert!(is_picker_cancelled(&e));
            assert!(!is_picker_cancelled(&anyhow!("boom")));
        }
    }

    /// **The idle-monitor acceptance test**, against the real portal and a real
    /// relay: share a monitor on which nothing is repainting, and prove a
    /// viewer gets a frame and keeps getting them.
    ///
    /// What it exercises: `capture::start_sharing` must not fail while the
    /// monitor is idle (the compositor delivers nothing until something on it
    /// changes — see [`branch::wait_for_source_frame`]), must come up the
    /// moment something changes, and must then hold a steady rate on
    /// `keepalive-time` alone.
    ///
    /// The "something changes" is a pointer movement onto the picked monitor
    /// (`ydotool`, after `E2E_NUDGE_AFTER_SECS`, default 7 — past both 5s IDR
    /// deadlines the old code burned), restored afterwards via `mmsg get
    /// cursorpos`. Set `E2E_NUDGE_AFTER_SECS=off` to skip it and watch the
    /// share wait. `E2E_NUDGE_X`/`E2E_NUDGE_Y` are the global coordinates to
    /// move onto (default: DP-1's centre on this machine).
    /// `SSS_LIVE_SECONDS` (default 60) is how long the viewer is observed.
    ///
    /// Run via the picker-driving script, never unattended: it opens a portal
    /// dialog, and needs `SSSHARE_SERVER_BIN` pointing at a relay binary.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "opens a real portal picker, moves the pointer, and spawns a real relay"]
    async fn real_portal_idle_monitor_share_reaches_a_viewer_and_stays_alive() {
        use crate::end_to_end_test::{connect_with_retry, pick_free_udp_port, spawn_relay_server};
        use crate::net::ClientEvent;
        use crate::protocol::RoomId;

        const STREAM_ID: u8 = 1;
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();

        let port = pick_free_udp_port().expect("port");
        let _server = spawn_relay_server(port).expect("relay");
        let join = crate::net::format_join_string(
            &format!("127.0.0.1:{port}"),
            &rand::random::<RoomId>(),
            &rand::random::<[u8; 32]>(),
        );
        let (sharer, _s, mut events_a) = connect_with_retry(&join, "sharer").await.expect("sharer joins");
        let (viewer, _s, mut events_b) = connect_with_retry(&join, "viewer").await.expect("viewer joins");
        let owner = sharer.conn_id();

        // The nudge: pointer onto the monitor, then back where it was.
        let nudge_after = std::env::var("E2E_NUDGE_AFTER_SECS").unwrap_or_else(|_| "7".into());
        let nudge = (nudge_after != "off").then(|| nudge_after.parse::<u64>().expect("E2E_NUDGE_AFTER_SECS"));
        let nudge_task = tokio::spawn(async move {
            let Some(after) = nudge else { return None };
            tokio::time::sleep(Duration::from_secs(after)).await;
            let sh = |cmd: &str| {
                let out = std::process::Command::new("sh").arg("-c").arg(cmd).output().unwrap();
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            };
            let before: serde_json::Value = serde_json::from_str(&sh("mmsg get cursorpos")).expect("cursorpos");
            let x: i64 = std::env::var("E2E_NUDGE_X").ok().and_then(|v| v.parse().ok()).unwrap_or(2880);
            let y: i64 = std::env::var("E2E_NUDGE_Y").ok().and_then(|v| v.parse().ok()).unwrap_or(1620);
            // ydotool's absolute y is offset by the layout's -1080 on this machine.
            sh(&format!("ydotool mousemove --absolute -x {x} -y {}", y - 1080));
            let nudged_at = Instant::now();
            tokio::time::sleep(Duration::from_millis(400)).await;
            sh(&format!(
                "ydotool mousemove --absolute -x {} -y {}",
                before["x"].as_i64().unwrap(),
                before["y"].as_i64().unwrap() - 1080
            ));
            Some(nudged_at)
        });

        let begun = Instant::now();
        eprintln!("[idle] t=0 calling start_sharing (real portal)");
        let handle = match start_sharing(&sharer, STREAM_ID, DestKey(0), 8_000, 30).await {
            Ok(h) => h,
            Err(e) => panic!("sharing an idle monitor must not fail: {e:#}"),
        };
        let came_up = begun.elapsed();
        eprintln!("[idle] share came up after {came_up:?}");
        let nudged_at = nudge_task.await.expect("nudge task");
        if let Some(nudged_at) = nudged_at {
            eprintln!("[idle] pointer nudge was at t={:?}", nudged_at.duration_since(begun));
        }

        viewer.subscribe(owner, STREAM_ID).await.expect("viewer subscribe");
        let seconds: u64 = std::env::var("SSS_LIVE_SECONDS").ok().and_then(|v| v.parse().ok()).unwrap_or(60);
        let watch_start = Instant::now();
        let mut decoder = crate::decode::StreamDecoder::new(STREAM_ID).expect("decoder");
        let (mut frames, mut window_frames, mut decoded) = (0u32, 0u32, 0u32);
        let mut first_frame_at: Option<Duration> = None;
        let mut first_decoded_at: Option<Duration> = None;
        let mut window_start = Instant::now();
        let mut min_window_fps = f64::MAX;
        let deadline = tokio::time::sleep(Duration::from_secs(seconds));
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => break,
                ev = events_a.recv() => match ev {
                    Some(ClientEvent::SubscriberCount { stream_id, count }) if stream_id == STREAM_ID => {
                        handle.notify_subscriber_count(DestKey(0), count);
                    }
                    Some(ClientEvent::KeyframeRequest { stream_id }) if stream_id == STREAM_ID => {
                        handle.notify_keyframe_request(DestKey(0));
                    }
                    Some(_) => {}
                    None => break,
                },
                ev = events_b.recv() => match ev {
                    Some(ClientEvent::MediaFrame(frame)) if frame.conn_id == owner && frame.stream_id == STREAM_ID => {
                        frames += 1;
                        window_frames += 1;
                        first_frame_at.get_or_insert(watch_start.elapsed());
                        // Decode only until the first picture: a debug-build
                        // 1080p decode + colour conversion per frame is slower
                        // than the stream and would make *this loop* the
                        // bottleneck, reporting a rate the network never had.
                        if first_decoded_at.is_none() {
                            decoder.feed(&frame);
                            if decoder.latest_frame().is_some() {
                                decoded += 1;
                                first_decoded_at = Some(watch_start.elapsed());
                            }
                        }
                    }
                    Some(_) => {}
                    None => break,
                },
            }
            if window_start.elapsed() >= Duration::from_secs(5) {
                let fps = window_frames as f64 / window_start.elapsed().as_secs_f64();
                eprintln!("[idle] t={:?} viewer media fps over the last 5s: {fps:.1}", watch_start.elapsed());
                // The first window includes the subscribe/keyframe round trip.
                if watch_start.elapsed() > Duration::from_secs(6) {
                    min_window_fps = min_window_fps.min(fps);
                }
                window_frames = 0;
                window_start = Instant::now();
            }
        }
        eprintln!(
            "[idle] viewer: {frames} media frames, {decoded} decoded; first frame {first_frame_at:?} and first \
             decoded frame {first_decoded_at:?} after subscribing; slowest 5s window {min_window_fps:.1} fps; \
             sharer encoder fps {}",
            handle.encoder_fps(DestKey(0))
        );
        handle.stop().await.expect("the share should stop cleanly");
        drop(viewer);

        assert!(first_decoded_at.is_some_and(|t| t < Duration::from_secs(5)), "viewer decoded nothing promptly: {first_decoded_at:?}");
        if seconds > 12 {
            assert!(min_window_fps >= 10.0, "the share did not stay alive on a static screen: {min_window_fps:.1} fps");
        }
    }

    /// Exercises the real xdg-desktop-portal `ScreenCast` flow end to end,
    /// including the interactive permission dialog — not runnable
    /// unattended/in CI, hence `#[ignore]`. Run manually with:
    /// `cargo test capture::tests::live_portal_probe -- --ignored --nocapture`
    /// and click "Share" in the dialog that appears; a `node_id` printed to
    /// stderr means the whole portal handshake (session creation, source
    /// selection, start, PipeWire remote fd) worked.
    #[cfg(unix)]
    #[tokio::test]
    #[ignore]
    async fn live_portal_probe() {
        match open_portal_capture().await {
            Ok(p) => eprintln!("portal succeeded: node_id={}", p.node_id),
            Err(e) => eprintln!("portal failed: {e:#}"),
        }
    }

    /// LIVE end to end for a rotated monitor: real portal -> encoder -> relay
    /// -> a viewer's native decoder. Writes the first picture the viewer
    /// decodes to `$SSS_DUMP_PPM` (binary PPM) so it can be compared with
    /// `grim -o <output>`. Needs a relay binary (`SSSHARE_SERVER_BIN`) and a
    /// human or script to drive the portal picker.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "opens a real portal picker and spawns a real relay"]
    async fn real_portal_viewer_frame_dump() {
        use crate::end_to_end_test::{connect_with_retry, pick_free_udp_port, spawn_relay_server};
        use crate::net::ClientEvent;
        use crate::protocol::RoomId;

        const STREAM_ID: u8 = 1;
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        let port = pick_free_udp_port().expect("port");
        let _server = spawn_relay_server(port).expect("relay");
        let join = crate::net::format_join_string(
            &format!("127.0.0.1:{port}"),
            &rand::random::<RoomId>(),
            &rand::random::<[u8; 32]>(),
        );
        let (sharer, _s, mut events_a) = connect_with_retry(&join, "sharer").await.expect("sharer joins");
        let (viewer, _s, mut events_b) = connect_with_retry(&join, "viewer").await.expect("viewer joins");
        let owner = sharer.conn_id();
        let handle = start_sharing(&sharer, STREAM_ID, DestKey(0), 8_000, 30).await.expect("share starts");
        viewer.subscribe(owner, STREAM_ID).await.expect("viewer subscribe");

        let mut decoder = crate::decode::StreamDecoder::new(STREAM_ID).expect("decoder");
        let deadline = tokio::time::sleep(Duration::from_secs(20));
        tokio::pin!(deadline);
        let mut picture = None;
        while picture.is_none() {
            tokio::select! {
                _ = &mut deadline => break,
                ev = events_a.recv() => match ev {
                    Some(ClientEvent::SubscriberCount { stream_id, count }) if stream_id == STREAM_ID => {
                        handle.notify_subscriber_count(DestKey(0), count);
                    }
                    Some(ClientEvent::KeyframeRequest { stream_id }) if stream_id == STREAM_ID => {
                        handle.notify_keyframe_request(DestKey(0));
                    }
                    Some(_) => {}
                    None => break,
                },
                ev = events_b.recv() => match ev {
                    Some(ClientEvent::MediaFrame(frame)) if frame.conn_id == owner && frame.stream_id == STREAM_ID => {
                        decoder.feed(&frame);
                        picture = decoder.latest_frame();
                    }
                    Some(_) => {}
                    None => break,
                },
            }
        }
        handle.stop().await.expect("the share should stop cleanly");
        drop(viewer);
        let picture = picture.expect("the viewer decoded a picture");
        let [w, h] = picture.size;
        eprintln!("[viewer] decoded picture {w}x{h}");
        if let Ok(path) = std::env::var("SSS_DUMP_PPM") {
            let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
            for px in &picture.pixels {
                out.extend_from_slice(&[px.r(), px.g(), px.b()]);
            }
            std::fs::write(&path, out).expect("write ppm");
            eprintln!("[viewer] wrote {path}");
        }
    }

    /// After a reconnect the session hands the capture a *new* client for the
    /// rebuilt destination; the room-preview task must move over to it instead
    /// of failing forever on the dead connection
    /// ([`CaptureHandle::ensure_preview_task`]).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "spawns a real relay server (SSSHARE_SERVER_BIN)"]
    async fn preview_task_moves_to_the_new_client_after_a_reconnect() {
        use crate::end_to_end_test::{connect_with_retry, pick_free_udp_port, spawn_relay_server};
        use crate::protocol::RoomId;

        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        let port = pick_free_udp_port().expect("port");
        let _server = spawn_relay_server(port).expect("relay");
        let join = crate::net::format_join_string(
            &format!("127.0.0.1:{port}"),
            &rand::random::<RoomId>(),
            &rand::random::<[u8; 32]>(),
        );
        let (first, _s, _e1) = connect_with_retry(&join, "sharer").await.expect("sharer joins");
        let mut handle = start_sharing_from_test_source(
            &first,
            1,
            DEFAULT_BITRATE_KBPS,
            DEFAULT_FRAMERATE,
            "videotestsrc is-live=true ! video/x-raw,width=320,height=180",
        )
        .await
        .expect("share starts");
        assert_eq!(handle.preview_client.as_ref().map(|c| c.conn_id()), Some(first.conn_id()));

        // Same client again (e.g. a second room on a live connection): untouched.
        let params = handle.destination_runtime(DestKey(0)).expect("runtime").lock().unwrap().params();
        let same = handle
            .spawn_add_destination(&first, 2, DestKey(1), params.clone())
            .await
            .expect("channel")
            .expect("destination");
        handle.register_destination(Some(&first), same);
        assert_eq!(handle.preview_client.as_ref().map(|c| c.conn_id()), Some(first.conn_id()));

        // The connection dies; a new client takes over.
        first.connection().close(0u32.into(), b"test drop");
        let (second, _s, _e2) = connect_with_retry(&join, "sharer").await.expect("sharer rejoins");
        assert_ne!(first.conn_id(), second.conn_id());
        let rebuilt = handle
            .spawn_add_destination(&second, 3, DestKey(2), params)
            .await
            .expect("channel")
            .expect("destination");
        handle.register_destination(Some(&second), rebuilt);
        assert_eq!(
            handle.preview_client.as_ref().map(|c| c.conn_id()),
            Some(second.conn_id()),
            "the preview task must be restarted on the new client"
        );
        assert!(handle.preview_handle.is_some());
        handle.stop().await.expect("stops");
    }

    // ----- orientation stage ([`orientation_fragment`]) -----

    /// An RGB picture as rows of pixels, for the orientation tests.
    type Pic = Vec<Vec<[u8; 3]>>;

    fn transpose(p: &Pic) -> Pic {
        (0..p[0].len()).map(|x| p.iter().map(|row| row[x]).collect()).collect()
    }
    fn flip_h(p: &Pic) -> Pic {
        p.iter().map(|row| row.iter().rev().copied().collect()).collect()
    }
    fn flip_v(p: &Pic) -> Pic {
        p.iter().rev().cloned().collect()
    }

    /// How an `image-orientation` tag value must change the picture to make it
    /// upright: the map [`orientation_fragment`]'s doc comment says this crate
    /// deliberately does not own. It lives here only to pin down what
    /// `video-direction=auto` does with each of the eight values `pipewiresrc`
    /// can send (one per `wl_output` transform). `rotate-N` turns the picture N
    /// degrees clockwise. `flip-rotate-N` is EXIF's mirrored orientations.
    fn expected_upright(tag: &str, p: &Pic) -> Pic {
        let cw = |p: &Pic| flip_h(&transpose(p));
        match tag {
            "rotate-0" => p.clone(),
            "rotate-90" => cw(p),
            "rotate-180" => flip_v(&flip_h(p)),
            "rotate-270" => flip_v(&transpose(p)),
            "flip-rotate-0" => flip_h(p),
            "flip-rotate-90" => flip_v(&flip_h(&transpose(p))),
            "flip-rotate-180" => flip_v(p),
            "flip-rotate-270" => transpose(p),
            other => panic!("unknown orientation tag {other}"),
        }
    }

    fn pull_rgb(sink: &gst_app::AppSink) -> Pic {
        pull_rgb_sample(&sink.try_pull_sample(gst::ClockTime::from_seconds(10)).expect("a preview frame"))
    }

    fn pull_rgb_sample(sample: &gst::Sample) -> Pic {
        let caps = sample.caps().expect("caps");
        let info = gst_video::VideoInfo::from_caps(caps).expect("video info");
        let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(sample.buffer().expect("buffer"), &info)
            .expect("map frame");
        let (w, h) = (frame.width() as usize, frame.height() as usize);
        let stride = frame.plane_stride()[0] as usize;
        let data = frame.plane_data(0).expect("plane");
        (0..h).map(|y| (0..w).map(|x| [data[y * stride + 3 * x], data[y * stride + 3 * x + 1], data[y * stride + 3 * x + 2]]).collect()).collect()
    }

    /// A 96x48 picture with no mirror or rotation symmetry (an "F" in the top
    /// left, a block bottom right, a dot top right), as a looping source
    /// fragment. A JPEG so no extra decoder crate is needed; shapes are
    /// multiples of 16 px so chroma subsampling cannot smear them.
    fn asymmetric_test_source() -> String {
        let (w, h) = (96u32, 48u32);
        let mut rgb = vec![0u8; (w * h * 3) as usize];
        let mut fill = |x0: u32, y0: u32, x1: u32, y1: u32, c: [u8; 3]| {
            for y in y0..y1 {
                for x in x0..x1 {
                    rgb[((y * w + x) * 3) as usize..][..3].copy_from_slice(&c);
                }
            }
        };
        fill(0, 0, 16, 48, [255, 0, 0]);
        fill(0, 0, 48, 16, [255, 0, 0]);
        fill(0, 16, 32, 32, [255, 0, 0]);
        fill(64, 32, 96, 48, [0, 255, 0]);
        fill(80, 0, 96, 16, [0, 0, 255]);
        let path = std::env::temp_dir().join(format!("sss-orientation-test-{}.jpg", std::process::id()));
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 100)
            .encode(&rgb, w, h, image::ExtendedColorType::Rgb8)
            .expect("encode jpeg");
        std::fs::write(&path, jpeg).expect("write jpeg");
        format!(
            // Forward slashes: in gst-launch syntax a backslash is an escape, which
            // mangled `C:\Users\...` into a path that does not exist on Windows.
            "filesrc location={} ! jpegdec ! imagefreeze num-buffers=30 ! video/x-raw,framerate=30/1",
            path.display().to_string().replace('\\', "/")
        )
    }

    /// Runs the capture pipeline over a test pattern carrying `tag` (what
    /// `pipewiresrc` sends for a rotated output) and returns the frame the
    /// preview branch sees, i.e. what every branch hanging off the `tee` gets.
    fn frame_through_orientation_stage(orientation: &str, tag: &str) -> Pic {
        let source = format!("{} ! taginject tags=image-orientation={tag}", asymmetric_test_source());
        let built = build_pipeline_with_orientation(&source, orientation).expect("pipeline builds");
        let elements_before = built.pipeline.children().len();
        built.pipeline.set_state(gst::State::Playing).expect("playing");
        let pic = pull_rgb(&built.preview_sink);
        let spliced = built.pipeline.children().len() - elements_before;
        built.pipeline.set_state(gst::State::Null).expect("null");
        assert_eq!(spliced, usize::from(tag != "rotate-0"), "{orientation}: stage spliced in iff the tag says rotated ({tag})");
        pic
    }

    fn mean_abs_diff(a: &Pic, b: &Pic) -> f64 {
        if a.len() != b.len() || a[0].len() != b[0].len() {
            return f64::MAX;
        }
        let mut sum = 0u64;
        for (ra, rb) in a.iter().zip(b) {
            for (pa, pb) in ra.iter().zip(rb) {
                for c in 0..3 {
                    sum += (pa[c] as i32 - pb[c] as i32).unsigned_abs() as u64;
                }
            }
        }
        sum as f64 / (a.len() * a[0].len() * 3) as f64
    }

    const ORIENTATION_TAGS: [&str; 8] = [
        "rotate-0",
        "rotate-90",
        "rotate-180",
        "rotate-270",
        "flip-rotate-0",
        "flip-rotate-90",
        "flip-rotate-180",
        "flip-rotate-270",
    ];

    /// The `image-orientation` tag `pipewiresrc` sends for a rotated output
    /// must come out upright for all eight transforms, with width and height
    /// swapped for the 90/270 family, through both implementations of the
    /// stage. `vapostproc` round-trips through a YUV format, so frames are
    /// compared by nearest match (smaller mean error than every other
    /// transform) instead of bit-exactly.
    fn assert_orientation_stage_uprights_every_transform(orientation: &str) {
        if gst::init().is_err() {
            eprintln!("skipping: GStreamer unavailable");
            return;
        }
        let first = orientation.split_whitespace().next().unwrap();
        if gst::ElementFactory::find(first).is_none() {
            eprintln!("skipping: {first} is not installed");
            return;
        }
        let reference = frame_through_orientation_stage(orientation, "rotate-0");
        assert_eq!((reference[0].len(), reference.len()), (96, 48), "rotate-0 must not change the size");
        let all: Vec<Pic> = ORIENTATION_TAGS.iter().map(|t| expected_upright(t, &reference)).collect();
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert!(mean_abs_diff(a, b) > 10.0, "the test pattern must tell the eight orientations apart");
            }
        }
        for (tag, expected) in ORIENTATION_TAGS.iter().zip(&all) {
            let got = frame_through_orientation_stage(orientation, tag);
            let (ew, eh) = (expected[0].len(), expected.len());
            assert_eq!((got[0].len(), got.len()), (ew, eh), "{orientation}: {tag} must {} width and height", if ew == 48 { "swap" } else { "keep" });
            let best = ORIENTATION_TAGS
                .iter()
                .zip(&all)
                .map(|(t, e)| (mean_abs_diff(&got, e), *t))
                .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap())
                .unwrap();
            assert_eq!(best.1, *tag, "{orientation}: tag {tag} produced a picture that looks like {} (err {:.1})", best.1, best.0);
        }
    }

    #[test]
    fn videoflip_orientation_stage_uprights_every_transform() {
        assert_orientation_stage_uprights_every_transform(ORIENTATION_VIDEOFLIP);
    }

    #[test]
    fn vapostproc_orientation_stage_uprights_every_transform() {
        // Needs a working VA device, exactly like the stage's own selection.
        if detect_encoder_backend() != EncoderBackend::Vaapi {
            eprintln!("skipping: no VA device");
            return;
        }
        assert_orientation_stage_uprights_every_transform(ORIENTATION_VAPOSTPROC);
    }

    /// An un-rotated output (no tag at all, which is what a compositor with a
    /// normal transform produces) must pass through the stage untouched and
    /// keep its size.
    #[test]
    fn untagged_source_passes_through_the_orientation_stage_unchanged() {
        if gst::init().is_err() {
            return;
        }
        for orientation in [ORIENTATION_VIDEOFLIP, ORIENTATION_VAPOSTPROC] {
            if gst::ElementFactory::find(orientation.split_whitespace().next().unwrap()).is_none() {
                continue;
            }
            if orientation == ORIENTATION_VAPOSTPROC && detect_encoder_backend() != EncoderBackend::Vaapi {
                continue;
            }
            let built = build_pipeline_with_orientation(&asymmetric_test_source(), orientation).expect("pipeline builds");
            let elements_before = built.pipeline.children().len();
            built.pipeline.set_state(gst::State::Playing).expect("playing");
            let pic = pull_rgb(&built.preview_sink);
            // Nothing was spliced in: an un-rotated source pays nothing.
            assert_eq!(built.pipeline.children().len(), elements_before, "{orientation}: no stage without a rotation tag");
            built.pipeline.set_state(gst::State::Null).expect("null");
            let tagged = frame_through_orientation_stage(orientation, "rotate-0");
            assert_eq!((pic[0].len(), pic.len()), (96, 48));
            assert!(mean_abs_diff(&pic, &tagged) < 1.0, "{orientation}: untagged and rotate-0 must agree");
        }
    }

    /// LIVE: shares one monitor through the real portal and writes the frame
    /// the `tee` carries to `$SSS_DUMP_PPM` (a binary PPM), logging every
    /// orientation tag `pipewiresrc` sends. This is the check that a rotated
    /// output really arrives upright: compare the dump with `grim -o <output>`.
    /// Run: `SSS_DUMP_PPM=/path/frame.ppm cargo test
    /// capture::tests::live_portal_frame_dump -- --ignored --nocapture`.
    #[cfg(unix)]
    #[tokio::test]
    #[ignore]
    async fn live_portal_frame_dump() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        ensure_gstreamer_initialized().expect("gstreamer init");
        let source = CaptureSource::Portal(open_portal_capture().await.expect("portal session"));
        let fragment = source.launch_fragment(DEFAULT_FRAMERATE);
        // `SSS_ORIENT=identity|videoflip|vapostproc` swaps the stage for A/B
        // cost measurements on the same rotated output; default = production.
        let stage = match std::env::var("SSS_ORIENT").as_deref() {
            Ok("identity") => "identity",
            Ok("videoflip") => ORIENTATION_VIDEOFLIP,
            Ok("vapostproc") => ORIENTATION_VAPOSTPROC,
            _ => orientation_fragment(),
        };
        let built = build_pipeline_with_orientation(&fragment, stage).expect("capture pipeline builds");
        built.pipeline.set_state(gst::State::Playing).expect("playing");

        // Log every orientation tag that reaches the tee, and what the stage chose.
        if let Some(pad) = built.tee.static_pad("sink") {
            pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, |_, info| {
                if let Some(gst::PadProbeData::Event(ev)) = &info.data
                    && let gst::EventView::Tag(t) = ev.view()
                    && let Some(o) = t.tag().get::<gst::tags::ImageOrientation>()
                {
                    eprintln!("[dump] image-orientation tag at the tee: {}", o.get());
                }
                gst::PadProbeReturn::Ok
            });
        }
        let (w, h) = {
            let mut tries = 0;
            loop {
                if let Some(s) = negotiated_size(&built.tee) {
                    break s;
                }
                tries += 1;
                assert!(tries < 100, "no caps at the tee");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        };
        eprintln!("[dump] tee size {w}x{h}; stage = {stage}");

        // Take the *last* frame within a couple of seconds so that a tag sent
        // with the first buffers has long since been applied.
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut pic = None;
        while Instant::now() < deadline {
            if let Some(s) = built.preview_sink.try_pull_sample(gst::ClockTime::from_mseconds(200)) {
                pic = Some(pull_rgb_sample(&s));
            }
        }
        let pic = pic.expect("a frame from the portal");
        let (ph, pw) = (pic.len(), pic[0].len());
        eprintln!("[dump] frame {pw}x{ph}");
        if let Ok(path) = std::env::var("SSS_DUMP_PPM") {
            let mut out = format!("P6\n{pw} {ph}\n255\n").into_bytes();
            for row in &pic {
                for px in row {
                    out.extend_from_slice(px);
                }
            }
            std::fs::write(&path, out).expect("write ppm");
            eprintln!("[dump] wrote {path}");
        }
        if let Ok(secs) = std::env::var("SSS_RATE_SECS") {
            let secs: u64 = secs.parse().unwrap_or(5);
            eprintln!(
                "[dump] raw frames off the tee with stage '{stage}': {:.1} fps",
                branch::tests::tap_tee_rate(&built.pipeline, &built.tee, Duration::from_secs(secs))
            );
        }
        built.pipeline.set_state(gst::State::Null).expect("null");
    }
}
