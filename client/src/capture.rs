//! Screen capture (Linux: xdg-desktop-portal `ScreenCast` + PipeWire;
//! Windows: GStreamer's `d3d11screencapturesrc`) → H.264 encode (GStreamer) →
//! AEAD-encrypt → fragment → send, over the QUIC media-datagram plane wired
//! up in `net.rs`.
//!
//! # Pipeline
//!
//! ```text
//! <platform source> ! videoconvert ! tee name=t
//!   t. ! queue ! valve name=encoder_valve drop=false drop-mode=forward-sticky-events
//!      ! videorate drop-only=false max-duplication-time=0 ! video/x-raw,format=I420,framerate=<fps>/1
//!      ! x264enc tune=zerolatency bitrate=<kbps> key-int-max=<gop> speed-preset=ultrafast
//!      ! video/x-h264,stream-format=byte-stream,alignment=au
//!      ! appsink name=sink
//!   t. ! queue leaky=downstream max-size-buffers=1 ! videorate ! video/x-raw,framerate=1/1
//!      ! videoconvert ! video/x-raw,format=RGB
//!      ! appsink name=preview_sink
//!   t. ! queue leaky=downstream max-size-buffers=1 ! videorate ! video/x-raw,framerate=12/1
//!      ! videoconvert ! video/x-raw,format=RGB
//!      ! appsink name=self_preview_sink
//! ```
//!
//! The `valve` gates only the encoder branch: `pump_loop` toggles its
//! `drop` property in response to `ClientEvent::SubscriberCount` (pausing
//! at 0 subscribers, resuming — with a forced keyframe — on the next
//! subscriber) without touching the preview `tee` branch or the pipeline's
//! `Playing` state. See [`CaptureControlEvent`]/[`force_keyframe`].
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
//! The `tee` feeds a second, low-rate branch whose RGB8 samples back
//! [`preview::spawn_preview_task`] (see [`start_sharing`]) — the
//! already-implemented periodic thumbnail/preview sender in `preview.rs`,
//! which itself has zero GStreamer dependency and only samples the latest
//! frame off whatever [`preview::FrameStream`] it's handed every 30s, so
//! capping the tee branch to ~1fps here is purely a CPU-saving measure, not
//! something the preview task requires.
//!
//! A *third* tee branch (`self_preview_sink`, [`SELF_PREVIEW_BRANCH_FRAMERATE`])
//! feeds the sender-side self-preview shown in `ui.rs`'s share panel — a
//! small live view of what's actually being captured, so the person sharing
//! can confirm what viewers see without waiting on the (much slower, JPEG-
//! encoded) 30s thumbnail. It is deliberately its own branch rather than a
//! second consumer of `preview_sink`'s samples.
//!
//! Why: `preview_sink` is capped to ~1fps purely because
//! [`preview::spawn_preview_task`] only ever samples the latest frame every
//! 30s — bumping that branch's own `videorate` cap to feed a smooth live
//! preview would also mean paying for `videoconvert`/appsink work at that
//! higher rate on every frame the JPEG-thumbnail path *doesn't* need, and
//! would couple the two features' cadences together for no reason. A
//! dedicated branch keeps the JPEG-thumbnail path's 1fps and the live
//! self-preview's [`SELF_PREVIEW_BRANCH_FRAMERATE`] fully independent —
//! changing one's rate can never accidentally affect the other — at the cost
//! of one extra `queue`/`videorate`/`videoconvert`/`appsink` chain off the
//! tee, which is cheap next to the encoder branch. See
//! `superdoc/features/self-preview.md`.
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
//! The two halves are joined by [`branch::swap_to_new_branch`] (tier 2),
//! [`branch::stall_all`] and [`branch::rebuild_after_source_restart`]
//! (tier 3). [`start_sharing`] already creates one
//! [`destination::DestinationRuntime`] and [`pump_loop`] drives it for real
//! (wire PTS, IDR caching, first-IDR `Starting` → `Live`, the stall
//! keepalive); what a later stage still owns is replacing this pipeline's
//! single *fixed* encoder branch with one dynamic branch per destination, so
//! one [`CaptureHandle`] carries N of them.
//!
//! See `superdoc/architecture/overview.md`'s "Per-destination lifecycle"
//! section for the full model.

