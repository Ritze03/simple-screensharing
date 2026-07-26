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
//! # Pacing and frame-skipping
//!
//! Decoded frames are not displayed the instant they finish decoding: they
//! are paced against the sender's capture-time PTS (`pts_us`, threaded
//! through end-to-end from `capture.rs`) by [`PacingState`], owned by
//! `appsink`'s `new_sample` callback. Pacing alone bounds *latency*
//! (falling too far behind re-bases instead of accumulating an
//! ever-growing delay) but says nothing about *throughput*: if a sender is
//! capturing/encoding at a higher framerate than this viewer's decode
//! pipeline can sustain (e.g. a 240fps sender against a decoder that only
//! keeps up with 120fps), re-basing alone still displays every decoded
//! frame — just each one a little later than it should be — which reads as
//! the picture slowly drifting out of sync with real elapsed time rather
//! than as a stall.
//!
//! `Why:` `PacingState` tracks a running *pts budget* — how much sender
//! pts-time has accumulated toward "earning" the next display slot — and
//! skips (tells `new_sample` to drop the frame, never writing it into the
//! display sink) whenever a frame hasn't earned enough budget yet. This is
//! the standard accumulator/DDA technique for down-converting a frame rate:
//! it settles into skipping almost exactly the fraction the sustained rate
//! mismatch demands (e.g. ~50% for a 240fps sender against a 120fps-capable
//! viewer), not 0% (every frame displayed late, forever drifting) or ~100%
//! (skip-storm with no recovery). See [`PacingState`]'s own doc comment for
//! the full rationale, including why a *consecutive-badly-behind-events*
//! counter (an earlier version of this mechanism) does not work for a
//! gradual per-frame differential.
//!
//! `PacingState` also carries a second, independent defense: an absolute,
//! wall-clock-anchored staleness backstop (`epoch`/[`PACING_STALENESS_THRESHOLD`]).
//! The pts-budget mechanism above only ever compares *consecutive* frames,
//! which is exactly right for a gradual, sustained rate mismatch but blind
//! to a *bursty* one: if something upstream of `feed`/`decide` (e.g. an
//! unbounded or throttled event-delivery channel between the network layer
//! and this module's caller) lets a batch of frames back up and then
//! delivers the whole backlog at once, `pts_budget`'s consecutive-frame
//! view sees each backlogged frame as "decode finished early" and paces it
//! out with a real-time sleep — a literal real-time replay of stale
//! history, the "fast-forward catch-up" symptom this backstop exists to
//! prevent (see `superdoc/brainstorm/ui-redesign-multiroom-streaming.md`,
//! "Performance/Rendering" round 2, and the "fps bug fixes" bullet in that
//! doc's Final Plan section). The two mechanisms are complementary, not
//! redundant: `pts_budget` handles *sustained* mismatch, the staleness
//! backstop handles *bursty* delivery; see [`PacingState::epoch`]'s own doc
//! comment for the full design (in particular why it re-anchors only on
//! the first frame and a hard reset, and why the check is scoped to just
//! the "ahead of schedule" branch of [`PacingState::decide`]).
//!
//! This backstop only covers the pacing math itself. It does not fix the
//! upstream cause of a delivery burst — decoupling `MediaFrame` ingestion
//! from the UI's repaint cadence, so a backlog never has a chance to form
//! in the first place — which is a separate, `ui.rs`-side fix tracked in
//! the same plan doc entry.
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
//! side. Decoded PCM is handed straight to an audio sink (`autoaudiosink`
//! in production) rather than back to `ui.rs` as samples to render — there
//! is nothing for the UI to draw each frame, only a volume knob to expose
//! (see [`AudioDecoder::set_volume`]).
//!
//! Packets are *not* pushed into the pipeline in raw arrival order,
//! though: they go through [`AudioJitterBuffer`] first, which restores
//! `pts`-order and tells `opusdec` about genuinely-missing packets so it
//! can conceal them. See "Reordering and packet-loss concealment" below.
//!
//! ## Reordering and packet-loss concealment
//!
//! Media travels over QUIC *datagrams* (spec §6), which are unreliable and
//! unordered, and `fragment.rs`'s `Reassembler` deliberately emits a frame
//! the moment its last fragment lands — so when fragments race, frames can
//! and do complete out of `frame_id` order. For video that barely matters
//! (`avdec_h264` tolerates it and the GOP re-syncs), but for audio it is
//! audible immediately: a backwards timestamp makes the audio sink drop or
//! resync, and a *missing* packet leaves a hole that nothing fills.
//!
//! Two separate defects were in play here, and both had to be fixed:
//!
//! 1. **Nothing reordered.** `AudioDecoder::feed` pushed every packet
//!    straight into `appsrc` as it arrived.
//! 2. **`opusdec`'s PLC was off and unreachable.** `gst-inspect-1.0
//!    opusdec` shows `plc` defaults to **`false`**, so even a correctly
//!    signalled gap would have been ignored — and nothing signalled one
//!    anyway. A dropped packet simply vanished from the decoded PCM.
//!
//! Measured on this machine (GStreamer 1.28.5) by feeding 33 real Opus
//! packets through `appsrc ! opusdec ! audioconvert ! appsink` three ways —
//! this is what `gap_event_makes_opusdec_conceal_a_missing_packet` below
//! asserts:
//!
//! | case | decoded bytes | `opusdec` `stats` |
//! | --- | --- | --- |
//! | all 33 packets | 126720 | `plc-num-samples=0` |
//! | one packet dropped, nothing signalled | 122880 | `plc-num-samples=0` |
//! | one packet dropped, `GAP` event sent | 126720 | `plc-num-samples=960`, `plc-duration=20ms` |
//!
//! i.e. an unsignalled loss silently deletes exactly one 20 ms frame of
//! PCM (3840 bytes = 960 samples × 2ch × 2 bytes) — that deletion *is* the
//! reported stutter — whereas a signalled one is filled in by libopus's
//! own concealment.
//!
//! **Why a `GstEvent::Gap` and not a `GAP`-flagged `gst::Buffer`:** the
//! concealment hook lives in the `GstAudioDecoder` base class, which runs
//! PLC (by calling the subclass's `handle_frame(NULL)`, which is exactly
//! `opusdec`'s `opus_decode(NULL)` path) when it receives a `GST_EVENT_GAP`
//! *and* `plc` is enabled. A `GAP`-flagged buffer is not that hook — it
//! would just be an empty payload for `opusdec` to fail to parse. `GAP` is
//! a serialized event, and `appsrc` queues serialized events in its own
//! internal queue alongside buffers (GStreamer ≥ 1.14), so
//! `appsrc.send_event(gap)` stays correctly interleaved with the
//! surrounding `push_buffer` calls rather than jumping the queue — the
//! measurement above confirms it (the concealed frame lands in the right
//! slot, and the output's total length and end timestamp both match the
//! loss-free case).
//!
//! **Why `pts` and not `frame_id` as the sequence number:** the wire
//! `frame_id` is the connection's *nonce counter*
//! (`net::NonceSequence::next_nonce`, passed straight through as
//! `frame_id` by `audio.rs::send_encoded_sample`), and that counter is
//! shared by every stream on the connection — video, audio, and previews
//! all draw from it. So consecutive audio packets have *non*-consecutive
//! `frame_id`s whenever anything else is also being sent, which makes
//! `frame_id` useless for telling a real gap apart from "video sent three
//! frames in between". `pts` has none of that problem: it comes from
//! `opusenc`'s own output buffers, so it is per-stream, monotonic, and
//! advances by exactly one Opus frame duration per packet. It gives
//! ordering *and* gap detection from one field `DecryptedMediaFrame`
//! already carries — which is why no `net.rs` change was needed here.
//!
//! The jitter buffer's own sizing/latency rationale lives on
//! [`AudioJitterBuffer`], next to the constants it uses.
//!
//! ## Sender PTS vs. the receiver's playout clock
//!
//! A third defect lived in the same place and had to be fixed with the
//! other two: the sender's `pts` was being pushed at the sink *as if* it
//! were on the receiver's clock. It isn't — it is the sender's own
//! capture-pipeline running time, so the offset between the two is "how
//! long the sharer had been sending audio before this viewer subscribed".
//! With the sink on `sync=true`, that offset is a playout delay: a viewer
//! joining 30 s in got **no audio at all** (measured — see
//! [`AudioTimeline`] for the numbers). [`AudioTimeline`] re-bases every
//! timestamp onto this pipeline's running time before it reaches `appsrc`,
//! preserving the sender's *intervals* while replacing its origin.
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
//!
//! The [`AudioJitterBuffer`] and [`AudioTimeline`] are owned *exclusively*
//! by that pipeline thread, inside `pump_audio_loop` — neither is shared,
//! so they need no lock at all and no `Arc`/`Mutex` was added anywhere.
//! Two reasons the jitter buffer lives there rather than in
//! `AudioDecoder::feed`:
//!
//! - It needs a **clock tick** independent of packet arrivals. The whole
//!   point of the buffer is to notice that a packet *didn't* arrive, and
//!   `feed()` is by definition not called when that happens. The pipeline
//!   thread already loops on `rx.recv_timeout(..)`, so it has a natural
//!   timer; `feed()` has none.
//! - It keeps `feed()` exactly as non-blocking and as cheap as it was, on
//!   whatever thread `ui.rs` happens to call it from.
//!
//! Note this is a *different* threading situation from the video pacing
//! fix, which runs inside `appsink`'s `new_sample` callback on GStreamer's
//! own streaming thread and is therefore wrapped in a `Mutex` to satisfy
//! that callback's `Fn`/`Sync` bound. Nothing here ever runs on a
//! GStreamer streaming thread, and nothing here ever sleeps.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

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

/// Sliding-window size for [`StreamDecoder::current_fps`] — wide enough to
/// smooth over normal frame-to-frame jitter but narrow enough to react
/// quickly to a real change (e.g. playback stalling, or the sender's
/// configured framerate changing).
const DECODE_FPS_WINDOW: Duration = Duration::from_secs(2);

/// Capacity of the video decode pipeline's pre-decode ingest channel (see
/// [`StreamDecoder::new`]/[`StreamDecoder::feed`]). Small on purpose: this
/// channel exists only to hand access units from `feed()`'s caller thread
/// to the pipeline thread without blocking the caller, not to buffer a
/// backlog — a slow decoder should show a slightly stale live frame (via
/// the existing appsink drop policy), not an ever-growing queue of stale
/// input. A handful of frames is enough to absorb normal scheduling jitter
/// between the two threads.
const VIDEO_INGEST_CHANNEL_CAPACITY: usize = 8;

/// How large a *single* decode-to-decode wall-clock gap (`dt_wall` between
/// two consecutive calls to [`PacingState::decide`]) has to be before it's
/// treated as a genuinely pathological stall — a system suspend/resume, the
/// process stopped under a debugger, a stream restart — rather than
/// ordinary (even fairly bad) decode/display backlog. Chosen generously
/// large — larger than any realistic single decode hiccup, and in the same
/// ballpark as the H.264 stream's own worst-case 2-second GOP length
/// mentioned above — specifically so it never fires during the sustained
/// but *gradual* rate mismatch [`PacingState::decide`]'s pts-budget
/// mechanism is designed to handle on its own (see that doc comment):
/// crossing this gate hard-resets the pacer (budget zeroed, frame displayed
/// immediately) instead of letting the budget mechanism spend potentially
/// many frames' worth of skips draining a multi-second gap. Also the
/// self-heal path for a PTS discontinuity (a keyframe arriving right after
/// resubscribing, where the new PTS origin has no relation to the old one)
/// on the rare occasion that particular discontinuity happens to coincide
/// with a large wall-clock gap too.
const PACING_HARD_STALL_GAP: Duration = Duration::from_secs(2);

/// How far past a frame's *absolute*, wall-clock-scheduled display time
/// (see [`PacingState::epoch`]) `now` is allowed to be before
/// [`PacingState::decide`] rejects the frame outright, in addition to (not
/// instead of) the relative `pts_budget` accounting above — see
/// [`PacingState::epoch`]'s doc comment for the "fast-forward catch-up" bug
/// this defends against and why the check is scoped to only the
/// "ahead of schedule" branch of `decide`.
///
/// `Why` 250ms: the middle of the ~200-300ms starting range the planning
/// doc suggested (`superdoc/brainstorm/ui-redesign-multiroom-streaming.md`,
/// "Performance/Rendering" round 2 / "fps bug fixes" in the Final Plan).
/// Verified empirically against this file's own tests rather than trusted
/// blindly: neither `pacing_settles_near_theoretical_skip_ratio_for_*`
/// sustained-mismatch test ever enters the branch this threshold gates at
/// all (their `dt_wall` is always *larger* than `dt_pts` — that is
/// definitionally what "the viewer's decode can't keep up" means for a
/// steady-rate sender, so they never look "ahead of schedule"), so this
/// value has zero effect on them regardless of how tight or loose it is.
/// `pacing_sleeps_for_frames_ahead_of_schedule` (an isolated, ordinary
/// early-decode) only ever has tens of milliseconds of slack, comfortably
/// under 250ms. What the value does need to do is reliably trip on a
/// genuine multi-hundred-millisecond-to-multi-second backlog draining in a
/// tight loop — see `pacing_staleness_backstop_*` below, using the plan
/// doc's own repro shape (~600ms of silence, then a burst).
const PACING_STALENESS_THRESHOLD: Duration = Duration::from_millis(250);

/// Tracks how often [`StreamDecoder::latest_frame`] actually yields a *new*
/// decoded frame (not just how often it's polled — `ui.rs` polls once per
/// repaint regardless of whether anything new has decoded, which would
/// otherwise just measure the UI's own repaint rate instead of real decode
/// throughput) over a rolling [`DECODE_FPS_WINDOW`], for display as a live
/// "is playback actually smooth" indicator.
struct FpsCounter {
    timestamps: VecDeque<Instant>,
}

impl FpsCounter {
    fn new() -> Self {
        Self { timestamps: VecDeque::new() }
    }

    /// Record one newly-yielded frame.
    fn record(&mut self) {
        let now = Instant::now();
        self.timestamps.push_back(now);
        self.prune(now);
    }

    fn prune(&mut self, now: Instant) {
        while let Some(&oldest) = self.timestamps.front() {
            if now.duration_since(oldest) > DECODE_FPS_WINDOW {
                self.timestamps.pop_front();
            } else {
                break;
            }
        }
    }

    /// Current rate: frames recorded within the last [`DECODE_FPS_WINDOW`],
    /// divided by the window length. Pruning also happens here (not just on
    /// `record`) so the rate correctly decays toward 0 if playback stalls
    /// and nothing new is recorded for a while, rather than sitting frozen
    /// at its last computed value.
    fn fps(&mut self) -> f64 {
        let now = Instant::now();
        self.prune(now);
        self.timestamps.len() as f64 / DECODE_FPS_WINDOW.as_secs_f64()
    }
}