#[cfg(unix)]
use std::os::fd::{AsRawFd, OwnedFd};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
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
///
/// On Windows this naturally always resolves to [`EncoderBackend::Software`]:
/// [`probe_vaapi_encoder`]'s `vah264enc` element simply isn't installed
/// there, so the same "construct the element, fail gracefully" probe below
/// does the right thing with no `#[cfg]` needed. Windows hardware encode via
/// Media Foundation's `mfvideoenc` (the natural Windows analogue of VAAPI
/// here) is a deliberate fast-follow, not an oversight — same staging as
/// VAAPI itself was after software-only worked first on Linux.
fn detect_encoder_backend() -> EncoderBackend {
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

/// Default encoder-branch framerate cap, preselected in `ui.rs`'s framerate
/// dropdown. Deliberately conservative (matches this pipeline's previous
/// hardcoded `CAPTURE_FRAMERATE` value) rather than jumping straight to the
/// spec's 240Hz ceiling ([`MAX_FRAMERATE`]): a higher framerate multiplies
/// both the encoder's CPU cost and (per this module's doc comment on
/// `build_pipeline`'s `videorate` cap) how finely the same configured
/// bitrate gets sliced, so a cautious default — especially so soon after the
/// bitrate-starvation bug this same knob was originally added to fix (see
/// this module's doc comment) — is safer than maximizing it by default.
/// Users who want up to 240fps (desktop-only per the original design; the
/// browser client can stay lower) can select it explicitly.
pub const DEFAULT_FRAMERATE: u32 = 30;

/// Hard ceiling on the user-selectable capture framerate, per the original
/// project design's "desktop clients support up to 240Hz streaming"
/// requirement — the browser client isn't held to this, but this is the
/// native desktop client. [`build_pipeline`] clamps whatever `framerate` it's
/// given into `MIN_FRAMERATE..=MAX_FRAMERATE` as a defensive belt-and-
/// suspenders measure (`ui.rs`'s dropdown already only offers values in
/// range), so a bad/malicious caller can never hand this a `framerate` of 0
/// (which would make `key-int-max` 0, i.e. "never insert a keyframe past the
/// first") or something absurdly large.
pub const MAX_FRAMERATE: u32 = 240;

/// See [`MAX_FRAMERATE`].
pub const MIN_FRAMERATE: u32 = 1;

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
    #[cfg(test)]
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
                // a new buffer when something visually changed on screen, as
                // a bandwidth/CPU-saving optimization at the compositor
                // level. When the desktop is static, `pipewiresrc` pushes
                // *nothing at all*, and the `videorate` cap further
                // downstream (see [`build_pipeline`]) cannot invent frames
                // out of nothing — it only fills gaps *between* buffers it
                // actually receives, by duplicating on receipt of the next
                // one. With no next buffer ever arriving, capture output
                // (and therefore the stream sent to the viewer) simply stops
                // until the screen changes again, at which point everything
                // that piled up since the last real frame gets flushed
                // through in a burst — this is the reported "sender fps
                // looks constant but the viewer's fluctuates, and right
                // after a change the image is frozen and artifacted" bug:
                // the encoder's rate control was still converging from the
                // last burst of motion when frame production stopped dead,
                // so the blockiest frame from that burst is what stays on
                // screen indefinitely.
                //
                // `pipewiresrc`'s `keepalive-time` (ms, 0 = disabled per
                // `gst-inspect-1.0 pipewiresrc`) makes the element itself
                // periodically resend its last buffer even when the
                // compositor delivers nothing new, so there is always a
                // fresh buffer for `videorate` to work with at (at least) the
                // selected framerate — turning "zero frames during an idle
                // gap of arbitrary length" into "the last frame duplicated on
                // a steady clock", which is exactly the "fully static
                // framerate, always the selected one" behavor asked for.
                // Set from the *selected* `framerate` (floor of the frame
                // interval in ms, so this fires at least as often as the
                // target rate — `videorate`'s own `framerate={framerate}/1`
                // caps below normalize the exact rate regardless) rather
                // than a fixed constant, so raising/lowering the selected
                // framerate keeps this in lockstep. Never 0 for any
                // in-range `framerate` (`MIN_FRAMERATE..=MAX_FRAMERATE` is
                // `1..=240`, and `1000 / 240 == 4`), so this can never
                // accidentally disable the keepalive.
                let keepalive_ms = (1000 / framerate.clamp(MIN_FRAMERATE, MAX_FRAMERATE)).max(1);
                format!(
                    "pipewiresrc fd={} path={} do-timestamp=true keepalive-time={keepalive_ms}",
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
            #[cfg(test)]
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

/// Preview branch is capped to this framerate off the tee (see this
/// module's doc comment) — [`preview::spawn_preview_task`] only ever samples
/// the latest frame every 30s, so anything above "occasionally" here is
/// wasted `videoconvert`/appsink work.
const PREVIEW_BRANCH_FRAMERATE: u32 = 1;

/// Self-preview branch's framerate cap off the tee (see this module's doc
/// comment and `superdoc/features/self-preview.md`). Chosen to look smooth
/// enough for a small "is this really what I'm sharing" live view without
/// paying anywhere near the encoder branch's CPU/`videoconvert` cost: a
/// self-preview thumbnail doesn't need to track the full capture framerate
/// (up to [`MAX_FRAMERATE`]'s 240) to be useful, and this is comfortably
/// above the ~10-15fps a small preview panel needs to read as live rather
/// than a slideshow.
const SELF_PREVIEW_BRANCH_FRAMERATE: u32 = 12;

/// Element name of the tee's third branch — the live sender-side
/// self-preview appsink (see this module's doc comment and
/// [`SELF_PREVIEW_BRANCH_FRAMERATE`]), distinct from `preview_sink`'s
/// existing JPEG-thumbnail branch.
const SELF_PREVIEW_SINK_NAME: &str = "self_preview_sink";

/// Name of the `valve` element gating the encoder branch (between the
/// tee's `queue` and the raw-format capsfilter feeding the encoder) — see
/// [`build_pipeline`]. Toggled via its `drop` property by [`pump_loop`] in
/// response to `ClientEvent::SubscriberCount { count: 0, .. }`/`> 0`, so a
/// stream can stop encoding (and burning CPU/bandwidth) while it has no
/// subscribers without touching the preview tee branch or the pipeline's
/// `Playing` state.
const ENCODER_VALVE_NAME: &str = "encoder_valve";

/// `max-size-buffers` for the encoder branch's `queue leaky=downstream`
/// right after the tee (see [`build_pipeline`]'s doc comment on that queue
/// for the full "why" — briefly: without a small bounded/leaky config here,
/// a real capture source's incoming raw-frame rate spiking under motion can
/// outpace the encoder and this queue's own defaults, blocking the whole
/// pipeline solid rather than dropping raw frames). A couple of frames'
/// worth of slack (unlike the preview branch's `max-size-buffers=1`, which
/// only ever needs the single newest frame) smooths brief scheduling jitter
/// without ever letting a real backlog accumulate.
const ENCODER_QUEUE_MAX_BUFFERS: u32 = 2;

/// Builds and links (but does not start) the capture/encode pipeline
/// described in this module's doc comment, returning the pipeline, its
/// encoded-output `appsink`, the preview branch's raw-RGB `appsink`, the
/// self-preview branch's raw-RGB `appsink` (see [`SELF_PREVIEW_SINK_NAME`]),
/// and the encoder branch's gating `valve` (see [`ENCODER_VALVE_NAME`]).
/// `source_fragment` is [`CaptureSource::launch_fragment`]'s output — the
/// only platform-specific piece of the description below.
fn build_pipeline(
    source_fragment: &str,
    bitrate_kbps: u32,
    framerate: u32,
    backend: EncoderBackend,
) -> Result<(gst::Pipeline, gst_app::AppSink, gst_app::AppSink, gst_app::AppSink, gst::Element)> {
    // Defensive clamp — see [`MAX_FRAMERATE`]'s doc comment. `ui.rs`'s
    // dropdown only ever offers values already inside this range, so this
    // is a backstop against a 0/absurd value reaching `key-int-max`, not the
    // primary validation.
    let framerate = framerate.clamp(MIN_FRAMERATE, MAX_FRAMERATE);
    let gop_frames = GOP_SECONDS * framerate;
    let encoder = backend.launch_fragment(bitrate_kbps, gop_frames);
    let raw_format = backend.raw_format();

    // `drop-mode=forward-sticky-events` (rather than the element's default
    // `drop-all`) is load-bearing, not cosmetic: GstValve's default mode
    // drops *events*, not just buffers, while `drop=true` — and per
    // `gst/gstevent.h`, `GST_EVENT_EOS` carries the `STICKY` flag, so a
    // closed valve under the default mode silently swallows EOS too.
    // `CaptureHandle::stop()` sends a real EOS event to drain the pipeline
    // cleanly (see its doc comment), and `pump_loop`'s `appsink_stream` only
    // ever yields its terminal `None` once that EOS actually reaches the
    // appsink — so stopping a stream while its encoder happens to be paused
    // (0 subscribers) would hang `stop()` for its full 5s timeout,
    // discovered live via this module's own multi-cycle integration test
    // (`end_to_end_test::repeated_pause_resume_cycles_keep_resuming`, which
    // left the encoder paused at the end of its last cycle before tearing
    // down). `forward-sticky-events` still drops ordinary (non-sticky)
    // buffers exactly as before — that's the whole point of the valve — but
    // lets EOS (and caps/segment, though those don't change mid-stream here)
    // through regardless of `drop`.
    // `queue leaky=downstream max-size-buffers={ENCODER_QUEUE_MAX_BUFFERS}`
    // right after the tee is load-bearing, not cosmetic, and fixes a
    // distinct bug from the `videorate` cap below: a plain `queue` (as this
    // was before) defaults to `leaky=no` with `max-size-bytes=10485760`
    // (10MB) — at even 1080p NV12 (~3.1MB/raw frame) that default byte cap
    // holds only ~3 raw frames (at 4K NV12, ~12.4MB/frame, a *single*
    // buffered frame already exceeds it), so the instant the downstream
    // chain (`videorate`+`{encoder}`, sharing this one streaming thread)
    // falls even slightly behind the raw capture rate — which spikes well
    // above the configured `framerate` under real sustained motion, per the
    // `videorate` doc comment below — this queue fills within a fraction of
    // a second and, being non-leaky, *blocks* rather than drops. That block
    // propagates upstream through the tee to `videoconvert` and the real
    // capture source itself: on a real portal `pipewiresrc` (push-driven by
    // the compositor against a small fixed PipeWire buffer pool, unlike a
    // self-paced `videotestsrc`), a permanently blocked downstream means our
    // side stops returning buffers to the pool, which stalls the
    // compositor's own frame delivery once that pool is exhausted — a real
    // freeze, not graceful degradation. This is exactly the live-reported
    // "works for a few seconds under light motion, then freezes solid under
    // sustained heavy motion" symptom, and matches this project's own
    // established real-time policy everywhere else in this same pipeline
    // (the preview branch's own `queue leaky=downstream max-size-buffers=1`
    // a few lines down; both appsinks' `drop=true`): prefer dropping a raw
    // frame over blocking. `max-size-buffers={ENCODER_QUEUE_MAX_BUFFERS}`
    // (rather than the preview branch's 1) keeps a little slack to smooth
    // over brief scheduling jitter without blocking, while still being far
    // below the default's multi-second worst case.
    // `videorate ! video/x-raw,...,framerate={framerate}/1` on the encoder
    // branch (mirroring the preview branch's own `videorate` a few lines
    // down) is load-bearing, not cosmetic: `<platform source>` (portal
    // `pipewiresrc`/Windows `d3d11screencapturesrc`) delivers frames at
    // whatever variable, content-driven rate the compositor/OS pushes them —
    // observed live ranging from ~1fps on an idle desktop up to the
    // monitor's full refresh rate (100+ fps) under continuous motion, with
    // nothing upstream constraining it to the user's chosen `framerate`.
    // Every encoder property that assumes a nominal fps
    // (`key-int-max={gop_frames}` for the spec's 2s GOP, and — critically —
    // `bitrate={bitrate_kbps}` itself, since both `x264enc` and `vah264enc`
    // allocate their per-frame bit budget by dividing the target bitrate
    // across the negotiated caps' framerate) silently breaks without this:
    // fed at, say, 100fps instead of the configured 30, the encoder divides
    // the same kbit/s budget across >3x as many frames, so each frame gets a
    // fraction of its intended allocation. Live-measured without this cap:
    // every delta frame landed at exactly the same ~222 bytes regardless of
    // real on-screen motion (mouse movement, window content changes) or the
    // configured bitrate (8000 kbps) — the classic signature of a starved
    // rate controller dividing a fixed budget across far more frames than
    // intended, not an actual per-frame bitrate/unit bug (`gst-inspect-1.0`
    // confirms both backends' `bitrate` properties are already correctly in
    // kbit/s). This also fixes the GOP interval: `key-int-max={gop_frames}`
    // assumes the same `framerate` value passed in here (scaled by
    // `GOP_SECONDS`, so the ~2-second keyframe interval holds regardless of
    // which framerate the user picks), which was previously only true by
    // accident whenever the source happened to deliver exactly the
    // hardcoded 30fps.
    //
    // `drop-only=false max-duplication-time=0` on that same `videorate` are
    // spelled out explicitly (rather than left at their already-matching
    // defaults — see `gst-inspect-1.0 videorate`) because they're the other
    // load-bearing half of "fully static framerate, always the selected
    // one": `drop-only=false` is what lets `videorate` duplicate the last
    // frame to *fill up to* `framerate`, not just drop excess frames down to
    // it; `max-duplication-time=0` disables the cap on how large a gap it
    // will still fill with duplicates (a non-zero value would make it give
    // up filling — and just pass frames through unfilled — once the gap
    // since the last real buffer exceeds that duration, reintroducing the
    // exact fluctuating-rate/stuck-frame bug this exists to fix). Both
    // properties only get a real workout because `keepalive-time` on the
    // Linux source fragment (see [`CaptureSource::launch_fragment`]) ensures
    // there's always a fresh buffer for `videorate` to duplicate from, even
    // when the desktop is fully static and the compositor pushes nothing —
    // without that upstream fix, `videorate` still can't invent frames out
    // of a total silence with no next buffer to trigger on. `PacingState` in
    // `decode.rs` (viewer-side display pacing) then reproduces this constant
    // cadence faithfully — it was already correct; the irregular cadence it
    // was faithfully displaying originated entirely here.
    let description = format!(
        "{source_fragment} \
         ! videoconvert \
         ! tee name=t \
         t. ! queue leaky=downstream max-size-buffers={ENCODER_QUEUE_MAX_BUFFERS} \
            ! valve name={ENCODER_VALVE_NAME} drop=false drop-mode=forward-sticky-events \
            ! videorate drop-only=false max-duplication-time=0 \
            ! video/x-raw,format={raw_format},framerate={framerate}/1 \
            ! {encoder} \
            ! video/x-h264,stream-format=byte-stream,alignment=au \
            ! appsink name=sink emit-signals=false sync=false max-buffers=4 drop=true \
         t. ! queue leaky=downstream max-size-buffers=1 \
            ! videorate \
            ! video/x-raw,framerate={PREVIEW_BRANCH_FRAMERATE}/1 \
            ! videoconvert \
            ! video/x-raw,format=RGB \
            ! appsink name=preview_sink emit-signals=false sync=false max-buffers=1 drop=true \
         t. ! queue leaky=downstream max-size-buffers=1 \
            ! videorate \
            ! video/x-raw,framerate={SELF_PREVIEW_BRANCH_FRAMERATE}/1 \
            ! videoconvert \
            ! video/x-raw,format=RGB \
            ! appsink name={SELF_PREVIEW_SINK_NAME} emit-signals=false sync=false max-buffers=1 drop=true"
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

    let preview_sink = pipeline
        .by_name("preview_sink")
        .ok_or_else(|| anyhow!("pipeline has no element named 'preview_sink'"))?
        .downcast::<gst_app::AppSink>()
        .map_err(|_| anyhow!("'preview_sink' element is not an AppSink"))?;

    let self_preview_sink = pipeline
        .by_name(SELF_PREVIEW_SINK_NAME)
        .ok_or_else(|| anyhow!("pipeline has no element named '{SELF_PREVIEW_SINK_NAME}'"))?
        .downcast::<gst_app::AppSink>()
        .map_err(|_| anyhow!("'{SELF_PREVIEW_SINK_NAME}' element is not an AppSink"))?;

    let encoder_valve = pipeline
        .by_name(ENCODER_VALVE_NAME)
        .ok_or_else(|| anyhow!("pipeline has no element named '{ENCODER_VALVE_NAME}'"))?;

    Ok((pipeline, sink, preview_sink, self_preview_sink, encoder_valve))
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
    source_fragment: &str,
    bitrate_kbps: u32,
    framerate: u32,
    backend: EncoderBackend,
) -> Result<(gst::Pipeline, gst_app::AppSink, gst_app::AppSink, gst_app::AppSink, gst::Element)> {
    let (pipeline, appsink, preview_sink, self_preview_sink, encoder_valve) =
        build_pipeline(source_fragment, bitrate_kbps, framerate, backend)?;

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

    Ok((pipeline, appsink, preview_sink, self_preview_sink, encoder_valve))
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
pub struct CaptureHandle {
    stream_id: u8,
    client: ScreenshareClient,
    pipeline: gst::Pipeline,
    capture_source: CaptureSource,
    pump_task: tokio::task::JoinHandle<()>,
    // `Option` (rather than a bare `PreviewHandle`) solely so `stop` can
    // `.take()` it out of `&mut self` to call `PreviewHandle::stop(self)`,
    // which consumes by value — required now that `CaptureHandle: Drop`
    // forbids moving fields out of `self` by value anywhere, not just in
    // `drop` itself. Always `Some` until `stop` runs; letting a `Some` drop
    // normally (bare-drop backstop path) is fine, per `PreviewHandle`'s own
    // doc comment (dropping `stop_tx` promptly stops its background task).
    preview_handle: Option<PreviewHandle>,
    // The other end of `pump_loop`'s `control_rx` (see [`CaptureControlEvent`]).
    // The caller (`ui.rs`'s main event loop) forwards `ClientEvent::SubscriberCount`/
    // `ClientEvent::KeyframeRequest` for this `stream_id` here via
    // [`CaptureHandle::notify_subscriber_count`]/[`CaptureHandle::notify_keyframe_request`].
    // Dropped by `stop`/`Drop` along with the rest of `self`, which makes
    // `control_rx.recv()` observe channel closure and stop selecting on it —
    // no explicit shutdown signal needed.
    control_tx: mpsc::UnboundedSender<CaptureControlEvent>,
    // Sharer-side live FPS readout: the encoder branch's real output rate,
    // updated by `pump_loop` (see [`ENCODER_FPS_WINDOW`]) every time an
    // encoded access unit reaches the appsink, and read back here via
    // [`CaptureHandle::encoder_fps`] — lets `ui.rs` show "is the configured
    // framerate actually being hit", not just "what was selected". An
    // `Arc<AtomicU32>` rather than a channel: this is a plain last-value
    // gauge, not an event stream, so a lock-free shared cell is simpler than
    // threading another channel through.
    encoder_fps: Arc<AtomicU32>,
    // Latest frame off the self-preview tee branch (see
    // [`SELF_PREVIEW_BRANCH_FRAMERATE`]), written by [`pump_loop`] and read
    // back (and cleared) via [`CaptureHandle::self_preview_frame`] — the same
    // "last-value slot behind a `Mutex`" shape `decode.rs`'s
    // `StreamDecoder::latest_frame` uses for the equivalent viewer-side
    // texture-upload pattern in `ui.rs`.
    self_preview_frame: Arc<Mutex<Option<egui::ColorImage>>>,
    // This capture's destination lifecycle state — the `DestinationState`
    // machine, the monotonic wire-PTS clock, and the stall protocol's cached
    // IDR (see [`destination::DestinationRuntime`]). Shared with `pump_loop`,
    // which writes it as frames flow, and read by `ui.rs` to render the
    // destination's state.
    //
    // One runtime per handle, because this stage's `start_sharing` pipeline
    // still has exactly one fixed encoder branch and therefore exactly one
    // destination. Fan-out (N branch-backed destinations off one capture)
    // holds one of these per destination and drives them with
    // [`branch::swap_to_new_branch`]/[`branch::stall_all`]/
    // [`branch::rebuild_after_source_restart`]; nothing about this type
    // changes when that lands.
    destination: destination::SharedDestination,
}

impl Drop for CaptureHandle {
    /// Synchronous backstop for when [`CaptureHandle::stop`] was never
    /// called (a panic, an app crash, or a caller that just drops the handle
    /// instead of awaiting graceful teardown). Without this, the GStreamer
    /// pipeline is left `Playing` — its element threads (`pipewiresrc`/
    /// `d3d11screencapturesrc`, `x264enc`/`vah264enc`, …) keep running
    /// indefinitely.
    ///
    /// This is a backstop, not a replacement for `stop()`'s graceful
    /// teardown: closing the ashpd portal session is an async D-Bus call and
    /// can't run in this synchronous `drop`, so on the Linux/portal path the
    /// OS recording indicator can stay lit until the OS itself notices this
    /// process's PipeWire fd close (or until `stop()` actually runs). This
    /// only guarantees the GStreamer pipeline itself stops.
    ///
    /// When `stop()` *did* run first, `self` is already fully torn down by
    /// the time this runs, so setting an already-`Null` pipeline to `Null`
    /// again is a harmless no-op (same established pattern as
    /// `AudioCaptureHandle`'s `Drop`).
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// Starts sharing this machine's screen on `stream_id`: acquires this
/// platform's capture source (Linux: runs the portal permission flow;
/// Windows: no consent flow needed, see [`acquire_capture_source`]), builds
/// and starts the capture/encode pipeline, and (once the first encoded frame
/// has actually gone out, i.e. capture is really flowing — not just
/// "pipeline exists") calls [`ScreenshareClient::announce_stream`] so other
/// room members see the stream appear.
///
/// `bitrate_kbps` is plumbed straight into the encoder (see
/// [`DEFAULT_BITRATE_KBPS`] for a reasonable default). `framerate` caps the
/// encoder branch's output rate (see [`build_pipeline`]'s doc comment) and
/// scales the GOP/keyframe interval to match — see [`DEFAULT_FRAMERATE`]/
/// [`MAX_FRAMERATE`] for the selectable range; `ui.rs`'s framerate dropdown
/// is the UI for choosing it.
///
/// Returns `Err` — never panics — if the capture source is unavailable (on
/// Linux: the portal is unreachable, or the user denies/cancels the
/// screencast permission dialog), or the GStreamer pipeline fails to
/// construct/start.
pub async fn start_sharing(
    client: &ScreenshareClient,
    stream_id: u8,
    bitrate_kbps: u32,
    framerate: u32,
) -> Result<CaptureHandle> {
    ensure_gstreamer_initialized()?;

    let capture_source = acquire_capture_source().await?;
    start_sharing_with_source(client, stream_id, bitrate_kbps, framerate, capture_source).await
}

/// TEST-ONLY seam (see [`CaptureSource::TestSource`]): starts the exact same
/// tee→encode→appsink→encrypt→fragment→send pipeline [`start_sharing`] uses
/// internally, sourced from a caller-supplied raw `gst-launch`-syntax source
/// fragment (e.g. `"videotestsrc is-live=true pattern=smpte ! \
/// video/x-raw,width=320,height=240,framerate=15/1"`) instead of the
/// interactive portal/pipewiresrc (or Windows `d3d11screencapturesrc`) flow.
///
/// Exists solely so an automated integration test can drive this module's
/// real capture → encrypt → fragment → send path end to end without a human
/// clicking the interactive Wayland portal consent dialog (which cannot be
/// automated). `#[cfg(test)]`-gated: this function does not exist at all in
/// a non-test build, so it changes nothing about [`start_sharing`]'s
/// production API or behavior.
#[cfg(test)]
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
        bitrate_kbps,
        framerate,
        CaptureSource::TestSource(source_fragment.to_string()),
    )
    .await
}

/// Shared body of [`start_sharing`] / [`start_sharing_from_test_source`]:
/// everything from "a [`CaptureSource`] has been acquired" onward (building
/// and starting the encode pipeline, announcing the stream, wiring up the
/// send pump and preview task).
async fn start_sharing_with_source(
    client: &ScreenshareClient,
    stream_id: u8,
    bitrate_kbps: u32,
    framerate: u32,
    capture_source: CaptureSource,
) -> Result<CaptureHandle> {
    let source_fragment = capture_source.launch_fragment(framerate);

    // Prefer hardware (VAAPI) encode when it's actually usable on this
    // machine, per detect_encoder_backend's probe; fall back to software.
    // Beyond that up-front probe, also fall back if the *real* pipeline
    // (with the real capture source) fails to construct or reach PLAYING —
    // belt-and-suspenders against VAAPI issues the cheap probe pipeline
    // didn't happen to catch — so a flaky/partial VAAPI setup never fails
    // the whole capture when software encode would have worked. On Windows
    // this branch is inert: detect_encoder_backend() always resolves to
    // Software there (see its doc comment), so the Vaapi arm never matches.
    let preferred_backend = detect_encoder_backend();
    let (pipeline, appsink, preview_sink, self_preview_sink, encoder_valve) =
        match start_pipeline(&source_fragment, bitrate_kbps, framerate, preferred_backend) {
            Ok(started) => started,
            Err(e) if preferred_backend == EncoderBackend::Vaapi => {
                tracing::warn!("VAAPI capture pipeline failed to start ({e:#}); falling back to software encoder");
                start_pipeline(&source_fragment, bitrate_kbps, framerate, EncoderBackend::Software)
                    .context("software fallback capture pipeline also failed to start")?
            }
            Err(e) => return Err(e),
        };

    // Width/height for AnnounceStream: read off the negotiated sink caps
    // now that the pipeline is playing, falling back to 0x0 (unknown) if
    // caps haven't landed yet rather than failing the whole call over a
    // cosmetic field.
    let (width, height) = negotiated_size(&appsink).unwrap_or((0, 0));

    // The pad `force_keyframe` pushes its `UpstreamForceKeyUnitEvent` from:
    // specifically the encoded-output appsink's sink pad, not e.g.
    // `pipeline.send_event()`, because this pipeline has *two* leaf sink
    // elements (this one and `preview_sink`) — `Pipeline::send_event`'s
    // "pick a random sink element" behavior for upstream events could just
    // as easily pick the preview branch, which doesn't reach the encoder at
    // all. Pushing from this specific pad guarantees the event travels
    // upstream through exactly the encoder branch (arriving at the
    // encoder's src pad, where `GstVideoEncoder`'s base class — which both
    // `x264enc` and `vah264enc` derive from — intercepts it).
    let encoder_sink_pad = appsink
        .static_pad("sink")
        .ok_or_else(|| anyhow!("encoded-output appsink has no sink pad"))?;

    let stream = appsink.stream();
    let bus = pipeline.bus().ok_or_else(|| anyhow!("pipeline has no bus"))?;
    let bus_stream = bus.stream();
    // Bridged into `pump_loop` alongside the encoder-output/bus streams below
    // (see this module's doc comment's "Why" note on the dedicated third tee
    // branch) rather than via a separate spawned task — reusing the exact
    // same `gst_app::AppSink::stream()` bridging `pump_loop` already does for
    // the encoded-output appsink keeps this handle's lifecycle (one
    // `pump_task`, joined by `CaptureHandle::stop`) unchanged.
    let self_preview_stream = self_preview_sink.stream();

    let (control_tx, control_rx) = mpsc::unbounded_channel();
    let encoder_fps = Arc::new(AtomicU32::new(0));
    let self_preview_frame: Arc<Mutex<Option<egui::ColorImage>>> = Arc::new(Mutex::new(None));

    // This destination's lifecycle state. `pts_epoch` is anchored here — once,
    // now — and never moves again for as long as this destination exists; the
    // `on_pipeline_start` call right after re-anchors only the *offset* onto
    // the pipeline that was just started. See
    // [`destination::DestinationRuntime`]'s doc comment for why that split is
    // what makes a later capture restart look like low framerate rather than a
    // glitch.
    let destination_runtime = destination::DestinationRuntime::shared(
        Instant::now(),
        destination::VideoParams {
            width: width as u32,
            height: height as u32,
            framerate,
            bitrate_kbps,
            backend: preferred_backend,
        },
    );
    {
        let running_time_us = pipeline.current_running_time().map(|t| t.useconds()).unwrap_or(0);
        destination_runtime
            .lock()
            .unwrap()
            .on_pipeline_start(running_time_us, Instant::now());
    }

    let pump_client = client.clone();
    let pump_encoder_fps = encoder_fps.clone();
    let self_preview_bridge = SelfPreviewBridge {
        stream: self_preview_stream,
        frame: self_preview_frame.clone(),
    };
    let pump_task = tokio::spawn(pump_loop(
        pump_client,
        stream_id,
        stream,
        bus_stream,
        control_rx,
        encoder_valve,
        encoder_sink_pad,
        pump_encoder_fps,
        self_preview_bridge,
        destination_runtime.clone(),
    ));

    client
        .announce_stream(stream_id, StreamKind::Video, "Screen", width, height)
        .await
        .context("failed to announce video stream to the relay")?;

    // Now that the pipeline is confirmed Playing and the stream has been
    // announced, wire up the (already-implemented, capture-agnostic)
    // periodic preview/thumbnail sender to the tee's low-rate RGB branch.
    let preview_frames: FrameStream = preview_frame_stream(preview_sink);
    let preview_handle = preview::spawn_preview_task(client.clone(), stream_id, preview_frames);

    Ok(CaptureHandle {
        stream_id,
        client: client.clone(),
        pipeline,
        capture_source,
        pump_task,
        preview_handle: Some(preview_handle),
        control_tx,
        encoder_fps,
        self_preview_frame,
        destination: destination_runtime,
    })
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

/// Converts one `self_preview_sink` RGB8 sample (see this module's doc
/// comment) into an `egui::ColorImage`, for [`CaptureHandle::self_preview_frame`]
/// to hand `ui.rs`. Stride-corrects via [`gst_video::VideoFrameRef`] exactly
/// like [`sample_to_raw_frame`] does for the JPEG-thumbnail branch, then
/// widens tightly-packed RGB rows to RGBA (alpha opaque) since
/// `egui::ColorImage::from_rgba_unmultiplied` — the same constructor
/// `decode.rs`'s `pull_color_image` uses for the viewer-side decoded frame —
/// expects 4 bytes/pixel, while this branch's negotiated caps are RGB8 (3
/// bytes/pixel, cheaper for `videoconvert` to produce off the tee than
/// negotiating RGBA all the way through).
fn sample_to_color_image(sample: &gst::Sample) -> Result<egui::ColorImage> {
    let buffer = sample.buffer().ok_or_else(|| anyhow!("self-preview sample had no buffer"))?;
    let caps = sample.caps().ok_or_else(|| anyhow!("self-preview sample had no caps"))?;
    let info = gst_video::VideoInfo::from_caps(caps).context("failed to parse self-preview capture caps")?;

    let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info)
        .map_err(|_| anyhow!("failed to map self-preview capture buffer readable"))?;

    let width = frame.width() as usize;
    let height = frame.height() as usize;
    let stride = frame.plane_stride()[0] as usize;
    let plane = frame
        .plane_data(0)
        .map_err(|_| anyhow!("self-preview capture frame had no plane 0 data"))?;

    let row_bytes = width * 3;
    let mut rgba = Vec::with_capacity(width * height * 4);
    for row in 0..height {
        let start = row * stride;
        let end = start + row_bytes;
        let Some(row_slice) = plane.get(start..end) else {
            anyhow::bail!("self-preview capture frame plane shorter than expected (stride/height mismatch)");
        };
        for pixel in row_slice.chunks_exact(3) {
            rgba.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 0xFF]);
        }
    }

    Ok(egui::ColorImage::from_rgba_unmultiplied([width, height], &rgba))
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
    /// the portal session (Linux only — Windows has no session to close, see
    /// [`CaptureSource`]), tells the relay this stream is gone (best-effort
    /// — a network error here is logged, not returned, since the pipeline is
    /// already stopped either way), and waits for the background send task
    /// to finish draining.
    pub async fn stop(mut self) -> Result<()> {
        // Ask the pipeline to end gracefully by sending it a real EOS event
        // *before* forcing it to `Null`, and wait for the pump task to
        // notice.
        //
        // This isn't just tidiness: `gstreamer_app::AppSinkStream` (what
        // `pump_loop` awaits via `appsink_stream.next()`) only ever yields
        // its terminal `None` once the appsink's `is_eos()` flag is actually
        // set, which only happens once a real EOS event has flowed all the
        // way through the pipeline into it (see that type's `poll_next`).
        // Jumping straight to `set_state(Null)` (as this used to do) never
        // sends that event — it just tears the elements down — so the
        // appsink's stream would sit `Pending` forever and `pump_task` would
        // never complete, hanging this whole function indefinitely. This
        // was caught by the end-to-end integration test
        // (`end_to_end_test.rs`) actually awaiting a real `stop()` on a live
        // pipeline for the first time; no prior test exercised this path.
        // `Pipeline::send_event` on a downstream/serialized event like EOS
        // delegates to the bin's source element(s), so it originates
        // upstream of the `tee` exactly like a natural end-of-stream would.
        //
        // The `timeout` is a backstop, not the expected path: a healthy
        // pipeline should drain and reach EOS in well under a second, but
        // this guarantees `stop()` itself can never hang the caller forever
        // if a source somehow never honors EOS (e.g. an unresponsive
        // hardware capture device).
        if !self.pipeline.send_event(gst::event::Eos::new()) {
            tracing::warn!("capture pipeline did not accept the EOS event; forcing a hard stop");
        }

        // `&mut` rather than moving `self.pump_task` out: `CaptureHandle`
        // implements `Drop` (see above), which forbids moving individual
        // fields out of `self` by value — `JoinHandle` is `Unpin`, so
        // awaiting through `&mut` works identically. Same fix already
        // applied to `AudioCaptureHandle::stop`.
        match tokio::time::timeout(Duration::from_secs(5), &mut self.pump_task).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!("capture send task did not shut down cleanly: {e}"),
            Err(_) => tracing::warn!(
                "capture send task did not observe EOS within 5s; forcing pipeline to Null anyway"
            ),
        }

        if let Err(e) = self.pipeline.set_state(gst::State::Null) {
            tracing::warn!("error stopping capture pipeline: {e}");
        }

        // `.take()` rather than moving `self.preview_handle` out directly,
        // for the same reason as `pump_task` above: `PreviewHandle::stop`
        // consumes `self` by value, so it can't be called through a
        // reference. `preview_handle` is always `Some` here (only `stop`
        // itself ever takes it, and `stop` consumes `self`), so the `Some`
        // arm is the only one that actually runs.
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
            // split); a `match` (rather than the irrefutable `let` this used
            // to be) is what lets the `#[cfg(test)]`-only `TestSource`
            // variant coexist here without an exhaustiveness error in test
            // builds — it's a no-op for that variant, since a test source
            // never opened a portal session to close.
            match &self.capture_source {
                CaptureSource::Portal(portal) => {
                    if let Err(e) = portal.session.close().await {
                        tracing::warn!("error closing screencast portal session: {e}");
                    }
                }
                #[cfg(test)]
                CaptureSource::TestSource(_) => {}
            }
        }

        if let Err(e) = self.client.stop_stream(self.stream_id).await {
            tracing::warn!("error notifying relay that stream {} stopped: {e:#}", self.stream_id);
        }

        Ok(())
    }

    /// Forwards a `ClientEvent::SubscriberCount { stream_id, count }` for
    /// this handle's `stream_id` to [`pump_loop`], which pauses (`count ==
    /// 0`) or resumes (`count > 0`, forcing a keyframe so the next frame any
    /// new subscriber gets is decodable) the encoder branch. The caller
    /// (`ui.rs`'s event loop) is responsible for matching the incoming
    /// `stream_id` against the right `CaptureHandle` before calling this —
    /// this method itself doesn't check it.
    ///
    /// A no-op (silently dropped) if `pump_loop` has already exited (e.g.
    /// `stop()` is in flight) — there is nothing useful left to pause/resume.
    pub fn notify_subscriber_count(&self, count: u32) {
        let _ = self.control_tx.send(CaptureControlEvent::SubscriberCount(count));
    }

    /// Forwards a `ClientEvent::KeyframeRequest { stream_id }` for this
    /// handle's `stream_id` to [`pump_loop`], which forces the encoder to
    /// produce a keyframe on its next output frame (see
    /// [`force_keyframe`]). Same caller-matches-`stream_id` contract and
    /// already-stopped no-op behavior as [`CaptureHandle::notify_subscriber_count`].
    pub fn notify_keyframe_request(&self) {
        let _ = self.control_tx.send(CaptureControlEvent::KeyframeRequest);
    }

    /// Live sharer-side encode rate: the encoder branch's actual output
    /// framerate over the last [`ENCODER_FPS_WINDOW`], updated by
    /// [`pump_loop`] every time an encoded access unit reaches the appsink.
    /// `0` until enough samples have arrived to compute a rate (e.g. right
    /// after starting, or while the encoder is paused at 0 subscribers — see
    /// [`CaptureControlEvent::SubscriberCount`]). Intended for `ui.rs` to
    /// show next to the configured framerate, confirming the pipeline is
    /// really hitting the selected rate rather than silently capping at some
    /// other value — exactly the class of bug this area of the code has hit
    /// twice before (see this module's doc comment on the `videorate` cap).
    pub fn encoder_fps(&self) -> u32 {
        self.encoder_fps.load(Ordering::Relaxed)
    }

    /// Non-blocking poll for the most recently captured self-preview frame
    /// (see [`SELF_PREVIEW_BRANCH_FRAMERATE`] and
    /// `superdoc/features/self-preview.md`) — `None` if nothing new has
    /// arrived since the last call. Mirrors `decode.rs`'s
    /// `StreamDecoder::latest_frame`'s take-and-clear shape: intended for
    /// `ui.rs` to call once per repaint and, when `Some`, upload via
    /// `egui::TextureHandle::set`/`ctx.load_texture`.
    pub fn self_preview_frame(&self) -> Option<egui::ColorImage> {
        self.self_preview_frame.lock().unwrap().take()
    }

    /// Shared handle onto this capture's destination lifecycle state — see
    /// [`destination::DestinationRuntime`]. `ui.rs` clones this into its own
    /// `Destination` so the UI can read the state machine without going
    /// through the handle on every repaint.
    pub fn destination(&self) -> destination::SharedDestination {
        self.destination.clone()
    }

    /// Snapshot of this capture's destination state. `Starting` until the
    /// first IDR actually reaches the wire, then `Live`.
    pub fn destination_state(&self) -> destination::DestinationState {
        self.destination.lock().unwrap().state()
    }

    /// Tier-3 entry point: the shared capture source is going down, so this
    /// destination stalls — no `StopStream`, no teardown, just the cached-IDR
    /// keepalive at ~2fps until the source is back (see
    /// [`send_stall_keepalive`]). Callers must pair this with
    /// [`CaptureHandle::note_source_restarted`] once the capture is producing
    /// again; nothing clears a stall on its own except a real frame arriving.
    pub fn begin_stall(&self, reason: destination::StallReason) {
        self.destination.lock().unwrap().begin_stall(reason, Instant::now());
    }

    /// The capture source is back: re-anchor the wire clock onto the restarted
    /// pipeline's running time so PTS stays monotonic across the gap (see
    /// [`destination::DestinationRuntime::on_pipeline_start`]). The stall
    /// itself ends when the next real IDR is sent, not here — "the pipeline
    /// restarted" and "the viewer has something decodable again" are different
    /// events and only the second one should clear the placeholder.
    pub fn note_source_restarted(&self) {
        let running_time_us = self.pipeline.current_running_time().map(|t| t.useconds()).unwrap_or(0);
        self.destination.lock().unwrap().on_pipeline_start(running_time_us, Instant::now());
    }
}