/// Paces decoded video frames against their capture-time PTS instead of
/// displaying each one the instant it finishes decoding.
///
/// # Why pacing lives here instead of `sync=true` on the appsink
///
/// `pts_us` (threaded through decrypt -> [`PipelineMsg::AccessUnit`] ->
/// stamped onto the `appsrc` buffer in `pump_loop`) is the *sender's* own
/// running time — `start.elapsed()` since that capture session began (see
/// `capture.rs`/`audio.rs`) — not wall-clock/epoch time, and has no defined
/// relationship to this pipeline's own running-time clock. Flipping the
/// appsink to `sync=true` would make GStreamer pace against its own
/// running-time axis instead, which is a different clock with no known
/// offset from the sender's — so pacing has to be done by hand here,
/// relative to the first frame this decoder instance sees, rather than
/// trusting GStreamer's sync machinery.
///
/// # Why a bounded catch-up window instead of always honoring `pts_us`
///
/// Always sleeping until the sender's pts-time says to would let the
/// decoder fall permanently behind after any stall (e.g. a slow decode, or
/// a burst of frames after a channel drop), and would misbehave completely
/// across a PTS discontinuity (a stream restart, or the first keyframe
/// after resubscribing, where the new stream's PTS origin has nothing to do
/// with the old one — e.g. a much smaller `pts_us` than the previous
/// frame's). [`Self::decide`] handles both the same way: a single
/// decode-to-decode wall-clock gap larger than [`PACING_HARD_STALL_GAP`]
/// gives up catching up and hard-resets pacing on the current frame
/// instead, bounding the worst-case added latency and self-healing rather
/// than accumulating drift forever.
///
/// # Why a hard reset alone isn't enough, and frames get skipped too
///
/// A hard reset only fires on a huge single-step gap. It says nothing about
/// *sustained* throughput: if the sender's output rate durably exceeds what
/// this viewer can decode and display (e.g. a 240fps sender against a
/// decoder that can only sustain 120fps), no single step is ever more than
/// a fraction of a millisecond "worse" than the last — the lag only
/// *accumulates*, a little at a time, forever. A per-step or
/// consecutive-event threshold cannot see that: measured one step at a
/// time the lag never crosses any fixed bar, so a design built purely
/// around "is this one step bad enough" never engages (this was a real bug
/// in an earlier version of this mechanism — see git history / the
/// implementation notes doc). [`Self::decide`] instead runs a small
/// accumulator, `pts_budget`: every call adds this frame's own `dt_pts`
/// (how far the sender's clock advanced since the last frame) to the
/// budget, and every *displayed* frame spends `dt_wall` (how much real
/// time actually elapsed) out of it. A frame is displayed only if the
/// budget can afford that real-time cost; otherwise it's skipped and the
/// budget is left untouched (carried into the next frame's accounting
/// rather than being wiped to zero). This is the standard accumulator/DDA
/// technique used for frame-rate down-conversion, and it has the right
/// asymptotic behavior for free: sustained `dt_wall > dt_pts` by some
/// constant factor converges to skipping almost exactly `1 - dt_pts/dt_wall`
/// of frames (e.g. 240fps sender vs. 120fps-sustained viewer -> ~50%
/// skipped), not 0% (drifting forever) and not ~100% (skip-storm with no
/// way back) — and it recovers on its own, within a frame or two, the
/// moment `dt_wall` returns to matching `dt_pts` (viewer catches up, or the
/// sender's rate drops), because nothing about it depends on how long the
/// mismatch has been going on, only on the current step's own numbers.
struct PacingState {
    /// `(pts_us, local_instant)` of the most recent frame `decide` was
    /// given — *any* decision, not just displayed ones — used to compute
    /// the next call's `dt_pts`/`dt_wall`. `None` until the first frame is
    /// seen.
    last: Option<(u64, Instant)>,
    /// Running pts-time budget: how much of the sender's clock has advanced
    /// since the budget was last spent (by displaying a frame) or zeroed
    /// (by a [`PACING_HARD_STALL_GAP`] hard reset). See [`Self::decide`]'s
    /// doc comment ("Why a hard reset alone isn't enough...") for the full
    /// accounting rule. Never negative (there's no such thing as a Duration
    /// below zero) — an "ahead of schedule" step still adds to it before
    /// immediately spending `dt_wall` back out, same as any other display.
    pts_budget: Duration,
    /// Fixed `(pts_us, local_instant)` reference point [`Self::decide`]
    /// measures a frame's *absolute* wall-clock staleness against -- a
    /// defensive backstop layered on top of (not instead of) the relative
    /// `pts_budget` accounting above. `None` until the first frame is
    /// seen; re-anchored to `(pts_us, now)` of the current frame on the
    /// first-frame bootstrap and on a [`PACING_HARD_STALL_GAP`] hard reset
    /// (a genuine pathological stall should re-anchor, not carry a stale
    /// epoch forward) -- never on an ordinary call in between. It is
    /// deliberately *not* a rolling "last frame" reference: rolling it
    /// forward every call would just re-derive `dt_wall`/`dt_pts`, which
    /// the relative mechanism above already computes and which is exactly
    /// what fails to catch the bug this exists for (see below).
    ///
    /// `Why:` this is the fix for the "fast-forward catch-up" bug (see
    /// `superdoc/brainstorm/ui-redesign-multiroom-streaming.md`,
    /// "Performance/Rendering" round 2). Root cause: something upstream of
    /// `decide` (an unbounded/throttled event channel, a paused UI thread,
    /// ...) lets a batch of frames pile up undelivered, then hands them to
    /// `decide` all at once in a tight loop -- many calls in a row with
    /// almost no real wall-clock time between them (`dt_wall` tiny) but
    /// normal PTS spacing (`dt_pts` normal), i.e. exactly the signature of
    /// the "ahead of schedule" branch in [`Self::decide`]. The *relative*
    /// `pts_budget` mechanism cannot tell that apart from "decode finished
    /// early" -- its whole model is built from consecutive-frame deltas, so
    /// it happily paces every backlogged frame out with a real-time sleep,
    /// faithfully re-displaying however many hundred milliseconds (or
    /// seconds) of history piled up, one frame at a time, in real time.
    /// An absolute check fixes this because it compares each frame against
    /// a *fixed* real-world point instead of only the previous frame: a
    /// frame whose own PTS says it belongs far in the past relative to
    /// *right now* gets rejected outright by [`PACING_STALENESS_THRESHOLD`],
    /// no matter how small `dt_wall`/`dt_pts` made it look moment-to-moment.
    ///
    /// `Why` the check only applies within the "ahead of schedule" branch
    /// (see [`Self::decide`]) rather than at the top of every call,
    /// unconditionally: that branch is the *only* one that can ever return
    /// a real-time sleep in the first place (the other branches either
    /// already skip, or display immediately with no delay), so it's the
    /// only place the "replay history in real time" failure mode can
    /// originate from -- and scoping it there means this backstop cannot
    /// interfere with the ordinary relative-budget skip/display decisions
    /// at all. That matters because a truly unconditional check (applied
    /// on every call, gating on nothing) was tried first and does not
    /// survive this file's own tests: under a genuine *sustained* rate
    /// mismatch (`pacing_settles_near_theoretical_skip_ratio_for_*` below),
    /// `now` and `pts_us` both advance every call, but `now` advances
    /// *faster* than `pts_us` by construction (`dt_wall > dt_pts` always --
    /// that is definitionally what "the viewer can't keep up" means), so
    /// absolute staleness relative to a fixed epoch grows without bound
    /// over the run even though the frame-to-frame behavior is exactly the
    /// intended, correct downsampling -- an unconditional check would cross
    /// any threshold small enough to also catch a real burst within the
    /// first few dozen frames and then, having nothing that ever shrinks
    /// the gap back down under a matched-rate (not faster-than-real-time)
    /// recovery, latch into rejecting every subsequent frame forever.
    /// Scoping to the "ahead of schedule" branch sidesteps this entirely:
    /// that branch is never entered at all when `dt_wall >= dt_pts`
    /// (proven false in both sustained-mismatch tests, and in the ordinary
    /// on-pace/isolated-stall-recovery tests, which recover at a matched
    /// rather than faster-than-real-time pace), so this backstop has zero
    /// effect on any of them regardless of threshold.
    epoch: Option<(u64, Instant)>,
}

/// What [`PacingState::decide`] wants the caller to do with the next
/// decoded frame.
#[derive(Debug, PartialEq, Eq)]
enum PacingDecision {
    /// Display the frame. `Some(duration)` means sleep that long first;
    /// `None` means display it immediately (already on pace, or already
    /// past due).
    Display(Option<Duration>),
    /// Not enough accumulated pts-budget to justify displaying this frame
    /// right now: drop it instead. The caller just must not write it to the
    /// display sink; `pts_budget` already accounts for the frame internally
    /// (see [`PacingState::decide`]), the caller has nothing further to do.
    Skip,
}

impl PacingState {
    fn new() -> Self {
        Self { last: None, pts_budget: Duration::ZERO, epoch: None }
    }

    /// Given the next decoded frame's `pts_us` and the current time `now`,
    /// decide what to do with it — see [`PacingDecision`]. Pure/
    /// deterministic given its inputs — kept free of any GStreamer or
    /// thread-sleeping so it's unit-testable on its own; see
    /// `tests::pacing_*` below.
    fn decide(&mut self, pts_us: u64, now: Instant) -> PacingDecision {
        let Some((last_pts, last_instant)) = self.last else {
            // First frame ever: nothing to pace against yet, display now
            // and start tracking from here. Also anchors `epoch` — see
            // that field's doc comment.
            self.last = Some((pts_us, now));
            self.pts_budget = Duration::ZERO;
            self.epoch = Some((pts_us, now));
            return PacingDecision::Display(None);
        };

        // How far the sender's clock and the wall clock each advanced since
        // the last frame `decide` saw. `saturating_sub`/`saturating_duration_since`
        // rather than plain subtraction: a PTS discontinuity (stream
        // restart) can make `pts_us` smaller than `last_pts`, and this must
        // not panic — it just yields `dt_pts == 0` for that one frame,
        // which is exactly the right answer ("the sender's clock didn't
        // meaningfully advance towards this new origin").
        let dt_pts = Duration::from_micros(pts_us.saturating_sub(last_pts));
        let dt_wall = now.saturating_duration_since(last_instant);
        self.last = Some((pts_us, now));

        if dt_wall > PACING_HARD_STALL_GAP {
            // A genuinely pathological single-step gap (system suspend,
            // debugger pause, stream restart) — don't make the viewer sit
            // through however many frames it'd take the budget accounting
            // below to drain a multi-second backlog; just start fresh and
            // show this frame right away, like a stall that self-heals
            // instantly rather than one that's merely being paid down.
            // Also re-anchors `epoch`: a genuine pathological stall should
            // re-anchor, not carry a stale epoch forward into whatever
            // comes next (see that field's doc comment).
            self.pts_budget = Duration::ZERO;
            self.epoch = Some((pts_us, now));
            return PacingDecision::Display(None);
        }

        if dt_wall < dt_pts {
            // Ahead of schedule, per the *relative* dt_wall/dt_pts
            // comparison alone: decode finished before the sender's clock
            // caught up to this frame's pts. Before trusting that and
            // pacing this frame out with a real-time sleep, check it
            // against the *absolute* wall-clock backstop — see `epoch`'s
            // doc comment for why this specific branch is the one that
            // needs it (it's the only branch that can ever return a sleep
            // at all) and why the check sits here, before
            // `self.pts_budget += dt_pts` below, rather than after: a
            // frame this backstop rejects must not contribute its `dt_pts`
            // to the budget either, or a whole rejected backlog's worth of
            // banked budget would just trigger a *second*, budget-driven
            // catch-up burst the moment staleness next drops back under
            // threshold — precisely the failure mode this backstop exists
            // to prevent, just deferred by a few frames instead of
            // avoided.
            let (epoch_pts, epoch_wall) =
                self.epoch.expect("epoch is set by the first-frame and hard-reset branches above");
            let expected_wall = epoch_wall + Duration::from_micros(pts_us.saturating_sub(epoch_pts));
            if now.saturating_duration_since(expected_wall) > PACING_STALENESS_THRESHOLD {
                return PacingDecision::Skip;
            }

            // Ahead of schedule, and not stale enough to reject outright:
            // spend only the real time that actually elapsed and sleep out
            // the rest, same as the pre-existing behavior.
            self.pts_budget += dt_pts;
            self.pts_budget = self.pts_budget.saturating_sub(dt_wall);
            return PacingDecision::Display(Some(dt_pts - dt_wall));
        }

        self.pts_budget += dt_pts;

        if self.pts_budget >= dt_wall {
            // On pace, or behind but with enough banked pts-time to afford
            // showing this frame right now: spend it and display.
            self.pts_budget -= dt_wall;
            PacingDecision::Display(None)
        } else {
            // Not enough budget yet to afford this frame's real-time cost —
            // skip it. `pts_budget` is left as-is (not reset to zero): the
            // next frame's `dt_pts` adds to the same running total, so a
            // sustained mismatch naturally settles into skipping the right
            // proportion instead of oscillating between "reset, catch up
            // fully" and "give up entirely".
            PacingDecision::Skip
        }
    }
}

/// Decodes one subscribed video stream's incoming H.264 access units into
/// `egui::ColorImage` frames. See the module docs for the pipeline shape and
/// threading rationale.
pub struct StreamDecoder {
    stream_id: StreamId,
    tx: std_mpsc::SyncSender<PipelineMsg>,
    latest_frame: LatestFrameSlot,
    /// An H.264 decoder can't produce anything from a mid-GOP delta frame
    /// alone, so frames are dropped until the first keyframe is seen —
    /// otherwise every frame between subscribing and the next keyframe (up
    /// to the spec's 2-second GOP) would just be decode-error log spam.
    seen_keyframe: bool,
    thread: Option<thread::JoinHandle<()>>,
    /// Live viewer-side FPS tracker — see [`FpsCounter`]/[`Self::current_fps`].
    fps_counter: FpsCounter,
    /// When this decoder was created — the reference point for
    /// [`Self::is_stalled`] before any frame has ever arrived.
    created_at: Instant,
    /// When the last media frame for this stream arrived (any frame, including
    /// ones dropped by the pre-keyframe gate in [`Self::feed`] — this measures
    /// "is the sender still talking to us", not "is decode succeeding").
    last_frame_at: Option<Instant>,
    /// Whether the most recent frame carried the sender's advisory
    /// [`crate::protocol::MEDIA_FLAG_STALLED`] bit.
    last_frame_stalled: bool,
}

/// How long without any frame at all before [`StreamDecoder::is_stalled`]
/// reports a stall on its own, with no signal from the sender.
///
/// `Why a silence backstop in addition to the flag:` the flag only arrives
/// from a sender that is healthy enough to keep sending — it covers the
/// *graceful* case (capture restarting, stall protocol running). A sender that
/// crashed, was killed, or lost its network sends nothing at all, and the
/// viewer would otherwise sit on a frozen frame with no indication anything
/// was wrong. 3s is comfortably above both the spec's 2s GOP and the stall
/// protocol's own ~2fps keepalive (`capture::destination::STALL_RESEND_INTERVAL`,
/// 500ms), so a stalled-but-alive sender is always caught by the flag path
/// first and never trips this one; only genuine silence does.
const NO_FRAME_STALL_TIMEOUT: Duration = Duration::from_secs(3);