// ---------------------------------------------------------------------
// Encode → encrypt → fragment → send pump
// ---------------------------------------------------------------------

/// The self-preview branch's appsink stream plus the frame slot it feeds
/// (see [`SELF_PREVIEW_BRANCH_FRAMERATE`]), bundled into a single
/// [`pump_loop`] parameter rather than two — the pieces are always
/// constructed and consumed together, and this keeps `pump_loop`'s argument
/// count from growing by two instead of one.
struct SelfPreviewBridge {
    stream: gst_app::app_sink::AppSinkStream,
    frame: Arc<Mutex<Option<egui::ColorImage>>>,
}

/// Control-plane events that affect an active share's encoder, forwarded
/// from the relay's `RelayToClient::SubscriberCount`/`RelayToClient::KeyframeRequest`
/// (surfaced to this client as `net::ClientEvent::SubscriberCount`/
/// `net::ClientEvent::KeyframeRequest`) via [`CaptureHandle::notify_subscriber_count`]/
/// [`CaptureHandle::notify_keyframe_request`]. See [`pump_loop`] for how
/// each variant is handled.
enum CaptureControlEvent {
    /// New subscriber count for this stream. `0` pauses the encoder branch
    /// (via [`ENCODER_VALVE_NAME`]'s `drop` property) to save CPU/bandwidth
    /// while nobody is watching; a transition from `0` to non-zero resumes
    /// it and forces a keyframe (see [`force_keyframe`]) so the newly
    /// (re)subscribed viewer's decoder has something to start from.
    SubscriberCount(u32),
    /// A new subscriber joined (relay-debounced). Forces a keyframe — and,
    /// if the encoder branch happened to be paused, resumes it too (a
    /// `KeyframeRequest` implies at least one subscriber exists, regardless
    /// of whether its paired `SubscriberCount` update has arrived yet).
    KeyframeRequest,
}

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

/// Drains encoded access units from `appsink` and pipeline error/EOS
/// messages from `bus` concurrently, for as long as either stream keeps
/// producing, while also reacting to [`CaptureControlEvent`]s on
/// `control_rx` (pausing/resuming `encoder_valve` and forcing keyframes via
/// `encoder_sink_pad` — see [`force_keyframe`]) and bridging
/// `self_preview`'s raw-RGB samples into its frame slot (see
/// [`sample_to_color_image`], [`SelfPreviewBridge`], and
/// [`CaptureHandle::self_preview_frame`]). Exits its GStreamer-facing half
/// when the appsink, bus, and self-preview streams have all ended (normal
/// teardown, see [`CaptureHandle::stop`]) or the bus reports a pipeline
/// error; `control_rx` naturally stops contributing new work once
/// `CaptureHandle` (and its `control_tx`) drops, so it doesn't gate the
/// loop's exit the same way the other three do.
async fn pump_loop(
    client: ScreenshareClient,
    stream_id: u8,
    mut appsink_stream: gst_app::app_sink::AppSinkStream,
    mut bus_stream: gst::bus::BusStream,
    mut control_rx: mpsc::UnboundedReceiver<CaptureControlEvent>,
    encoder_valve: gst::Element,
    encoder_sink_pad: gst::Pad,
    encoder_fps: Arc<AtomicU32>,
    mut self_preview: SelfPreviewBridge,
    runtime: destination::SharedDestination,
) {
    let start = Instant::now();
    // The stall protocol's clock. Ticks unconditionally (a `select!` arm
    // can't be conditionally scheduled without re-creating the interval), but
    // `send_stall_keepalive` is a cheap early-return whenever the destination
    // isn't stalled, and the loop's own exit check runs before the `select!`
    // — so this costs one wakeup every `STALL_RESEND_INTERVAL` and can never
    // hold the loop open past teardown.
    let mut stall_ticker = tokio::time::interval(destination::STALL_RESEND_INTERVAL);
    stall_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut appsink_done = false;
    let mut bus_done = false;
    // Gates loop exit alongside `appsink_done`/`bus_done` (see the doc
    // comment above) so a self-preview branch that never reaches EOS for any
    // reason can't hang `pump_loop` — mirrors those two flags' shape exactly.
    let mut self_preview_done = false;
    // Sliding window of the `Instant`s each encoded access unit reached the
    // appsink, used to compute [`CaptureHandle::encoder_fps`]'s live rate —
    // see [`ENCODER_FPS_WINDOW`].
    let mut encoded_frame_times: VecDeque<Instant> = VecDeque::new();
    // Once `control_tx` (on `CaptureHandle`) drops, `control_rx.recv()`
    // resolves to `None` immediately on every poll — this flag stops that
    // from turning into a busy-spin on this `select!` arm for however much
    // longer the loop keeps running on the other two streams (normally
    // zero, since `stop()` always awaits `pump_task` to completion before
    // `self`/`control_tx` drop; only the bare-drop `Drop` backstop path,
    // which doesn't join `pump_task`, could otherwise hit this).
    let mut control_done = false;
    // Mirrors the valve's actual `drop` property so a `SubscriberCount`
    // transition from one non-zero count to another (e.g. 2 -> 1) doesn't
    // spuriously re-force a keyframe — only an actual 0 -> non-zero (or an
    // explicit `KeyframeRequest`) does.
    let mut paused = false;

    loop {
        if appsink_done && bus_done && self_preview_done {
            return;
        }
        tokio::select! {
            sample = appsink_stream.next(), if !appsink_done => {
                match sample {
                    Some(sample) => {
                        record_encoder_frame(&mut encoded_frame_times, &encoder_fps);
                        if let Err(e) = send_encoded_sample(&client, stream_id, &sample, start, &runtime) {
                            tracing::warn!("dropping encoded frame for stream {stream_id}: {e:#}");
                        }
                    }
                    None => appsink_done = true,
                }
            }
            _ = stall_ticker.tick() => {
                send_stall_keepalive(&client, stream_id, &runtime);
            }
            msg = bus_stream.next(), if !bus_done => {
                match msg {
                    Some(msg) => handle_bus_message(&msg, &mut bus_done),
                    None => bus_done = true,
                }
            }
            sample = self_preview.stream.next(), if !self_preview_done => {
                match sample {
                    Some(sample) => match sample_to_color_image(&sample) {
                        Ok(image) => *self_preview.frame.lock().unwrap() = Some(image),
                        Err(e) => tracing::warn!("dropping malformed self-preview frame for stream {stream_id}: {e:#}"),
                    },
                    None => self_preview_done = true,
                }
            }
            ctrl = control_rx.recv(), if !control_done => {
                match ctrl {
                    Some(CaptureControlEvent::SubscriberCount(0)) => {
                        if !paused {
                            tracing::info!("stream {stream_id} has 0 subscribers; pausing encoder to save CPU/bandwidth");
                            set_valve_drop(&encoder_valve, true, stream_id);
                            paused = true;
                            // No more encoded frames will arrive while
                            // paused, so `record_encoder_frame`'s window
                            // won't naturally decay on its own (it only
                            // recomputes on a new frame) — reset the
                            // published rate to 0 immediately rather than
                            // leaving `encoder_fps()` stuck reporting its
                            // last pre-pause value.
                            encoded_frame_times.clear();
                            encoder_fps.store(0, Ordering::Relaxed);
                        }
                    }
                    Some(CaptureControlEvent::SubscriberCount(_)) => {
                        if paused {
                            tracing::info!("stream {stream_id} has a subscriber again; resuming encoder");
                            set_valve_drop(&encoder_valve, false, stream_id);
                            paused = false;
                            // Resuming after a pause means the first frame
                            // back out of the encoder must be a keyframe —
                            // nothing decodable was produced while paused.
                            force_keyframe(&encoder_sink_pad);
                        }
                    }
                    Some(CaptureControlEvent::KeyframeRequest) => {
                        if paused {
                            // A subscribe implies at least one subscriber
                            // exists even if the paired `SubscriberCount`
                            // update hasn't arrived yet — don't leave the
                            // encoder paused (and the new subscriber with
                            // nothing to decode) waiting on message order.
                            tracing::info!("stream {stream_id} got a keyframe request while paused; resuming encoder");
                            set_valve_drop(&encoder_valve, false, stream_id);
                            paused = false;
                        }
                        force_keyframe(&encoder_sink_pad);
                    }
                    None => control_done = true,
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
    }

    impl VideoParams {
        pub fn to_encode_settings(self) -> branch::EncodeSettings {
            branch::EncodeSettings {
                width: self.width,
                height: self.height,
                framerate: self.framerate,
                bitrate_kbps: self.bitrate_kbps,
                backend: self.backend,
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
        params: VideoParams,
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
                params,
            }
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
        pub fn begin_rebuild(&mut self, now: Instant) {
            if matches!(self.state, DestinationState::Failed(_)) {
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
            }
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
        /// See the type's doc comment — created at most once per branch.
        stream: Option<gst_app::app_sink::AppSinkStream>,
    }

    impl EncodeBranch {
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
    /// to negotiate. The existing preview/self-preview branches already
    /// establish this exact pattern for the same reason (they need RGB, not
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
        let gop_frames = GOP_SECONDS * framerate;
        let encoder = settings.backend.launch_fragment(settings.bitrate_kbps, gop_frames);
        let raw_format = settings.backend.raw_format();
        let width = settings.width;
        let height = settings.height;

        let appsink_name = format!("branch_appsink_{index}");
        let bin_name = format!("branch_{index}");

        let description = format!(
            "queue leaky=downstream max-size-buffers={ENCODER_QUEUE_MAX_BUFFERS} \
             ! videoconvert \
             ! videoscale \
             ! videorate drop-only=false max-duplication-time=0 \
             ! video/x-raw,format={raw_format},width={width},height={height},framerate={framerate}/1 \
             ! {encoder} \
             ! video/x-h264,stream-format=byte-stream,alignment=au \
             ! appsink name={appsink_name} emit-signals=false sync=false max-buffers=4 drop=true"
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

        let branch_sink_pad = bin.static_pad("sink").ok_or_else(|| {
            anyhow!("branch bin has no ghost sink pad (the queue's sink pad should have been auto-ghosted)")
        })?;

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

        Ok(EncodeBranch { id, appsink, bin, tee_pad, stream: None })
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
        let mut branch = add_branch_with_fallback(pipeline, tee, id, settings)?;
        match branch.wait_for_first_idr(FIRST_IDR_TIMEOUT).await {
            Ok(sample) => Ok((branch, sample)),
            Err(e) if settings.backend == EncoderBackend::Vaapi => {
                tracing::warn!("VAAPI encode branch {id:?} produced no IDR ({e:#}); retrying on software");
                // Drain/remove the dud before building its replacement, so a
                // failed attempt never leaves an orphan branch on the tee.
                let _ = remove_branch(pipeline, tee, branch).await;
                let mut fallback = add_branch(
                    pipeline,
                    tee,
                    id,
                    EncodeSettings { backend: EncoderBackend::Software, ..settings },
                )
                .context("software fallback encode branch failed to build")?;
                let sample = fallback.wait_for_first_idr(FIRST_IDR_TIMEOUT).await?;
                Ok((fallback, sample))
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
        let EncodeBranch { id: _, appsink: _, bin, tee_pad, stream } = branch;
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
        // (the real encoder, the preview, the self-preview) dies with it,
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
        let probe_sink_pad = branch_sink_pad.clone();
        let sent_eos = std::sync::atomic::AtomicBool::new(false);
        tee_pad.add_probe(gst::PadProbeType::BLOCK_DOWNSTREAM, move |_pad, _info| {
            if !sent_eos.swap(true, Ordering::Relaxed) && !probe_sink_pad.send_event(gst::event::Eos::new()) {
                tracing::warn!("encode branch removal: EOS was not accepted by the branch's entry pad");
            }
            gst::PadProbeReturn::Drop
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

        if let Err(e) = pipeline.remove(&bin) {
            tracing::warn!("failed to remove encode branch bin from the pipeline: {e}");
        }
        if let Err(e) = bin.set_state(gst::State::Null) {
            tracing::warn!("failed to stop encode branch bin: {e}");
        }
        tee.release_request_pad(&tee_pad);

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

    /// Tier-2 ([`super::destination::ChangeTier::BranchSwap`]) end to end:
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
        let (new_branch, _first_idr) = match built {
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
            let mut runtime = runtime.lock().unwrap();
            runtime.set_params(new_params);
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

        let (branch, first_idr) = add_branch_live(pipeline, tee, id, params.to_encode_settings())
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
    mod tests {
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

        fn small_settings() -> EncodeSettings {
            EncodeSettings {
                width: 64,
                height: 64,
                framerate: 15,
                bitrate_kbps: 500,
                backend: EncoderBackend::Software,
            }
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
            let encoder = EncoderBackend::Software.launch_fragment(500, 30);
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

            // A width of 0 cannot negotiate, so the replacement branch fails
            // to build — a stand-in for any real "these settings don't work on
            // this machine" failure.
            let impossible = VideoParams { width: 0, height: 0, ..small_params() };
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
            assert!(
                wait_for_sample(&old.appsink, Duration::from_secs(5)),
                "the original branch must still be producing after a failed swap"
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
    }
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
        let encoder = backend.launch_fragment(DEFAULT_BITRATE_KBPS, GOP_SECONDS * DEFAULT_FRAMERATE);
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
        let encoder = backend.launch_fragment(DEFAULT_BITRATE_KBPS, 10_000);
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
        let encoder = backend.launch_fragment(DEFAULT_BITRATE_KBPS, GOP_SECONDS * DEFAULT_FRAMERATE);
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

    /// Real, measured proof that [`build_pipeline`]'s `framerate` parameter
    /// actually reaches the encoder branch's output rate — not just that the
    /// pipeline parses/links (that alone would not have caught either of the
    /// two prior bugs in this exact area: the original bitrate-starvation
    /// bug where the `videorate` cap was simply missing, and the general
    /// "looks configured in the UI but isn't actually applied" bug shape
    /// this task's own instructions call out). Feeds a fixed, fast 60fps
    /// `videotestsrc` (well above every `framerate` under test, so the
    /// `videorate` cap — not the source — is what's actually limiting
    /// output) through two different `build_pipeline` `framerate` values,
    /// measures how many encoded access units actually arrive at the appsink
    /// over a real wall-clock window, and asserts the measured rate tracks
    /// the configured one — and that the two configured rates produce
    /// visibly different measured rates, ruling out "always outputs
    /// whatever the source happens to deliver, ignoring the parameter"
    /// silently passing this test by coincidence.
    #[test]
    fn build_pipeline_framerate_parameter_actually_caps_encoder_output_rate() {
        if gst::init().is_err() {
            eprintln!("skipping: gstreamer not available in this environment");
            return;
        }

        // Small resolution and software encoder keep this fast/deterministic
        // on any machine (no VAAPI dependency), while `is-live=true` and a
        // fixed 60/1 source framerate (comfortably above every configured
        // rate under test) makes the `videorate` cap on the encoder branch —
        // not the source's own delivery rate — the thing actually being
        // measured, mirroring how a real portal/pipewiresrc source can
        // deliver well above the configured cap under motion.
        let source = "videotestsrc is-live=true pattern=smpte \
             ! video/x-raw,width=64,height=64,framerate=60/1";

        fn measure_output_fps(source: &str, configured_fps: u32) -> f64 {
            let (pipeline, sink, _preview_sink, _self_preview_sink, _valve) =
                build_pipeline(source, DEFAULT_BITRATE_KBPS, configured_fps, EncoderBackend::Software)
                    .expect("pipeline should construct");
            pipeline.set_state(gst::State::Playing).expect("pipeline should reach PLAYING");

            let measure_window = Duration::from_secs(2);
            let deadline = Instant::now() + measure_window;
            let mut count = 0u32;
            while Instant::now() < deadline {
                if sink.try_pull_sample(gst::ClockTime::from_mseconds(100)).is_some() {
                    count += 1;
                }
            }

            pipeline.set_state(gst::State::Null).expect("pipeline should stop cleanly");
            count as f64 / measure_window.as_secs_f64()
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
}