impl StreamDecoder {
    /// Build and start a decode pipeline dedicated to `stream_id`. Calls
    /// `gstreamer::init()` (idempotent — safe alongside other
    /// `StreamDecoder`s or a future `capture.rs`).
    pub fn new(stream_id: StreamId) -> Result<Self> {
        gstreamer::init().context("failed to initialize GStreamer")?;

        let latest_frame: LatestFrameSlot = Arc::new(Mutex::new(None));
        // Bounded, not `std_mpsc::channel()`'s unbounded queue: this is the
        // pre-decode ingest path, and the module's own stated policy is
        // "drop and continue" everywhere else (the appsink downstream is
        // already `max-buffers=1`/`drop=true`). An unbounded channel here
        // would let a sustained high framerate that outpaces decode
        // throughput grow latency/memory without bound instead. Bounded to
        // a small `VIDEO_INGEST_CHANNEL_CAPACITY` and fed via `try_send` in
        // `feed()` (drop-newest-on-full — see that method's doc comment for
        // why that policy over drop-oldest).
        let (tx, rx) = std_mpsc::sync_channel::<PipelineMsg>(VIDEO_INGEST_CHANNEL_CAPACITY);

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
            fps_counter: FpsCounter::new(),
            created_at: Instant::now(),
            last_frame_at: None,
            last_frame_stalled: false,
        })
    }

    pub fn stream_id(&self) -> StreamId {
        self.stream_id
    }

    /// Feed one already-reassembled-and-decrypted media frame (i.e. a
    /// `ClientEvent::MediaFrame` payload from `net.rs`). Frames for a
    /// different `stream_id` are ignored defensively (callers are expected
    /// to filter before calling, but a misrouted frame here must never
    /// panic). Never blocks: this pushes onto a small bounded channel
    /// ([`VIDEO_INGEST_CHANNEL_CAPACITY`]) via `try_send` that the pipeline
    /// thread drains, and if the pipeline is falling behind and the channel
    /// is momentarily full, the new access unit is dropped rather than
    /// queued or blocking the caller — drop-newest, not drop-oldest,
    /// because `std::sync::mpsc::SyncSender` has no way to reach in and
    /// evict an already-sent item from the receiver side (that side is
    /// owned by the pipeline thread), so drop-newest is what the channel
    /// type actually gives us with no extra synchronization; it's no worse
    /// than drop-oldest here since either policy just bounds the backlog,
    /// and the receiver drains in order so the dropped access unit was the
    /// most recent one anyway.
    pub fn feed(&mut self, frame: &DecryptedMediaFrame) {
        if frame.stream_id != self.stream_id {
            return;
        }
        // Stall bookkeeping happens *before* the pre-keyframe gate below: a
        // frame arriving at all is evidence the sender is alive, even if this
        // decoder can't use it yet. Gating this behind `seen_keyframe` would
        // make a freshly-subscribed stream report "stalled" for up to a full
        // GOP purely because it hasn't hit its first keyframe.
        self.last_frame_at = Some(Instant::now());
        self.last_frame_stalled = frame.stalled;
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
        match self.tx.try_send(msg) {
            Ok(()) => {}
            Err(std_mpsc::TrySendError::Full(_)) => {
                // Decode can't keep up with the incoming rate right now;
                // drop this access unit rather than growing an unbounded
                // backlog. Matches the "drop and continue" policy the
                // appsink already applies downstream of decode.
                tracing::debug!(
                    "decode ingest channel for stream {} is full; dropping access unit (decoder falling behind)",
                    self.stream_id
                );
            }
            Err(std_mpsc::TrySendError::Disconnected(_)) => {
                // The pipeline thread only stops after a `Shutdown` sent
                // from `Drop`, so this means it exited/panicked already
                // (e.g. a GStreamer element failed to build). Log and keep
                // going — one dead decoder shouldn't take the whole UI down.
                tracing::warn!(
                    "decode pipeline thread for stream {} is gone; dropping frame",
                    self.stream_id
                );
            }
        }
    }

    /// Non-blocking poll for the most recently decoded frame. Returns
    /// `None` if nothing new has been decoded since the last call. Intended
    /// to be called once per `ui.rs` repaint and, when `Some`, uploaded via
    /// an `egui::TextureHandle::set`.
    ///
    /// Every `Some` return is recorded by [`Self::current_fps`]'s counter —
    /// this is what makes `current_fps` measure real decode throughput
    /// rather than `ui.rs`'s repaint rate: a slow/stalled decoder still gets
    /// polled every repaint, but only actually *yields* a new frame at the
    /// real decode rate.
    pub fn latest_frame(&mut self) -> Option<egui::ColorImage> {
        let frame = self.latest_frame.lock().unwrap().take();
        if frame.is_some() {
            self.fps_counter.record();
        }
        frame
    }

    /// Live viewer-side FPS: how many *new* frames [`Self::latest_frame`]
    /// actually yielded over the last [`DECODE_FPS_WINDOW`] — i.e. real
    /// playback smoothness, not just how often the UI happens to poll.
    /// Intended for `ui.rs` to show next to the watched video panel.
    pub fn current_fps(&mut self) -> f64 {
        self.fps_counter.fps()
    }

    /// Whether this stream should be shown with a "waiting for capture…"
    /// placeholder: the sender told us it's stalled (its advisory
    /// [`crate::protocol::MEDIA_FLAG_STALLED`] bit — see
    /// `capture::send_stall_keepalive`), **or** nothing has arrived for
    /// [`NO_FRAME_STALL_TIMEOUT`] at all.
    ///
    /// Both halves are needed and neither subsumes the other: the flag catches
    /// a graceful capture restart promptly (the sender is still streaming, just
    /// re-sending its last IDR, so the silence check would never fire), while
    /// the silence check catches a sender that died without being able to
    /// signal anything.
    ///
    /// `Why this is only a UI hint:` the flag half of this rides the media
    /// header's cleartext, unauthenticated `flags` byte (see
    /// `net::DecryptedMediaFrame::stalled`). Callers may use this to decide
    /// what to *draw*; nothing may use it to unsubscribe, tear down, or make
    /// any other decision with a security or protocol consequence. The
    /// silence half is trustworthy on its own — it's derived purely from local
    /// observation — but the two are combined here, so treat the result as
    /// advisory throughout.
    pub fn is_stalled(&self) -> bool {
        self.is_stalled_at(Instant::now())
    }

    /// [`Self::is_stalled`] against an injected `now`, so the timing half can
    /// be tested without sleeping (same pattern as `PacingState::decide`).
    pub fn is_stalled_at(&self, now: Instant) -> bool {
        if self.last_frame_stalled {
            return true;
        }
        let reference = self.last_frame_at.unwrap_or(self.created_at);
        now.saturating_duration_since(reference) > NO_FRAME_STALL_TIMEOUT
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
    // `Mutex`, not a plain field, because `new_sample` below is `Fn`, not
    // `FnMut` (GStreamer may in principle re-invoke it concurrently, and
    // the closure trait bound requires `Sync`) — same pattern already used
    // for `sink_frame_slot` just above. This callback runs on GStreamer's
    // own streaming thread downstream of the `queue` element (see module
    // docs "Threading"), never on an async runtime thread, so blocking it
    // with `std::thread::sleep` below for pacing is correct and matches
    // how a GStreamer `sync=true` sink element would behave internally.
    let pacing = Mutex::new(PacingState::new());
    appsink.set_callbacks(
        AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                match pull_color_image(sink) {
                    Ok(Some((image, Some(pts_us)))) => {
                        match pacing.lock().unwrap().decide(pts_us, Instant::now()) {
                            PacingDecision::Display(Some(delay)) => {
                                std::thread::sleep(delay);
                                *sink_frame_slot.lock().unwrap() = Some(image);
                            }
                            PacingDecision::Display(None) => {
                                *sink_frame_slot.lock().unwrap() = Some(image);
                            }
                            PacingDecision::Skip => {
                                // Not enough accumulated pts-budget to
                                // afford this frame's real-time cost (see
                                // `PacingState::decide`) — decode throughput
                                // sustainably below the sender's output
                                // rate: drop this frame — do not write it
                                // into the sink slot — rather than display
                                // every decoded frame at a rate the viewer
                                // can't sustain, which is what caused the
                                // slow drift this mechanism replaced.
                            }
                        }
                    }
                    Ok(Some((image, None))) => {
                        // No PTS on the decoded buffer (shouldn't normally
                        // happen since `pump_loop` always stamps one, but
                        // never worth failing the whole stream over) —
                        // display immediately rather than pace blindly.
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
/// this must not assume a tightly packed buffer). Also returns the
/// decoded buffer's PTS (in microseconds), if it has one, for
/// [`PacingState`] to schedule display against — this is the same
/// capture-time `pts_us` `pump_loop` stamped onto the pre-decode buffer;
/// GStreamer propagates it downstream through `h264parse`/`avdec_h264`/
/// `videoconvert` unless something along the chain drops it.
fn pull_color_image(sink: &AppSink) -> Result<Option<(egui::ColorImage, Option<u64>)>> {
    let sample = match sink.try_pull_sample(ClockTime::from_mseconds(0)) {
        Some(s) => s,
        None => return Ok(None),
    };
    let buffer = sample.buffer().ok_or_else(|| anyhow!("sample had no buffer"))?;
    let pts_us = buffer.pts().map(|pts| pts.useconds());
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

    Ok(Some((
        egui::ColorImage::from_rgba_unmultiplied([width, height], &packed),
        pts_us,
    )))
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

/// Longest an out-of-sequence packet is held waiting for a missing
/// predecessor before [`AudioJitterBuffer`] gives up on it and conceals
/// the hole instead. Two Opus frames at this project's 20 ms frame size —
/// deliberately at the *low* end of a normal reorder window, see
/// [`AudioJitterBuffer`]'s "added latency" note for why holding longer is
/// actively counterproductive here.
const AUDIO_JITTER_MAX_HOLD: Duration = Duration::from_millis(40);

/// Hard ceiling on how many packets [`AudioJitterBuffer`] will hold at
/// once. Once exceeded, the head is released immediately regardless of its
/// hold deadline. This is the buffer's absolute bound: it can never hold
/// more than this many packets nor delay one by more than
/// [`AUDIO_JITTER_MAX_HOLD`], so a sustained gap degrades into a glitch
/// rather than into unbounded latency or memory.
const AUDIO_JITTER_MAX_DEPTH: usize = 8;

/// Most consecutive missing slots that will be signalled to `opusdec` for
/// concealment before the buffer just accepts the discontinuity and moves
/// on. 100 ms at 20 ms frames — Opus PLC degrades to near-silence well
/// before that anyway, so concealing further is pure busywork; the
/// remaining hole is simply left in the timeline for the sink to handle.
const AUDIO_MAX_CONCEAL_SLOTS: u64 = 5;

/// Assumed Opus frame duration until a real one has been observed. This is
/// what `audio.rs`'s sender actually produces: it builds `opusenc` with no
/// `frame-size` override, and `gst-inspect-1.0 opusenc` reports that
/// property's default as `20` (ms).
const AUDIO_DEFAULT_FRAME_DURATION_US: u64 = 20_000;

/// How many recent PTS deltas [`AudioJitterBuffer`] keeps to infer the
/// sender's Opus frame duration from. Bounded (rather than an all-time
/// minimum) so one anomalous small delta can't poison gap detection
/// forever — it ages out after this many packets.
const AUDIO_FRAME_DURATION_WINDOW: usize = 16;

/// A backwards PTS jump larger than this is read as the sender having
/// restarted its capture session (a new `audio.rs` `start` origin), not as
/// reordering, and resets the buffer. Same self-healing philosophy as
/// [`PacingState`]'s re-base on the video side.
const AUDIO_PTS_DISCONTINUITY_US: u64 = 1_000_000;

/// One Opus packet waiting its turn in [`AudioJitterBuffer`].
struct PendingAudioPacket {
    data: Vec<u8>,
    /// When this packet entered the buffer — the reference point for its
    /// [`AUDIO_JITTER_MAX_HOLD`] deadline. Deliberately per-packet rather
    /// than a single buffer-wide deadline, so a packet that arrives while
    /// an older hole is already being waited out gets its own full (and
    /// no longer than its own) share of patience.
    arrived: Instant,
}

/// What [`AudioJitterBuffer`] wants pushed into `appsrc` next.
///
/// Deliberately a plain data enum with no GStreamer types in it, so the
/// buffer's whole decision-making is unit-testable without a pipeline —
/// the same trick the video pacing fix used to make [`PacingState::decide`]
/// testable. [`apply_audio_actions`] is the (trivial, untested) glue that
/// turns these into actual GStreamer calls.
#[derive(Debug, PartialEq, Eq)]
enum AudioAction {
    /// Push this Opus packet into `appsrc`, stamped with `pts_us`.
    Push { pts_us: u64, data: Vec<u8> },
    /// Send a `GAP` event for a packet that never arrived, so `opusdec`
    /// (built with `plc=true`) runs Opus packet-loss concealment for that
    /// slot instead of leaving a hole. See the module docs' "Reordering
    /// and packet-loss concealment" section for the measurement proving
    /// this is what actually triggers PLC.
    Conceal { pts_us: u64, duration_us: u64 },
}

/// Re-orders incoming Opus packets by `pts` and turns genuine losses into
/// explicit concealment requests.
///
/// # Why it holds packets only when it has to
///
/// A packet that is the immediate successor of the last released one is
/// released *immediately* — the buffer adds **zero latency** to a healthy,
/// in-order stream. Latency is spent only when something is actually wrong
/// (a missing or out-of-order packet), and even then it is bounded by
/// [`AUDIO_JITTER_MAX_HOLD`] (40 ms) and [`AUDIO_JITTER_MAX_DEPTH`].
///
/// This is not the usual "always pre-roll N frames" jitter buffer, and the
/// difference is deliberate. A classic pre-roll buys the sink extra *lead
/// time* by delaying the whole timeline. This buffer cannot do that: it
/// forwards each packet's original `pts` untouched, so delaying a push
/// does not move the packet's playout slot — it only brings the push
/// *closer to* (or past) a deadline that stays put. A fixed pre-roll here
/// would therefore make late packets later, not safer. Holding is worth it
/// only when it can actually change the outcome, i.e. when the alternative
/// is pushing packets out of order or with an unannounced hole. Hence:
/// hold on disorder, never on principle.
///
/// The added latency, stated plainly, is **0 ms in the steady state and at
/// most 40 ms during a loss/reorder event**, after which it returns to 0
/// on the next in-sequence packet. For comparison, the video path only
/// hard-resets on a pathological single-step gap of
/// [`PACING_HARD_STALL_GAP`] (2 seconds) — smaller, sustained lag is instead
/// absorbed by [`PacingState`]'s pts-budget skip mechanism, trading
/// framerate for latency; audio has no equivalent "skip" path and is held
/// to a much tighter bound because lip-sync drift and audio latency are far
/// more noticeable than a late (or dropped) video frame.
///
/// # Bounds
///
/// Every input is bounded: at most [`AUDIO_JITTER_MAX_DEPTH`] packets held
/// (the head is force-released past that, see [`Self::drain`]), at most
/// [`AUDIO_JITTER_MAX_HOLD`] of delay per packet, at most
/// [`AUDIO_MAX_CONCEAL_SLOTS`] concealment events per hole, and at most
/// [`AUDIO_FRAME_DURATION_WINDOW`] remembered deltas. Nothing here grows
/// with stream length or with how badly the network is behaving.
struct AudioJitterBuffer {
    /// Held packets keyed by `pts_us`. `BTreeMap` (not a ring/`VecDeque`)
    /// because the whole job is "release in PTS order regardless of
    /// arrival order", and that is exactly what iterating a `BTreeMap`
    /// from the front gives, with de-duplication by key for free. Bounded
    /// by [`AUDIO_JITTER_MAX_DEPTH`], so its `O(log n)` is over n ≤ 8.
    pending: BTreeMap<u64, PendingAudioPacket>,
    /// `pts_us` of the last packet handed to `appsrc`; `None` before the
    /// first one (and again after a discontinuity reset).
    last_released_pts: Option<u64>,
    /// Sliding window of recent *contiguous* release-to-release PTS
    /// deltas. The minimum of these is the inferred Opus frame duration —
    /// see [`Self::frame_duration_us`].
    recent_deltas: VecDeque<u64>,
}

impl AudioJitterBuffer {
    fn new() -> Self {
        Self {
            pending: BTreeMap::new(),
            last_released_pts: None,
            recent_deltas: VecDeque::new(),
        }
    }

    fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// The sender's Opus frame duration, inferred as the smallest delta
    /// seen between consecutive released packets in the recent past.
    ///
    /// Inferred rather than hardcoded to [`AUDIO_DEFAULT_FRAME_DURATION_US`]
    /// because the whole gap-detection scheme is "how many frame-sized
    /// slots fit in this hole", which silently produces nonsense if the
    /// assumed slot size is wrong — and `opusenc`'s `frame-size` is a
    /// property `audio.rs` could plausibly start setting one day (2.5, 5,
    /// 10, 20, 40 or 60 ms are all legal). Taking the *minimum* is what
    /// makes this work at all under loss: a hole inflates a delta to a
    /// multiple of the frame duration, never to less than it, so lossy
    /// deltas can only ever be ignored by a `min`, not mistaken for the
    /// truth.
    fn frame_duration_us(&self) -> u64 {
        self.recent_deltas
            .iter()
            .copied()
            .min()
            .unwrap_or(AUDIO_DEFAULT_FRAME_DURATION_US)
    }

    fn record_delta(&mut self, delta_us: u64) {
        // Clamp to the range of legal Opus frame sizes (2.5 ms .. 60 ms):
        // a delta outside it is measurement noise or a discontinuity, and
        // letting it into the window would skew every later gap count.
        let delta = delta_us.clamp(2_500, 60_000);
        self.recent_deltas.push_back(delta);
        while self.recent_deltas.len() > AUDIO_FRAME_DURATION_WINDOW {
            self.recent_deltas.pop_front();
        }
    }

    /// Forget the current timeline (used on a PTS discontinuity). The
    /// learned frame duration is deliberately *kept*: it is a property of
    /// the sender's encoder configuration, not of the timeline, so a
    /// capture restart is no reason to re-enter the bootstrap window
    /// during which gaps go undetected.
    fn reset_timeline(&mut self) {
        self.pending.clear();
        self.last_released_pts = None;
    }

    /// Accept one decrypted Opus packet and return everything that became
    /// releasable as a result. Deterministic given `(pts_us, data, now)`
    /// and the buffer's state — no clock reads, no GStreamer, no sleeping.
    fn insert(&mut self, pts_us: u64, data: Vec<u8>, now: Instant) -> Vec<AudioAction> {
        if let Some(last) = self.last_released_pts {
            if pts_us.saturating_add(AUDIO_PTS_DISCONTINUITY_US) < last {
                // Far enough backwards that no amount of reordering
                // explains it: the sender restarted its capture session
                // and its PTS origin moved. Anything still held belongs to
                // the dead timeline.
                tracing::debug!(
                    "audio PTS discontinuity ({pts_us} << {last}); resetting jitter buffer to the new timeline"
                );
                self.reset_timeline();
            }
        }

        if let Some(last) = self.last_released_pts {
            if pts_us <= last {
                // Its slot is already gone downstream; pushing it now would
                // hand `opusdec` a backwards timestamp, which is worse than
                // the loss it was meant to repair. (Also covers exact
                // duplicates of the last released packet.)
                tracing::debug!("dropping audio packet pts={pts_us} that arrived after its slot ({last}) shipped");
                return self.drain(now);
            }
        }

        if self.pending.contains_key(&pts_us) {
            tracing::debug!("dropping duplicate audio packet pts={pts_us}");
            return self.drain(now);
        }

        self.pending.insert(pts_us, PendingAudioPacket { data, arrived: now });
        self.drain(now)
    }

    /// Time-driven counterpart to [`Self::insert`]: releases anything whose
    /// hold deadline has expired. Must be called periodically, because a
    /// hole in the stream means no `insert` is coming to drive it.
    fn tick(&mut self, now: Instant) -> Vec<AudioAction> {
        self.drain(now)
    }

    /// Release everything still held, in PTS order, without waiting or
    /// concealing — used on a clean shutdown so the tail of the stream
    /// isn't swallowed. No concealment because there is no point running
    /// PLC for audio that is about to be followed by EOS.
    fn flush(&mut self) -> Vec<AudioAction> {
        let mut out = Vec::with_capacity(self.pending.len());
        while let Some((pts_us, packet)) = self.pending.pop_first() {
            self.last_released_pts = Some(pts_us);
            out.push(AudioAction::Push { pts_us, data: packet.data });
        }
        out
    }

    /// The actual release policy. Repeatedly considers the lowest-PTS held
    /// packet and either releases it, conceals ahead of it, or stops and
    /// waits for its missing predecessor.
    fn drain(&mut self, now: Instant) -> Vec<AudioAction> {
        let mut out = Vec::new();

        loop {
            let Some((&pts, packet)) = self.pending.first_key_value() else {
                break;
            };

            let Some(last) = self.last_released_pts else {
                // First packet of a timeline: nothing to order it against
                // and nothing it could be a gap after. Release at once —
                // waiting here would be pure startup latency.
                self.release(pts, &mut out);
                continue;
            };

            let frame = self.frame_duration_us();
            // `pts > last` is guaranteed: `insert` drops anything at or
            // below `last_released_pts` before it can be queued.
            let delta = pts - last;

            // Treat the packet as contiguous if it lands within one and a
            // half frame durations of the last release — half a frame of
            // slack absorbs any non-exact timestamping without ever
            // reaching the next real slot.
            if delta <= frame + frame / 2 {
                self.record_delta(delta);
                self.release(pts, &mut out);
                continue;
            }

            // At least one slot looks missing. Wait a moment in case it is
            // merely late (UDP reordering), but not past this packet's own
            // hold deadline, and never past the depth cap.
            let overdue = now.saturating_duration_since(packet.arrived) >= AUDIO_JITTER_MAX_HOLD;
            let over_depth = self.pending.len() > AUDIO_JITTER_MAX_DEPTH;
            if !overdue && !over_depth {
                break;
            }

            // Bootstrap: with no delta observed yet, `frame` is still only
            // [`AUDIO_DEFAULT_FRAME_DURATION_US`], a guess — and this
            // branch is precisely the case where the guess disagrees with
            // reality. Converting a guess into a slot count would be
            // circular (a legitimate 40 ms-frame stream would look like an
            // unbroken run of 20 ms losses, and, because concealed deltas
            // are never recorded, it would never learn otherwise). So the
            // first delta of a timeline is *defined* to be the frame
            // duration and is never concealed. The cost is one hold
            // (≤ 40 ms) at stream start, and only when the very first
            // delta isn't the assumed 20 ms; note the packet was still
            // held first, so a merely-reordered second packet is repaired
            // normally rather than mislearned.
            if self.recent_deltas.is_empty() {
                self.record_delta(delta);
                self.release(pts, &mut out);
                continue;
            }

            // Give up on the missing packet(s) and tell `opusdec`, so the
            // hole is filled with concealment rather than with nothing.
            let missing = (delta / frame).saturating_sub(1).min(AUDIO_MAX_CONCEAL_SLOTS);
            for slot in 0..missing {
                out.push(AudioAction::Conceal {
                    pts_us: last + (slot + 1) * frame,
                    duration_us: frame,
                });
            }
            // Deliberately *not* `record_delta(delta)`: this delta is
            // inflated by the hole, so feeding it to the frame-duration
            // estimator would only ever make the estimate worse.
            self.release(pts, &mut out);
        }

        out
    }

    /// Move the packet at `pts` out of `pending` and onto the action list.
    fn release(&mut self, pts: u64, out: &mut Vec<AudioAction>) {
        let packet = self
            .pending
            .remove(&pts)
            .expect("release is only ever called for a pts just observed in `pending`");
        self.last_released_pts = Some(pts);
        out.push(AudioAction::Push { pts_us: pts, data: packet.data });
    }
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
    ///
    /// Packets may arrive here in any order and with holes; this method
    /// does not care. Ordering and loss handling happen once, downstream,
    /// in the pipeline thread's [`AudioJitterBuffer`] — see the module
    /// docs' "Reordering and packet-loss concealment" section.
    ///
    /// # Why this channel stays unbounded, unlike video's
    ///
    /// [`StreamDecoder::feed`] pushes onto a small `sync_channel` and drops
    /// on overflow; this one uses a plain unbounded `channel`. That
    /// asymmetry is deliberate on both counts:
    ///
    /// - **It is not the buffer that needed bounding.** The bug this path
    ///   had was a *missing* buffer (no reordering, no concealment), and
    ///   the one added — [`AudioJitterBuffer`] — is hard-capped at
    ///   [`AUDIO_JITTER_MAX_DEPTH`] packets and [`AUDIO_JITTER_MAX_HOLD`]
    ///   of delay. No unbounded buffer was introduced here.
    /// - **The volumes are not comparable.** Video queues whole H.264
    ///   access units (tens of KB, 30-60/s); audio queues Opus packets at
    ///   ~50/s of ~100 bytes each, so even a pathological multi-second
    ///   stall on the pipeline thread costs single-digit KB. Bounding that
    ///   buys nothing.
    /// - **Bounding it would risk a hang.** A `SyncSender` blocks when
    ///   full, and `Drop` *must* get its `Shutdown` through or the pipeline
    ///   thread never exits and `join()` never returns. This module's
    ///   `set_volume` is documented as non-blocking too. Trading a
    ///   few-KB memory bound for a new way to wedge the UI thread is a bad
    ///   deal — and this repo has already had to fix that exact shape of
    ///   hang twice (`AudioCaptureHandle::stop()`, `CaptureHandle::stop()`).
    ///
    /// If the pipeline thread ever does fall behind, the packets still
    /// arrive in order and simply get pushed late; the jitter buffer
    /// releases them contiguously without concealing anything, because a
    /// backlog is a latency problem, not a loss problem.
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

/// Builds the `opusdec` element with packet-loss concealment enabled.
///
/// Split out from [`run_audio_pipeline`] purely so the `plc` setting is
/// reachable from a test (everything else in that function is owned by a
/// background thread the moment it starts) — see
/// `production_opus_decoder_has_plc_enabled`. It is worth pinning down:
/// PLC is OFF by default in `opusdec` (verified: `gst-inspect-1.0 opusdec`
/// → "plc ... Boolean. Default: false"), so without this line every `GAP`
/// event [`AudioJitterBuffer`] emits would be silently ignored and every
/// lost packet would stay an audible hole — i.e. the concealment half of
/// this module's fix would be dead code, and nothing else would fail
/// loudly enough to notice.
///
/// `use-inband-fec` is deliberately left off: it needs the *sender* to
/// encode redundancy, and `audio.rs` builds `opusenc` with its default
/// `inband-fec=false`, so there would be no FEC data to recover from. This
/// is concealment, not recovery.
fn make_opus_decoder() -> Result<Element> {
    let decoder = ElementFactory::make("opusdec")
        .build()
        .context("opusdec element unavailable (is gst-plugins-base installed?)")?;
    decoder.set_property("plc", true);
    Ok(decoder)
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
    let decoder = make_opus_decoder()?;
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

/// Poll interval while [`AudioJitterBuffer`] is holding something. Has to
/// be well under [`AUDIO_JITTER_MAX_HOLD`] so a hold deadline fires close
/// to on time rather than up to a whole idle-poll late — the tick *is* the
/// buffer's clock.
const AUDIO_HELD_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Poll interval while the buffer is empty. Unchanged from the loop's
/// original timeout: with nothing held there is no deadline to miss, and
/// this only paces how often the GStreamer bus gets drained.
const AUDIO_IDLE_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Feed loop: runs incoming Opus packets through [`AudioJitterBuffer`] and
/// pushes what it releases into `appsrc`, applies volume changes to the
/// `volume` element, and drains the bus for error/warning logging in
/// between, until told to shut down. Mirrors video's `pump_loop`, plus the
/// jitter buffer (which video does not need — see the module docs).
fn pump_audio_loop(rx: &std_mpsc::Receiver<AudioPipelineMsg>, appsrc: &AppSrc, volume: &Element, bus: &Bus) -> Result<()> {
    // Both owned outright by this thread — see the module docs' audio
    // "Threading" note for why they need no lock.
    let mut jitter = AudioJitterBuffer::new();
    let mut timeline = AudioTimeline::new();

    loop {
        while let Some(msg) = bus.pop() {
            log_bus_message(&msg);
        }

        let timeout = if jitter.is_empty() {
            AUDIO_IDLE_POLL_INTERVAL
        } else {
            AUDIO_HELD_POLL_INTERVAL
        };

        match rx.recv_timeout(timeout) {
            Ok(AudioPipelineMsg::Packet { pts_us, data }) => {
                let actions = jitter.insert(pts_us, data, Instant::now());
                apply_audio_actions(appsrc, &mut timeline, actions);
            }
            Ok(AudioPipelineMsg::SetVolume(v)) => {
                volume.set_property("volume", v);
            }
            Ok(AudioPipelineMsg::Shutdown) => {
                apply_audio_actions(appsrc, &mut timeline, jitter.flush());
                return Ok(());
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => {
                // The buffer's clock tick: this is what lets a hole in the
                // stream be noticed at all, since no packet arrives to
                // drive `insert`.
                let actions = jitter.tick(Instant::now());
                apply_audio_actions(appsrc, &mut timeline, actions);
            }
            Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                apply_audio_actions(appsrc, &mut timeline, jitter.flush());
                return Ok(());
            }
        }
    }
}

/// How far *ahead* of this pipeline's current running time the first
/// packet of a timeline is scheduled by [`AudioTimeline`], and therefore
/// the steady-state playout latency the receiver adds on purpose.
///
/// Has to comfortably exceed [`AUDIO_JITTER_MAX_HOLD`] (40 ms): a packet
/// the jitter buffer held while waiting for a missing predecessor reaches
/// the sink that much later in wall-clock terms than its in-order
/// siblings, and anything that reaches a `sync=true` sink *after* its
/// scheduled playout time is dropped rather than played. The rest is
/// headroom for network jitter that the very first packet happened not to
/// suffer. 100 ms is inaudible as latency in a screensharing call and
/// leaves 60 ms of that headroom.
const AUDIO_TARGET_LEAD_US: i64 = 100_000;

/// How late a mapped packet may be (i.e. how far its scheduled playout
/// time may have slipped into the past) before [`AudioTimeline`] gives up
/// on the current mapping and re-bases. Generous compared to
/// [`AUDIO_TARGET_LEAD_US`] because a re-base is itself a discontinuity
/// the sink has to resynchronize over: better to let the sink drop a
/// couple of individually-late packets than to re-base on every jitter
/// spike.
const AUDIO_MAX_LATE_US: i64 = 200_000;

/// How far *beyond* [`AUDIO_TARGET_LEAD_US`] a mapped packet may drift
/// into the future before [`AudioTimeline`] re-bases. Catches the sender
/// restarting its capture session (PTS origin moves forwards) and the slow
/// accumulation from a sender clock that runs faster than this receiver's.
const AUDIO_MAX_EARLY_US: i64 = AUDIO_TARGET_LEAD_US + 1_000_000;

/// Maps the *sender's* capture-time PTS onto *this* pipeline's running
/// time, which is the clock the audio sink actually schedules against.
///
/// # Why this is needed at all
///
/// `pts_us` is the sender's own running time — how long its capture
/// pipeline had been running when the packet was encoded (see
/// `audio.rs::send_encoded_sample`). The receiver's audio sink runs with
/// `sync=true`, so it plays each buffer when *this* pipeline's running
/// time (0 at the moment [`run_audio_pipeline`] hits `Playing`, i.e. at
/// subscribe) reaches that buffer's PTS. The two clocks have no defined
/// relationship whatsoever: the offset between them is exactly "how long
/// the sender had been sharing audio before this viewer subscribed".
///
/// Pushing the sender's raw PTS therefore schedules the first packet that
/// many seconds into the receiver's future. Measured on this machine
/// (GStreamer 1.28.5) with `appsrc ! opusdec ! audioconvert !
/// audioresample ! volume ! <sink sync=true>`, feeding 61 real Opus
/// packets in real time:
///
/// | first PTS | buffers that reached the sink | buffers rendered |
/// | --- | --- | --- |
/// | at the pipeline's running time | 61 / 61 | 61 |
/// | 30 s ahead of it | 1 / 61 (`fakesink`), 6 / 61 (`autoaudiosink`) | 0 |
///
/// i.e. a viewer who joins 30 s after the sharer started gets **no audio
/// at all**, and the sink's blocked streaming thread back-pressures the
/// whole chain so packets pile up inside `appsrc` instead. This is the
/// same footgun [`PacingState`] documents on the video side, where it was
/// avoided by leaving the appsink at `sync=false` and pacing by hand; the
/// audio path cannot do that (there is no manual "display" step — the sink
/// *is* the clock), so it re-bases the timestamps instead.
///
/// # How
///
/// The first packet of a timeline is scheduled at `now +
/// `[`AUDIO_TARGET_LEAD_US`], and every later packet keeps its exact
/// spacing relative to it by carrying the same offset. So the sender's
/// intervals — the only part of its timeline that means anything here —
/// are preserved, while the origin becomes one this pipeline can act on.
/// The offset is recomputed only when the mapping has clearly stopped
/// describing reality ([`AUDIO_MAX_LATE_US`] / [`AUDIO_MAX_EARLY_US`]),
/// which covers a sender capture restart and long-run clock drift alike —
/// the same self-healing-on-discontinuity approach as [`PacingState`].
struct AudioTimeline {
    /// Added to a sender PTS to get a receiver running-time PTS. Signed:
    /// a sender that started before this viewer subscribed needs a
    /// negative offset. `None` until the first packet establishes it.
    offset_us: Option<i64>,
}

impl AudioTimeline {
    fn new() -> Self {
        Self { offset_us: None }
    }

    /// Map one packet's PTS, (re-)establishing the offset if there isn't a
    /// usable one. Pure/deterministic given its inputs — no clock reads, no
    /// GStreamer — so it's unit-testable on its own; see
    /// `tests::timeline_*`.
    fn map_push(&mut self, sender_pts_us: u64, running_now_us: u64) -> u64 {
        if let Some(offset) = self.offset_us {
            let target = sender_pts_us as i64 + offset;
            let lead = target - running_now_us as i64;
            if (-AUDIO_MAX_LATE_US..=AUDIO_MAX_EARLY_US).contains(&lead) {
                return target.max(0) as u64;
            }
            tracing::debug!(
                "re-basing audio timeline: sender pts={sender_pts_us} mapped to {lead} us from now, \
                 outside [{}, {}]",
                -AUDIO_MAX_LATE_US,
                AUDIO_MAX_EARLY_US
            );
        }

        let target = running_now_us as i64 + AUDIO_TARGET_LEAD_US;
        self.offset_us = Some(target - sender_pts_us as i64);
        target as u64
    }

    /// Map a concealment slot's PTS. Never re-bases: a `GAP` sits *between*
    /// two real packets and has to stay on whatever timeline they are on,
    /// and it carries no new information about how well that timeline is
    /// tracking. `None` before any packet has established an offset (which
    /// [`AudioJitterBuffer`] never actually produces — it only conceals
    /// after a release — but is not worth guessing at if it ever did).
    fn map_gap(&self, sender_pts_us: u64) -> Option<u64> {
        let offset = self.offset_us?;
        Some((sender_pts_us as i64 + offset).max(0) as u64)
    }
}

/// This pipeline's current running time in microseconds — the axis a
/// `sync=true` sink schedules playout on, and what [`AudioTimeline`] maps
/// sender PTS onto. `0` if the pipeline has no clock/base time yet (it
/// does by the time `Playing` is set, but a missing clock must not panic).
fn running_time_us(element: &AppSrc) -> u64 {
    element.current_running_time().map(|t| t.useconds()).unwrap_or(0)
}

/// Turns [`AudioJitterBuffer`]'s decisions into actual GStreamer calls,
/// re-basing each timestamp from the sender's timeline onto this
/// pipeline's via `timeline` (see [`AudioTimeline`] for why that is not
/// optional). Kept as thin as possible so essentially all of the logic
/// stays in the pipeline-free, unit-tested [`AudioJitterBuffer`] and
/// [`AudioTimeline`].
fn apply_audio_actions(appsrc: &AppSrc, timeline: &mut AudioTimeline, actions: Vec<AudioAction>) {
    if actions.is_empty() {
        return;
    }
    // One clock read per batch: a batch is emitted in a single pass with
    // no waiting in between, so reading it per action would only add noise
    // to timestamps that are meant to keep their exact relative spacing.
    let now_us = running_time_us(appsrc);

    for action in actions {
        match action {
            AudioAction::Push { pts_us, data } => {
                let mapped_pts_us = timeline.map_push(pts_us, now_us);
                let mut buffer = Buffer::from_slice(data);
                if let Some(buffer_mut) = buffer.get_mut() {
                    buffer_mut.set_pts(ClockTime::from_useconds(mapped_pts_us));
                    // Deliberately no `set_duration`: libopus derives each
                    // packet's real duration from its TOC byte, and
                    // stamping a *guessed* duration that disagrees with it
                    // would be worse than stamping none.
                }
                // Same "glitch and continue" policy as video: a bad packet
                // downstream of a dropped fragment is logged and skipped,
                // never fatal.
                if let Err(e) = appsrc.push_buffer(buffer) {
                    tracing::warn!("appsrc dropped an audio packet: {e:?}");
                }
            }
            AudioAction::Conceal { pts_us, duration_us } => {
                let Some(mapped_pts_us) = timeline.map_gap(pts_us) else {
                    tracing::debug!("skipping GAP at pts={pts_us}: no audio timeline established yet");
                    continue;
                };
                let gap = gstreamer::event::Gap::builder(ClockTime::from_useconds(mapped_pts_us))
                    .duration(ClockTime::from_useconds(duration_us))
                    .build();
                // `Gap` is a serialized event, so `appsrc` queues it in
                // its own internal queue alongside buffers instead of
                // letting it overtake them — which is what keeps the
                // concealed frame in its correct slot.
                if !appsrc.send_event(gap) {
                    tracing::debug!("appsrc refused a GAP event at pts={pts_us}; loss will not be concealed");
                }
            }
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

    /// First frame is always displayed immediately (no delay), and it
    /// establishes the `last` reference point later frames are paced
    /// against.
    #[test]
    fn pacing_first_frame_has_no_delay() {
        let mut pacing = PacingState::new();
        let now = Instant::now();
        assert_eq!(pacing.decide(1_000_000, now), PacingDecision::Display(None));
        assert_eq!(pacing.last, Some((1_000_000, now)));
        assert_eq!(pacing.pts_budget, Duration::ZERO);
    }

    /// A frame whose PTS is ahead of elapsed wall-clock time (decode kept
    /// up and finished early) should be paced: delayed by exactly the
    /// remaining gap, not displayed immediately.
    #[test]
    fn pacing_sleeps_for_frames_ahead_of_schedule() {
        let mut pacing = PacingState::new();
        let start = Instant::now();
        assert_eq!(pacing.decide(0, start), PacingDecision::Display(None));

        // Second frame is 33ms later by PTS, but only 5ms of wall-clock
        // time actually elapsed decoding it: should wait out the other 28ms.
        let now = start + Duration::from_millis(5);
        assert_eq!(pacing.decide(33_000, now), PacingDecision::Display(Some(Duration::from_millis(28))));
        assert_eq!(pacing.last, Some((33_000, now)));
    }

    /// While the viewer keeps up exactly (each frame's `dt_wall` matches
    /// its `dt_pts`), the pts-budget never builds up a backlog and nothing
    /// is ever skipped — this is the ordinary, common-case behavior the
    /// skip mechanism must never disturb.
    #[test]
    fn pacing_never_skips_when_perfectly_on_pace() {
        let mut pacing = PacingState::new();
        let start = Instant::now();
        assert_eq!(pacing.decide(0, start), PacingDecision::Display(None));

        let mut now = start;
        for i in 1..200u64 {
            now += Duration::from_micros(16_667);
            assert_eq!(
                pacing.decide(i * 16_667, now),
                PacingDecision::Display(None),
                "frame {i}: matched rate must never skip or need to sleep"
            );
        }
        assert_eq!(pacing.pts_budget, Duration::ZERO, "budget must not accumulate when perfectly on pace");
    }

    /// An isolated moderate stall (a single decode-to-decode gap well behind
    /// the sender's output, but nowhere near `PACING_HARD_STALL_GAP`) costs
    /// at most one skipped frame's worth of catch-up, then playback returns
    /// immediately to normal display — it must not turn into sustained
    /// skipping. This replaces the old design's "an isolated badly-behind
    /// frame must still display" test: under the pts-budget mechanism, a
    /// single ~450ms deficit *is* worth skipping exactly one frame to pay
    /// down (rather than silently letting the viewer sit 450ms further
    /// behind, which the old re-base-and-still-display behavior did) — see
    /// the module docs' "Why a hard reset alone isn't enough" section for
    /// why that's the intended trade rather than a regression.
    #[test]
    fn pacing_skips_isolated_moderate_stall_then_recovers() {
        let mut pacing = PacingState::new();
        let start = Instant::now();
        assert_eq!(pacing.decide(0, start), PacingDecision::Display(None));

        // Decode stalled for 500ms (well under PACING_HARD_STALL_GAP) while
        // PTS only advanced 50ms: budget (50ms) can't afford the 500ms real
        // cost, so this one frame is skipped.
        let now = start + Duration::from_millis(500);
        assert_eq!(pacing.decide(50_000, now), PacingDecision::Skip, "a 450ms deficit costs exactly one skip");

        // Normal spacing resumes (60fps-ish, dt_pts == dt_wall): every
        // subsequent frame must display — the single skip already paid the
        // whole deficit down, nothing carries forward indefinitely.
        let mut now = now;
        let mut pts = 50_000u64;
        for i in 0..30u64 {
            now += Duration::from_micros(16_667);
            pts += 16_667;
            assert_eq!(
                pacing.decide(pts, now),
                PacingDecision::Display(None),
                "frame {i} after the stall: must have fully recovered, not still paying it down"
            );
        }
    }

    /// A single decode-to-decode gap larger than `PACING_HARD_STALL_GAP`
    /// (system suspend/resume, debugger pause, ...) must hard-reset the
    /// pacer and display the frame immediately — not spend potentially
    /// hundreds of skipped frames draining a multi-second backlog through
    /// the ordinary pts-budget mechanism.
    #[test]
    fn pacing_hard_resets_after_pathological_stall_gap() {
        let mut pacing = PacingState::new();
        let start = Instant::now();
        assert_eq!(pacing.decide(0, start), PacingDecision::Display(None));

        let now = start + PACING_HARD_STALL_GAP + Duration::from_secs(1);
        assert_eq!(
            pacing.decide(16_667, now),
            PacingDecision::Display(None),
            "a multi-second gap must hard-reset and display immediately, not skip"
        );
        assert_eq!(pacing.pts_budget, Duration::ZERO, "hard reset must zero the budget");
        assert_eq!(pacing.last, Some((16_667, now)));
    }

    /// A PTS discontinuity (stream restart / first keyframe after
    /// resubscribing) can make the new `pts_us` smaller than the previous
    /// frame's — `saturating_sub` must not panic — and must self-heal
    /// (at most a single skip while the budget catches up) rather than
    /// stalling forever or panicking.
    #[test]
    fn pacing_recovers_from_pts_discontinuity_without_panicking() {
        let mut pacing = PacingState::new();
        let start = Instant::now();
        assert_eq!(pacing.decide(5_000_000, start), PacingDecision::Display(None));

        // New stream's PTS restarts near zero, far below the previous
        // frame's (`saturating_sub` clamps `dt_pts` to 0 rather than
        // panicking); a plausible resubscribe gap of 350ms elapses too.
        let now = start + Duration::from_millis(350);
        assert_eq!(
            pacing.decide(1_000, now),
            PacingDecision::Skip,
            "zero pts progress can't afford a 350ms real-time cost"
        );

        // Normal spacing resumes immediately after.
        let now2 = now + Duration::from_micros(16_667);
        assert_eq!(
            pacing.decide(1_000 + 16_667, now2),
            PacingDecision::Display(None),
            "must recover on the very next frame, not stall forever"
        );
    }

    /// The scenario this whole redesign targets, using QC's exact numbers:
    /// a 240fps sender (`dt_pts` ~= 4167us) against a viewer whose
    /// decode/display loop only actually manages to cycle every ~8333us
    /// (120fps-equivalent throughput). The old consecutive-rebase design
    /// produced `skipped == 0` over 2000 such frames (the bug this test
    /// guards against); the pts-budget accumulator must instead converge to
    /// skipping close to the theoretical `1 - dt_pts/dt_wall == 50%`.
    ///
    /// The exact steady-state ratio is not asserted as an exact percentage
    /// (a slightly different accumulator design could land at, say, 49% or
    /// 51% and still be correct) — instead a generous-but-meaningful band
    /// around 50% is asserted, wide enough to tolerate implementation
    /// variance but tight enough that 0% or ~100% (both real bugs this test
    /// must catch) fail it.
    #[test]
    fn pacing_settles_near_theoretical_skip_ratio_for_240fps_over_120fps_viewer() {
        let mut pacing = PacingState::new();
        let start = Instant::now();
        assert_eq!(pacing.decide(0, start), PacingDecision::Display(None));

        const DT_PTS: Duration = Duration::from_micros(4_167);
        const DT_WALL: Duration = Duration::from_micros(8_333);
        const NUM_FRAMES: u64 = 2000;

        let mut now = start;
        let mut pts = 0u64;
        let mut skipped = 0u64;
        let mut displayed = 0u64;
        for _ in 0..NUM_FRAMES {
            now += DT_WALL;
            pts += DT_PTS.as_micros() as u64;
            match pacing.decide(pts, now) {
                PacingDecision::Skip => skipped += 1,
                PacingDecision::Display(_) => displayed += 1,
            }
        }

        assert_eq!(skipped + displayed, NUM_FRAMES);
        let skip_fraction = skipped as f64 / NUM_FRAMES as f64;
        assert!(
            (0.40..=0.60).contains(&skip_fraction),
            "expected a skip fraction near the theoretical 1 - 4167/8333 ~= 50% \
             (proportional to the 2x rate mismatch), got {:.1}% ({skipped}/{NUM_FRAMES}) \
             -- 0% means the trigger never engages (the original bug), ~100% means it \
             skips everything with no recovery (the secondary bug)",
            skip_fraction * 100.0
        );
    }

    /// Same idea as the 240/120 test above but with a different, non-2x
    /// ratio (300fps sender vs. a 100fps-sustained viewer, a 3x mismatch,
    /// theoretical skip fraction `1 - 3333/10000 ~= 66.7%`) — checked so the
    /// proportionality isn't an artifact tuned to the specific 2x case.
    #[test]
    fn pacing_settles_near_theoretical_skip_ratio_for_300fps_over_100fps_viewer() {
        let mut pacing = PacingState::new();
        let start = Instant::now();
        assert_eq!(pacing.decide(0, start), PacingDecision::Display(None));

        const DT_PTS: Duration = Duration::from_micros(3_333);
        const DT_WALL: Duration = Duration::from_micros(10_000);
        const NUM_FRAMES: u64 = 3000;

        let mut now = start;
        let mut pts = 0u64;
        let mut skipped = 0u64;
        let mut displayed = 0u64;
        for _ in 0..NUM_FRAMES {
            now += DT_WALL;
            pts += DT_PTS.as_micros() as u64;
            match pacing.decide(pts, now) {
                PacingDecision::Skip => skipped += 1,
                PacingDecision::Display(_) => displayed += 1,
            }
        }

        assert_eq!(skipped + displayed, NUM_FRAMES);
        let skip_fraction = skipped as f64 / NUM_FRAMES as f64;
        assert!(
            (0.55..=0.78).contains(&skip_fraction),
            "expected a skip fraction near the theoretical 1 - 3333/10000 ~= 66.7%, \
             got {:.1}% ({skipped}/{NUM_FRAMES})",
            skip_fraction * 100.0
        );
    }

    /// The exact "fast-forward catch-up" repro shape from the plan doc:
    /// `decide` simply isn't called for a real stretch of wall-clock time
    /// (modeling an upstream stall -- an unbounded/throttled event
    /// channel, a UI thread parked behind a focus-gated repaint, ...),
    /// then the whole backlog that piled up arrives at `decide` in a tight
    /// loop -- many calls back to back with essentially zero elapsed
    /// wall-clock time between them, each with normal PTS spacing. Without
    /// the staleness backstop, every one of these lands in the "ahead of
    /// schedule" branch (`dt_wall` ~= 0 < `dt_pts`) and gets paced out
    /// with a real-time sleep -- a literal real-time replay of the whole
    /// backlog. With the backstop, the frames that are still far in the
    /// past relative to *right now* (most of the backlog) must be skipped
    /// outright instead.
    #[test]
    fn pacing_staleness_backstop_skips_stale_backlog_after_delivery_gap() {
        let mut pacing = PacingState::new();
        let start = Instant::now();
        assert_eq!(pacing.decide(0, start), PacingDecision::Display(None));

        const FRAME_INTERVAL_US: u64 = 33_333; // ~30fps
        const GAP: Duration = Duration::from_millis(600);
        const BACKLOG_FRAMES: u64 = 20;

        // The whole backlog "arrives" at the same instant: real wall-clock
        // time did not advance between these decide() calls, only PTS did
        // -- exactly what draining a backed-up channel in a tight loop
        // looks like.
        let burst_now = start + GAP;

        let mut skipped = 0u64;
        let mut displayed = 0u64;
        let mut total_sleep = Duration::ZERO;
        for i in 1..=BACKLOG_FRAMES {
            let pts = i * FRAME_INTERVAL_US;
            match pacing.decide(pts, burst_now) {
                PacingDecision::Skip => skipped += 1,
                PacingDecision::Display(delay) => {
                    displayed += 1;
                    total_sleep += delay.unwrap_or(Duration::ZERO);
                }
            }
        }
        assert_eq!(skipped + displayed, BACKLOG_FRAMES);

        assert!(
            skipped >= 8,
            "most of a 600ms backlog delivered in a tight loop must be rejected as stale, got only \
             {skipped}/{BACKLOG_FRAMES} skipped"
        );
        assert!(
            displayed > 0,
            "the tail of the backlog, once close enough to real time, must still display -- the \
             backstop must not turn into a permanent stall"
        );
        // The bug this backstop exists to prevent is exactly "every
        // backlogged frame gets displayed, paced out with a real-time
        // sleep": unmitigated, this loop would accumulate close to
        // BACKLOG_FRAMES * FRAME_INTERVAL (~667ms) of total sleep across
        // the burst. Assert it stays well below that.
        assert!(
            total_sleep < Duration::from_millis(500),
            "backlog must not be paced through with real-time sleeps like the fast-forward-catch-up \
             bug did -- total accumulated sleep across the whole burst was {total_sleep:?}"
        );

        // A concrete, non-fuzzy check that the backstop -- not just the
        // ordinary pts_budget skip on the very first post-gap frame -- is
        // what's rejecting the backlog: the second backlog frame lands
        // squarely in the "ahead of schedule" branch (dt_wall ~= 0 <<
        // dt_pts) purely by the relative dt_wall/dt_pts comparison, and is
        // still deep in the stale part of the backlog (~567ms of absolute
        // staleness), so it must be skipped.
        let mut pacing2 = PacingState::new();
        assert_eq!(pacing2.decide(0, start), PacingDecision::Display(None));
        assert_eq!(pacing2.decide(FRAME_INTERVAL_US, burst_now), PacingDecision::Skip);
        assert_eq!(
            pacing2.decide(2 * FRAME_INTERVAL_US, burst_now),
            PacingDecision::Skip,
            "a frame in the 'ahead of schedule' branch that is still hundreds of ms stale in \
             absolute wall-clock terms must be rejected by the backstop"
        );
    }

    /// Companion to the burst test above: once the backlog has actually
    /// drained down to something close to real time, ordinary matched-rate
    /// playback resumes normally -- the backstop must not leave the pacer
    /// permanently stuck skipping, and it must not interfere at all once
    /// frames are arriving at a genuine (non-bursty) real-time cadence.
    #[test]
    fn pacing_staleness_backstop_recovers_to_normal_playback_after_backlog_drains() {
        let mut pacing = PacingState::new();
        let start = Instant::now();
        assert_eq!(pacing.decide(0, start), PacingDecision::Display(None));

        const FRAME_INTERVAL_US: u64 = 33_333;
        const GAP: Duration = Duration::from_millis(600);
        const BACKLOG_FRAMES: u64 = 20;

        let burst_now = start + GAP;
        for i in 1..=BACKLOG_FRAMES {
            pacing.decide(i * FRAME_INTERVAL_US, burst_now);
        }

        // Playback now resumes at a genuine, matched real-time cadence
        // (not another burst): each subsequent frame's `dt_wall` actually
        // elapses in step with its `dt_pts`, exactly like ordinary
        // steady-state playback elsewhere in this file's tests. The
        // backstop only ever gates the "ahead of schedule" branch
        // (`dt_wall < dt_pts`), which matched-rate playback never enters
        // (`dt_wall == dt_pts` every step, never strictly less) -- so
        // every one of these frames must display, regardless of how the
        // preceding burst left `pts_budget` or `epoch`.
        let mut now = burst_now;
        let mut pts = BACKLOG_FRAMES * FRAME_INTERVAL_US;
        for i in 0..30u64 {
            now += Duration::from_micros(FRAME_INTERVAL_US);
            pts += FRAME_INTERVAL_US;
            assert_eq!(
                pacing.decide(pts, now),
                PacingDecision::Display(None),
                "frame {i} after the backlog drained: matched-rate playback must have fully \
                 recovered, not still be affected by the earlier burst"
            );
        }
    }

    // -----------------------------------------------------------------
    // Viewer-side stall signal (`StreamDecoder::is_stalled`)
    // -----------------------------------------------------------------
    //
    // These use the injected-`now` `is_stalled_at` rather than sleeping, the
    // same discipline as the `pacing_*` tests above.

    fn stall_test_frame(stalled: bool) -> DecryptedMediaFrame {
        DecryptedMediaFrame {
            conn_id: 1,
            stream_id: 0,
            pts: 0,
            keyframe: true,
            stalled,
            // Not decodable, and deliberately so: `is_stalled` is about frame
            // *arrival*, not decode success, and must not depend on the
            // pipeline managing to make a picture out of anything.
            payload: vec![0, 0, 0, 1, 0x65],
        }
    }

    /// A decoder that has just been created hasn't missed anything yet — but
    /// a subscription that never receives a single frame does eventually count
    /// as stalled, measured from creation.
    #[test]
    fn a_fresh_decoder_is_not_immediately_reported_as_stalled() {
        gstreamer::init().expect("gstreamer init");
        let created = Instant::now();
        let decoder = StreamDecoder::new(0).expect("failed to build StreamDecoder");
        assert!(!decoder.is_stalled_at(created + Duration::from_millis(500)));
        assert!(
            decoder.is_stalled_at(created + NO_FRAME_STALL_TIMEOUT + Duration::from_secs(1)),
            "a stream that never delivered anything at all should eventually report stalled too"
        );
    }

    /// The graceful path: the sender is still streaming, but says its capture
    /// is down. Reported immediately, without waiting out the silence timeout.
    #[test]
    fn the_senders_advisory_stalled_flag_is_reported_immediately() {
        gstreamer::init().expect("gstreamer init");
        let mut decoder = StreamDecoder::new(0).expect("failed to build StreamDecoder");

        decoder.feed(&stall_test_frame(false));
        assert!(!decoder.is_stalled_at(Instant::now()));

        decoder.feed(&stall_test_frame(true));
        assert!(
            decoder.is_stalled_at(Instant::now()),
            "a frame carrying MEDIA_FLAG_STALLED must flip the signal on the spot — the sender is still \
             sending, so the silence backstop would never fire on its own"
        );

        // ...and clears again the moment real content resumes.
        decoder.feed(&stall_test_frame(false));
        assert!(!decoder.is_stalled_at(Instant::now()));
    }

    /// The ungraceful path: a sender that died without being able to signal
    /// anything. Nothing arrives at all, so only the local silence timer can
    /// catch it.
    #[test]
    fn silence_longer_than_the_timeout_is_reported_as_stalled() {
        gstreamer::init().expect("gstreamer init");
        let mut decoder = StreamDecoder::new(0).expect("failed to build StreamDecoder");

        let arrival = Instant::now();
        decoder.feed(&stall_test_frame(false));

        assert!(!decoder.is_stalled_at(arrival + Duration::from_millis(500)));
        assert!(
            !decoder.is_stalled_at(arrival + NO_FRAME_STALL_TIMEOUT),
            "exactly at the timeout is not yet a stall"
        );
        assert!(
            decoder.is_stalled_at(arrival + NO_FRAME_STALL_TIMEOUT + Duration::from_millis(1)),
            "no frame at all for longer than the timeout must report stalled even with no flag from the sender"
        );
    }

    /// The two halves of the signal must not race: a stalled-but-alive sender
    /// keeps re-sending at `STALL_RESEND_INTERVAL`, so the silence backstop can
    /// only ever fire for a sender that has genuinely gone quiet — never merely
    /// because the keepalive is slow.
    #[test]
    fn the_stall_keepalive_cadence_stays_well_inside_the_silence_backstop() {
        gstreamer::init().expect("gstreamer init");

        // The constants themselves have to hold this relationship; if someone
        // raises the keepalive interval past the silence timeout, every stalled
        // stream would start double-reporting for reasons that look like a bug
        // in the sender.
        assert!(
            crate::capture::destination::STALL_RESEND_INTERVAL * 2 < NO_FRAME_STALL_TIMEOUT,
            "the stall keepalive must fire several times over inside the silence timeout"
        );

        let mut decoder = StreamDecoder::new(0).expect("failed to build StreamDecoder");
        // Frames arriving at the keepalive cadence — but *without* the flag,
        // isolating the silence half — must never trip the backstop. Each
        // iteration asks "if the next keepalive were due right now, would the
        // backstop already have fired?", which is the question that matters.
        for _ in 0..10 {
            decoder.feed(&stall_test_frame(false));
            assert!(
                !decoder.is_stalled_at(Instant::now() + crate::capture::destination::STALL_RESEND_INTERVAL),
                "the silence backstop fired within one keepalive interval of a delivered frame"
            );
        }
    }

    /// A stream that hasn't hit its first keyframe yet is *not* stalled — the
    /// sender is plainly talking to us, this decoder just can't use the frames
    /// yet. Guards the ordering inside `feed` (bookkeeping before the
    /// pre-keyframe gate).
    #[test]
    fn frames_dropped_before_the_first_keyframe_still_count_as_liveness() {
        gstreamer::init().expect("gstreamer init");
        let mut decoder = StreamDecoder::new(0).expect("failed to build StreamDecoder");

        let mut delta = stall_test_frame(false);
        delta.keyframe = false;
        for _ in 0..5 {
            decoder.feed(&delta);
        }
        assert!(!decoder.seen_keyframe, "the test's premise: nothing was actually accepted for decode");
        assert!(
            !decoder.is_stalled_at(Instant::now()),
            "pre-keyframe frames are still evidence the sender is alive"
        );
    }

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
                stalled: false,
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
                stalled: false,
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
        decoder.feed(&DecryptedMediaFrame {
            conn_id: 1,
            stream_id: 200,
            pts: 0,
            keyframe: true,
            stalled: false,
            payload: vec![1, 2, 3],
        });

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

    // -----------------------------------------------------------------
    // AudioJitterBuffer — pure reordering / gap-detection logic
    //
    // These are the audio counterpart of the `pacing_*` tests above: the
    // whole decision-making is a plain struct over `(pts, data, Instant)`
    // with no GStreamer and no sleeping in it, so every case below is
    // deterministic and runs instantly. The one thing they can't prove —
    // that a `GAP` event actually makes `opusdec` conceal — is covered
    // separately by `gap_event_makes_opusdec_conceal_a_missing_packet`.
    // -----------------------------------------------------------------

    /// Distinguishable stand-in payload; the buffer never looks inside it.
    fn opus_stub(tag: u8) -> Vec<u8> {
        vec![tag; 4]
    }

    fn pushed(actions: &[AudioAction]) -> Vec<u64> {
        actions
            .iter()
            .filter_map(|a| match a {
                AudioAction::Push { pts_us, .. } => Some(*pts_us),
                _ => None,
            })
            .collect()
    }

    fn concealed(actions: &[AudioAction]) -> Vec<u64> {
        actions
            .iter()
            .filter_map(|a| match a {
                AudioAction::Conceal { pts_us, .. } => Some(*pts_us),
                _ => None,
            })
            .collect()
    }

    /// Feeds `pts` values in order at the same instant and returns every
    /// action produced, so tests can set a buffer up to a known state.
    fn feed_in_order(buffer: &mut AudioJitterBuffer, ptss: &[u64], now: Instant) -> Vec<AudioAction> {
        let mut out = Vec::new();
        for (i, &pts) in ptss.iter().enumerate() {
            out.extend(buffer.insert(pts, opus_stub(i as u8), now));
        }
        out
    }

    /// The steady state, and the single most important property of this
    /// design: a healthy in-order stream is released packet-for-packet with
    /// **no delay and no concealment**. The buffer must not be a pre-roll —
    /// see [`AudioJitterBuffer`]'s doc comment for why holding packets on
    /// principle would make late audio later, not safer.
    #[test]
    fn jitter_releases_an_in_order_stream_with_zero_delay() {
        let mut buffer = AudioJitterBuffer::new();
        let now = Instant::now();

        for i in 0..10u64 {
            let actions = buffer.insert(i * 20_000, opus_stub(i as u8), now);
            assert_eq!(
                pushed(&actions),
                vec![i * 20_000],
                "each in-order packet must be released by the very insert that delivered it"
            );
            assert!(concealed(&actions).is_empty(), "a lossless stream must never trigger concealment");
        }
        assert!(buffer.is_empty(), "nothing should still be held after an in-order stream");
    }

    /// Two packets that raced over UDP and landed swapped must come back
    /// out in PTS order, with no concealment — this is a reordering event,
    /// not a loss, and telling `opusdec` a packet was lost when it wasn't
    /// would replace good audio with PLC.
    #[test]
    fn jitter_reorders_a_swapped_pair_without_concealing() {
        let mut buffer = AudioJitterBuffer::new();
        let t0 = Instant::now();

        assert_eq!(pushed(&buffer.insert(0, opus_stub(0), t0)), vec![0]);

        // 40_000 overtakes 20_000: it must be held, not pushed.
        let held = buffer.insert(40_000, opus_stub(2), t0);
        assert!(held.is_empty(), "an out-of-sequence packet must be held, not pushed immediately");
        assert!(!buffer.is_empty());

        // The straggler shows up 5ms later, well inside the hold window.
        let actions = buffer.insert(20_000, opus_stub(1), t0 + Duration::from_millis(5));
        assert_eq!(pushed(&actions), vec![20_000, 40_000], "both packets must be released in PTS order");
        assert!(
            concealed(&actions).is_empty(),
            "a packet that merely arrived late must not be reported to opusdec as lost"
        );
        assert!(buffer.is_empty());
    }

    /// A genuinely lost packet: after the hold window expires with no
    /// straggler, the buffer must emit a `Conceal` for the missing slot
    /// *before* releasing the packet that follows it, so `opusdec` runs PLC
    /// in the right place instead of leaving a hole.
    #[test]
    fn jitter_conceals_a_lost_packet_once_the_hold_expires() {
        let mut buffer = AudioJitterBuffer::new();
        let t0 = Instant::now();

        // Two good packets first, so the 20ms frame duration is learned.
        feed_in_order(&mut buffer, &[0, 20_000], t0);

        // 40_000 is lost; 60_000 arrives.
        let held = buffer.insert(60_000, opus_stub(3), t0);
        assert!(held.is_empty(), "must wait before declaring a loss — it could just be reordering");

        // Still nothing just before the deadline.
        let early = buffer.tick(t0 + AUDIO_JITTER_MAX_HOLD - Duration::from_millis(1));
        assert!(early.is_empty(), "must not give up on the missing packet before the hold expires");

        let actions = buffer.tick(t0 + AUDIO_JITTER_MAX_HOLD);
        assert_eq!(
            actions,
            vec![
                AudioAction::Conceal { pts_us: 40_000, duration_us: 20_000 },
                AudioAction::Push { pts_us: 60_000, data: opus_stub(3) },
            ],
            "the gap must be announced at the missing packet's own slot, ahead of the next real packet"
        );
        assert!(buffer.is_empty());
    }

    /// The depth cap is the buffer's hard ceiling: once more than
    /// [`AUDIO_JITTER_MAX_DEPTH`] packets are held, the head is released
    /// immediately even though its hold window hasn't expired. Without this
    /// a sustained one-way hole would let the buffer (and latency) grow.
    #[test]
    fn jitter_depth_cap_forces_release_before_the_hold_expires() {
        let mut buffer = AudioJitterBuffer::new();
        let t0 = Instant::now();
        feed_in_order(&mut buffer, &[0, 20_000], t0);

        // 40_000 is lost. Everything after it arrives at the *same instant*,
        // so no hold deadline can ever expire — only the depth cap can
        // break the deadlock.
        let mut released_at = None;
        for i in 0..(AUDIO_JITTER_MAX_DEPTH + 1) {
            let pts = 60_000 + (i as u64) * 20_000;
            let actions = buffer.insert(pts, opus_stub(9), t0);
            if !actions.is_empty() {
                released_at = Some((i, actions));
                break;
            }
            assert!(
                buffer.pending.len() <= AUDIO_JITTER_MAX_DEPTH,
                "buffer exceeded its own depth cap while still holding everything"
            );
        }

        let (i, actions) = released_at.expect("the depth cap must eventually force a release");
        assert_eq!(
            i, AUDIO_JITTER_MAX_DEPTH,
            "release should happen exactly when the cap is exceeded, not earlier or later"
        );
        assert_eq!(concealed(&actions), vec![40_000], "the lost slot must still be concealed on a forced release");
        assert!(buffer.is_empty(), "a forced release must drain the whole backlog, not just one packet");
    }

    /// A packet whose slot has already been shipped downstream must be
    /// dropped, not pushed. Pushing it would hand `opusdec` a backwards
    /// timestamp, which is a worse defect than the loss it would repair.
    #[test]
    fn jitter_drops_a_packet_that_arrives_after_its_slot_shipped() {
        let mut buffer = AudioJitterBuffer::new();
        let t0 = Instant::now();
        feed_in_order(&mut buffer, &[0, 20_000, 40_000], t0);

        let late = buffer.insert(20_000, opus_stub(1), t0 + Duration::from_millis(100));
        assert!(late.is_empty(), "a packet later than the last released one must never be pushed");
        assert_eq!(buffer.last_released_pts, Some(40_000), "a late packet must not rewind the timeline");
        assert!(buffer.is_empty());
    }

    /// Duplicate delivery (QUIC datagrams can be duplicated by middleboxes,
    /// and a retransmitted fragment can complete the same frame twice) must
    /// not produce two pushes of the same slot.
    #[test]
    fn jitter_drops_duplicate_packets() {
        let mut buffer = AudioJitterBuffer::new();
        let t0 = Instant::now();
        feed_in_order(&mut buffer, &[0, 20_000], t0);

        // Duplicate of a packet still held (it's out of sequence, so it
        // stays in `pending` rather than being released immediately).
        assert!(buffer.insert(80_000, opus_stub(4), t0).is_empty());
        assert!(buffer.insert(80_000, opus_stub(4), t0).is_empty());
        assert_eq!(buffer.pending.len(), 1, "a duplicate must not occupy a second slot in the buffer");

        let actions = buffer.tick(t0 + AUDIO_JITTER_MAX_HOLD);
        assert_eq!(pushed(&actions), vec![80_000], "the duplicated packet must be pushed exactly once");
    }

    /// A sender restarting its capture session restarts its PTS origin near
    /// zero (`audio.rs` timestamps against a fresh `Instant`). Without a
    /// reset every subsequent packet would look "late" forever and audio
    /// would go permanently silent. Same self-healing intent as
    /// [`PacingState`]'s re-base on the video side.
    #[test]
    fn jitter_resets_on_a_backwards_pts_discontinuity() {
        let mut buffer = AudioJitterBuffer::new();
        let t0 = Instant::now();
        feed_in_order(&mut buffer, &[10_000_000, 10_020_000, 10_040_000], t0);
        assert_eq!(buffer.last_released_pts, Some(10_040_000));

        // New session: PTS restarts near zero.
        let actions = buffer.insert(0, opus_stub(0), t0 + Duration::from_millis(500));
        assert_eq!(pushed(&actions), vec![0], "the first packet of the new timeline must play, not be discarded");
        assert!(
            concealed(&actions).is_empty(),
            "a restart is a discontinuity, not a 500-second hole to conceal"
        );

        // And the new timeline continues normally, with the frame duration
        // learned from the old one carried over (no second bootstrap).
        let actions = buffer.insert(20_000, opus_stub(1), t0 + Duration::from_millis(520));
        assert_eq!(pushed(&actions), vec![20_000]);
        assert!(concealed(&actions).is_empty());
    }

    /// The frame duration is inferred, not hardcoded: a sender configured
    /// with `opusenc frame-size=40` must not have every one of its packets
    /// read as a 20ms loss. Costs one bootstrap hold at stream start, then
    /// gap detection works on the real 40ms grid.
    #[test]
    fn jitter_learns_a_frame_duration_other_than_the_20ms_default() {
        let mut buffer = AudioJitterBuffer::new();
        let t0 = Instant::now();

        assert_eq!(pushed(&buffer.insert(0, opus_stub(0), t0)), vec![0]);

        // 40ms delta doesn't match the assumed 20ms grid, so it's held
        // first (it could be a reorder) and then accepted as the real frame
        // duration — never concealed, since nothing has been learned yet.
        assert!(buffer.insert(40_000, opus_stub(1), t0).is_empty());
        let actions = buffer.tick(t0 + AUDIO_JITTER_MAX_HOLD);
        assert_eq!(pushed(&actions), vec![40_000]);
        assert!(
            concealed(&actions).is_empty(),
            "the first delta of a stream defines the frame duration; it must not be mistaken for a loss"
        );
        assert_eq!(buffer.frame_duration_us(), 40_000, "the 40ms frame duration should now be learned");

        // On the learned grid, a further 40ms step is contiguous...
        let t1 = t0 + AUDIO_JITTER_MAX_HOLD;
        let actions = buffer.insert(80_000, opus_stub(2), t1);
        assert_eq!(pushed(&actions), vec![80_000]);
        assert!(concealed(&actions).is_empty());

        // ...and an 80ms step is exactly one missing 40ms slot, not two
        // missing 20ms ones.
        assert!(buffer.insert(160_000, opus_stub(4), t1).is_empty());
        let actions = buffer.tick(t1 + AUDIO_JITTER_MAX_HOLD);
        assert_eq!(
            actions,
            vec![
                AudioAction::Conceal { pts_us: 120_000, duration_us: 40_000 },
                AudioAction::Push { pts_us: 160_000, data: opus_stub(4) },
            ]
        );
    }

    /// A long outage must not turn into an unbounded burst of concealment
    /// events. Beyond [`AUDIO_MAX_CONCEAL_SLOTS`] the buffer accepts the
    /// discontinuity and moves on — Opus PLC has decayed to near-silence
    /// long before that anyway.
    #[test]
    fn jitter_caps_concealment_for_a_long_outage() {
        let mut buffer = AudioJitterBuffer::new();
        let t0 = Instant::now();
        feed_in_order(&mut buffer, &[0, 20_000], t0);

        // ~2 seconds of audio missing.
        assert!(buffer.insert(2_020_000, opus_stub(7), t0).is_empty());
        let actions = buffer.tick(t0 + AUDIO_JITTER_MAX_HOLD);

        let conceals = concealed(&actions);
        assert_eq!(
            conceals.len() as u64,
            AUDIO_MAX_CONCEAL_SLOTS,
            "concealment must be capped, not proportional to the outage length"
        );
        assert_eq!(conceals, vec![40_000, 60_000, 80_000, 100_000, 120_000]);
        assert_eq!(pushed(&actions), vec![2_020_000], "playback must resume at the first packet that did arrive");
    }

    /// Shutdown must not swallow whatever was still held: `flush` releases
    /// everything in PTS order (and, deliberately, without concealing —
    /// there is no point running PLC for audio immediately followed by EOS).
    #[test]
    fn jitter_flush_releases_everything_still_held_in_order() {
        let mut buffer = AudioJitterBuffer::new();
        let t0 = Instant::now();
        feed_in_order(&mut buffer, &[0, 20_000], t0);

        assert!(buffer.insert(100_000, opus_stub(5), t0).is_empty());
        assert!(buffer.insert(80_000, opus_stub(4), t0).is_empty());

        let actions = buffer.flush();
        assert_eq!(pushed(&actions), vec![80_000, 100_000]);
        assert!(concealed(&actions).is_empty());
        assert!(buffer.is_empty());
    }

    /// The measurement quoted in this module's "Reordering and packet-loss
    /// concealment" docs, as an executable assertion — and the one part of
    /// the fix the pure tests above cannot reach, since it depends entirely
    /// on GStreamer/libopus behavior rather than on this module's logic.
    ///
    /// Runs three variants of `appsrc ! opusdec ! audioconvert ! appsink`
    /// over the same real Opus clip and compares them:
    ///
    /// 1. **all packets** — the reference output length.
    /// 2. **one packet dropped, nothing signalled** — what this module used
    ///    to do. Must come out *short* by exactly one frame of PCM, and
    ///    with `opusdec` never having concealed anything. This is the
    ///    reported stutter, reproduced.
    /// 3. **one packet dropped, `GAP` event sent** (with `plc=true`, as
    ///    `run_audio_pipeline` now builds it) — must match the reference
    ///    length again, with `opusdec`'s own `stats` confirming PLC ran.
    ///
    /// Asserting (2) as well as (3) is what makes this a discriminating
    /// test rather than a smoke test: it proves the fix changes the outcome,
    /// instead of merely proving the fixed pipeline doesn't crash.
    #[test]
    fn gap_event_makes_opusdec_conceal_a_missing_packet() {
        gstreamer::init().expect("gstreamer init");

        const SKIP: usize = 10;
        let packets = encode_test_opus_clip(30);
        assert!(packets.len() > SKIP + 2, "need a clip long enough to punch a hole in the middle");

        let baseline = run_opus_loss_case(&packets, None, false);
        let unsignalled = run_opus_loss_case(&packets, Some(SKIP), false);
        let signalled = run_opus_loss_case(&packets, Some(SKIP), true);

        assert!(baseline.decoded_bytes > 0, "reference decode produced no PCM at all");
        assert_eq!(baseline.plc_samples, 0, "a lossless stream must not invoke PLC");

        // (2) The bug: a silent hole in the decoded PCM.
        assert!(
            unsignalled.decoded_bytes < baseline.decoded_bytes,
            "an unsignalled dropped packet should leave a hole in the decoded PCM \
             (baseline {} bytes, with hole {} bytes) — if these are equal this test \
             can no longer tell the bug from the fix",
            baseline.decoded_bytes,
            unsignalled.decoded_bytes
        );
        assert_eq!(
            unsignalled.plc_samples, 0,
            "without a GAP event opusdec has no way to know a packet was lost, so it must not have concealed"
        );

        // (3) The fix: the hole is filled by real Opus PLC.
        assert_eq!(
            signalled.decoded_bytes, baseline.decoded_bytes,
            "with a GAP event the decoded PCM should be back to full length"
        );
        assert!(
            signalled.plc_samples > 0,
            "opusdec reported no concealed samples, so the GAP event never reached its PLC path"
        );
        assert_eq!(
            signalled.num_gap, 1,
            "opusdec should have seen exactly the one GAP event that was sent"
        );
        // The concealed run must be a whole frame, in the right place: same
        // end timestamp as the reference means the GAP was serialized into
        // its correct slot rather than overtaking the buffers around it.
        assert_eq!(
            signalled.end_pts_us, baseline.end_pts_us,
            "concealment landed in the wrong slot: output timeline doesn't match the lossless one"
        );
    }

    /// The production decoder must have PLC on, or every `GAP` event the
    /// jitter buffer emits is silently discarded and half this module's fix
    /// does nothing. Cheap to assert, and the failure mode it guards
    /// against is completely silent otherwise (no error, no log, just the
    /// original stutter back again).
    #[test]
    fn production_opus_decoder_has_plc_enabled() {
        gstreamer::init().expect("gstreamer init");
        let decoder = make_opus_decoder().expect("opusdec unavailable");
        assert!(
            decoder.property::<bool>("plc"),
            "opusdec's `plc` defaults to false; run_audio_pipeline must turn it on \
             or GAP events are ignored and lost packets stay audible holes"
        );
    }

    // -----------------------------------------------------------------
    // `AudioTimeline` — sender PTS -> receiver running time
    // -----------------------------------------------------------------

    #[test]
    fn timeline_schedules_the_first_packet_a_lead_ahead_of_now() {
        let mut t = AudioTimeline::new();
        // Sender has been capturing for 30 s; receiver pipeline started
        // 5 ms ago. Without re-basing this packet would be scheduled ~30 s
        // into the receiver's future and never play.
        assert_eq!(t.map_push(30_000_000, 5_000), 5_000 + AUDIO_TARGET_LEAD_US as u64);
    }

    #[test]
    fn timeline_preserves_sender_intervals_after_the_first_packet() {
        let mut t = AudioTimeline::new();
        let first = t.map_push(30_000_000, 0);
        // Later packets keep their exact spacing regardless of when the
        // pipeline thread got round to them.
        assert_eq!(t.map_push(30_020_000, 3_000), first + 20_000);
        assert_eq!(t.map_push(30_040_000, 25_000), first + 40_000);
        assert_eq!(t.map_push(30_060_000, 44_000), first + 60_000);
    }

    #[test]
    fn timeline_holds_its_offset_through_normal_jitter() {
        let mut t = AudioTimeline::new();
        let first = t.map_push(1_000_000, 0);
        // A packet the jitter buffer held for its full window arrives late
        // in wall-clock terms but is still ahead of its playout slot, so
        // the mapping must not be disturbed.
        let held = t.map_push(1_020_000, AUDIO_JITTER_MAX_HOLD.as_micros() as u64);
        assert_eq!(held, first + 20_000);
    }

    #[test]
    fn timeline_rebases_when_the_sender_restarts_its_capture_session() {
        let mut t = AudioTimeline::new();
        t.map_push(30_000_000, 0);
        // Sender restarted: PTS origin drops back to ~0, which under the
        // old offset would map ~30 s into the past.
        let after = t.map_push(0, 500_000);
        assert_eq!(after, 500_000 + AUDIO_TARGET_LEAD_US as u64);
        // ...and the new offset is the one that sticks.
        assert_eq!(t.map_push(20_000, 520_000), after + 20_000);
    }

    #[test]
    fn timeline_rebases_when_it_drifts_too_far_into_the_future() {
        let mut t = AudioTimeline::new();
        t.map_push(0, 0);
        // Receiver clock ran far behind the sender's (or the pipeline
        // thread stalled): the mapping now schedules absurdly far ahead.
        let after = t.map_push(5_000_000, 1_000_000);
        assert_eq!(after, 1_000_000 + AUDIO_TARGET_LEAD_US as u64);
    }

    #[test]
    fn timeline_maps_gaps_onto_the_same_offset_without_rebasing() {
        let mut t = AudioTimeline::new();
        let first = t.map_push(30_000_000, 0);
        assert_eq!(t.map_gap(30_020_000), Some(first + 20_000));
        // A GAP must never move the timeline, however stale it looks.
        assert_eq!(t.map_gap(99_000_000), Some(first + 69_000_000));
        assert_eq!(t.map_push(30_040_000, 25_000), first + 40_000);
    }

    #[test]
    fn timeline_has_no_gap_mapping_before_the_first_packet() {
        let t = AudioTimeline::new();
        assert_eq!(t.map_gap(1_000), None);
    }

    /// The bug [`AudioTimeline`] exists for, end to end through a real
    /// `sync=true` sink: a stream whose PTS starts well ahead of the
    /// receiver pipeline's running time (i.e. any viewer who subscribes a
    /// while after the sharer started) plays *nothing* if its timestamps
    /// are pushed as-is, and plays normally once they're re-based.
    ///
    /// Discriminating on purpose, in the same style as
    /// `gap_event_makes_opusdec_conceal_a_missing_packet`: it asserts the
    /// broken case is broken as well as the fixed case working, so it
    /// can't quietly degrade into a smoke test. Note the third case —
    /// PTS starting near zero — passes either way, which is exactly why
    /// the bug survived the existing tests.
    #[test]
    fn sync_true_sink_stalls_on_raw_sender_pts_and_plays_on_rebased_pts() {
        gstreamer::init().expect("gstreamer init");

        let packets = encode_test_opus_clip(20);
        assert!(packets.len() > 10, "need a reasonably long clip");

        const SENDER_UPTIME_US: u64 = 30_000_000;

        let raw_late_joiner = run_audio_sync_case(&packets, SENDER_UPTIME_US, false);
        let rebased_late_joiner = run_audio_sync_case(&packets, SENDER_UPTIME_US, true);
        let raw_instant_joiner = run_audio_sync_case(&packets, 0, false);

        assert_eq!(
            raw_late_joiner, 0,
            "expected a sync=true sink to play nothing when handed PTS {} us ahead of its own \
             running time — if this now plays, the bug this test guards is gone and the test \
             can no longer tell the fix from the break",
            SENDER_UPTIME_US
        );
        assert!(
            rebased_late_joiner > 0,
            "re-basing onto the pipeline's running time must make a late-joining viewer's audio play"
        );
        assert!(
            raw_instant_joiner > 0,
            "sanity: PTS that already starts near the pipeline's running time plays either way \
             (this is why the bug was invisible to the other audio tests)"
        );
    }

    /// Pushes `packets` through `appsrc ! opusdec ! audioconvert !
    /// audioresample ! volume ! appsink(sync=true)` — the production audio
    /// chain with an observable sink — stamping PTS from `first_pts_us`
    /// onwards, optionally re-based through [`AudioTimeline`] exactly the
    /// way [`apply_audio_actions`] does it. Returns how many decoded
    /// buffers actually made it out of the sink within a bounded wait.
    fn run_audio_sync_case(packets: &[Vec<u8>], first_pts_us: u64, rebase: bool) -> usize {
        const FRAME_US: u64 = 20_000;

        let pipeline = Pipeline::new();
        let src = ElementFactory::make("appsrc").build().expect("appsrc unavailable");
        let decoder = make_opus_decoder().expect("opusdec unavailable");
        let convert = ElementFactory::make("audioconvert").build().expect("audioconvert unavailable");
        let resample = ElementFactory::make("audioresample").build().expect("audioresample unavailable");
        let volume = ElementFactory::make("volume").build().expect("volume unavailable");
        let sink = ElementFactory::make("appsink").build().expect("appsink unavailable");
        // The whole point: schedule playout against the pipeline clock,
        // like the real `autoaudiosink` does.
        sink.set_property("sync", true);

        pipeline.add_many([&src, &decoder, &convert, &resample, &volume, &sink]).unwrap();
        Element::link_many([&src, &decoder, &convert, &resample, &volume, &sink]).unwrap();

        let appsrc = src.dynamic_cast::<AppSrc>().unwrap();
        appsrc.set_caps(Some(&Caps::builder("audio/x-opus").field("channel-mapping-family", 0i32).build()));
        appsrc.set_is_live(true);
        appsrc.set_format(gstreamer::Format::Time);
        let appsink = sink.dynamic_cast::<AppSink>().unwrap();

        pipeline.set_state(State::Playing).expect("failed to start audio sync-case pipeline");

        let mut timeline = AudioTimeline::new();
        for (i, packet) in packets.iter().enumerate() {
            let sender_pts_us = first_pts_us + (i as u64) * FRAME_US;
            let pts_us = if rebase {
                timeline.map_push(sender_pts_us, running_time_us(&appsrc))
            } else {
                sender_pts_us
            };
            let mut buffer = Buffer::from_slice(packet.clone());
            if let Some(buffer_mut) = buffer.get_mut() {
                buffer_mut.set_pts(ClockTime::from_useconds(pts_us));
            }
            appsrc.push_buffer(buffer).expect("appsrc rejected a valid Opus packet");
            // Packets really do arrive spread out over wall-clock time;
            // dumping the clip in instantly would let the whole thing sit
            // in `appsrc`'s queue and prove nothing about the sink.
            std::thread::sleep(Duration::from_micros(FRAME_US));
        }

        // Long enough for every packet that is *going* to be played to
        // have been: the clip is `packets.len()` * 20 ms long and was fed
        // in real time, so only the deliberate lead is still outstanding.
        let mut played = 0usize;
        while appsink
            .try_pull_sample(ClockTime::from_mseconds(300))
            .is_some()
        {
            played += 1;
        }

        let _ = pipeline.set_state(State::Null);
        played
    }

    struct OpusLossOutcome {
        decoded_bytes: usize,
        plc_samples: u64,
        num_gap: u64,
        end_pts_us: Option<u64>,
    }

    /// Decodes `packets` through a throwaway `appsrc ! opusdec !
    /// audioconvert ! appsink` pipeline, optionally omitting the packet at
    /// `skip` and optionally announcing that omission with a `GAP` event —
    /// exactly the two things [`apply_audio_actions`] does for real.
    /// Reports how much PCM came out and what `opusdec`'s own `stats`
    /// property says it did.
    fn run_opus_loss_case(packets: &[Vec<u8>], skip: Option<usize>, send_gap: bool) -> OpusLossOutcome {
        const FRAME_US: u64 = 20_000;

        let pipeline = Pipeline::new();
        let src = ElementFactory::make("appsrc").build().expect("appsrc unavailable");
        let decoder = ElementFactory::make("opusdec").build().expect("opusdec unavailable");
        decoder.set_property("plc", true);
        let convert = ElementFactory::make("audioconvert").build().expect("audioconvert unavailable");
        let sink = ElementFactory::make("appsink").build().expect("appsink unavailable");
        sink.set_property("sync", false);

        pipeline.add_many([&src, &decoder, &convert, &sink]).unwrap();
        Element::link_many([&src, &decoder, &convert, &sink]).unwrap();

        let appsrc = src.dynamic_cast::<AppSrc>().unwrap();
        appsrc.set_caps(Some(&Caps::builder("audio/x-opus").field("channel-mapping-family", 0i32).build()));
        appsrc.set_format(gstreamer::Format::Time);
        let appsink = sink.dynamic_cast::<AppSink>().unwrap();

        pipeline.set_state(State::Playing).expect("failed to start opus loss-case pipeline");

        for (i, packet) in packets.iter().enumerate() {
            let pts_us = (i as u64) * FRAME_US;
            if Some(i) == skip {
                if send_gap {
                    let gap = gstreamer::event::Gap::builder(ClockTime::from_useconds(pts_us))
                        .duration(ClockTime::from_useconds(FRAME_US))
                        .build();
                    assert!(appsrc.send_event(gap), "appsrc refused the GAP event");
                }
                continue;
            }
            let mut buffer = Buffer::from_slice(packet.clone());
            if let Some(buffer_mut) = buffer.get_mut() {
                buffer_mut.set_pts(ClockTime::from_useconds(pts_us));
            }
            appsrc.push_buffer(buffer).expect("appsrc rejected a valid Opus packet");
        }
        appsrc.end_of_stream().expect("failed to signal EOS");

        let mut decoded_bytes = 0usize;
        let mut end_pts_us = None;
        loop {
            match appsink.try_pull_sample(ClockTime::from_mseconds(300)) {
                Some(sample) => {
                    if let Some(buffer) = sample.buffer() {
                        if let Ok(map) = buffer.map_readable() {
                            decoded_bytes += map.len();
                        }
                        if let (Some(pts), Some(dur)) = (buffer.pts(), buffer.duration()) {
                            end_pts_us = Some(pts.useconds() + dur.useconds());
                        }
                    }
                }
                None => break,
            }
        }

        let stats = decoder.property::<gstreamer::Structure>("stats");
        let plc_samples = stats.get::<u64>("plc-num-samples").unwrap_or(0);
        let num_gap = stats.get::<u64>("num-gap").unwrap_or(0);

        let _ = pipeline.set_state(State::Null);

        OpusLossOutcome { decoded_bytes, plc_samples, num_gap, end_pts_us }
    }

    /// Integration check that the whole [`AudioDecoder`] survives the exact
    /// network conditions this fix exists for — packets delivered out of
    /// order and with holes — rather than only the tidy in-order feed the
    /// test above it uses. Complements the pure `jitter_*` tests: those
    /// prove the decisions are right, this proves the real pipeline
    /// (`opusdec plc=true`, real `GAP` events, real reordered pushes)
    /// accepts them without erroring out.
    #[test]
    fn audio_decoder_survives_reordered_and_lossy_delivery() {
        gstreamer::init().expect("gstreamer init");

        let packets = encode_test_opus_clip(40);
        assert!(packets.len() > 12, "need a reasonably long clip");

        let mut decoder = AudioDecoder::new_with_sink(11, "fakesink", false).expect("failed to build AudioDecoder");
        let start_deadline = Instant::now() + Duration::from_secs(5);
        while !decoder.is_alive() && Instant::now() < start_deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(decoder.is_alive(), "audio decode pipeline never reached Playing");

        // Deliver with: every 5th packet dropped outright (loss), and every
        // 3rd/4th pair swapped (reordering).
        let mut order: Vec<usize> = Vec::new();
        let mut i = 0;
        while i < packets.len() {
            if i % 5 == 4 {
                i += 1; // lost
                continue;
            }
            if i % 3 == 0 && i + 1 < packets.len() {
                order.push(i + 1);
                order.push(i);
                i += 2;
            } else {
                order.push(i);
                i += 1;
            }
        }

        for &i in &order {
            decoder.feed(&DecryptedMediaFrame {
                stalled: false,
                conn_id: 1,
                stream_id: 11,
                // PTS follows the packet's true position, not its arrival
                // position — that's the whole point.
                pts: (i as u64) * 20_000,
                keyframe: true,
                payload: packets[i].clone(),
            });
            // Real packets arrive spread out in time, and the jitter
            // buffer's hold windows are wall-clock; feeding the whole clip
            // in a single instant would exercise only the depth cap.
            std::thread::sleep(Duration::from_millis(2));
        }

        // Long enough for every outstanding hold window to expire and be
        // pumped through.
        std::thread::sleep(AUDIO_JITTER_MAX_HOLD + Duration::from_millis(500));
        assert!(
            decoder.is_alive(),
            "audio decode pipeline died on out-of-order/lossy delivery — exactly the case it was fixed for"
        );
    }
}
