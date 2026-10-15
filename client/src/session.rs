//! Session state and business logic for the desktop client.
//!
//! Everything in this module is rendering-agnostic: it owns the room/capture/
//! destination/bandwidth data model and every state transition (connect,
//! join, share, watch, fan-out, transfer, per-destination edits, bandwidth
//! allocation, ...) and spawns the background tasks behind them. The egui
//! shell (`ui/`, in the bin crate) only renders it and calls
//! [`AppState`]/[`RoomSession`]'s methods instead of owning any logic itself.
//!
//! # Session data model (rooms, captures, destinations)
//!
//! [`AppState`] owns three collections that together describe "what this
//! client is doing right now". None of it is persisted — every launch starts
//! empty (only `rooms.rs`'s `RoomStore`/`AppSettings` survive a restart):
//!
//! - `rooms: IndexMap<RoomHandle, RoomSession>` — one entry per joined room.
//!   A room *is* one QUIC connection: the protocol welds a `ScreenshareClient`
//!   to exactly one room, so N rooms means N connections (all riding the one
//!   shared `quinn::Endpoint` in `AppState::endpoint`). Everything genuinely
//!   room-scoped — roster, announced streams, watches, audio watches, preview
//!   cache/textures, the `stream_id` allocator — lives here, which is what
//!   keeps two rooms from colliding on a shared key space.
//! - `captures: IndexMap<CaptureSourceId, LocalCapture>` — one entry per local
//!   GStreamer capture pipeline (a screen share, or an audio source). **All**
//!   capture/pipeline state lives here, not under a room.
//! - each `LocalCapture` owns `destinations: IndexMap<DestinationId, Destination>`
//!   — one entry per room this capture is being sent into.
//!
//! **Fan-out is live**: a capture can feed several rooms at once, each
//! destination backed by its own dynamic `capture::branch` encode branch with
//! its own encoder, bitrate, `stream_id`, room key and `DestinationState`.
//! [`AppState::add_destination`] (fan-out), [`AppState::transfer_destination`]
//! (make-before-break transfer between rooms) and
//! [`AppState::apply_destination_edit`] (settings edits routed through
//! `capture::destination::classify_change`) are the operations; `ui.rs`'s
//! Stream Control tab is their rendering. See
//! `superdoc/features/stream-control.md`.
//!
//! Absolute-Maximum bandwidth mode's live wiring — rate limiting, hysteresis,
//! and remembering what each destination's encoder was last told — is
//! [`BandwidthController`], driven every frame by
//! [`AppState::apply_bandwidth_allocation`].
//!
//! **Reconnecting and leaving.** A room that drops unexpectedly is suspended
//! and reconnected by itself ([`ReconnectState`], [`AppState::reconnects`]; the
//! "Auto-reconnect and clean exit" section of the `AppState` impl has the
//! design), keeping its [`RoomHandle`], the user's watches and our own shares.
//! A user Leave / Forget / exit never reconnects and tells the relay we left
//! ([`AppState::disconnect_room`], [`AppState::shutdown_blocking`]).
//!
//! See `ui.rs`'s module docs for the app-shell/nav-tab layer this sits under.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use tokio::sync::{mpsc, oneshot};

use crate::audio::{self, AudioCaptureHandle, AudioSource};
use crate::bandwidth::{self, AllocInput, Allocation, DestId, DestSpec};
use crate::capture::{self, CaptureHandle, DestKey};
use crate::decode::{self, AudioDecoder, PreviewCache, StreamDecoder};
use crate::net::{ClientEvent, CloseCause, PresenceSnapshotState, ScreenshareClient};
use crate::protocol::{ConnId, Member, PresenceEvent, StreamId, StreamInfo, StreamKind};
use crate::rooms::{AppSettings, BandwidthMode, PresentationMode, RoomStore, StartStreamDefaults};

pub type ConnectResult = Result<(ScreenshareClient, PresenceSnapshotState, mpsc::UnboundedReceiver<ClientEvent>), ConnectError>;

/// Why a connect attempt failed: the text the UI shows, plus the
/// [`CloseCause`] the reconnect policy decides on (`retry_delay`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectError {
    pub message: String,
    pub cause: CloseCause,
}

impl ConnectError {
    fn other(message: String) -> Self {
        Self { message, cause: CloseCause::Other }
    }

    fn from_anyhow(e: &anyhow::Error) -> Self {
        Self { message: format!("{e:#}"), cause: CloseCause::from_error(e) }
    }
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

// ---------------------------------------------------------------------
// Auto-reconnect policy (pure; the state machine is `ReconnectState`)
// ---------------------------------------------------------------------

/// First step of the backoff after the immediate retry.
pub const RECONNECT_BACKOFF_BASE: Duration = Duration::from_millis(500);
/// The backoff never waits longer than this between attempts.
pub const RECONNECT_BACKOFF_CAP: Duration = Duration::from_secs(15);
/// Minimum wait between attempts while the relay says the room is full: the
/// room only frees up when someone leaves, so hammering it helps nobody.
pub const ROOM_FULL_RETRY: Duration = Duration::from_secs(5);
/// One reconnect attempt gives up after this long (shorter than a manual
/// connect's [`DEFAULT_CONNECT_TIMEOUT`]): a dead relay would otherwise hold
/// the single connect slot for 15 s per attempt and starve the backoff.
pub const RECONNECT_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(8);
/// How long after a reconnect a watch whose stream has not been re-announced
/// yet is still waited for. Matches the browser client's "resume" list (60 s).
pub const REWATCH_GRACE: Duration = Duration::from_secs(60);
/// How long after a watched stream was *stopped by its sharer* (relay
/// `StreamStopped`) the same sharer's next stream of the same kind and label
/// is picked up as the same watch again.
///
/// `Why resume at all:` stop-and-restart is how a sharer switches monitor or
/// window or fixes a stuck share, and a viewer who was watching them means to
/// keep watching: the new stream takes the dead tile's place, with no click.
/// `Why only a short window (not [`REWATCH_GRACE`]):` unlike a connection drop
/// this was a deliberate stop; minutes later an automatic subscribe (and the
/// bandwidth it costs) would be a surprise. The portal picker takes a few
/// seconds, 30 s covers "stop, pick something else, go live".
pub const RESUME_AFTER_STOP: Duration = Duration::from_secs(30);

/// Wait before the attempt that follows `failed` failed attempts of one
/// outage: 0 (immediate), then 0.5 s, 1, 2, 4, 8, capped at 15 s.
///
/// `Why immediate first:` the common outage is a relay restart or a blip, and
/// the connection is usually back within a second; waiting before the first
/// try just adds latency to the common case. `Why exponential after:` a relay
/// that is really down should not be probed in a tight loop by every client.
pub fn backoff_delay(failed: u32) -> Duration {
    if failed == 0 {
        return Duration::ZERO;
    }
    let steps = (failed - 1).min(16);
    RECONNECT_BACKOFF_BASE.saturating_mul(1u32 << steps).min(RECONNECT_BACKOFF_CAP)
}

/// What to do after the connection (or a reconnect attempt) ended with
/// `cause`, `failed` attempts into the outage: `Some(wait)` before the next
/// attempt, or `None` to stop and surface the error.
///
/// - [`CloseCause::RelayShutdown`] (`0x200`): the relay told us it is going
///   away, so the first retry is immediate; later failures back off normally.
/// - [`CloseCause::Malformed`] (`0x101`): retrying the same `Join` cannot
///   succeed, so stop.
/// - [`CloseCause::RoomFull`] (`0x102`): keep trying, but not faster than
///   [`ROOM_FULL_RETRY`] (the browser's "room full, still trying").
/// - [`CloseCause::Other`]: backoff.
pub fn retry_delay(cause: CloseCause, failed: u32) -> Option<Duration> {
    match cause {
        CloseCause::Malformed => None,
        CloseCause::RoomFull => Some(backoff_delay(failed).max(ROOM_FULL_RETRY)),
        CloseCause::RelayShutdown | CloseCause::Other => Some(backoff_delay(failed)),
    }
}

/// A reasonable default encode bitrate, preselected on the bitrate slider.
pub const DEFAULT_BITRATE_KBPS: u32 = 8000;
/// Slider ceiling, matching the project's 40 Mbit/s ceiling — also the
/// per-destination ceiling `AppState::allocation_input` hands the allocator,
/// so this lives here rather than purely in `ui.rs`.
pub const MAX_BITRATE_KBPS: u32 = 40000;

/// Default capture framerate, preselected on the framerate dropdown — mirrors
/// `capture::DEFAULT_FRAMERATE`.
pub const DEFAULT_FRAMERATE: u32 = 30;

// ---------------------------------------------------------------------
// Bandwidth: Absolute-Maximum mode's allocator wiring
// ---------------------------------------------------------------------

/// How often the Absolute-Maximum allocation is recomputed at most.
///
/// `Why` a rate limit at all (and why ~1s): `allocate` is pure and cheap, but
/// *applying* its result is a property write into a live encoder's rate
/// controller, and both `x264enc` and `vah264enc` need time to converge on a
/// new target. Recomputing on every repaint (up to 240/s) would hand the rate
/// controller a new target faster than it can settle on the last one, which
/// shows up as visible quality oscillation rather than a smooth adjustment.
/// The allocation's inputs also change in bursts — joining a room, starting a
/// fan-out destination, a transfer's make-before-break overlap where the old
/// and new destination briefly both exist — so a fixed ~1s cadence collapses
/// each burst into a single re-apply instead of a storm of them. One second
/// is the plan doc's own figure and is comfortably below any human-noticeable
/// delay in a bandwidth adjustment.
pub const ALLOC_RECOMPUTE_INTERVAL: Duration = Duration::from_secs(1);

/// Hysteresis: the minimum change worth writing into a live encoder.
///
/// `Why both a relative and an absolute term, combined with OR:` at a high
/// bitrate (say 8000 kbit/s) 5% is 400 kbit/s, and waiting for a swing that
/// large before reacting would leave real headroom unused; at a low bitrate
/// (say 400 kbit/s) an absolute 100 kbit/s threshold is a *quarter* of the
/// stream and would make the destination unresponsive to allocation changes
/// entirely. Triggering on whichever bound is hit first ("differs by more
/// than 5% **or** more than 100 kbit/s", exactly as the plan doc words it)
/// keeps the threshold proportionate at both ends while still filtering the
/// ±1 kbit/s churn that largest-remainder rounding produces when an unrelated
/// destination's weight changes slightly.
pub const ALLOC_HYSTERESIS_PERCENT: u32 = 5;
/// See [`ALLOC_HYSTERESIS_PERCENT`].
pub const ALLOC_HYSTERESIS_KBPS: u32 = 100;

/// Whether a newly allocated bitrate is different enough from what this
/// destination's encoder was last actually told to be worth another live
/// property write. A destination that has never been written to always is.
pub fn needs_reapply(current: Option<u32>, target: u32) -> bool {
    let Some(current) = current else { return true };
    let delta = current.abs_diff(target);
    // `u64` for the percentage comparison: both sides are bounded by the
    // 40 Mbit/s ceiling in practice, but `current` comes from a hand-editable
    // settings file by way of the allocator, and `delta * 100` on a
    // near-`u32::MAX` value would wrap — turning "this changed enormously"
    // into "not worth writing".
    delta > ALLOC_HYSTERESIS_KBPS || delta as u64 * 100 > current as u64 * ALLOC_HYSTERESIS_PERCENT as u64
}

/// What one [`BandwidthController::tick`] decided.
pub struct BandwidthTick {
    /// The encoder writes to actually perform — already filtered by
    /// hysteresis, so an empty vec means "nothing changed enough to be worth
    /// touching the encoders".
    pub writes: Vec<(DestinationId, u32)>,
    /// The full allocation this tick produced, kept so the UI can show each
    /// destination's share and the overshoot warning.
    pub allocation: Allocation,
}

/// Drives `bandwidth::allocate` for Absolute-Maximum mode: rate-limits how
/// often it recomputes, remembers what each destination's encoder was last
/// told, and suppresses writes that don't clear the hysteresis threshold.
///
/// Deliberately owns no `AppState` state and calls no clock of its own (`now`
/// is passed in), for the same reason `bandwidth::allocate` is a pure
/// function and `decode.rs`'s `PacingState` takes an explicit `Instant`: the
/// whole point of this layer is the timing behaviour, so the timing behaviour
/// has to be testable without sleeping.
pub struct BandwidthController {
    last_recompute: Option<Instant>,
    /// Last value actually written to each destination's encoder.
    applied: HashMap<DestinationId, u32>,
    /// Most recent allocation, for the UI.
    last_allocation: Option<Allocation>,
}

impl BandwidthController {
    pub fn new() -> Self {
        Self { last_recompute: None, applied: HashMap::new(), last_allocation: None }
    }

    /// Recompute and decide the writes, or `None` if the rate limit says it's
    /// too soon. Recording the writes as applied is the caller's job
    /// ([`BandwidthController::note_applied`]) — so a write that couldn't be
    /// delivered (destination already gone) doesn't get remembered as if it
    /// had been.
    pub fn tick(&mut self, now: Instant, input: &AllocInput) -> Option<BandwidthTick> {
        if let Some(last) = self.last_recompute
            && now.duration_since(last) < ALLOC_RECOMPUTE_INTERVAL
        {
            return None;
        }
        self.last_recompute = Some(now);

        let allocation = bandwidth::allocate(input);
        let writes: Vec<(DestinationId, u32)> = allocation
            .per_destination
            .iter()
            .map(|(id, kbps)| (DestinationId(id.0), *kbps))
            .filter(|(id, kbps)| needs_reapply(self.applied.get(id).copied(), *kbps))
            .collect();
        self.last_allocation = Some(allocation.clone());
        Some(BandwidthTick { writes, allocation })
    }

    pub fn note_applied(&mut self, id: DestinationId, kbps: u32) {
        self.applied.insert(id, kbps);
    }

    pub fn applied_kbps(&self, id: DestinationId) -> Option<u32> {
        self.applied.get(&id).copied()
    }

    pub fn forget(&mut self, id: DestinationId) {
        self.applied.remove(&id);
    }

    /// Leaving Absolute-Maximum mode: nothing the allocator wrote is
    /// authoritative any more (Per-Stream mode re-applies each destination's
    /// own manual rate), so drop the memory of it. Deliberately does **not**
    /// touch any stored per-destination or total setting — only this
    /// controller's own bookkeeping.
    pub fn reset(&mut self) {
        self.last_recompute = None;
        self.applied.clear();
        self.last_allocation = None;
    }

    pub fn allocation(&self) -> Option<&Allocation> {
        self.last_allocation.as_ref()
    }
}

impl Default for BandwidthController {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------
// Session ids
// ---------------------------------------------------------------------
//
// `Why:` (opaque `u32` newtypes with a monotonic per-`AppState` counter, not
// indices into the maps and not the protocol's own ids) three properties are
// wanted at once. **Not indices**: `IndexMap` shifts indices on removal, so an
// id held across a removal would silently start addressing a *different*
// room/capture. Allocating from a counter that never rewinds within a session
// means a stale id can only ever miss, never alias. **Not protocol ids**: a
// `StreamId` is one byte and is only unique within one connection — exactly
// the collision the multi-room refactor exists to remove — and a `ConnId` is
// assigned by the relay, so neither can key local state that spans rooms.
// **Distinct types rather than a shared `Id(u32)`**: passing a
// `CaptureSourceId` where a `RoomHandle` belongs is then a compile error, and
// these get passed through a lot of plumbing. `u32` (not `u64`) because these
// count user actions — joins, screen shares — in one process lifetime.

/// Identifies one joined room (one QUIC connection) for this session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RoomHandle(pub u32);

/// Identifies one local capture pipeline (one screen grab, or one audio
/// source) for this session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CaptureSourceId(pub u32);

/// Identifies one outgoing stream: one capture, encoded and encrypted for one
/// room.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DestinationId(pub u32);

/// Identifies one live [`audio::AudioLevelTapHandle`] — a candidate source's
/// dropdown-entry meter, not a real share (see that type's doc comment).
/// Scoped tightly to however long one dropdown is open on the frontend: see
/// [`build_audio_level_tap`] + [`AppState::insert_audio_level_tap`]/[`AppState::stop_audio_level_tap`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AudioLevelTapId(pub u32);

/// Either kind of running outbound share this client can hold onto.
pub enum ShareHandle {
    Video(CaptureHandle),
    Audio(AudioCaptureHandle),
}

/// Per-share state machine. Starting/stopping are async
/// (`capture::start_sharing`/`audio::start_audio_sharing`, and each handle's
/// `.stop()`), so each transition is spawned and its result delivered back
/// via a oneshot, mirroring `Status`/`connect_rx` above.
pub enum ShareStatus {
    /// The start result's `Err` is a display string, except for the one value
    /// [`SHARE_START_CANCELLED`], which means "the user dismissed the screen
    /// picker" and is removed silently rather than shown as `share_error`.
    Starting(oneshot::Receiver<Result<ShareHandle, String>>),
    Active(ShareHandle),
    Stopping(oneshot::Receiver<Result<(), String>>),
}

/// The `Err` value a share-start task sends when the user **dismissed the OS
/// screen picker** (`capture::is_picker_cancelled`). `poll_capture_states`
/// removes such a capture without setting `share_error`: cancelling a dialog
/// is not a failure and must not raise an error toast.
///
/// `Why a marker string and not a typed error:` `ShareStatus::Starting`
/// carries `Result<ShareHandle, String>` and is constructed by tests and the
/// UI directly; widening the error type would touch all of them for the sake
/// of one value that is produced in exactly one place ([`start_error_string`])
/// and consumed in exactly one (`poll_capture_states`). The leading NUL can
/// never begin a real error message.
pub const SHARE_START_CANCELLED: &str = "\0share-start-cancelled";

/// Turn a capture-start failure into the `Err` a `ShareStatus::Starting`
/// channel carries: [`SHARE_START_CANCELLED`] for a dismissed picker, the
/// full error chain otherwise.
fn start_error_string(e: &anyhow::Error) -> String {
    if capture::is_picker_cancelled(e) { SHARE_START_CANCELLED.to_string() } else { format!("{e:#}") }
}

/// Records a share failure for the UI (`AppState::share_error`, which the UI
/// turns into an error toast) and writes it to the log.
///
/// `Why log it too:` the toast is gone after a few seconds and the staged
/// "Waiting for the screen picker" entry vanishes with it, so a share that
/// failed to start (task #59: portal died during start-up) left no trace
/// anywhere a bug report could point at.
fn note_share_error(slot: &mut Option<String>, error: String) {
    tracing::warn!("share error: {error}");
    *slot = Some(error);
}

/// Audio destinations carry the video params struct inertly — there is no
/// per-destination audio geometry to describe, and their `runtime` stays
/// `None` because the video `DestinationState`/stall machinery has no audio
/// counterpart (audio's own always-on-mixer design makes toggling invisible
/// by construction; see the plan doc).
pub const AUDIO_DESTINATION_PARAMS: capture::destination::VideoParams = capture::destination::VideoParams {
    width: 0,
    height: 0,
    framerate: 0,
    bitrate_kbps: 0,
    backend: capture::EncoderBackend::Software,
    tune: capture::TuneMode::LowLatency,
    quality_level: capture::QualityLevel::Performance,
    gop_millis: capture::DEFAULT_GOP_MILLIS,
    codec: capture::VideoCodec::H264,
};

/// A destination's in-progress lifecycle beyond what `DestinationState`
/// covers — those states describe a *live* destination's encode branch; these
/// describe one that isn't (yet, or any more) a normal destination at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingRole {
    /// A second (third, …) destination off an existing capture is being
    /// brought up — the fan-out case.
    FanOut,
    /// "Go live" clicked on a staged card: this is a staged capture's
    /// *first* destination coming up (see `AppState::go_live`). Mechanically
    /// identical to `FanOut` (same `add_destination_with_params` call), kept
    /// as its own variant purely so the UI can say "Going live…" instead of
    /// the fan-out card's "Also sending here…" while it waits for the first
    /// IDR.
    GoingLive,
    /// Make-before-break transfer: this is the *new* destination in the
    /// target room, still proving itself. The old one keeps streaming until
    /// this succeeds.
    TransferTarget(RoomHandle),
    /// Make-before-break transfer: this is the *old* destination, still live
    /// and still authoritative, waiting for its replacement to confirm.
    TransferSource(RoomHandle),
}

/// The editable half of a destination row: the user's in-progress choices,
/// kept as UI-friendly values (a downscale percentage rather than a pixel
/// size) and resolved against the live source caps at apply time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DestEdit {
    /// One of `ui.rs`'s `RESOLUTION_SCALES` percentages.
    pub scale_percent: u32,
    pub framerate: u32,
    /// The Per-Stream-Maximum manual rate for this destination.
    ///
    /// `Why` this survives Absolute-Maximum mode untouched: the two modes'
    /// numbers are independent (see `rooms::AppSettings`' `Why:` note).
    /// While Absolute mode is on, the allocator writes live bitrates straight
    /// into the encoders without ever touching this field, so switching back
    /// to Per-Stream restores exactly what the user had set here.
    pub bitrate_kbps: u32,
    /// Low-latency vs. best-quality — `capture::TuneMode`. Testing-oriented
    /// encoder tuning knob; see that type's doc comment.
    pub tune: capture::TuneMode,
    /// Performance vs. quality encode speed — `capture::QualityLevel`.
    pub quality_level: capture::QualityLevel,
    /// Keyframe/GOP interval in milliseconds — see
    /// `capture::destination::VideoParams::gop_millis`. Preset-driven (the
    /// Gaming preset uses a shorter interval); not a raw Advanced-tuning
    /// control of its own.
    pub gop_millis: u32,
    /// H.264/H.265/AV1 — `capture::VideoCodec`.
    pub codec: capture::VideoCodec,
}

/// One room this capture is being sent into: the room, plus the `stream_id`
/// announced on *that* room's connection.
///
/// `Why:` (no back-pointer to the owning capture, no copy of its own
/// `DestinationId`) a `Destination` is only ever reachable by walking
/// `AppState::captures` → `LocalCapture::destinations`, so both the owning
/// `CaptureSourceId` and this destination's own `DestinationId` are already
/// in hand at every use site. Storing them here would be a second copy to
/// keep in sync, with nothing to gain.
pub struct Destination {
    pub room: RoomHandle,
    /// This destination's `stream_id` on `room`'s connection — allocated from
    /// that `RoomSession`'s own `next_stream_id`, so the same capture fanned
    /// out to two rooms can (and generally will) carry a different
    /// `stream_id` in each.
    pub stream_id: StreamId,
    /// This destination's own encode settings. Genuinely per-destination: two
    /// rooms fed by the same capture can run different resolutions, framerates
    /// and bitrates, each served by its own tee branch's
    /// `videoscale`/`videorate`/encoder chain (`capture::branch::add_branch`).
    pub params: capture::destination::VideoParams,
    /// Shared handle onto this destination's live lifecycle state — the
    /// `DestinationState` machine, its monotonic wire-PTS clock, and the stall
    /// protocol's cached IDR. See `capture::destination::DestinationRuntime`.
    ///
    /// `None` only while the capture is still `ShareStatus::Starting` (the
    /// runtime is created inside `capture::start_sharing`, so it doesn't exist
    /// until the handle comes back), while a fanned-out/transferred
    /// destination's branch is still being built (`pending`), and for audio
    /// destinations, which have no video lifecycle. `AppState::poll_capture_states`
    /// / `AppState::poll_pending_destinations` attach it the moment the
    /// handle lands; `Destination::state` reports `Starting` until then, which
    /// is exactly what a not-yet-started destination is.
    pub runtime: Option<capture::destination::SharedDestination>,
    /// Set while this destination is being brought up (or, for the *source*
    /// side of a transfer, torn down) so the UI can say which of the two it
    /// is instead of silently swapping rows. See [`PendingRole`].
    pub pending: Option<PendingRole>,
    /// The settings currently being edited in this destination's row, applied
    /// (through `capture::destination::classify_change`) when "Apply" is
    /// clicked. Separate from `params` so a half-typed edit never becomes the
    /// live configuration, and so the row can show "changed / not applied".
    pub edit: DestEdit,
    /// Set while an "Apply" is in flight, resolving to `Ok(())` once the
    /// branch swap completes — see `capture::CaptureHandle::change_destination`.
    pub apply_rx: Option<oneshot::Receiver<Result<(), String>>>,
}

impl Destination {
    /// This destination's current lifecycle state — `Starting` before the
    /// capture handle exists (see `runtime`), otherwise whatever the pump task
    /// has most recently recorded.
    pub fn state(&self) -> capture::destination::DestinationState {
        match &self.runtime {
            Some(runtime) => runtime.lock().unwrap().state(),
            None => capture::destination::DestinationState::Starting,
        }
    }

    /// The parameters actually in force: the live runtime's if this
    /// destination has one (the pipeline's real negotiated size, the
    /// framerate/bitrate its branch is really running at), otherwise the
    /// requested ones.
    pub fn live_params(&self) -> capture::destination::VideoParams {
        match &self.runtime {
            Some(runtime) => runtime.lock().unwrap().params(),
            None => self.params,
        }
    }

    /// A fresh edit buffer matching this destination's current settings — the
    /// state a row shows before the user changes anything.
    pub fn edit_from(params: capture::destination::VideoParams) -> DestEdit {
        DestEdit {
            scale_percent: 100,
            framerate: params.framerate,
            bitrate_kbps: params.bitrate_kbps,
            tune: params.tune,
            quality_level: params.quality_level,
            gop_millis: params.gop_millis,
            codec: params.codec,
        }
    }

    /// Resolve this row's edit buffer into concrete `VideoParams` against what
    /// the capture source is producing. Never upscales — the scale is applied
    /// to the source size, and `capture::destination::clamp_to_source` is the
    /// backstop for anything that still overshoots.
    pub fn edited_params(&self, source: capture::destination::SourceCaps) -> capture::destination::VideoParams {
        let (width, height) = if source.width == 0 || source.height == 0 {
            (0, 0)
        } else {
            (
                (source.width * self.edit.scale_percent / 100).max(2) & !1,
                (source.height * self.edit.scale_percent / 100).max(2) & !1,
            )
        };
        capture::destination::VideoParams {
            width,
            height,
            framerate: self.edit.framerate,
            bitrate_kbps: self.edit.bitrate_kbps,
            backend: self.live_params().backend,
            tune: self.edit.tune,
            quality_level: self.edit.quality_level,
            gop_millis: self.edit.gop_millis,
            codec: self.edit.codec,
        }
    }
}

/// One local capture pipeline (video or audio) and every room it feeds.
/// Several can run at once — e.g. two monitors, or a screen share plus an
/// audio share. Owned by `AppState::captures`, never by a room: see this
/// module's "Session data model" docs for why.
pub struct LocalCapture {
    pub kind: StreamKind,
    /// Human-readable label for this capture's row: "Screen share" for video,
    /// the source's name for audio.
    pub label: String,
    pub status: ShareStatus,
    /// For an **audio** capture: the video capture this audio share was
    /// started as a companion to, if any. `None` for every video capture, and
    /// for a standalone audio share started without a screen share to attach
    /// to.
    ///
    /// `Why the link is explicit and not inferred:` the Stream Control tab no
    /// longer gives audio a card of its own — a shared application's audio
    /// renders as an extra row inside its screen share's card (see
    /// `superdoc/features/stream-control.md`), so the frontend has to know
    /// *which* card a given audio share belongs in. "Same room" alone is
    /// ambiguous the moment someone shares two monitors into one room: both
    /// cards would claim the same audio row, and Stop on either would kill
    /// audio for both without saying so. Recording the pairing at the moment
    /// the user clicks "Add audio" on a specific card is unambiguous and
    /// costs one `Option`.
    pub audio_for_capture: Option<CaptureSourceId>,
    /// Rooms this capture is being sent into.
    pub destinations: IndexMap<DestinationId, Destination>,
}

impl LocalCapture {
    /// The lifecycle state to *show* for `dest`, folding in this capture's
    /// own start/stop status.
    ///
    /// `Why this exists instead of just` `dest.state()`: `Destination::state`
    /// derives everything from `runtime`, the video encode branch's shared
    /// state — and an **audio** destination never has one (see
    /// [`AUDIO_DESTINATION_PARAMS`]'s note). `Destination::state` maps
    /// `runtime: None` to `Starting`, which for video means "the branch
    /// hasn't emitted its first IDR yet" and resolves within a beat, but for
    /// audio was permanent: every audio share sat on "STARTING" forever, no
    /// matter how healthy it was. Reported by the project owner the first
    /// time the feature was used for real.
    ///
    /// So for audio the capture handle *is* the state: `Starting` while
    /// `audio::start_audio_sharing` is still in flight, `Live` once it has
    /// landed (it only returns `Ok` after the pipeline is playing **and**
    /// verified to be capturing the right PipeWire node — see
    /// `audio::wait_for_capture_link`). Audio has no per-destination
    /// stall/rebuild machinery for the other states to come from.
    pub fn destination_state(&self, dest: &Destination) -> capture::destination::DestinationState {
        use capture::destination::DestinationState;
        if self.kind != StreamKind::Audio {
            return dest.state();
        }
        match &self.status {
            ShareStatus::Starting(_) => DestinationState::Starting,
            ShareStatus::Active(_) => DestinationState::Live,
            // Nothing in `DestinationState` means "being torn down"; the row
            // is about to disappear either way, and `Starting` is the one
            // non-alarming transient state, which is what "Stopping" is.
            ShareStatus::Stopping(_) => DestinationState::Starting,
        }
    }
}

/// One stream currently being watched. The decoder is behind an
/// `Arc<Mutex<_>>` because the media-ingestion task feeds it from its own task
/// while the UI thread reads decoded frames from it.
pub struct WatchEntry {
    pub decoder: Arc<Mutex<StreamDecoder>>,
    /// The stream_id of the same sender's audio stream, if that sender
    /// currently has one announced — the link that makes "watching someone's
    /// video automatically hears their audio" work (see
    /// `RoomSession::sync_audio_watches`). Looked up in
    /// `RoomSession::audio_watches` to render the volume slider next to the
    /// video panel. `None` while the sender has no audio stream announced.
    ///
    /// `Why it is re-derived continuously rather than latched at watch time:`
    /// a sharer can enable "Share Audio" at any moment, including long after
    /// someone started watching their screen. Latching this once, in
    /// `toggle_watch`, meant every viewer who was already watching stayed
    /// permanently silent — see `sync_audio_watches`' doc comment.
    pub audio_stream_id: Option<StreamId>,
}

/// One audio stream currently being decoded/played, whether auto-subscribed
/// alongside a video `WatchEntry` (see `WatchEntry::audio_stream_id`) or
/// started standalone via a "Listen" click on an audio-only stream (no video
/// from the same sender). Keyed the same way as `watches`, in
/// `RoomSession::audio_watches`.
pub struct AudioWatch {
    pub decoder: AudioDecoder,
    /// 0-200, the *desired* value bound to this stream's volume slider —
    /// kept as-is even while `muted` is true, so unmuting restores it
    /// rather than resuming from 0. The value actually applied to
    /// `decoder` is `if muted { 0.0 } else { volume_percent / 100.0 }` (see
    /// [`effective_gain`]).
    pub volume_percent: u32,
    /// Mirrors the frontend's own per-tile mute toggle. Kept as its own
    /// bool (2026-07-29) rather than folded into `volume_percent` (which
    /// used to just get zeroed by the frontend before it was sent) so a
    /// backend query (`RoomSession::volume`) can tell a viewer's "muted at
    /// 40%" apart from "explicitly dragged to 0%" — both windows (View tab
    /// and any pop-out) need that distinction to restore the right slider
    /// state. See `superdoc/features/video-pipeline.md`'s 2026-07-29 entry.
    pub muted: bool,
    /// `true` when this watch exists only because we're watching the same
    /// sender's *video* (`sync_audio_watches` owns it, and will tear it down
    /// again the moment that video watch or that audio stream goes away).
    /// `false` for a standalone "Listen" on an audio-only sender
    /// (`toggle_listen`), which only the user turns off again.
    ///
    /// `Why the flag is needed:` both kinds live in the same
    /// `audio_watches` map, and `sync_audio_watches` reconciles that map
    /// against "who am I watching video from" — without a marker it would
    /// happily tear down a deliberate standalone Listen as "not desired".
    pub auto: bool,
}

/// Default playback volume, percent — what a freshly created audio watch
/// starts at, and what [`VolumeState::default`] reports for "no audio watch
/// yet".
pub const DEFAULT_VOLUME_PERCENT: u32 = 100;

/// A viewer's volume/mute state for one sender's audio — what a tile's volume
/// slider shows. See [`RoomSession::volume`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VolumeState {
    /// 0-200, the slider's *desired* value — never zeroed just because
    /// `muted` is true, so unmuting can restore it.
    pub volume_percent: u32,
    pub muted: bool,
}

impl Default for VolumeState {
    fn default() -> Self {
        Self { volume_percent: DEFAULT_VOLUME_PERCENT, muted: false }
    }
}

/// The gain actually handed to the audio decoder: `0.0` while muted,
/// otherwise `volume_percent / 100`.
pub fn effective_gain(volume_percent: u32, muted: bool) -> f64 {
    if muted { 0.0 } else { volume_percent as f64 / 100.0 }
}

/// The `stream_id` of `owner`'s announced audio stream in `streams`, if any.
pub fn sibling_audio_stream_in(streams: &[StreamInfo], owner: ConnId) -> Option<StreamId> {
    streams.iter().find(|s| s.owner == owner && s.kind == StreamKind::Audio).map(|s| s.stream_id)
}

/// Which audio stream a volume call about `stream_id` means: `stream_id`
/// itself if `has_audio_watch(stream_id)` (a direct hit on the audio id),
/// otherwise `owner`'s sibling audio stream (the caller passed the *video*
/// stream id). See [`RoomSession::set_volume`] for why both are accepted.
pub fn resolve_audio_stream_id(
    streams: &[StreamInfo],
    owner: ConnId,
    stream_id: StreamId,
    has_audio_watch: impl Fn(StreamId) -> bool,
) -> Option<StreamId> {
    if has_audio_watch(stream_id) { Some(stream_id) } else { sibling_audio_stream_in(streams, owner) }
}

pub enum Status {
    Idle,
    /// A connect attempt is in flight, or at least one room is waiting to
    /// reconnect (see [`AppState::reconnects`]). Kept set for the whole outage,
    /// not just while an attempt runs, because the UI only keeps its frame loop
    /// (and so the retry timers) ticking while this is `Connecting`.
    Connecting,
    Error(String),
}

/// A stream the user was watching (or, for audio-only senders, listening to)
/// before a reconnect, named by what survives it. See [`match_wanted`].
///
/// `Why not the `(ConnId, StreamId)` key:` both change when the owner
/// reconnects (a `ConnId` is fresh per connection, a `StreamId` is re-allocated
/// per announce), so after a relay restart the old key names nothing. The
/// display name plus kind plus label is what a person would call "the same
/// stream", and it is the browser client's `stableKey` minus the room (the room
/// is implied here).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WantedStream {
    pub owner_name: String,
    pub kind: StreamKind,
    pub label: String,
    /// The viewer's volume for this owner, carried over so a re-watch does not
    /// reset a slider the user had moved.
    pub volume_percent: Option<u32>,
}

/// Find the stream that `wanted` now refers to: an announced stream of the
/// right kind and label whose owner is a member with the same display name,
/// that `is_taken` does not already claim (so two wanted streams of one owner
/// with the same label, such as two "Screen"s, pick two different streams).
/// Our own streams (`me`) never match. Candidates are tried in
/// `(owner, stream_id)` order so the result is deterministic.
///
/// Limits (same as the browser's): two members with one name sharing one label
/// are interchangeable, and a renamed member or a changed label is a different
/// stream.
pub fn match_wanted(
    wanted: &WantedStream,
    me: ConnId,
    members: &[Member],
    streams: &[StreamInfo],
    is_taken: impl Fn(ConnId, StreamId) -> bool,
) -> Option<(ConnId, StreamId)> {
    let mut candidates: Vec<&StreamInfo> = streams
        .iter()
        .filter(|s| s.owner != me && s.kind == wanted.kind && s.label == wanted.label)
        .filter(|s| members.iter().any(|m| m.conn_id == s.owner && m.display_name == wanted.owner_name))
        .filter(|s| !is_taken(s.owner, s.stream_id))
        .collect();
    candidates.sort_by_key(|s| (s.owner, s.stream_id));
    candidates.first().map(|s| (s.owner, s.stream_id))
}

/// One joined room: its QUIC connection and everything genuinely scoped to
/// that room. Replaces the old singleton `Connected`.
///
/// Every collection in here is room-scoped *by construction* — living inside
/// the `RoomSession` is what makes `(ConnId, StreamId)` keys unambiguous
/// again, since a `StreamId` is only unique within one connection and a
/// second room would otherwise reuse the same small integers for entirely
/// different streams.
pub struct RoomSession {
    pub client: ScreenshareClient,
    pub event_rx: mpsc::UnboundedReceiver<ClientEvent>,
    pub members: Vec<Member>,
    pub streams: Vec<StreamInfo>,
    /// The `server|room-id|key` string this session was connected with.
    ///
    /// `Why` it lives on the session: the sidebar renders `room_store ⟕
    /// rooms`, and the join string is the only key a `SavedRoom` and a live
    /// session both carry (see `RoomStore::name_for`).
    pub join_string: String,
    /// Rename field for this room's `RoomStore` entry, bound to the Stream
    /// Control tab's "save this room as" row.
    pub save_name: String,
    /// Set once the user has saved/renamed this room from that row, so the
    /// confirmation shows exactly once per edit.
    pub saved: bool,

    /// What this client is currently sending *into this room*, in the order
    /// the user started them. Back-references only: the `Destination`s
    /// themselves are owned by their `LocalCapture` in `AppState::captures`
    /// (see the module docs), because a capture outlives any single room it
    /// feeds.
    pub destinations: Vec<DestinationId>,
    /// Single per-room counter for allocating our own `stream_id`s on *this*
    /// connection, shared across video and audio (protocol nonce-domain
    /// separation is per stream_id, not per media kind). Per room rather than
    /// global: `stream_id` is scoped to a connection, so every room starts
    /// from 0 independently and two rooms handing out the same number is
    /// correct, not a collision.
    pub next_stream_id: StreamId,

    /// Streams we're currently subscribed to and decoding in this room, keyed
    /// by (owner conn_id, stream_id).
    ///
    /// `Why` `Arc<Mutex<_>>` around the whole map: the dedicated
    /// per-connection media-ingestion task spawned by `AppState::insert_room`
    /// (see `spawn_media_ingestion_task`) needs to look decoders up by
    /// `(ConnId, StreamId)` the instant a `MediaFrame` event arrives, from a
    /// different task than whatever's currently running the UI thread — this
    /// is what lets media delivery bypass the UI's throttled repaint cadence
    /// entirely.
    pub watches: Arc<Mutex<HashMap<(ConnId, StreamId), WatchEntry>>>,
    pub watch_error: Option<String>,

    /// Audio streams we're currently subscribed to and playing, keyed by
    /// (owner conn_id, audio stream_id) — the audio stream's own id, not the
    /// paired video's. Populated both by watching a video stream whose sender
    /// also has an active audio stream (auto-subscribe, see `toggle_watch`)
    /// and by clicking "Listen" on an audio-only stream (see `toggle_listen`).
    ///
    /// Shared with the media-ingestion task the same way and for the same
    /// reason as `watches` above.
    pub audio_watches: Arc<Mutex<HashMap<(ConnId, StreamId), AudioWatch>>>,

    /// Latest decoded preview still per active stream in this room,
    /// regardless of whether it's being watched — previews arrive for every
    /// active stream (spec design), so the "Streams" list row for any of them
    /// can show a thumbnail once one has arrived.
    pub preview_cache: PreviewCache,
    /// Lazily created/updated texture per stream for the "Streams" list
    /// thumbnail, keyed the same way as `preview_cache`/`watches`. Mirrors
    /// `WatchEntry::texture`'s create-once/`set`-in-place pattern.
    pub preview_textures: HashMap<(ConnId, StreamId), egui::TextureHandle>,

    /// Last volume percent (0-200) a viewer explicitly chose for each
    /// sender, keyed by owner `ConnId` alone — *not* `(ConnId, StreamId)`.
    ///
    /// `Why owner-only:` `AudioWatch::volume_percent` (the actually-playing
    /// copy) lives inside the `audio_watches` map keyed by the audio
    /// stream's own volatile `stream_id`, which is torn down and
    /// re-allocated by `sync_audio_watches` every time the sender restarts
    /// or switches their audio source (`teardown_audio_watch` drops the
    /// whole `AudioWatch`, `start_audio_watch` creates a fresh one). Without
    /// this owner-scoped copy surviving that churn, every restart/switch
    /// silently reset the viewer's chosen volume back to the
    /// hardcoded-100 default in `start_audio_watch`. Set from
    /// `RoomSession::set_volume` and `ui.rs`'s inline slider (the only two
    /// places a volume ever changes), consulted by `start_audio_watch` as
    /// the initial value for a newly created `AudioWatch`.
    ///
    /// Keying by `ConnId` alone (not paired with any stream id) is safe
    /// because `ConnId` is fresh-random per connection (see
    /// `superdoc/architecture/protocol-spec.md` §3.1), not a small
    /// server-assigned counter — a stale entry surviving a `MemberLeft`
    /// cannot practically collide with a later, unrelated sender's
    /// `ConnId`. Cleared on `MemberLeft` anyway (see
    /// `AppState::drain_events`), purely so this map doesn't grow forever
    /// across a long-lived room with many join/leave cycles.
    pub audio_volume_by_owner: HashMap<ConnId, u32>,

    /// Watches carried over from before a reconnect that are still waiting for
    /// their stream to be announced again (the owner may be reconnecting too,
    /// and a relay restart empties the room). Resolved against the roster by
    /// [`Self::apply_rewatch`] on every event pass; dropped at
    /// `rewatch_deadline` ([`REWATCH_GRACE`]) so a stream that never comes back
    /// does not start watching minutes later out of the blue.
    pub rewatch: Vec<WantedStream>,
    pub rewatch_deadline: Option<Instant>,
    /// Watches whose stream the sharer stopped (see [`RESUME_AFTER_STOP`]),
    /// each with its own deadline. Resolved by [`Self::apply_rewatch`] the
    /// same way as `rewatch`, but kept apart because the deadlines differ
    /// per entry and the two have different causes.
    pub resume_after_stop: Vec<(WantedStream, Instant)>,
}

impl RoomSession {
    /// The streams this session is watching or listening to, as
    /// [`WantedStream`]s (video watches and standalone-Listen audio; the audio
    /// that rides along with a video watch is re-derived by
    /// [`Self::sync_audio_watches`]). Entries whose stream or owner is no
    /// longer in the roster are skipped: there is nothing to match them by.
    pub fn wanted_streams(&self) -> Vec<WantedStream> {
        let name_of = |owner: ConnId| self.members.iter().find(|m| m.conn_id == owner).map(|m| m.display_name.clone());
        let info_of = |owner: ConnId, stream_id: StreamId| self.streams.iter().find(|s| s.owner == owner && s.stream_id == stream_id);
        let mut out = Vec::new();
        let mut keys: Vec<(ConnId, StreamId)> = self.watches.lock().unwrap().keys().copied().collect();
        keys.sort();
        for key in keys {
            if let (Some(owner_name), Some(info)) = (name_of(key.0), info_of(key.0, key.1)) {
                out.push(WantedStream {
                    owner_name,
                    kind: info.kind,
                    label: info.label.clone(),
                    volume_percent: self.audio_volume_by_owner.get(&key.0).copied(),
                });
            }
        }
        let mut keys: Vec<(ConnId, StreamId)> =
            self.audio_watches.lock().unwrap().iter().filter(|(_, w)| !w.auto).map(|(k, _)| *k).collect();
        keys.sort();
        for key in keys {
            if let (Some(owner_name), Some(info)) = (name_of(key.0), info_of(key.0, key.1)) {
                out.push(WantedStream {
                    owner_name,
                    kind: info.kind,
                    label: info.label.clone(),
                    volume_percent: self.audio_volume_by_owner.get(&key.0).copied(),
                });
            }
        }
        out
    }

    /// Resolve [`Self::rewatch`] and [`Self::resume_after_stop`] against the
    /// current roster: start watching every wanted stream that is announced
    /// (again), keep waiting for the rest until their deadline. Cheap when
    /// both are empty (the normal case), so `drain_events` calls it for every
    /// room every pass.
    pub fn apply_rewatch(&mut self, now: Instant) {
        if self.rewatch.is_empty() && self.resume_after_stop.is_empty() {
            return;
        }
        if self.rewatch_deadline.is_some_and(|deadline| now >= deadline) {
            self.rewatch.clear();
        }
        let mut still_waiting = Vec::new();
        for wanted in std::mem::take(&mut self.rewatch) {
            if !self.rewatch_one(&wanted) {
                still_waiting.push(wanted);
            }
        }
        self.rewatch = still_waiting;

        let mut still_waiting = Vec::new();
        for (wanted, deadline) in std::mem::take(&mut self.resume_after_stop) {
            if now < deadline && !self.rewatch_one(&wanted) {
                still_waiting.push((wanted, deadline));
            }
        }
        self.resume_after_stop = still_waiting;
    }

    /// Start `wanted` if a matching stream is announced right now. Returns
    /// whether it was matched (and so is no longer waiting).
    fn rewatch_one(&mut self, wanted: &WantedStream) -> bool {
        let me = self.client.conn_id();
        let found = {
            let watches = self.watches.lock().unwrap();
            let audio = self.audio_watches.lock().unwrap();
            match_wanted(wanted, me, &self.members, &self.streams, |owner, id| {
                watches.contains_key(&(owner, id)) || audio.contains_key(&(owner, id))
            })
        };
        let Some((owner, stream_id)) = found else { return false };
        if let Some(volume) = wanted.volume_percent {
            self.audio_volume_by_owner.insert(owner, volume);
        }
        match wanted.kind {
            StreamKind::Video => self.toggle_watch(owner, stream_id),
            // Audio rides along with its owner's video watch
            // (`sync_audio_watches`); only a sender with no video needs an
            // explicit Listen.
            StreamKind::Audio => {
                if !self.owner_has_video(owner) {
                    self.toggle_listen(owner, stream_id);
                }
            }
        }
        true
    }

    /// The sharer stopped `(owner, stream_id)` (relay `StreamStopped`): end
    /// our video watch of it, so its tile, decoder and GPU view go away
    /// instead of freezing on the last frame, and remember it for
    /// [`RESUME_AFTER_STOP`] so a re-share by the same person takes its place.
    /// Call *before* [`Self::note_stream_stopped`]: the owner's name and the
    /// stream's label are read from the roster. No-op when we are not watching
    /// that stream. Audio needs nothing here: [`Self::sync_audio_watches`]
    /// already drops audio whose stream is no longer announced.
    ///
    /// No `unsubscribe` is sent: the relay dropped the stream together with
    /// its subscriber list, so there is nothing left to unsubscribe from.
    pub fn end_watch_of_stopped(&mut self, owner: ConnId, stream_id: StreamId, now: Instant) {
        let key = (owner, stream_id);
        let wanted = {
            let owner_name = self.members.iter().find(|m| m.conn_id == owner).map(|m| m.display_name.clone());
            let info = self.streams.iter().find(|s| s.owner == owner && s.stream_id == stream_id);
            owner_name.zip(info).map(|(owner_name, info)| WantedStream {
                owner_name,
                kind: info.kind,
                label: info.label.clone(),
                volume_percent: self.audio_volume_by_owner.get(&owner).copied(),
            })
        };
        if self.watches.lock().unwrap().remove(&key).is_none() {
            return;
        }
        if let Some(wanted) = wanted {
            self.resume_after_stop.push((wanted, now + RESUME_AFTER_STOP));
        }
    }

    /// The `stream_id` of `owner`'s currently-announced audio stream, if
    /// any — used by `toggle_watch` to auto-subscribe audio alongside video
    /// from the same sender.
    pub fn sibling_audio_stream(&self, owner: ConnId) -> Option<StreamId> {
        sibling_audio_stream_in(&self.streams, owner)
    }

    /// Set playback volume (0-200%) and mute state for `owner`'s audio.
    ///
    /// `stream_id` may be either the audio stream's own id *or* the id of the
    /// video stream the UI is showing for that sender. `Why accept both:`
    /// audio follows video (see [`Self::sync_audio_watches`]) and is
    /// deliberately never surfaced to a viewer as its own stream, so the View
    /// tab's per-tile volume control only ever knows the *video* stream_id it
    /// rendered the tile from — it has no audio stream_id to pass and
    /// shouldn't need one. Resolving the sibling here keeps that ignorance in
    /// the UI, where it belongs, instead of leaking the audio stream id back
    /// into a UI whose whole point is that audio isn't a separate thing. A
    /// direct hit on the audio id still works unchanged, for the standalone
    /// audio-only "Listen" case.
    ///
    /// `volume_percent` is stored as-is (the slider's *desired* value) even
    /// while `muted` is true — [`AudioWatch::volume_percent`]'s doc comment
    /// explains why: it lets the eventual unmute (or another window's
    /// [`Self::volume`] query) recover the pre-mute value instead of seeing a
    /// flattened 0. The decoder itself still gets muted immediately
    /// ([`effective_gain`]).
    pub fn set_volume(&mut self, owner: ConnId, stream_id: StreamId, volume_percent: u32, muted: bool) {
        {
            let mut watches = self.audio_watches.lock().unwrap();
            let audio_stream_id =
                resolve_audio_stream_id(&self.streams, owner, stream_id, |id| watches.contains_key(&(owner, id)));
            if let Some(audio_stream_id) = audio_stream_id
                && let Some(watch) = watches.get_mut(&(owner, audio_stream_id))
            {
                watch.volume_percent = volume_percent;
                watch.muted = muted;
                watch.decoder.set_volume(effective_gain(volume_percent, muted));
            }
        }
        // Remember this owner's chosen volume independent of the current
        // audio stream id, so it survives the sender restarting/switching
        // their audio source (which tears down and recreates the
        // `AudioWatch` under a fresh `stream_id` — see
        // `RoomSession::audio_volume_by_owner`'s doc comment).
        self.audio_volume_by_owner.insert(owner, volume_percent);
    }

    /// Read back `owner`'s current volume/mute state — the counterpart query
    /// to [`Self::set_volume`], so a tile's slider can show the real state
    /// instead of a per-window default. [`RoomSession::audio_watches`] is the
    /// single source of truth every window reads when it renders a tile it
    /// hasn't seen before.
    ///
    /// Resolves `stream_id` the same video-or-audio-id-both-work way
    /// [`Self::set_volume`] does. Defaults to 100%/unmuted
    /// ([`VolumeState::default`]) when no audio watch is found yet, e.g. a
    /// tile whose audio subscription hasn't been reconciled by
    /// `sync_audio_watches` the instant the tile first renders.
    pub fn volume(&self, owner: ConnId, stream_id: StreamId) -> VolumeState {
        let watches = self.audio_watches.lock().unwrap();
        resolve_audio_stream_id(&self.streams, owner, stream_id, |id| watches.contains_key(&(owner, id)))
            .and_then(|audio_stream_id| watches.get(&(owner, audio_stream_id)))
            .map(|watch| VolumeState { volume_percent: watch.volume_percent, muted: watch.muted })
            .unwrap_or_default()
    }

    /// Whether `owner` currently has an announced video stream — used to
    /// decide whether an audio stream gets its own "Listen" row in the
    /// Streams list (only audio-only senders do; a sender with video has
    /// their audio auto-subscribed via `toggle_watch` instead).
    pub fn owner_has_video(&self, owner: ConnId) -> bool {
        self.streams.iter().any(|s| s.owner == owner && s.kind == StreamKind::Video)
    }

    /// How many *other* people are streaming into this room right now — the
    /// "N active" sidebar count the ROOMS row shows.
    ///
    /// Distinct owners, not stream count: someone sharing two monitors is one
    /// active user. Our own streams are excluded — this counts who *else* is
    /// on, not what we're personally sending (that's `destinations.len()`, a
    /// different number that stays 0 whenever nobody local is sharing even
    /// though someone else in the room is).
    pub fn active_stream_count(&self) -> usize {
        let me = self.client.conn_id();
        self.streams.iter().filter(|s| s.owner != me).map(|s| s.owner).collect::<std::collections::HashSet<_>>().len()
    }

    /// Record that `info.owner` has started a stream: bump their member
    /// row's `stream_count` and drop `info` into `streams`, replacing any
    /// existing entry for the same `(owner, stream_id)` pair rather than
    /// duplicating it.
    ///
    /// `Why this exists as a reusable method, not just inline in the
    /// presence-delta handler:` the relay deliberately excludes the
    /// announcing connection from its own `StreamStarted`/`StreamStopped`
    /// broadcasts (`except: Some(conn_id)` in the server's `relay.rs`), so a
    /// stream *we* just started never arrives back as a `PresenceEvent` —
    /// nothing would otherwise ever add our own stream to `streams`, and the
    /// sidebar's LIVE badge would never light up for ourselves.
    /// The call sites that actually bring a local destination live
    /// (`AppState::poll_pending_destinations`, `AppState::poll_capture_
    /// states`) call this directly, synthesizing exactly the `StreamInfo`
    /// the server's own broadcast would have carried; `AppState::drain_
    /// events`'s `StreamStarted` handler calls it too, for every *other*
    /// member's stream, so both paths stay identical.
    pub fn note_stream_started(&mut self, info: StreamInfo) {
        if let Some(m) = self.members.iter_mut().find(|m| m.conn_id == info.owner) {
            m.stream_count = m.stream_count.saturating_add(1);
        }
        self.streams.retain(|s| !(s.owner == info.owner && s.stream_id == info.stream_id));
        self.streams.push(info);
    }

    /// The stop-side of [`Self::note_stream_started`] — decrements
    /// `owner`'s `stream_count` and drops the matching `(owner, stream_id)`
    /// entry out of `streams`. Called both for our own stream stopping
    /// (from `AppState`'s various teardown paths, for the same "the relay
    /// never echoes this back to us" reason as `note_stream_started`) and,
    /// from `AppState::drain_events`, for every other member's
    /// `StreamStopped`.
    pub fn note_stream_stopped(&mut self, owner: ConnId, stream_id: StreamId) {
        if let Some(m) = self.members.iter_mut().find(|m| m.conn_id == owner) {
            m.stream_count = m.stream_count.saturating_sub(1);
        }
        self.streams.retain(|s| !(s.owner == owner && s.stream_id == stream_id));
    }

    /// Start decoding/playing `(owner, stream_id)`'s audio if not already
    /// doing so, and subscribe to it. Shared by `toggle_watch`'s
    /// auto-subscribe-audio-alongside-video path and `toggle_listen`'s
    /// standalone audio-only path.
    pub fn start_audio_watch(&mut self, owner: ConnId, stream_id: StreamId, auto: bool) {
        let key = (owner, stream_id);
        if self.audio_watches.lock().unwrap().contains_key(&key) {
            return;
        }
        match AudioDecoder::new(stream_id) {
            Ok(mut decoder) => {
                // Resume at whatever this owner's audio was last set to
                // (survives the sender restarting/switching their audio
                // source, which tears down and recreates the `AudioWatch`
                // under a fresh `stream_id` — see `audio_volume_by_owner`'s
                // doc comment) — default to 100 only if this owner has no
                // recorded preference yet.
                //
                // `muted` always starts `false` here: `audio_volume_by_owner`
                // only remembers the numeric slider value, not mute — a
                // known, narrow gap (a viewer who muted a sender, then has
                // that sender restart/switch their audio source before
                // unmuting, hears audio resume rather than staying silent).
                // Not the bug this field was added to fix (that's the
                // cross-window View-tab/pop-out sync — see `muted`'s doc
                // comment on `AudioWatch`), and out of scope to close here;
                // closing it would mean widening `audio_volume_by_owner` to
                // remember mute too, which touches `ui.rs`'s egui slider and
                // `end_to_end_test.rs`'s existing regression test for this
                // map's original purpose.
                let volume_percent = self.audio_volume_by_owner.get(&owner).copied().unwrap_or(DEFAULT_VOLUME_PERCENT);
                if volume_percent != 100 {
                    decoder.set_volume(volume_percent as f64 / 100.0);
                }
                self.audio_watches.lock().unwrap().insert(key, AudioWatch { decoder, volume_percent, muted: false, auto });
                self.watch_error = None;

                let client = self.client.clone();
                tokio::spawn(async move {
                    if let Err(e) = client.subscribe(owner, stream_id).await {
                        tracing::warn!("subscribe (audio) ({owner:016x}, {stream_id}) failed: {e:#}");
                    }
                });
            }
            Err(e) => {
                self.watch_error = Some(format!("{e:#}"));
            }
        }
    }

    /// Tear down `(owner, stream_id)`'s audio watch, if one is active, and
    /// unsubscribe. Dropping the removed `AudioWatch` drops its
    /// `AudioDecoder`, which synchronously shuts down its pipeline thread
    /// (see `AudioDecoder`'s `Drop` impl).
    pub fn teardown_audio_watch(&mut self, owner: ConnId, stream_id: StreamId) {
        if self.audio_watches.lock().unwrap().remove(&(owner, stream_id)).is_some() {
            let client = self.client.clone();
            tokio::spawn(async move {
                if let Err(e) = client.unsubscribe(owner, stream_id).await {
                    tracing::warn!("unsubscribe (audio) ({owner:016x}, {stream_id}) failed: {e:#}");
                }
            });
        }
    }

    /// Make the set of auto-subscribed audio watches match reality: exactly
    /// one audio watch per sender we currently have a *video* watch on, for
    /// as long as that sender has an audio stream announced — and nothing
    /// else.
    ///
    /// This is the whole "audio follows video" feature. One user action
    /// (Watch) gets both media; there is no separate audio subscribe/watch
    /// step, and no viewer-facing audio stream to click. See
    /// `superdoc/features/video-pipeline.md`'s 2026-07-28 block.
    ///
    /// `Why a reconcile pass instead of doing this once inside toggle_watch:`
    /// the naive version (latch `sibling_audio_stream(owner)` at the instant
    /// the video watch starts, tear it down when it stops) is only correct if
    /// the sender's audio stream exists *before* the viewer clicks Watch and
    /// never changes afterwards. Neither holds:
    ///
    /// - A sharer typically starts their screen share first and only then
    ///   picks an app in the Share Audio row. Every viewer already watching
    ///   at that moment latched `None` and stayed permanently silent, with no
    ///   way to fix it short of un-watching and re-watching. That is the bug
    ///   the owner reported as "the viewer should just hear it".
    /// - A sender with two monitors shared is two `WatchEntry`s but one audio
    ///   stream. Un-watching one of them tore down the audio the *other* one
    ///   was still relying on.
    /// - `driver.rs`'s `prune_stale_watches` and a room's control-plane close
    ///   both drop `WatchEntry`s without ever going through `toggle_watch`,
    ///   so the paired audio watch (and its subscription) leaked.
    ///
    /// Reconciling from current state handles all four cases with one rule
    /// and no bookkeeping to keep in sync. Called from `AppState::drain_events`
    /// — i.e. right after the `StreamStarted`/`StreamStopped` events that can
    /// change the answer are applied, on both frontends' polling cadence.
    /// Idempotent and allocation-light; returns immediately in the common
    /// case of nothing being watched at all.
    pub fn sync_audio_watches(&mut self) {
        if self.watches.lock().unwrap().is_empty() && self.audio_watches.lock().unwrap().is_empty() {
            return;
        }

        // Desired auto set: the sibling audio stream of every sender we have
        // a video watch on. Also refreshes each `WatchEntry::audio_stream_id`
        // so the UI's volume control can find the audio stream behind a tile.
        let mut desired: Vec<(ConnId, StreamId)> = Vec::new();
        {
            let mut watches = self.watches.lock().unwrap();
            for (&(owner, _), entry) in watches.iter_mut() {
                let sibling = self.streams.iter().find(|s| s.owner == owner && s.kind == StreamKind::Audio).map(|s| s.stream_id);
                entry.audio_stream_id = sibling;
                if let Some(stream_id) = sibling
                    && !desired.contains(&(owner, stream_id))
                {
                    desired.push((owner, stream_id));
                }
            }
        }

        // Tear down anything that shouldn't be playing any more: an auto
        // watch whose video watch (or whose audio stream) is gone, and — for
        // both auto and standalone-Listen watches alike — anything whose
        // stream the sender stopped announcing.
        let stale: Vec<(ConnId, StreamId)> = self
            .audio_watches
            .lock()
            .unwrap()
            .iter()
            .filter(|(key, watch)| {
                let (owner, stream_id) = **key;
                let still_announced = self.streams.iter().any(|s| s.owner == owner && s.stream_id == stream_id);
                !still_announced || (watch.auto && !desired.contains(&(owner, stream_id)))
            })
            .map(|(key, _)| *key)
            .collect();
        for (owner, stream_id) in stale {
            self.teardown_audio_watch(owner, stream_id);
        }

        // `start_audio_watch` is a no-op for one that's already running, so
        // this is the "started sharing audio while I was already watching"
        // case and nothing else.
        for (owner, stream_id) in desired {
            self.start_audio_watch(owner, stream_id, true);
        }
    }

    /// "Watch"/"Stop Watching" clicked for `(owner, stream_id)`. Subscribing
    /// and unsubscribing are async (control-plane round trip) so they're
    /// fired onto the runtime and not awaited here; the decoder itself is
    /// created/dropped synchronously since that's cheap.
    ///
    /// The sender's audio, if they have any, comes along automatically — see
    /// [`RoomSession::sync_audio_watches`], which this calls on both edges so
    /// the audio starts/stops in the same user action rather than waiting for
    /// the next `drain_events` tick.
    pub fn toggle_watch(&mut self, owner: ConnId, stream_id: StreamId) {
        let key = (owner, stream_id);
        let removed = self.watches.lock().unwrap().remove(&key);
        if removed.is_some() {
            let client = self.client.clone();
            tokio::spawn(async move {
                if let Err(e) = client.unsubscribe(owner, stream_id).await {
                    tracing::warn!("unsubscribe({owner:016x}, {stream_id}) failed: {e:#}");
                }
            });
        } else {
            match StreamDecoder::new(stream_id) {
                Ok(decoder) => {
                    // The announced picture size lets the decoder crop an
                    // encoder's padding lines (AMD AV1 codes 1080 as 1082).
                    if let Some(info) = self.streams.iter().find(|s| s.owner == owner && s.stream_id == stream_id)
                        && info.width > 0
                        && info.height > 0
                    {
                        decoder.set_display_size(u32::from(info.width), u32::from(info.height));
                    }
                    self.watches.lock().unwrap().insert(
                        key,
                        WatchEntry {
                            decoder: Arc::new(Mutex::new(decoder)),
                            audio_stream_id: None,
                        },
                    );
                    self.watch_error = None;

                    let client = self.client.clone();
                    tokio::spawn(async move {
                        if let Err(e) = client.subscribe(owner, stream_id).await {
                            tracing::warn!("subscribe({owner:016x}, {stream_id}) failed: {e:#}");
                        }
                    });
                }
                Err(e) => {
                    self.watch_error = Some(format!("{e:#}"));
                    return;
                }
            }
        }
        self.sync_audio_watches();
    }

    /// "Listen"/"Stop Listening" clicked for an audio-only stream (a sender
    /// with no video announced — see `owner_has_video`). A *standalone*
    /// watch (`auto: false`): unlike the audio that rides along with a video
    /// watch, `sync_audio_watches` never turns this one off on its own.
    pub fn toggle_listen(&mut self, owner: ConnId, stream_id: StreamId) {
        if self.audio_watches.lock().unwrap().contains_key(&(owner, stream_id)) {
            self.teardown_audio_watch(owner, stream_id);
        } else {
            self.start_audio_watch(owner, stream_id, false);
        }
    }
}

/// A connect attempt in flight, tagged with what it was for so its result can
/// be routed back to the row (or the "+ Add Room" card) that started it.
///
/// `Why` a struct instead of the bare `oneshot::Receiver` this used to be:
/// with an always-on ROOMS list there is no longer one join form owning the
/// only connect in the app. A saved row's `[Reconnect]` and the Add Room
/// card's `[Join]` both land here, and the sidebar has to know *which room
/// row* is the one currently showing the amber reconnecting dot — which needs
/// the join string, since a not-yet-connected room has no `RoomHandle`.
pub struct PendingConnect {
    pub rx: oneshot::Receiver<ConnectResult>,
    /// The room being connected to. Also what the reconnecting-dot lookup
    /// matches a `SavedRoom` row against.
    pub join_string: String,
    /// Whether this attempt came from the inline "+ Add Room" card, so a
    /// failure can be reported inside that card (which stays open) rather
    /// than as a detached error under the ROOMS list.
    pub from_add_card: bool,
    /// Handle to the `tokio::spawn`ed connect task, so a user-initiated
    /// cancel (`AppState::cancel_connect`) can abort it outright rather than
    /// waiting for it to resolve on its own. Safe to abort at any point up to
    /// success: `ScreenshareClient::connect` doesn't spawn any fan-out tasks
    /// (`control_reader_loop`/`datagram_reader_loop`/`preview_uni_loop`) until
    /// *after* it returns `Ok`, so there is nothing left half-registered to
    /// unwind — see `net.rs`'s `connect`.
    pub abort: tokio::task::AbortHandle,
    /// `Some(handle)` when this attempt is an automatic reconnect of the room
    /// that used to be `handle` (see [`ReconnectState`]); the result then
    /// re-inserts the room under the same handle and restores what the user
    /// had, instead of being treated as a fresh join.
    pub reconnect: Option<RoomHandle>,
}

/// Something of ours that was being sent into a room when its connection
/// dropped, kept so it can be re-established on the new connection.
#[derive(Debug, Clone)]
pub enum SuspendedShare {
    /// A video destination. The capture itself keeps running (a screen grab
    /// is a local thing and the portal prompt must not come back); only the
    /// encode branch bound to the dead connection was removed.
    Video { capture: CaptureSourceId, params: capture::destination::VideoParams, role: PendingRole },
    /// An audio share paired with `for_capture`. Its capture is bound to the
    /// dead connection as a whole, so it was stopped; this restarts it.
    Audio { for_capture: CaptureSourceId, source: AudioSource },
}

/// One room that dropped unexpectedly and is being reconnected. Lives in
/// [`AppState::reconnects`] from the drop until the room is back (the entry is
/// then consumed), the user cancels or leaves, or the relay rejects the join
/// for good (`Malformed`). It is what makes [`AppState::is_reconnecting`] true
/// for the whole outage, including the waits between attempts.
///
/// Public fields are the read side for the UI (banner: attempt count, next
/// retry, last error; People panel: last-known roster). The captured user
/// intent (watches, shares) is private: it only ever flows back into the new
/// connection.
pub struct ReconnectState {
    pub join_string: String,
    /// The handle the room had and gets back: kept stable so everything
    /// keyed by it (a destination's `room`, `active_room`, the UI's staged
    /// shares and views) stays valid across the outage.
    pub handle: RoomHandle,
    /// Attempts started in this outage (0 before the first retry).
    pub attempts: u32,
    /// When the next attempt may start; `None` while one is in flight.
    pub next_retry: Option<Instant>,
    /// The error of the last failed attempt (or the reason the connection
    /// dropped, before the first attempt).
    pub last_error: Option<String>,
    /// Why the connection / last attempt ended; drives [`retry_delay`].
    pub cause: CloseCause,
    pub since: Instant,
    /// The roster and streams as they were when the connection dropped, so the
    /// People panel is not empty during the outage. Stale by definition.
    pub members: Vec<Member>,
    pub streams: Vec<StreamInfo>,
    shares: Vec<SuspendedShare>,
    wanted: Vec<WantedStream>,
    was_active_room: bool,
    /// Set by the legacy no-key `cancel_connect()`; the entry is dropped at the
    /// next poll unless `start_connect`/`reconnect_now` revives it first. See
    /// `AppState::cancel_connect`.
    cancelled: bool,
}

impl ReconnectState {
    /// Time until the next attempt starts (`ZERO` if due, `None` while one is
    /// in flight). For "retrying in 4 s".
    pub fn retry_in(&self, now: Instant) -> Option<Duration> {
        self.next_retry.map(|at| at.saturating_duration_since(now))
    }

    /// Whether an attempt is running right now.
    pub fn attempt_in_flight(&self) -> bool {
        self.next_retry.is_none()
    }

    /// Whether the relay said the room is full (the UI can say "room full,
    /// still trying").
    pub fn room_full(&self) -> bool {
        self.cause == CloseCause::RoomFull
    }
}

/// What a just-resolved connect attempt was — returned by
/// `AppState::poll_connect_result` so a caller with UI-only state (the "+ Add
/// Room" card's error line, its open/closed state) can react without that
/// state needing to live on `AppState`.
pub enum ConnectOutcome {
    /// Nothing resolved this poll.
    None,
    /// Joined successfully; this is the new room's handle.
    Joined(RoomHandle),
    /// Failed. `from_add_card` mirrors `PendingConnect::from_add_card`.
    Failed { error: String, from_add_card: bool },
}

/// A destination whose encode branch is being built on an already-running
/// capture — the fan-out and transfer paths (`capture::CaptureHandle::
/// spawn_add_destination`). Polled by `AppState::poll_pending_destinations`.
pub struct PendingDestination {
    pub capture_id: CaptureSourceId,
    pub destination_id: DestinationId,
    pub room: RoomHandle,
    pub role: PendingRole,
    pub rx: oneshot::Receiver<Result<capture::DestinationHandle, String>>,
}

/// The state/logic layer behind the UI: every room, capture and destination
/// this client currently has live, plus global (not-room-scoped) settings.
/// The egui-facing `ui::App` wraps one of these plus its own purely-UI fields
/// (nav selection, which rooms are expanded, ...) — see `ui.rs`'s module
/// docs.
pub struct AppState {
    pub display_name: String,
    /// Whether `display_name` was chosen by the user (a non-empty
    /// `AppSettings::display_name` is saved) rather than the `whoami_fallback`
    /// default. `false` makes the UI offer the "Choose your name" modal.
    pub display_name_set: bool,
    pub status: Status,
    pub pending_connect: Option<PendingConnect>,

    /// The one local QUIC endpoint every room connection rides (see
    /// `net.rs`'s "One endpoint, N connections"). Built once here, at
    /// `AppState` construction, rather than per `connect()` call. `Err`
    /// carries the bind failure so the join screen can report it instead of
    /// the app panicking at startup over a socket it couldn't open.
    pub endpoint: Result<quinn::Endpoint, String>,

    /// Every room currently joined, in join order. See the module docs.
    pub rooms: IndexMap<RoomHandle, RoomSession>,
    /// Every local capture pipeline currently running, in start order, with
    /// its destinations. See the module docs.
    pub captures: IndexMap<CaptureSourceId, LocalCapture>,
    /// Which room the (still single-room) Stream Control and View tabs drive.
    pub active_room: Option<RoomHandle>,

    /// Monotonic id allocators — never rewind, never reuse (see the "Session
    /// ids" note above).
    pub next_room_handle: u32,
    pub next_capture_id: u32,
    pub next_destination_id: u32,

    /// Last error from starting/stopping a capture. `AppState`-level, not
    /// per-room: a capture is not owned by a room.
    pub share_error: Option<String>,
    /// Bitrate (kbps) to use for the *next* video capture started.
    pub bitrate_kbps: u32,
    /// Capture framerate cap (fps) to use for the *next* video capture
    /// started.
    pub framerate: u32,
    /// Encoder tune mode to use for the *next* video capture started —
    /// mirrors `bitrate_kbps`/`framerate`'s "process-wide setting applied
    /// before the share starts" shape (set via `set_tune`, read by
    /// `start_video_share`), rather than a per-call argument. See
    /// `capture::TuneMode`.
    pub tune: capture::TuneMode,
    /// Encoder quality level to use for the *next* video capture started.
    /// See `tune`'s `Why:` note and `capture::QualityLevel`.
    pub quality_level: capture::QualityLevel,
    /// GOP/keyframe interval (ms) to use for the *next* video capture
    /// started. See `tune`'s `Why:` note and
    /// `capture::destination::VideoParams::gop_millis`.
    pub gop_millis: u32,
    /// Encoder codec (H.264/H.265/AV1) to use for the *next* video capture
    /// started. See `tune`'s `Why:` note and `capture::VideoCodec`.
    pub codec: capture::VideoCodec,
    /// Which room the "Start a stream" card will start the next capture in.
    /// `None` until a room is joined; re-pointed by the picker.
    pub new_stream_room: Option<RoomHandle>,

    /// Absolute-Maximum vs. Per-Stream-Maximum (persisted preference).
    pub bandwidth_mode: BandwidthMode,
    /// Absolute-Maximum mode's single per-client total, kbit/s (persisted).
    pub absolute_total_kbps: u32,
    /// Absolute-Maximum mode's guaranteed per-destination floor (persisted).
    pub destination_floor_kbps: u32,
    /// Ceiling on the View tab's jitter buffer, in milliseconds (persisted).
    /// See `rooms::AppSettings::max_jitter_buffer_ms`.
    pub max_jitter_buffer_ms: u32,
    /// Ceiling, as a percent of wall-clock time, on the jitter buffer's
    /// catch-up playback speed (persisted). See
    /// `rooms::AppSettings::catch_up_speed_percent`.
    pub catch_up_speed_percent: u32,
    /// Rate limiting, hysteresis and last-allocation bookkeeping for
    /// Absolute-Maximum mode. Never persisted: it describes this session's
    /// live encoders.
    pub bandwidth: BandwidthController,

    /// Destinations whose encode branch is still being built (fan-out and
    /// transfer targets), polled once per frame by
    /// `poll_pending_destinations`.
    pub pending_destinations: Vec<PendingDestination>,

    /// Audio sources discovered by the last `list_audio_sources()` call.
    pub audio_sources: Vec<AudioSource>,
    pub audio_sources_error: Option<String>,
    /// Set once we've kicked off the first automatic listing, so we don't
    /// re-trigger it every frame.
    pub audio_sources_requested: bool,
    /// In-flight `list_audio_sources()` call, if any.
    pub audio_sources_rx: Option<oneshot::Receiver<Result<Vec<AudioSource>, String>>>,
    pub selected_audio_source: Option<AudioSource>,

    /// Live non-announcing level taps backing the "Add audio"/"Switch"
    /// dropdown's per-candidate meters — see [`AudioLevelTapId`] and
    /// [`audio::AudioLevelTapHandle`]. Only ever holds entries for however
    /// long a dropdown is actually open on the frontend; there is no
    /// standing/background tap.
    pub audio_level_taps: HashMap<AudioLevelTapId, audio::AudioLevelTapHandle>,
    /// Monotonic allocator for [`AudioLevelTapId`] — same never-rewind,
    /// never-reuse rationale as the "Session ids" note above.
    pub next_audio_level_tap_id: u32,

    pub room_store: RoomStore,
    pub room_store_error: Option<String>,
    /// Join strings still waiting to be auto-connected at startup (see
    /// `main.rs`'s `.setup()` and [`Self::poll_auto_connect_queue`]). A queue
    /// rather than firing every `start_connect` at once: `pending_connect`
    /// is a single slot (one in-flight connect at a time — see
    /// `start_connect`'s doc comment), so starting a second auto-connect
    /// before the first resolves would silently overwrite and orphan it
    /// (its `oneshot` result would arrive with nowhere to be read, and the
    /// room, even if the handshake succeeded, would never make it into
    /// `self.rooms`). Drained one at a time by the driver's tick loop
    /// instead.
    pub auto_connect_queue: Vec<String>,
    /// Rooms whose connection dropped unexpectedly and are being reconnected,
    /// by their (kept) handle. See [`ReconnectState`] and the "Auto-reconnect"
    /// section of the `AppState` impl.
    pub reconnects: IndexMap<RoomHandle, ReconnectState>,
    /// The audio source each running audio share was started with, so a
    /// reconnect can restart it (the capture only keeps the source's name).
    pub audio_share_sources: HashMap<CaptureSourceId, AudioSource>,
    /// Set by [`Self::begin_shutdown`]: from then on nothing reconnects.
    pub shutting_down: bool,
    /// Current UI language ("en" or "de"), mirrored into `rust_i18n`'s global
    /// locale on startup and on every toggle.
    pub language: String,
    /// Current `presentation_mode` selection. Only takes effect on the *next*
    /// launch — see `superdoc/features/presentation-modes.md`.
    pub presentation_mode: PresentationMode,
    /// The presentation mode that was actually active when this window was
    /// created (i.e. what was persisted at launch, before any in-session
    /// change). Compared against `presentation_mode` to show a "restart to
    /// apply" hint only once the two diverge.
    pub launch_presentation_mode: PresentationMode,
    /// How long `start_connect` waits for `ScreenshareClient::connect` before
    /// giving up with a timeout error. `pub(crate)` and mutable (rather than a
    /// bare constant) purely so tests can shrink it instead of literally
    /// sleeping 15s — production always leaves this at
    /// [`DEFAULT_CONNECT_TIMEOUT`].
    pub(crate) connect_timeout: Duration,
    /// Per-attempt timeout of an automatic reconnect; test-shrinkable like
    /// `connect_timeout`. Production leaves [`RECONNECT_ATTEMPT_TIMEOUT`].
    pub(crate) reconnect_attempt_timeout: Duration,
}

/// Default value of [`AppState::connect_timeout`] — the feature request's
/// "connecting should time out after 15 seconds".
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

impl Default for AppState {
    fn default() -> Self {
        let (room_store, room_store_error) = match RoomStore::load() {
            Ok(store) => (store, None),
            Err(e) => (RoomStore::default(), Some(e.to_string())),
        };
        let settings = AppSettings::load();
        rust_i18n::set_locale(&settings.language);
        // The global display-name setting (Settings tab) wins once it's been
        // set; empty (never set, or explicitly cleared) falls back to the
        // OS-username default, same as this app's behavior before the
        // setting existed — see `rooms::AppSettings::display_name`.
        let display_name_set = !settings.display_name.trim().is_empty();
        let display_name = if display_name_set {
            settings.display_name.clone()
        } else {
            whoami_fallback()
        };
        Self {
            display_name,
            display_name_set,
            status: Status::Idle,
            pending_connect: None,
            endpoint: crate::net::build_client_endpoint().map_err(|e| format!("{e:#}")),
            rooms: IndexMap::new(),
            captures: IndexMap::new(),
            active_room: None,
            next_room_handle: 0,
            next_capture_id: 0,
            next_destination_id: 0,
            share_error: None,
            bitrate_kbps: DEFAULT_BITRATE_KBPS,
            framerate: DEFAULT_FRAMERATE,
            tune: capture::TuneMode::default(),
            quality_level: capture::QualityLevel::default(),
            gop_millis: capture::DEFAULT_GOP_MILLIS,
            codec: capture::VideoCodec::default(),
            new_stream_room: None,
            bandwidth_mode: settings.bandwidth_mode,
            absolute_total_kbps: settings.absolute_total_kbps,
            destination_floor_kbps: settings.destination_floor_kbps,
            max_jitter_buffer_ms: settings.max_jitter_buffer_ms,
            catch_up_speed_percent: settings.catch_up_speed_percent,
            bandwidth: BandwidthController::new(),
            pending_destinations: Vec::new(),
            audio_sources: Vec::new(),
            audio_sources_error: None,
            audio_sources_requested: false,
            audio_sources_rx: None,
            selected_audio_source: None,
            audio_level_taps: HashMap::new(),
            next_audio_level_tap_id: 0,
            room_store,
            room_store_error,
            auto_connect_queue: Vec::new(),
            reconnects: IndexMap::new(),
            audio_share_sources: HashMap::new(),
            shutting_down: false,
            language: settings.language,
            presentation_mode: settings.presentation_mode,
            launch_presentation_mode: settings.presentation_mode,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            reconnect_attempt_timeout: RECONNECT_ATTEMPT_TIMEOUT,
        }
    }
}

/// Persist the current language choice, logging (not panicking) on failure —
/// losing a settings preference across restarts is a minor inconvenience, not
/// worth surfacing as a UI error.
///
/// Loads the existing settings file first rather than constructing a fresh
/// `AppSettings` from just `language`: `AppSettings` now also carries
/// `presentation_mode`/bandwidth fields, and building a literal with only
/// `language` set would silently reset those to their defaults every time the
/// language toggle is touched.
pub fn save_language(language: &str) {
    let mut settings = AppSettings::load();
    settings.language = language.to_string();
    if let Err(e) = settings.save() {
        tracing::warn!("failed to save language preference: {e:#}");
    }
}

/// Persist the current presentation-mode choice. Mirrors `save_language`'s
/// load-modify-save shape for the same reason: don't clobber the sibling
/// setting.
pub fn save_presentation_mode(mode: PresentationMode) {
    let mut settings = AppSettings::load();
    settings.presentation_mode = mode;
    if let Err(e) = settings.save() {
        tracing::warn!("failed to save presentation mode preference: {e:#}");
    }
}

/// Persist the bandwidth preferences. Same load-modify-save shape as
/// `save_language`/`save_presentation_mode`, for the same reason: writing a
/// freshly-constructed `AppSettings` would reset every sibling field.
///
/// `Why` all three bandwidth fields are written together rather than one
/// per edit: they're one coherent preference set, and — crucially — writing
/// them together is what guarantees that changing the *mode* re-saves the
/// other mode's number unchanged instead of dropping it. See
/// `rooms::AppSettings`' `Why:` note on keeping both modes' values
/// independent.
pub fn save_bandwidth_settings(mode: BandwidthMode, absolute_total_kbps: u32, destination_floor_kbps: u32) {
    let mut settings = AppSettings::load();
    settings.bandwidth_mode = mode;
    settings.absolute_total_kbps = absolute_total_kbps;
    settings.destination_floor_kbps = destination_floor_kbps;
    if let Err(e) = settings.save() {
        tracing::warn!("failed to save bandwidth preferences: {e:#}");
    }
}

/// Persist the jitter buffer ceiling. Mirrors `save_language`/
/// `save_presentation_mode`'s load-modify-save shape, for the same reason:
/// don't clobber the sibling settings. Clamps to `AppSettings`' 0..=5000
/// range so a corrupt or out-of-range caller can't persist a value the
/// frontend's own range input wouldn't allow.
pub fn save_max_jitter_buffer_ms(ms: u32) {
    let mut settings = AppSettings::load();
    settings.max_jitter_buffer_ms = AppSettings::clamp_max_jitter_buffer_ms(ms);
    if let Err(e) = settings.save() {
        tracing::warn!("failed to save jitter buffer preference: {e:#}");
    }
}

/// Persist the catch-up speed ceiling. Mirrors `save_max_jitter_buffer_ms`'s
/// load-modify-save shape and defense-in-depth clamp — don't clobber the
/// sibling settings, and don't trust a corrupt/out-of-range caller over the
/// frontend's own 5-10 range input.
pub fn save_catch_up_speed_percent(percent: u32) {
    let mut settings = AppSettings::load();
    settings.catch_up_speed_percent = AppSettings::clamp_catch_up_speed_percent(percent);
    if let Err(e) = settings.save() {
        tracing::warn!("failed to save catch-up speed preference: {e:#}");
    }
}

/// Persist the "START A STREAM" form's current values. Same load-modify-save
/// shape as `save_bandwidth_settings`, and for the same reason all five are
/// written together: they're one coherent form, saved on every change to
/// that form, so a mid-edit crash never leaves the persisted defaults half-updated.
pub fn save_start_stream_defaults(
    framerate: u32,
    bitrate_kbps: u32,
    tune: crate::capture::TuneMode,
    quality_level: crate::capture::QualityLevel,
    codec: crate::capture::VideoCodec,
) {
    let mut settings = AppSettings::load();
    settings.start_stream_defaults =
        crate::rooms::StartStreamDefaults { framerate, bitrate_kbps, tune, quality_level, codec };
    if let Err(e) = settings.save() {
        tracing::warn!("failed to save start-stream defaults: {e:#}");
    }
}

/// Persist the global display name (Settings tab). Mirrors `save_language`'s
/// load-modify-save shape, for the same reason: don't clobber the sibling
/// settings. Empty is a valid value to save — it just means "unset", and
/// `AppState::default()` falls back to `whoami_fallback()` again on the next
/// launch (see that function's call site above).
pub fn save_display_name(display_name: &str) {
    let mut settings = AppSettings::load();
    settings.display_name = display_name.to_string();
    if let Err(e) = settings.save() {
        tracing::warn!("failed to save display name preference: {e:#}");
    }
}

/// A small local fallback so the display-name field isn't empty by default.
/// Not a real identity system — just a nicer starting point than "".
pub fn whoami_fallback() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| t!("common.default_display_name").to_string())
}

/// Resolve a `RoomSession::destinations` back-reference to the capture that
/// owns it. `None` if the capture is already gone (a stale back-reference is
/// harmless — ids are never reused, so a miss can only ever be a miss).
pub fn find_destination(
    captures: &IndexMap<CaptureSourceId, LocalCapture>,
    id: DestinationId,
) -> Option<(CaptureSourceId, &LocalCapture, &Destination)> {
    captures
        .iter()
        .find_map(|(capture_id, capture)| capture.destinations.get(&id).map(|dest| (*capture_id, capture, dest)))
}

/// The capture currently feeding `stream_id` into `room`, if any. This is the
/// multi-room-correct inverse of the old `active_shares[stream_id]` lookup:
/// `stream_id` alone is ambiguous once more than one connection exists, so
/// the room the event arrived on has to be part of the key.
/// Returns the destination's own id alongside the capture, because everything
/// a relay event drives (pause/resume, force keyframe) is now per
/// *destination* — one capture can be feeding several rooms, each with its own
/// encode branch and its own subscriber count.
pub fn capture_feeding(
    captures: &IndexMap<CaptureSourceId, LocalCapture>,
    room: RoomHandle,
    stream_id: StreamId,
) -> Option<(&LocalCapture, DestinationId)> {
    captures.values().find_map(|capture| {
        capture
            .destinations
            .iter()
            .find(|(_, d)| d.room == room && d.stream_id == stream_id)
            .map(|(destination_id, _)| (capture, *destination_id))
    })
}

/// The label a room row shows: its saved name if it has one, otherwise a
/// stable short id derived from the join string's room-id field.
///
/// `Why` an id-derived fallback rather than showing the raw join string: the
/// join string contains the room *key*, i.e. the secret that grants access. A
/// sidebar is exactly the sort of thing that ends up in a screenshot or a
/// screen share, so it must never render one.
pub fn room_label(room_store: &RoomStore, join_string: &str) -> String {
    if let Some(name) = room_store.name_for(join_string) {
        return name.to_string();
    }
    t!("rooms.unnamed", id = room_id_prefix(join_string)).to_string()
}

/// First 4 bytes of the room id, hex — short enough to read in a sidebar row,
/// long enough to tell two rooms apart. Falls back to a placeholder for a
/// join string that doesn't parse (which can't have connected, so this is
/// only ever reachable for a corrupt `rooms.toml` entry).
pub fn room_id_prefix(join_string: &str) -> String {
    match crate::net::parse_join_string(join_string) {
        Ok(parsed) => parsed.room_id.iter().take(4).map(|b| format!("{b:02x}")).collect(),
        Err(_) => t!("rooms.unknown_id").to_string(),
    }
}

/// Always-on per-connection task that reads `raw_events` — the exact
/// `mpsc::UnboundedReceiver<ClientEvent>` `ScreenshareClient::connect`
/// returns — as fast as events arrive, independent of the UI's repaint-driven
/// cadence, and forwards everything except `MediaFrame` on to `forward_tx`
/// (what `RoomSession::event_rx` actually is) for `AppState::drain_events` to
/// keep handling exactly as before, at its normal throttled pace. See
/// `ui.rs`'s original module docs (preserved in git history) for the full
/// "fast-forward catch-up" root-cause writeup this task fixes.
pub async fn spawn_media_ingestion_task(
    mut raw_events: mpsc::UnboundedReceiver<ClientEvent>,
    forward_tx: mpsc::UnboundedSender<ClientEvent>,
    watches: Arc<Mutex<HashMap<(ConnId, StreamId), WatchEntry>>>,
    audio_watches: Arc<Mutex<HashMap<(ConnId, StreamId), AudioWatch>>>,
) {
    loop {
        // Racing `raw_events.recv()` against `forward_tx.closed()` (rather
        // than only detecting a dropped `RoomSession` via a failed
        // `forward_tx.send()`) is what makes this task tear down immediately
        // on disconnect: `forward_tx.closed()` resolves the instant
        // `RoomSession::event_rx` (`forward_rx`) is dropped, independent of
        // whether any event is in flight.
        let event = tokio::select! {
            event = raw_events.recv() => event,
            _ = forward_tx.closed() => return,
        };
        let Some(event) = event else { return };
        match event {
            ClientEvent::MediaFrame(frame) => {
                let key = (frame.conn_id, frame.stream_id);
                // A given (conn_id, stream_id) only ever matches one of
                // `watches` (video) or `audio_watches` (audio) — each owner
                // allocates a distinct stream_id per share regardless of
                // kind — so both are checked unconditionally rather than
                // needing to know the frame's `StreamKind` up front.
                if let Some(entry) = watches.lock().unwrap().get(&key) {
                    entry.decoder.lock().unwrap().feed(&frame);
                }
                if let Some(audio_watch) = audio_watches.lock().unwrap().get_mut(&key) {
                    audio_watch.decoder.feed(&frame);
                }
            }
            other => {
                // Everything else is not latency-sensitive (membership
                // changes, stream announce/stop, subscriber counts) and
                // belongs on `drain_events`'s normal throttled cadence. A
                // send failure here means the room's `RoomSession` (and its
                // `forward_rx`) has already been dropped.
                if forward_tx.send(other).is_err() {
                    return;
                }
            }
        }
    }
}

/// Capture-side operations: everything that owns or drives a local GStreamer
/// pipeline, plus fan-out/transfer/edit and bandwidth-allocation wiring.
impl AppState {
    pub fn alloc_capture_id(&mut self) -> CaptureSourceId {
        let id = CaptureSourceId(self.next_capture_id);
        self.next_capture_id = self.next_capture_id.wrapping_add(1);
        id
    }

    pub fn alloc_destination_id(&mut self) -> DestinationId {
        let id = DestinationId(self.next_destination_id);
        self.next_destination_id = self.next_destination_id.wrapping_add(1);
        id
    }

    /// "Start streaming" clicked: allocate a capture + its first destination
    /// into `room_handle` (with a fresh `stream_id` drawn from *that room's*
    /// counter) and spawn `capture::start_sharing` onto the ambient tokio
    /// runtime. Can be called again while other captures (video or audio) are
    /// already running — each call prompts the OS portal picker again, so a
    /// second click lets the user pick a different monitor/window.
    ///
    /// To send the *same* capture into another room, use
    /// `add_destination` instead — that's fan-out (one screen grab, a second
    /// independently encoded and encrypted stream), not a second capture.
    pub fn start_video_share(&mut self, room_handle: RoomHandle) {
        let capture_id = self.alloc_capture_id();
        let destination_id = self.alloc_destination_id();

        let Some(room) = self.rooms.get_mut(&room_handle) else { return };
        let stream_id = room.next_stream_id;
        room.next_stream_id = room.next_stream_id.wrapping_add(1);
        room.destinations.push(destination_id);
        let client = room.client.clone();

        let bitrate_kbps = self.bitrate_kbps;
        let framerate = self.framerate;
        let params = capture::destination::VideoParams {
            // Width/height 0 = "whatever the source is": this destination
            // doesn't downscale, and the source's real size isn't known until
            // the pipeline has negotiated caps.
            width: 0,
            height: 0,
            framerate,
            bitrate_kbps,
            backend: capture::EncoderBackend::default(),
            // `self.tune`/`self.quality_level`, not
            // `TuneMode::default()`/etc.: these are the same "process-wide,
            // set via a pre-start command" setting as `bitrate_kbps`/
            // `framerate` above (see their own doc comments on `AppState`) —
            // the "START A STREAM" form's Preset/Advanced-tuning controls
            // set them via `set_tune`/`set_quality_level`
            // before calling this.
            tune: self.tune,
            quality_level: self.quality_level,
            gop_millis: self.gop_millis,
            codec: self.codec,
        };
        let (tx, rx) = oneshot::channel();
        self.captures.insert(
            capture_id,
            LocalCapture {
                kind: StreamKind::Video,
                label: t!("share.video_label").to_string(),
                status: ShareStatus::Starting(rx),
                audio_for_capture: None,
                destinations: IndexMap::from([(
                    destination_id,
                    Destination {
                        room: room_handle,
                        stream_id,
                        params,
                        runtime: None,
                        pending: None,
                        edit: Destination::edit_from(params),
                        apply_rx: None,
                    },
                )]),
            },
        );
        self.share_error = None;

        tokio::spawn(async move {
            let result = capture::start_sharing(&client, stream_id, DestKey(destination_id.0), bitrate_kbps, framerate)
                .await
                .map(ShareHandle::Video)
                .map_err(|e| start_error_string(&e));
            let _ = tx.send(result);
        });
    }

    /// "Share Screen"/"Share another screen" clicked: the staged half of the
    /// "Go live" flow (see `superdoc/features/stream-control.md`'s
    /// 2026-07-28 note). Allocates a capture and spawns
    /// [`capture::start_capture`] — same portal prompt, same real GStreamer
    /// pipeline `start_video_share` starts, but with **zero destinations**:
    /// nothing is sent to any room, and no `AnnounceStream` fires, until
    /// [`Self::go_live`] attaches the first one. Returns the new capture's id
    /// so the caller can address the staged card's "Go live"/"Cancel"
    /// buttons with it.
    ///
    /// `Why this can reuse `stop_share` for "Cancel":` a staged capture is
    /// `ShareStatus::Active` with an empty `destinations` map — exactly the
    /// state `stop_share` already knows how to tear down; no separate cancel
    /// path is needed.
    pub fn stage_video_share(&mut self) -> CaptureSourceId {
        let capture_id = self.alloc_capture_id();
        let framerate = self.framerate;
        let (tx, rx) = oneshot::channel();
        self.captures.insert(
            capture_id,
            LocalCapture {
                kind: StreamKind::Video,
                label: t!("share.video_label").to_string(),
                status: ShareStatus::Starting(rx),
                audio_for_capture: None,
                destinations: IndexMap::new(),
            },
        );
        self.share_error = None;

        tokio::spawn(async move {
            let result = capture::start_capture(framerate).await.map(ShareHandle::Video).map_err(|e| start_error_string(&e));
            let _ = tx.send(result);
        });
        capture_id
    }

    /// "Go live" clicked on a staged card: attach `capture_id`'s first
    /// destination into `room_handle`, using the *current* process-wide
    /// framerate/bitrate/tune/quality-level settings — the same
    /// ones the staged card's Preset/Advanced-tuning/Framerate/Bitrate
    /// controls already write via `set_framerate`/`set_bitrate`/`set_tune`/
    /// `set_quality_level` (mirroring `start_video_share`'s
    /// own "applied before the call, not passed as arguments" shape — see
    /// that method's doc comment). The caller persists those same
    /// values as the new defaults at this exact moment, not while the card is merely staged.
    ///
    /// Mechanically this *is* [`Self::add_destination`]'s fan-out primitive —
    /// see `capture::start_capture`'s doc comment for why attaching the first
    /// branch to a running, zero-destination tee is not architecturally
    /// different from attaching the second. The only addition here is
    /// raising the capture's source-framerate floor first, in case the user
    /// picked a higher framerate on the staged card than the capture actually
    /// started at (see [`capture::CaptureHandle::set_source_framerate`]'s
    /// doc comment for why that never needs a pipeline restart).
    pub fn go_live(&mut self, capture_id: CaptureSourceId, room_handle: RoomHandle) {
        // Unlike `start_video_share`'s `width: 0, height: 0` ("whatever the
        // source is negotiated to be" — resolved later inside
        // `capture::start_sharing_with_source`, which builds its own
        // `VideoParams` from `handle.source_caps()`/`detect_encoder_backend()`
        // after the pipeline exists), `go_live` attaches a destination to a
        // capture that has *already* been running since `stage_video_share` —
        // its real negotiated size is available right now via
        // `CaptureHandle::source_caps`, and this is the only place that ever
        // builds this destination's `VideoParams` (there is no later
        // "internal" rebuild step to fix a placeholder up, the way
        // `start_sharing_with_source` does for the legacy path). Leaving
        // `width`/`height` at 0 here made the "Go live" card permanently show
        // 0×0 instead of the real resolution the staged card had just shown.
        // Same reasoning for `backend`: `EncoderBackend::default()` is a
        // static `Software`, not a real probe, so it silently forced every
        // "Go live" share onto the CPU encoder — use `detect_encoder_backend`
        // (the same real hardware probe `start_sharing_with_source` and every
        // other production call site use) instead.
        let source = if let Some(capture) = self.captures.get(&capture_id)
            && let ShareStatus::Active(ShareHandle::Video(handle)) = &capture.status
        {
            handle.set_source_framerate(self.framerate);
            handle.source_caps()
        } else {
            capture::destination::SourceCaps { width: 0, height: 0, framerate: self.framerate }
        };
        let params = capture::destination::VideoParams {
            width: source.width,
            height: source.height,
            framerate: self.framerate,
            bitrate_kbps: self.bitrate_kbps,
            backend: capture::detect_encoder_backend(),
            tune: self.tune,
            quality_level: self.quality_level,
            gop_millis: self.gop_millis,
            codec: self.codec,
        };
        self.add_destination_with_params(capture_id, room_handle, PendingRole::GoingLive, params);
    }

    /// **Fan-out**: send an already-running capture into one more room, as a
    /// second fully independent encoded+encrypted stream off the same screen
    /// grab. `role` distinguishes plain fan-out from the make-before-break
    /// half of a transfer (see `transfer_destination`).
    ///
    /// `Why this is not "free replication":` the raw frames are shared (one
    /// capture, one `tee`), but everything downstream of that tee is not —
    /// each destination gets its own encoder, its own bitrate, and its own
    /// AEAD keys/nonce space, because a room key can never be reused across
    /// rooms. Two rooms therefore cost roughly two streams' bandwidth, which
    /// the Stream Control tab states outright rather than leaving the user to
    /// discover on their uplink.
    pub fn add_destination(&mut self, capture_id: CaptureSourceId, room_handle: RoomHandle, role: PendingRole) {
        let Some(capture) = self.captures.get(&capture_id) else { return };
        // Params for the new destination: copy whichever destination this
        // capture already has (its resolution/framerate are already valid for
        // this source), so fanning out doesn't silently reconfigure anything.
        //
        // Fallback (no sibling destination — a genuinely zero-destination
        // capture, e.g. `go_live` reaching this indirectly, or a future
        // caller): read the *real* negotiated size straight off the running
        // `CaptureHandle` (`source_caps()`) instead of a `width: 0, height: 0`
        // placeholder that nothing downstream ever resolves — see
        // `go_live`'s doc comment for why that placeholder is fine for
        // `start_video_share` (whose separate `capture::start_sharing_with_
        // source` path rebuilds real `VideoParams` internally) but wrong
        // here, where `add_destination_with_params` uses exactly what's
        // passed with no later fixup. Same reasoning for `backend`: probe for
        // real hardware via `detect_encoder_backend` rather than defaulting
        // to the static `Software` variant.
        let params = capture.destinations.values().next().map(|d| d.live_params()).unwrap_or_else(|| {
            let source = match &capture.status {
                ShareStatus::Active(ShareHandle::Video(handle)) => handle.source_caps(),
                _ => capture::destination::SourceCaps { width: 0, height: 0, framerate: self.framerate },
            };
            capture::destination::VideoParams {
                width: source.width,
                height: source.height,
                framerate: self.framerate,
                bitrate_kbps: self.bitrate_kbps,
                backend: capture::detect_encoder_backend(),
                tune: capture::TuneMode::default(),
                quality_level: capture::QualityLevel::default(),
                gop_millis: capture::DEFAULT_GOP_MILLIS,
                codec: capture::VideoCodec::default(),
            }
        });
        self.add_destination_with_params(capture_id, room_handle, role, params);
    }

    /// The guts of [`Self::add_destination`], taking the new destination's
    /// encode params explicitly rather than deriving them from a sibling
    /// destination. Split out for [`Self::retry_destination`], which needs to
    /// hand in the failed destination's *own* last-known params (its
    /// bitrate/tune/quality_level) instead of the "copy a
    /// sibling, or fall back to session defaults" rule that's right for a
    /// genuinely new fan-out but wrong for resurrecting one that already had
    /// its own tuning — see that method's `Why:` note.
    fn add_destination_with_params(
        &mut self,
        capture_id: CaptureSourceId,
        room_handle: RoomHandle,
        role: PendingRole,
        params: capture::destination::VideoParams,
    ) {
        let destination_id = self.alloc_destination_id();

        let Some(capture) = self.captures.get(&capture_id) else { return };
        if capture.kind != StreamKind::Video {
            return;
        }

        let Some(room) = self.rooms.get_mut(&room_handle) else { return };
        let stream_id = room.next_stream_id;
        room.next_stream_id = room.next_stream_id.wrapping_add(1);
        room.destinations.push(destination_id);
        let client = room.client.clone();

        // From here on, bail out through `abandon` rather than a bare
        // `return`: the room already holds a `destinations` back-reference to
        // an id that would otherwise never get a `Destination` — a dangling
        // entry that makes the room look like it is streaming something it
        // isn't (and blocks a silent leave, since `leave_needs_confirm` counts
        // exactly that list).
        let abandon = |state: &mut Self, error: Option<String>| {
            if let Some(room) = state.rooms.get_mut(&room_handle) {
                room.destinations.retain(|d| *d != destination_id);
            }
            state.share_error = error;
        };

        let running = self
            .captures
            .get(&capture_id)
            .is_some_and(|c| matches!(c.status, ShareStatus::Active(ShareHandle::Video(_))));
        if !running {
            // Only a running capture has a tee to hang another branch off.
            abandon(self, Some(t!("stream_control.capture_not_running").to_string()));
            return;
        }

        let Some(capture) = self.captures.get_mut(&capture_id) else {
            abandon(self, Some(t!("stream_control.capture_not_running").to_string()));
            return;
        };
        let ShareStatus::Active(ShareHandle::Video(handle)) = &capture.status else {
            unreachable!("just checked the capture is an active video capture");
        };
        let rx = handle.spawn_add_destination(&client, stream_id, DestKey(destination_id.0), params);

        capture.destinations.insert(
            destination_id,
            Destination {
                room: room_handle,
                stream_id,
                params,
                runtime: None,
                pending: Some(role),
                edit: Destination::edit_from(params),
                apply_rx: None,
            },
        );
        self.pending_destinations.push(PendingDestination {
            capture_id,
            destination_id,
            room: room_handle,
            role,
            rx,
        });
        self.share_error = None;
    }

    /// **Transfer**: move one outgoing stream to a different room,
    /// make-before-break — add a destination in the target room off the *same*
    /// capture, wait for it to actually go live, and only then stop the old
    /// one.
    ///
    /// `Why make-before-break rather than re-pointing the existing stream:` a
    /// destination's encryption keys and nonce space are room-bound (see
    /// `net.rs`), so "the same stream, now in another room" is not
    /// expressible — the bytes have to be re-encrypted under the target
    /// room's key on a stream announced in that room. Building the new
    /// destination first means the user's audience in the *old* room keeps
    /// seeing frames until the new room is genuinely ready, and a failure
    /// leaves them exactly where they were rather than off the air.
    ///
    /// `Why this reuses the branch add/remove primitives and never touches
    /// the capture source:` nothing about the screen grab changes — the same
    /// `tee` feeds both destinations during the overlap. Routing a transfer
    /// through the source-restart path would stall *every* destination of
    /// that capture to accomplish something that costs one extra branch for a
    /// second.
    pub fn transfer_destination(&mut self, destination_id: DestinationId, target_room: RoomHandle) {
        let Some((capture_id, _, dest)) = find_destination(&self.captures, destination_id) else { return };
        if dest.room == target_room || dest.pending.is_some() {
            return;
        }
        let source_room = dest.room;
        // Mark the old destination first: from here until the new one
        // confirms, the UI shows both rows as a transfer in flight rather
        // than as two unrelated streams.
        if let Some(capture) = self.captures.get_mut(&capture_id)
            && let Some(dest) = capture.destinations.get_mut(&destination_id)
        {
            dest.pending = Some(PendingRole::TransferSource(target_room));
        }
        self.add_destination(capture_id, target_room, PendingRole::TransferTarget(source_room));

        // If the add never even got started (no live capture, unknown room),
        // don't leave the old destination stuck looking like it's mid-transfer.
        if !self.pending_destinations.iter().any(|p| p.capture_id == capture_id && p.room == target_room)
            && let Some(capture) = self.captures.get_mut(&capture_id)
            && let Some(dest) = capture.destinations.get_mut(&destination_id)
        {
            dest.pending = None;
        }
    }

    /// Poll every in-flight fan-out/transfer destination. Call once per frame.
    pub fn poll_pending_destinations(&mut self) {
        let mut still_pending = Vec::new();
        for mut pending in std::mem::take(&mut self.pending_destinations) {
            match pending.rx.try_recv() {
                Ok(Ok(handle)) => {
                    // The destination was cancelled (or its whole capture
                    // stopped) while its branch was still coming up. That
                    // window is unbounded on an idle monitor, which only
                    // delivers a first frame once something on it changes, so
                    // this is a real case rather than a race: tear the
                    // just-finished stream down instead of adopting a ghost
                    // nobody can see or stop.
                    let still_wanted = self
                        .captures
                        .get(&pending.capture_id)
                        .is_some_and(|c| c.destinations.contains_key(&pending.destination_id));
                    if !still_wanted {
                        tokio::spawn(async move {
                            if let Err(e) = handle.stop().await {
                                tracing::warn!("stopping a cancelled destination did not finish cleanly: {e:#}");
                            }
                        });
                        continue;
                    }
                    let runtime = handle.runtime();
                    let client = self.rooms.get(&pending.room).map(|room| room.client.clone());
                    if let Some(capture) = self.captures.get_mut(&pending.capture_id) {
                        if let ShareStatus::Active(ShareHandle::Video(capture_handle)) = &mut capture.status {
                            capture_handle.register_destination(client.as_ref(), handle);
                        }
                        if let Some(dest) = capture.destinations.get_mut(&pending.destination_id) {
                            dest.runtime = Some(runtime);
                            dest.pending = None;
                        }
                    }
                    // This destination is now genuinely live (its `Announce
                    // Stream` already went out inside `spawn_add_destination`
                    // — see `capture::spawn_destination`'s doc comment on
                    // "announce after the first IDR"): synthesize the
                    // self-owned `StreamInfo` the relay's own `StreamStarted`
                    // broadcast would have carried, since the relay never
                    // echoes it back to the connection that announced it. See
                    // `RoomSession::note_stream_started`'s doc comment.
                    if let Some(dest) =
                        self.captures.get(&pending.capture_id).and_then(|c| c.destinations.get(&pending.destination_id))
                    {
                        let stream_id = dest.stream_id;
                        let (width, height) = (dest.params.width as u16, dest.params.height as u16);
                        if let Some(room) = self.rooms.get_mut(&pending.room) {
                            let owner = room.client.conn_id();
                            room.note_stream_started(StreamInfo {
                                owner,
                                stream_id,
                                kind: StreamKind::Video,
                                label: "Screen".to_string(),
                                width,
                                height,
                            });
                        }
                    }
                    // The new destination is live: this is the "break" half of
                    // make-before-break, and the only place the old
                    // destination is ever torn down.
                    if let PendingRole::TransferTarget(source_room) = pending.role {
                        let old = self
                            .captures
                            .get(&pending.capture_id)
                            .and_then(|capture| {
                                capture
                                    .destinations
                                    .iter()
                                    .find(|(id, d)| {
                                        d.room == source_room
                                            && **id != pending.destination_id
                                            && matches!(d.pending, Some(PendingRole::TransferSource(_)))
                                    })
                                    .map(|(id, _)| *id)
                            });
                        if let Some(old) = old {
                            self.stop_destination(old);
                        }
                    }
                }
                Ok(Err(e)) => {
                    // Not an error the user should hear about when they are
                    // the reason for it: the capture (or this destination) was
                    // stopped while it was still waiting for its first frame.
                    let still_wanted = self
                        .captures
                        .get(&pending.capture_id)
                        .is_some_and(|c| c.destinations.contains_key(&pending.destination_id));
                    if still_wanted {
                        note_share_error(&mut self.share_error, e);
                    }
                    self.forget_destination(pending.capture_id, pending.destination_id);
                    self.clear_transfer_marker(pending.capture_id, pending.role);
                }
                Err(oneshot::error::TryRecvError::Empty) => {
                    still_pending.push(pending);
                }
                Err(oneshot::error::TryRecvError::Closed) => {
                    note_share_error(&mut self.share_error, t!("errors.share_start_task_ended").to_string());
                    self.forget_destination(pending.capture_id, pending.destination_id);
                    self.clear_transfer_marker(pending.capture_id, pending.role);
                }
            }
        }
        self.pending_destinations = still_pending;
    }

    /// A transfer's target failed to come up: un-mark the source destination,
    /// which is still live and still authoritative, so it stops claiming to be
    /// mid-transfer.
    pub fn clear_transfer_marker(&mut self, capture_id: CaptureSourceId, role: PendingRole) {
        let PendingRole::TransferTarget(source_room) = role else { return };
        let Some(capture) = self.captures.get_mut(&capture_id) else { return };
        for dest in capture.destinations.values_mut() {
            if dest.room == source_room && matches!(dest.pending, Some(PendingRole::TransferSource(_))) {
                dest.pending = None;
            }
        }
    }

    /// Drop one destination from the local model (no pipeline work): used when
    /// bringing it up failed, so there is nothing to tear down.
    pub fn forget_destination(&mut self, capture_id: CaptureSourceId, destination_id: DestinationId) {
        let removed = self.captures.get_mut(&capture_id).and_then(|capture| capture.destinations.shift_remove(&destination_id));
        // Mirror `StreamStopped` for our own stream, same reasoning as
        // `poll_pending_destinations`'s `note_stream_started` call — see
        // `RoomSession::note_stream_started`'s doc comment. A destination
        // that never got as far as its `AnnounceStream` (the failed-to-come-
        // up path this function also serves) never had a `streams` entry to
        // begin with; `note_stream_stopped`'s `retain` is a safe no-op then.
        if let Some(dest) = &removed
            && let Some(room) = self.rooms.get_mut(&dest.room)
        {
            let owner = room.client.conn_id();
            room.note_stream_stopped(owner, dest.stream_id);
        }
        for room in self.rooms.values_mut() {
            room.destinations.retain(|d| *d != destination_id);
        }
        self.bandwidth.forget(destination_id);
    }

    /// Stop sending into *one* room, leaving the capture (and every other room
    /// it feeds) running. If it was the capture's last destination, the whole
    /// capture is stopped instead — a capture with nowhere to send is just a
    /// screen grab burning CPU.
    pub fn stop_destination(&mut self, destination_id: DestinationId) {
        let Some((capture_id, capture, _)) = find_destination(&self.captures, destination_id) else { return };
        if capture.destinations.len() <= 1 {
            self.stop_share(capture_id);
            return;
        }
        if let Some(capture) = self.captures.get_mut(&capture_id)
            && let ShareStatus::Active(ShareHandle::Video(handle)) = &mut capture.status
        {
            // Fire-and-forget: the destination is already gone from the
            // handle's registry, and the teardown (branch drain + `StopStream`)
            // has no result the UI can act on beyond a log line.
            let rx = handle.spawn_remove_destination(DestKey(destination_id.0));
            tokio::spawn(async move {
                if let Ok(Err(e)) = rx.await {
                    tracing::warn!("removing a destination did not finish cleanly: {e}");
                }
            });
        }
        self.forget_destination(capture_id, destination_id);
    }

    /// `true` while `capture_id` is a running video capture that has never
    /// produced a frame — the state an **idle monitor** is in until something
    /// on it changes (see `capture::branch::wait_for_source_frame`). A
    /// destination attached to it sits in `Starting` rather than failing, and
    /// this is what a UI can use to explain why ("waiting for the shared
    /// screen to change").
    pub fn capture_awaiting_first_frame(&self, capture_id: CaptureSourceId) -> bool {
        match self.captures.get(&capture_id).map(|c| &c.status) {
            Some(ShareStatus::Active(ShareHandle::Video(handle))) => handle.awaiting_first_frame(),
            _ => false,
        }
    }

    /// "Retry" clicked on a Failed destination card: tear the dead branch
    /// down and bring a fresh one right back up in the same room, off the
    /// same capture — preserving whatever encoder settings
    /// (bitrate/tune/quality_level) this destination had, not
    /// the capture's/session's defaults.
    ///
    /// `Why this doesn't just do `stop_destination` + `add_destination`
    /// (which is what the removed web front-end did, calling the
    /// stop/start commands back to back):` two
    /// problems with that pair. First, params: `add_destination` derives a
    /// *new* destination's params from a sibling destination still on the
    /// capture, or the session defaults if there is none — for the common
    /// case of a capture with exactly one (now-failed) destination, stopping
    /// it first leaves no sibling to copy from, so the retried destination
    /// silently resets to defaults instead of resuming with whatever the
    /// user had tuned (possibly via `edit_destination`). Second,
    /// `stop_destination` special-cases a capture's *last* destination by
    /// stopping the whole capture (see its own doc comment) — correct when
    /// the user is deliberately stopping their only stream, wrong here: a
    /// retry means one destination's branch died, not the screen grab
    /// itself, and tearing the whole capture down (racing whether it is even
    /// `Active` again by the time the replacement is requested) breaks retry
    /// for exactly the single-destination case it exists to fix. So this
    /// reads the failed destination's own `params` before removing it, then
    /// removes just that one branch (mirroring `stop_destination`'s non-last
    /// path) and re-adds with those same params via
    /// `add_destination_with_params`, leaving the capture itself untouched
    /// throughout.
    pub fn retry_destination(&mut self, destination_id: DestinationId) {
        let Some((capture_id, _, dest)) = find_destination(&self.captures, destination_id) else { return };
        let room_handle = dest.room;
        let params = dest.params;
        let role = dest.pending;

        if let Some(capture) = self.captures.get_mut(&capture_id)
            && let ShareStatus::Active(ShareHandle::Video(handle)) = &mut capture.status
        {
            let rx = handle.spawn_remove_destination(DestKey(destination_id.0));
            tokio::spawn(async move {
                if let Ok(Err(e)) = rx.await {
                    tracing::warn!("removing a failed destination during retry did not finish cleanly: {e}");
                }
            });
        }
        self.forget_destination(capture_id, destination_id);

        self.add_destination_with_params(capture_id, room_handle, role.unwrap_or(PendingRole::FanOut), params);
    }

    /// "Apply" clicked on a destination row: hand the edited settings to
    /// `capture::CaptureHandle::change_destination`, which classifies them
    /// (`capture::destination::classify_change`) into a live bitrate write, a
    /// branch swap, or a shared-source restart plus swap — this side never
    /// decides which.
    pub fn apply_destination_edit(&mut self, destination_id: DestinationId) {
        let Some((capture_id, _, _)) = find_destination(&self.captures, destination_id) else { return };
        let Some(capture) = self.captures.get_mut(&capture_id) else { return };
        let ShareStatus::Active(ShareHandle::Video(handle)) = &capture.status else { return };
        let source = handle.source_caps();
        let Some(dest) = capture.destinations.get(&destination_id) else { return };
        let requested = dest.edited_params(source);

        match handle.change_destination(DestKey(destination_id.0), requested) {
            Ok(change) => {
                let applied = change.applied;
                if let Some(dest) = capture.destinations.get_mut(&destination_id) {
                    dest.params = applied;
                    dest.apply_rx = change.done;
                }
                // A source restart changes what *every* destination of this
                // capture is fed from, so the capture's own framerate notion
                // moves with it.
                if change.tier == capture::destination::ChangeTier::SourceRestart {
                    self.framerate = applied.framerate;
                }
            }
            Err(e) => note_share_error(&mut self.share_error, format!("{e:#}")),
        }
    }

    /// Poll every in-flight "Apply" (branch swap). Call once per frame.
    pub fn poll_destination_edits(&mut self) {
        let mut error = None;
        let mut settled: Vec<CaptureSourceId> = Vec::new();
        for (capture_id, capture) in self.captures.iter_mut() {
            for dest in capture.destinations.values_mut() {
                let Some(rx) = &mut dest.apply_rx else { continue };
                match rx.try_recv() {
                    Ok(Ok(())) => {
                        dest.apply_rx = None;
                        settled.push(*capture_id);
                    }
                    Ok(Err(e)) => {
                        error = Some(e);
                        dest.apply_rx = None;
                        settled.push(*capture_id);
                    }
                    Err(oneshot::error::TryRecvError::Empty) => {}
                    Err(oneshot::error::TryRecvError::Closed) => {
                        dest.apply_rx = None;
                        settled.push(*capture_id);
                    }
                }
            }
        }
        for capture_id in settled {
            self.retune_source_framerate(capture_id);
        }
        if let Some(e) = error {
            note_share_error(&mut self.share_error, e);
        }
    }

    /// Bring a capture's shared source rate back in line with what its
    /// destinations actually need: `max(fps)` across them
    /// (`capture::destination::required_source_framerate`).
    ///
    /// `Why` this runs *after* a change settles rather than as part of it:
    /// raising the source is a precondition for a branch that wants a higher
    /// rate, so `change_destination` does that itself before swapping;
    /// *lowering* it is only safe once every branch has actually been re-capped
    /// below the new rate, or the still-old branch would briefly be asking for
    /// frames the source no longer produces. Running it here also means a
    /// destination being removed (fan-out shrinking, a transfer completing)
    /// naturally lets the capture drop back down instead of running hot at a
    /// rate nothing needs any more.
    pub fn retune_source_framerate(&mut self, capture_id: CaptureSourceId) {
        let Some(capture) = self.captures.get(&capture_id) else { return };
        let ShareStatus::Active(ShareHandle::Video(handle)) = &capture.status else { return };
        let required = capture::destination::required_source_framerate(
            capture.destinations.values().filter(|d| d.runtime.is_some()).map(|d| d.live_params().framerate),
        );
        handle.set_source_framerate(required);
    }

    // -----------------------------------------------------------------
    // Bandwidth
    // -----------------------------------------------------------------

    /// Every video destination that is currently live enough to be worth
    /// budgeting for, as `bandwidth::DestSpec`s — **across every room and
    /// every capture**, which is what makes Absolute-Maximum mode a per-client
    /// aggregate rather than a per-room one (plan doc, round-2 Q6).
    ///
    /// Destinations whose branch doesn't exist yet (still starting, or a
    /// fan-out/transfer target still proving itself) are left out: they have
    /// no encoder to write to, and including them would shrink everyone else's
    /// share for a stream that may never come up.
    pub fn allocation_input(&self) -> AllocInput {
        let mut destinations = Vec::new();
        for capture in self.captures.values() {
            if capture.kind != StreamKind::Video || !matches!(capture.status, ShareStatus::Active(_)) {
                continue;
            }
            for (destination_id, dest) in &capture.destinations {
                if dest.runtime.is_none() {
                    continue;
                }
                let params = dest.live_params();
                destinations.push(DestSpec {
                    id: DestId(destination_id.0),
                    // A destination that hasn't negotiated a size yet would
                    // weigh 0 and get nothing but its floor; fall back to the
                    // capture's source size so its weight is still
                    // proportionate to what it will encode.
                    width: params.width.max(1),
                    height: params.height.max(1),
                    fps: params.framerate.max(1),
                    floor_kbps: self.destination_floor_kbps,
                    // The project's own per-stream ceiling: no single
                    // destination is ever worth more than the 40 Mbit/s
                    // maximum the per-stream slider tops out at, no matter how
                    // much total budget is free.
                    ceiling_kbps: Some(MAX_BITRATE_KBPS),
                });
            }
        }
        AllocInput {
            total_kbps: self.absolute_total_kbps,
            // Audio is still its own capture with its own (Opus, constant,
            // small) rate rather than an attachment on a video destination —
            // the always-on-mixer rework is a later stage — so there is no
            // idle-audio baseline to reserve off the top yet. Left explicit at
            // 0 rather than omitted so the day that lands has an obvious
            // place to go.
            audio_reserved_kbps: 0,
            destinations,
        }
    }

    /// Absolute-Maximum mode's live wiring: recompute the allocation (at most
    /// once per `ALLOC_RECOMPUTE_INTERVAL`) and write each destination's new
    /// share into its encoder, skipping anything that doesn't clear the
    /// hysteresis threshold. Call once per frame; a cheap no-op in Per-Stream
    /// mode.
    pub fn apply_bandwidth_allocation(&mut self) {
        if self.bandwidth_mode != BandwidthMode::Absolute {
            return;
        }
        let input = self.allocation_input();
        if input.destinations.is_empty() {
            return;
        }
        let Some(tick) = self.bandwidth.tick(Instant::now(), &input) else { return };
        for (destination_id, kbps) in tick.writes {
            let Some((capture_id, _, _)) = find_destination(&self.captures, destination_id) else { continue };
            let Some(capture) = self.captures.get(&capture_id) else { continue };
            let ShareStatus::Active(ShareHandle::Video(handle)) = &capture.status else { continue };
            handle.set_destination_bitrate(DestKey(destination_id.0), kbps);
            self.bandwidth.note_applied(destination_id, kbps);
        }
    }

    /// Switch bandwidth mode, persisting the choice.
    ///
    /// Leaving Absolute mode re-applies every destination's own manual
    /// (Per-Stream) rate, which the allocator never touched — see
    /// `DestEdit::bitrate_kbps`'s `Why:` note. Neither mode's stored numbers
    /// are modified by the switch itself.
    pub fn set_bandwidth_mode(&mut self, mode: BandwidthMode) {
        if self.bandwidth_mode == mode {
            return;
        }
        self.bandwidth_mode = mode;
        save_bandwidth_settings(mode, self.absolute_total_kbps, self.destination_floor_kbps);
        if mode == BandwidthMode::PerStream {
            self.bandwidth.reset();
            self.reapply_manual_bitrates();
        }
    }

    /// Push every destination's manual Per-Stream rate back into its live
    /// encoder — what "Per-Stream Maximum is in charge again" means in
    /// practice.
    pub fn reapply_manual_bitrates(&mut self) {
        for capture in self.captures.values() {
            let ShareStatus::Active(ShareHandle::Video(handle)) = &capture.status else { continue };
            for (destination_id, dest) in &capture.destinations {
                if dest.runtime.is_none() {
                    continue;
                }
                handle.set_destination_bitrate(DestKey(destination_id.0), dest.edit.bitrate_kbps);
            }
        }
    }

    /// "Add audio" clicked: same shape as `start_video_share`, for the
    /// currently selected source. No-op (returns `Ok(())`) if no source is
    /// selected (the button is disabled in that case).
    ///
    /// `for_capture` is the video capture this audio is being attached to, so
    /// its row can be rendered inside that capture's card — see
    /// [`LocalCapture::audio_for_capture`]. There is no standalone-audio mode
    /// any more: **every** audio share must be paired to a video capture —
    /// see this crate's hard invariant ("no audio-only streams, ever"), so
    /// `for_capture` is mandatory here, not an `Option`.
    ///
    /// `Why this checks liveness, not just existence:` `for_capture` can
    /// legitimately name a capture that exists but is only **staged**
    /// (`stage_video_share` ran, `go_live` hasn't yet — zero destinations,
    /// nothing announced to any room). The frontend's staged card exposes
    /// "+Add audio" before "Go Live" is clicked, and until this check
    /// existed, clicking it there would announce a live audio stream paired
    /// to a video capture that was not actually sending anything —
    /// exactly the orphan, video-less "LIVE" row the hard invariant above
    /// forbids. A capture counts as live iff it has at least one destination
    /// (`add_destination_with_params`/`go_live` are the only things that ever
    /// push one), so `!destinations.is_empty()` is the same staged-vs-live
    /// test the rest of this module already relies on — no new flag needed.
    pub fn start_audio_share(&mut self, room_handle: RoomHandle, for_capture: CaptureSourceId) -> Result<(), String> {
        let is_live = self.captures.get(&for_capture).is_some_and(|c| !c.destinations.is_empty());
        if !is_live {
            return Err(
                "cannot start audio share: the paired screen share is not live yet — click \"Go Live\" first"
                    .to_string(),
            );
        }
        let Some(source) = self.selected_audio_source.clone() else {
            return Ok(());
        };
        self.start_audio_share_with(room_handle, for_capture, source)
    }

    /// [`Self::start_audio_share`] with the source given explicitly: what a
    /// reconnect uses to bring a share back with the source it was started
    /// with, not whatever the dropdown has selected by now.
    fn start_audio_share_with(
        &mut self,
        room_handle: RoomHandle,
        for_capture: CaptureSourceId,
        source: AudioSource,
    ) -> Result<(), String> {
        let capture_id = self.alloc_capture_id();
        let destination_id = self.alloc_destination_id();

        let Some(room) = self.rooms.get_mut(&room_handle) else { return Ok(()) };
        let stream_id = room.next_stream_id;
        room.next_stream_id = room.next_stream_id.wrapping_add(1);
        room.destinations.push(destination_id);
        let client = room.client.clone();

        self.audio_share_sources.insert(capture_id, source.clone());
        let (tx, rx) = oneshot::channel();
        self.captures.insert(
            capture_id,
            LocalCapture {
                kind: StreamKind::Audio,
                label: source.name.clone(),
                status: ShareStatus::Starting(rx),
                audio_for_capture: Some(for_capture),
                destinations: IndexMap::from([(
                    destination_id,
                    Destination {
                        room: room_handle,
                        stream_id,
                        // Audio destinations carry the video params struct
                        // inertly — there is no per-destination audio geometry
                        // to describe, and `runtime` stays `None` because the
                        // video `DestinationState`/stall machinery has no audio
                        // counterpart.
                        params: AUDIO_DESTINATION_PARAMS,
                        runtime: None,
                        pending: None,
                        edit: Destination::edit_from(AUDIO_DESTINATION_PARAMS),
                        apply_rx: None,
                    },
                )]),
            },
        );
        self.share_error = None;

        tokio::spawn(async move {
            let result = audio::start_audio_sharing(&client, stream_id, &source)
                .await
                .map(ShareHandle::Audio)
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
        });
        Ok(())
    }

    /// Mute/unmute an audio share without stopping it — the Mute toggle on
    /// the screen share's card. No-op for a video capture or one that isn't
    /// running yet. Returns whether the capture is muted afterwards, so the
    /// caller doesn't need a second round trip to render the new label.
    pub fn set_audio_share_muted(&mut self, capture_id: CaptureSourceId, muted: bool) -> bool {
        let Some(capture) = self.captures.get(&capture_id) else { return false };
        let ShareStatus::Active(ShareHandle::Audio(handle)) = &capture.status else { return false };
        handle.set_muted(muted);
        handle.is_muted()
    }

    /// Whether `capture_id` is a running, currently-muted audio share.
    pub fn is_audio_share_muted(&self, capture_id: CaptureSourceId) -> bool {
        match self.captures.get(&capture_id).map(|c| &c.status) {
            Some(ShareStatus::Active(ShareHandle::Audio(handle))) => handle.is_muted(),
            _ => false,
        }
    }

    /// The live capture level (0.0-1.0) of `capture_id` if it is a running
    /// audio share — what the card's VU meter draws.
    pub fn audio_share_level(&self, capture_id: CaptureSourceId) -> Option<f32> {
        match self.captures.get(&capture_id).map(|c| &c.status) {
            Some(ShareStatus::Active(ShareHandle::Audio(handle))) => Some(handle.level()),
            _ => None,
        }
    }

    /// Registers an already-constructed level tap (built by the caller via
    /// [`audio::start_level_tap`] *before* taking this lock — see
    /// [`build_audio_level_tap`]'s doc comment for why building the
    /// GStreamer pipeline can't happen while holding `AppState`'s mutex) and
    /// returns the id the frontend polls/stops it with.
    pub fn insert_audio_level_tap(&mut self, handle: audio::AudioLevelTapHandle) -> u32 {
        let id = self.next_audio_level_tap_id;
        self.next_audio_level_tap_id = self.next_audio_level_tap_id.wrapping_add(1);
        self.audio_level_taps.insert(AudioLevelTapId(id), handle);
        id
    }

    /// Newest reading (0.0-1.0) of a live level tap, or `None` if `tap_id`
    /// doesn't (or no longer) name one — e.g. it was already stopped, which
    /// the frontend's poll loop treats the same as "nothing to paint" rather
    /// than an error.
    pub fn audio_level_tap_level(&self, tap_id: u32) -> Option<f32> {
        self.audio_level_taps.get(&AudioLevelTapId(tap_id)).map(|h| h.level())
    }

    /// Tears one level tap down — called for every still-open tap the
    /// instant a dropdown closes (see [`AudioLevelTapId`]'s doc comment for
    /// why none of these are meant to outlive that). Spawned rather than
    /// awaited for the same reason `stop_share_one` below is: the caller
    /// (synchronous UI code) can't await, and there's no result the UI needs
    /// back — the tap is gone from `audio_level_taps`
    /// immediately, so a re-poll after this returns correctly sees nothing.
    pub fn stop_audio_level_tap(&mut self, tap_id: u32) {
        if let Some(handle) = self.audio_level_taps.remove(&AudioLevelTapId(tap_id)) {
            tokio::spawn(async move { handle.stop().await });
        }
    }

    /// "Stop"/"Stop Sharing Audio" clicked for `capture_id`: hand the
    /// underlying handle off to a spawned task that tears it down
    /// (`.stop()` is async on both `CaptureHandle` and `AudioCaptureHandle`).
    /// The `LocalCapture` stays in the map, in `Stopping`, so the UI can show
    /// that state; `poll_capture_states` reaps it once the stop lands.
    ///
    /// `Why this cascades to paired audio:` a video capture can have an audio
    /// capture riding along (`LocalCapture::audio_for_capture` pointing back
    /// at it) — either live, or merely staged waiting for the video to go
    /// live. Stopping/cancelling the video without also stopping that audio
    /// would leave it running forever with no video left to belong to,
    /// exactly the orphan-audio state the hard "no audio-only streams" rule
    /// forbids. This covers both cancelling a staged video card (which
    /// reuses this same function, see `stage_video_share`'s doc comment) and
    /// stopping a live one.
    pub fn stop_share(&mut self, capture_id: CaptureSourceId) {
        let paired_audio: Vec<CaptureSourceId> = self
            .captures
            .iter()
            .filter(|(id, c)| **id != capture_id && c.audio_for_capture == Some(capture_id))
            .map(|(id, _)| *id)
            .collect();
        for audio_capture_id in paired_audio {
            self.stop_share_one(audio_capture_id);
        }
        self.stop_share_one(capture_id);
    }

    /// The single-capture teardown [`Self::stop_share`] applies to
    /// `capture_id` itself and, first, to any paired audio it cascades to.
    fn stop_share_one(&mut self, capture_id: CaptureSourceId) {
        let Some(capture) = self.captures.get_mut(&capture_id) else {
            return;
        };
        if matches!(capture.status, ShareStatus::Starting(_)) {
            // Cancel a capture whose start is still in flight (e.g. the OS
            // screen picker is open). Tear-down-on-arrival, not cancel-early:
            // the start future lives in a spawned task that `ShareStatus` has
            // no abort handle for, and the portal dialog can't be closed from
            // here without changing `capture.rs`. So we swap to `Stopping`
            // and let a task wait for the start result: if it comes up, stop
            // the handle at once (closing the portal session, never going
            // live and never attaching a destination); if it fails or the
            // task died (user dismissed the picker), that's the desired
            // outcome, not an error. `poll_capture_states` then reaps the
            // entry and its destinations like any other stop.
            let (tx, rx) = oneshot::channel();
            let ShareStatus::Starting(start_rx) = std::mem::replace(&mut capture.status, ShareStatus::Stopping(rx))
            else {
                unreachable!("just matched Starting above");
            };
            tokio::spawn(async move {
                let result = match start_rx.await {
                    Ok(Ok(ShareHandle::Video(h))) => h.stop().await.map_err(|e| format!("{e:#}")),
                    Ok(Ok(ShareHandle::Audio(h))) => h.stop().await.map_err(|e| format!("{e:#}")),
                    Ok(Err(_)) | Err(_) => Ok(()),
                };
                let _ = tx.send(result);
            });
            return;
        }
        if !matches!(capture.status, ShareStatus::Active(_)) {
            return;
        }
        let (tx, rx) = oneshot::channel();
        let ShareStatus::Active(handle) = std::mem::replace(&mut capture.status, ShareStatus::Stopping(rx)) else {
            unreachable!("just matched Active above");
        };

        tokio::spawn(async move {
            let result = match handle {
                ShareHandle::Video(h) => h.stop().await,
                ShareHandle::Audio(h) => h.stop().await,
            }
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
        });
    }

    /// Abrupt teardown (relay/control connection lost, or the app quitting
    /// while sharing): drop `capture_id` and fire off `.stop()` for it if it
    /// was running, so the portal session gets closed and the relay gets a
    /// stop-stream notification, instead of just letting the handle drop
    /// synchronously when `AppState` goes away right after this call.
    pub fn drop_capture_fire_and_forget(&mut self, capture_id: CaptureSourceId) {
        let Some(capture) = self.captures.shift_remove(&capture_id) else { return };
        self.audio_share_sources.remove(&capture_id);
        self.forget_destinations_of(&capture);
        let ShareStatus::Active(handle) = capture.status else { return };
        tokio::spawn(async move {
            if let Err(e) = match handle {
                ShareHandle::Video(h) => h.stop().await,
                ShareHandle::Audio(h) => h.stop().await,
            } {
                tracing::warn!("stopping a capture during teardown failed: {e:#}");
            }
        });
    }

    /// Same, for every running capture at once (app exit).
    pub fn stop_all_captures_fire_and_forget(&mut self) {
        for capture_id in self.captures.keys().copied().collect::<Vec<_>>() {
            self.drop_capture_fire_and_forget(capture_id);
        }
    }

    /// Drop `capture`'s destinations out of the back-reference lists of every
    /// room they pointed into. Call whenever a `LocalCapture` leaves
    /// `self.captures`, so no room is left advertising an outgoing stream
    /// that no longer exists.
    pub fn forget_destinations_of(&mut self, capture: &LocalCapture) {
        for (destination_id, dest) in capture.destinations.iter() {
            if let Some(room) = self.rooms.get_mut(&dest.room) {
                room.destinations.retain(|d| d != destination_id);
                // Mirror `StreamStopped` for our own stream — same
                // "the relay never echoes this back to us" reasoning as
                // `forget_destination`/`poll_capture_states`'s `note_stream_
                // started`. A destination that never made it past `Starting`
                // (e.g. a failed capture start) never had a `streams` entry;
                // `note_stream_stopped`'s `retain` is a safe no-op then.
                let owner = room.client.conn_id();
                room.note_stream_stopped(owner, dest.stream_id);
            }
            // Drop the allocator's memory of what this destination's encoder
            // was last set to, so a stopped stream stops counting as
            // "already at the right bitrate". Ids never rewind, so a stale
            // entry could never alias a *new* destination — this is
            // housekeeping, not a correctness fix.
            self.bandwidth.forget(*destination_id);
        }
    }

    /// Poll every in-flight capture start/stop, if any. Call once per frame.
    pub fn poll_capture_states(&mut self) {
        let mut to_remove = Vec::new();
        // Self-owned streams to record once the loop below is done mutably
        // borrowing `self.captures` — see the push site's comment for why
        // this can't just call `self.rooms.get_mut(..)` inline. `owner` is
        // filled in from each room's own `ConnId` afterwards, not stored
        // here, since this loop never borrows `self.rooms`.
        let mut started: Vec<(RoomHandle, StreamId, StreamKind, String, u16, u16)> = Vec::new();
        for (capture_id, capture) in self.captures.iter_mut() {
            match &mut capture.status {
                ShareStatus::Starting(rx) => match rx.try_recv() {
                    Ok(Ok(handle)) => {
                        // Attach each destination's own lifecycle runtime, so
                        // `Destination::state` starts reporting the real state
                        // machine (`Starting` until its branch's first IDR,
                        // then `Live`) instead of its not-yet-started
                        // placeholder. Per destination, not one shared runtime:
                        // each has its own encode branch and therefore its own
                        // state, PTS clock and cached IDR.
                        if let ShareHandle::Video(video) = &handle {
                            for (destination_id, dest) in capture.destinations.iter_mut() {
                                dest.runtime = video.destination_runtime(DestKey(destination_id.0));
                                if let Some(runtime) = &dest.runtime {
                                    // The capture filled in the size it really
                                    // negotiated; adopt it so the row's edit
                                    // buffer and geometry aren't stuck on the
                                    // "0 = whatever the source is" placeholder.
                                    dest.params = runtime.lock().unwrap().params();
                                }
                            }
                        }
                        // By the time this task's `tx` resolved `Ok`, its
                        // `AnnounceStream` already went out (both
                        // `capture::start_sharing`/`start_sharing_with_source`
                        // and `audio::start_audio_sharing` only return success
                        // after announcing — see their own doc comments). This
                        // is `start_video_share`'s/`start_audio_share`'s
                        // "destination actually went live" moment, the
                        // `ShareStatus::Starting` counterpart to `poll_
                        // pending_destinations`'s `Ok(Ok(handle))` arm for
                        // `go_live`/fan-out/transfer — mirror `StreamStarted`
                        // for our own stream here too. See `RoomSession::
                        // note_stream_started`'s doc comment for why nothing
                        // else ever will.
                        for dest in capture.destinations.values() {
                            let label = match capture.kind {
                                StreamKind::Video => "Screen".to_string(),
                                StreamKind::Audio => capture.label.clone(),
                            };
                            let (width, height) = if capture.kind == StreamKind::Video {
                                (dest.params.width as u16, dest.params.height as u16)
                            } else {
                                (0, 0)
                            };
                            started.push((dest.room, dest.stream_id, capture.kind, label, width, height));
                        }
                        capture.status = ShareStatus::Active(handle);
                    }
                    Ok(Err(e)) => {
                        // A dismissed picker is the user's own choice, not a
                        // failure: drop the capture, say nothing.
                        if e != SHARE_START_CANCELLED {
                            note_share_error(&mut self.share_error, e);
                        }
                        to_remove.push(*capture_id);
                    }
                    Err(oneshot::error::TryRecvError::Empty) => {}
                    Err(oneshot::error::TryRecvError::Closed) => {
                        note_share_error(&mut self.share_error, t!("errors.share_start_task_ended").to_string());
                        to_remove.push(*capture_id);
                    }
                },
                ShareStatus::Stopping(rx) => match rx.try_recv() {
                    Ok(Ok(())) => to_remove.push(*capture_id),
                    Ok(Err(e)) => {
                        note_share_error(&mut self.share_error, e);
                        to_remove.push(*capture_id);
                    }
                    Err(oneshot::error::TryRecvError::Empty) => {}
                    Err(oneshot::error::TryRecvError::Closed) => {
                        note_share_error(&mut self.share_error, t!("errors.share_stop_task_ended").to_string());
                        to_remove.push(*capture_id);
                    }
                },
                ShareStatus::Active(_) => {}
            }
        }
        for (room_handle, stream_id, kind, label, width, height) in started {
            if let Some(room) = self.rooms.get_mut(&room_handle) {
                let owner = room.client.conn_id();
                room.note_stream_started(StreamInfo { owner, stream_id, kind, label, width, height });
            }
        }
        for capture_id in to_remove {
            self.audio_share_sources.remove(&capture_id);
            if let Some(capture) = self.captures.shift_remove(&capture_id) {
                self.forget_destinations_of(&capture);
            }
        }
    }

    /// Kick off (or re-kick-off) an `audio::list_audio_sources()` call on
    /// the ambient tokio runtime. That function shells out to `pw-dump`
    /// synchronously, so it's run via `spawn_blocking` rather than directly
    /// in the async task, matching the non-blocking intent of the rest of
    /// this module's spawn pattern.
    pub fn refresh_audio_sources(&mut self) {
        let (tx, rx) = oneshot::channel();
        self.audio_sources_rx = Some(rx);
        self.audio_sources_error = None;

        tokio::spawn(async move {
            let result = match tokio::task::spawn_blocking(audio::list_audio_sources).await {
                Ok(Ok(sources)) => Ok(sources),
                Ok(Err(e)) => Err(format!("{e:#}")),
                Err(e) => Err(format!("audio source listing task panicked: {e}")),
            };
            let _ = tx.send(result);
        });
    }

    /// Poll the in-flight `list_audio_sources()` call, if any. Call once per
    /// frame.
    pub fn poll_audio_sources(&mut self) {
        let Some(rx) = &mut self.audio_sources_rx else { return };
        match rx.try_recv() {
            Ok(Ok(sources)) => {
                self.audio_sources = sources;
                self.audio_sources_rx = None;
            }
            Ok(Err(e)) => {
                self.audio_sources_error = Some(e);
                self.audio_sources_rx = None;
            }
            Err(oneshot::error::TryRecvError::Empty) => {}
            Err(oneshot::error::TryRecvError::Closed) => {
                self.audio_sources_error = Some(t!("errors.audio_sources_task_ended").to_string());
                self.audio_sources_rx = None;
            }
        }
    }

    /// Rename the `RoomStore` entry for `join_string` to `new_name`.
    ///
    /// `RoomStore` is keyed by the room's identity (not its name), so this
    /// touches exactly the one entry for that room. A room that isn't saved
    /// yet is saved under `new_name` (the Auto-Connect checkbox relies on
    /// this for an unsaved live room). A blank name is ignored.
    pub fn rename_saved_room(&mut self, join_string: &str, new_name: String) {
        if self.room_store.get(join_string).is_some() {
            self.room_store.rename(join_string, &new_name);
        } else {
            self.room_store.upsert(new_name, join_string.to_string());
        }
        self.save_room_store();
    }

    /// Set the `RoomStore` entry for `join_string`'s `auto_connect` flag —
    /// the room-detail panel's "Auto-Connect" checkbox (see
    /// `superdoc/features/stream-control.md`'s 2026-07-28 entry). Rooms
    /// flagged this way get rejoined automatically in `main.rs`'s
    /// `.setup()`.
    ///
    /// `Why` this is a new, explicit flag rather than reusing anything
    /// already here: `AppState::default()` starts `rooms` empty and never
    /// reconnects anything on its own — there was no existing "resume
    /// whatever was connected last session" behavior to hook into
    /// (`is_reconnecting`/`pending_connect` only track an in-flight *manual*
    /// connect within the current run, not anything that survives a
    /// restart). This flag is the only mechanism that makes a saved room
    /// rejoin itself at launch, and it's a deliberate per-room choice the
    /// user sets, not an inference from whatever happened to be open when
    /// the app last closed.
    pub fn set_room_auto_connect(&mut self, join_string: &str, value: bool) {
        self.room_store.set_auto_connect(join_string, value);
        if let Err(e) = self.room_store.save() {
            self.room_store_error = Some(e.to_string());
        }
    }

    /// Move `f`'s edits onto `destination_id`'s row, wherever it lives.
    /// No-op if the destination is already gone.
    pub fn with_destination(&mut self, destination_id: DestinationId, f: impl FnOnce(&mut Destination)) {
        for capture in self.captures.values_mut() {
            if let Some(dest) = capture.destinations.get_mut(&destination_id) {
                f(dest);
                return;
            }
        }
    }
}

/// Room-scoped connect/join operations: presence, rooms, and events.
impl AppState {
    /// Kick off a connect to `join_string`. `from_add_card` routes a failure
    /// back into the inline "+ Add Room" card instead of the ROOMS list's own
    /// error line — the caller (`ui::App::start_connect`) owns that UI-only
    /// field, so failure is reported back via `Err` rather than mutated here.
    pub fn start_connect(&mut self, join_string: String, from_add_card: bool) -> Result<(), String> {
        // Connecting to a room that is already being reconnected is "Retry
        // now" (and revives an entry the legacy `cancel_connect` just parked):
        // same room, same intent, so it stays an automatic reconnect with its
        // attempt count instead of becoming a manual connect that gives up on
        // the first failure.
        if let Some(handle) = self.reconnect_handle_for(&join_string) {
            if let Some(entry) = self.reconnects.get_mut(&handle) {
                entry.cancelled = false;
            }
            self.reconnect_now(&join_string);
            return Ok(());
        }
        self.spawn_connect(join_string, from_add_card, None)
    }

    /// The one place a connect task is spawned. `reconnect` marks an
    /// automatic reconnect attempt (shorter timeout, result routed back into
    /// its [`ReconnectState`]).
    fn spawn_connect(&mut self, join_string: String, from_add_card: bool, reconnect: Option<RoomHandle>) -> Result<(), String> {
        let endpoint = match &self.endpoint {
            Ok(endpoint) => endpoint.clone(),
            Err(e) => {
                self.status = Status::Error(e.clone());
                return Err(e.clone());
            }
        };
        let display_name = self.display_name.clone();
        let timeout = if reconnect.is_some() { self.reconnect_attempt_timeout.min(self.connect_timeout) } else { self.connect_timeout };
        let connect_join_string = join_string.clone();
        let (tx, rx) = oneshot::channel();

        let join_handle = tokio::spawn(async move {
            let result = match tokio::time::timeout(
                timeout,
                ScreenshareClient::connect(&endpoint, &connect_join_string, &display_name),
            )
            .await
            {
                Ok(result) => result.map_err(|e| ConnectError::from_anyhow(&e)),
                Err(_elapsed) => Err(ConnectError::other(t!("errors.connect_timed_out").to_string())),
            };
            let _ = tx.send(result);
        });
        self.pending_connect = Some(PendingConnect {
            rx,
            join_string: join_string.clone(),
            from_add_card,
            abort: join_handle.abort_handle(),
            reconnect,
        });
        self.status = Status::Connecting;
        Ok(())
    }

    /// Cancel the in-flight connect attempt, if any — the right-click menu's
    /// "Disconnect" while a room row is showing "Reconnecting…". Aborts the spawned task outright
    /// rather than waiting for `ScreenshareClient::connect` to notice; see
    /// `PendingConnect::abort`'s doc comment for why that's safe pre-success.
    ///
    /// No-op if nothing is pending — the same "at most one" invariant
    /// `start_connect`/`poll_auto_connect_queue` already rely on (only one
    /// `pending_connect` slot exists), so there's nothing to disambiguate.
    ///
    /// **Reconnects.** This takes no room, so it cannot say which reconnect the
    /// user means. With exactly one room reconnecting there is nothing to
    /// disambiguate: that room is parked as `cancelled`, dropped at the next
    /// `poll_reconnects` unless `start_connect` revives it first, which is
    /// exactly the current UI's "Retry now" (`CancelConnect` then
    /// `Connect(key)` in one frame) versus its "Cancel" (`CancelConnect`
    /// alone). With several rooms reconnecting it only aborts the attempt in
    /// flight (that room just retries at once): never guess which of the user's
    /// rooms to give up on. Prefer [`Self::cancel_reconnect`] and
    /// [`Self::reconnect_now`], which name the room.
    pub fn cancel_connect(&mut self) {
        let single = self.reconnects.len() == 1;
        let mut touched = None;
        if let Some(pending) = self.pending_connect.take() {
            pending.abort.abort();
            touched = pending.reconnect;
        } else if single {
            touched = self.reconnects.keys().next().copied();
        }
        if let Some(entry) = touched.and_then(|h| self.reconnects.get_mut(&h)) {
            entry.next_retry = Some(Instant::now());
            entry.cancelled = single;
        }
        self.refresh_status();
    }

    /// Whether `join_string` is a room we are connecting to or waiting to
    /// reconnect — what puts a saved row into the amber "reconnecting" state
    /// and keeps the room page on its Reconnecting banner (not the "Connect"
    /// button) through the waits between attempts.
    pub fn is_reconnecting(&self, join_string: &str) -> bool {
        self.pending_connect.as_ref().is_some_and(|p| RoomStore::same_room(&p.join_string, join_string))
            || self.reconnect_handle_for(join_string).is_some()
    }

    /// Drain one entry off `auto_connect_queue` into `pending_connect`, if
    /// the slot is free. Called every driver tick (`driver.rs`'s `tick`,
    /// right before `poll_connect_result`) so a startup batch of
    /// auto-connect rooms gets kicked off one at a time as each previous one
    /// finishes, instead of all at once — see `auto_connect_queue`'s doc
    /// comment for why one-at-a-time is required, not just simpler.
    ///
    /// Also drives the reconnect timers ([`Self::poll_reconnects`]), first, so
    /// a room that dropped wins the connect slot over a queued fresh room.
    pub fn poll_auto_connect_queue(&mut self) {
        self.poll_reconnects();
        if self.pending_connect.is_some() {
            return;
        }
        if let Some(join_string) = self.auto_connect_queue.first().cloned() {
            self.auto_connect_queue.remove(0);
            // Best-effort: an immediate failure (e.g. endpoint never bound)
            // surfaces the same way a failed manual join does, via
            // `self.status`; it doesn't block the rest of the queue.
            let _ = self.start_connect(join_string, false);
        }
    }

    /// Insert a freshly connected room and return its handle. The one place a
    /// `RoomSession` is born — used by `poll_connect_result` and, in tests, to
    /// stand a session up directly.
    ///
    /// This is also where the always-on media-ingestion task is spawned (see
    /// [`spawn_media_ingestion_task`]) — every `RoomSession` is created here,
    /// so this is the one choke point that guarantees no room's `event_rx`
    /// ever bypasses it.
    pub fn insert_room(
        &mut self,
        client: ScreenshareClient,
        snapshot: PresenceSnapshotState,
        event_rx: mpsc::UnboundedReceiver<ClientEvent>,
        join_string: String,
    ) -> RoomHandle {
        let handle = RoomHandle(self.next_room_handle);
        self.next_room_handle = self.next_room_handle.wrapping_add(1);
        self.insert_room_at(handle, client, snapshot, event_rx, join_string);
        handle
    }

    /// [`Self::insert_room`] under a given handle: how a reconnected room gets
    /// the handle it had before the drop (see [`ReconnectState::handle`]).
    fn insert_room_at(
        &mut self,
        handle: RoomHandle,
        client: ScreenshareClient,
        snapshot: PresenceSnapshotState,
        event_rx: mpsc::UnboundedReceiver<ClientEvent>,
        join_string: String,
    ) {
        let watches: Arc<Mutex<HashMap<(ConnId, StreamId), WatchEntry>>> = Arc::new(Mutex::new(HashMap::new()));
        let audio_watches: Arc<Mutex<HashMap<(ConnId, StreamId), AudioWatch>>> = Arc::new(Mutex::new(HashMap::new()));

        // `event_rx` is `ScreenshareClient::connect`'s raw, unbounded,
        // straight-off-the-network channel. It is handed to the ingestion
        // task, not stored on `RoomSession` directly — see
        // `spawn_media_ingestion_task`'s doc comment for why. `forward_rx`
        // (what `RoomSession::event_rx` actually becomes) only ever receives
        // what that task didn't intercept, at the same throttled cadence
        // `drain_events` always drained at.
        let (forward_tx, forward_rx) = mpsc::unbounded_channel();
        tokio::spawn(spawn_media_ingestion_task(
            event_rx,
            forward_tx,
            watches.clone(),
            audio_watches.clone(),
        ));

        self.rooms.insert(
            handle,
            RoomSession {
                client,
                event_rx: forward_rx,
                members: snapshot.members,
                streams: snapshot.streams,
                save_name: room_label(&self.room_store, &join_string),
                join_string,
                saved: false,
                destinations: Vec::new(),
                next_stream_id: 0,
                watches,
                watch_error: None,
                audio_watches,
                preview_cache: PreviewCache::new(),
                preview_textures: HashMap::new(),
                audio_volume_by_owner: HashMap::new(),
                rewatch: Vec::new(),
                rewatch_deadline: None,
                resume_after_stop: Vec::new(),
            },
        );
    }

    /// Tear one room down: stop sending into it, drop the session — which
    /// drops its decoders — and re-point `active_room` at whatever is left.
    ///
    /// `Why` this removes *destinations* and only drops a capture once it has
    /// none left, rather than dropping every capture that fed this room: with
    /// fan-out, one capture can be feeding three rooms, and leaving one of
    /// them must not tear down a pipeline the other two are still using. That
    /// is the exact failure `AppState`-owned (rather than room-owned) captures
    /// exist to prevent — see the module docs' "Session data model".
    ///
    /// Does **not** touch `ui::App`'s UI-only fields (`expanded_rooms`,
    /// `leave_confirm`) — its thin wrapper of the same name clears those
    /// afterwards.
    ///
    /// **Leaves cleanly:** sends `Leave` and closes the QUIC connection
    /// ([`ScreenshareClient::leave_and_close`]) so the relay and the other
    /// members drop us now rather than at the relay's idle timeout (60-75 s).
    /// For a room that is only waiting to reconnect (no session) it ends the
    /// wait instead ([`Self::cancel_reconnect`]). Either way the room is not
    /// reconnected afterwards: reconnecting is for unexpected drops only.
    pub fn disconnect_room(&mut self, handle: RoomHandle) {
        if self.reconnects.contains_key(&handle) {
            self.discard_reconnect(handle);
            return;
        }
        let Some(room) = self.rooms.shift_remove(&handle) else { return };
        spawn_leave(room.client.clone());
        for destination_id in room.destinations {
            let Some((capture_id, capture, _)) = find_destination(&self.captures, destination_id) else {
                continue;
            };
            if capture.destinations.len() <= 1 {
                // Last room this capture was feeding: nothing left to send
                // anywhere, so stop the screen grab too.
                self.drop_capture_fire_and_forget(capture_id);
            } else {
                self.stop_destination(destination_id);
            }
        }
        if self.active_room == Some(handle) {
            self.active_room = self.rooms.keys().next().copied();
        }
        // Back on the join screen with nothing joined: reset the two pieces
        // of `AppState`-level capture state that used to die with the
        // singleton `Connected`, so a rejoin looks exactly like a first join
        // (a fresh audio-source listing, no stale error banner) rather than
        // inheriting the previous session's leftovers. Deliberately *only*
        // when the last room goes: with several rooms joined, one leaving
        // must not disturb what the others are showing.
        if self.rooms.is_empty() {
            self.audio_sources_requested = false;
            self.share_error = None;
        }
    }

    /// Whether leaving `handle` should raise a confirm dialog first.
    ///
    /// `Why` both conditions and not just watching: leaving a room you're
    /// *streaming to* gets the same treatment as leaving one you're watching
    /// — "you're killing an outgoing broadcast, not just navigating". A plain
    /// leave with neither is immediate: there is nothing to lose and a
    /// confirm would just be a speed bump on the most common case.
    ///
    /// Watching counts whether the tile is inline or popped out into its own
    /// OS window — `watches` holds both (`WatchEntry::popout` only says
    /// *where* it renders), and a popped-out window vanishing unannounced is
    /// if anything the more startling of the two.
    pub fn leave_needs_confirm(&self, handle: RoomHandle) -> bool {
        let Some(room) = self.rooms.get(&handle) else { return false };
        !room.destinations.is_empty()
            || !room.watches.lock().unwrap().is_empty()
            || !room.audio_watches.lock().unwrap().is_empty()
    }

    /// "Leave" clicked on a joined room row: tear it down now, or report that
    /// a confirm dialog is needed first (the confirm dialog itself is UI-only
    /// state, owned by `ui::App`) if there's active work in it. Returns
    /// whether the room was torn down immediately (`false` means the caller
    /// must raise its own confirm dialog).
    pub fn request_leave_room(&mut self, handle: RoomHandle) -> bool {
        if self.leave_needs_confirm(handle) {
            false
        } else {
            self.disconnect_room(handle);
            true
        }
    }

    /// Poll the in-flight connect attempt, if any. Call once per frame.
    ///
    /// Returns what (if anything) just resolved, so a caller with UI-only
    /// state to react to (e.g. `ui::App`'s "+ Add Room" card) can do so
    /// without that state needing to live on `AppState` — see
    /// [`ConnectOutcome`].
    pub fn poll_connect_result(&mut self) -> ConnectOutcome {
        let Some(pending) = &mut self.pending_connect else { return ConnectOutcome::None };
        let result = match pending.rx.try_recv() {
            Ok(result) => result,
            Err(oneshot::error::TryRecvError::Empty) => return ConnectOutcome::None,
            Err(oneshot::error::TryRecvError::Closed) => Err(ConnectError::other(t!("errors.connect_task_ended").to_string())),
        };
        let Some(pending) = self.pending_connect.take() else { return ConnectOutcome::None };

        // An automatic reconnect attempt has its own bookkeeping.
        if let Some(handle) = pending.reconnect
            && self.reconnects.contains_key(&handle)
        {
            return self.finish_reconnect_attempt(handle, result);
        }

        match result {
            Ok((client, snapshot, event_rx)) => {
                self.status = Status::Idle;
                let handle = self.insert_room(client, snapshot, event_rx, pending.join_string.clone());
                // A successful join *is* the save: `RoomStore` is this app's
                // only persistent "rooms I care about" state, and the Add Room
                // flow deliberately has no separate staging step — "on success
                // the room appears live in the list, on failure nothing is
                // saved". Saved under an id-derived name, which the Stream
                // Control tab's rename row can replace; a room already saved
                // keeps whatever name it has.
                if self.room_store.name_for(&pending.join_string).is_none() {
                    let name = room_label(&self.room_store, &pending.join_string);
                    self.room_store.upsert(name, pending.join_string.clone());
                    if let Err(e) = self.room_store.save() {
                        self.room_store_error = Some(e.to_string());
                    }
                }
                // Newly joined room becomes what the content tabs drive.
                self.active_room = Some(handle);
                self.refresh_status();
                ConnectOutcome::Joined(handle)
            }
            Err(e) => {
                self.status = Status::Error(e.message.clone());
                self.refresh_status();
                ConnectOutcome::Failed { error: e.message, from_add_card: pending.from_add_card }
            }
        }
    }

    /// Drain every joined room's event channel, not just one. Each room owns
    /// its own connection and therefore its own channel; nothing here is
    /// shared between rooms except `captures`, which is why `rooms` and
    /// `captures` are borrowed as separate fields rather than through `self`.
    ///
    /// Rooms whose control connection closed are collected and, after the
    /// loop (removing from `rooms` mid-iteration isn't allowed), suspended for
    /// reconnecting (`suspend_room`) or torn down. Also resolves carried-over
    /// watches as streams are re-announced and drives the reconnect timers
    /// (`poll_reconnects`).
    ///
    /// Takes `ctx: &egui::Context` purely to upload freshly decoded preview
    /// thumbnails (`ClientEvent::Preview`) the same frame they arrive —
    /// pre-existing before this module split, not a new egui dependency
    /// introduced by it.
    pub fn drain_events(&mut self, ctx: &egui::Context) {
        let mut closed: Vec<(RoomHandle, String)> = Vec::new();
        let AppState { rooms, captures, .. } = self;

        for (handle, room) in rooms.iter_mut() {
            let handle = *handle;
            while let Ok(event) = room.event_rx.try_recv() {
                match event {
                    ClientEvent::PresenceDelta(PresenceEvent::MemberJoined(member)) => {
                        room.members.retain(|m| m.conn_id != member.conn_id);
                        room.members.push(member);
                    }
                    ClientEvent::PresenceDelta(PresenceEvent::MemberLeft { conn_id }) => {
                        room.members.retain(|m| m.conn_id != conn_id);
                        // Not required for correctness — `ConnId` is
                        // fresh-random per connection, so a later, unrelated
                        // sender is never expected to collide with this one
                        // — but keeps `audio_volume_by_owner` from growing
                        // forever across a long-lived room with many
                        // join/leave cycles. See that field's doc comment.
                        room.audio_volume_by_owner.remove(&conn_id);
                    }
                    ClientEvent::PresenceDelta(PresenceEvent::StreamStarted(info)) => {
                        room.note_stream_started(info);
                    }
                    ClientEvent::PresenceDelta(PresenceEvent::StreamStopped { conn_id, stream_id }) => {
                        // Before the roster forgets the stream: ends a watch
                        // of it (no frozen tile) and queues its resume.
                        room.end_watch_of_stopped(conn_id, stream_id, Instant::now());
                        room.note_stream_stopped(conn_id, stream_id);
                        room.preview_cache.remove(conn_id, stream_id);
                        room.preview_textures.remove(&(conn_id, stream_id));
                    }
                    // Only meaningful for a stream we're actively *sending*
                    // (relay spec §9: both are sent only to the connection
                    // that owns `stream_id`) — forward to that stream's live
                    // `CaptureHandle` so `capture.rs`'s pump loop can
                    // pause/resume the encoder and force keyframes. Looked up
                    // by `(room, stream_id)`, never `stream_id` alone: the
                    // same small integer means a different stream in every
                    // room. A miss (no such destination, not `Active` yet, or
                    // an audio capture) is a normal, silent no-op: audio has
                    // no encoder to pause, and a `Starting`/`Stopping`/absent
                    // capture has nothing listening yet.
                    ClientEvent::SubscriberCount { stream_id, count } => {
                        if let Some((LocalCapture { status: ShareStatus::Active(ShareHandle::Video(h)), .. }, dest)) =
                            capture_feeding(captures, handle, stream_id)
                        {
                            h.notify_subscriber_count(DestKey(dest.0), count);
                        }
                    }
                    ClientEvent::KeyframeRequest { stream_id } => {
                        if let Some((LocalCapture { status: ShareStatus::Active(ShareHandle::Video(h)), .. }, dest)) =
                            capture_feeding(captures, handle, stream_id)
                        {
                            h.notify_keyframe_request(DestKey(dest.0));
                        }
                    }
                    ClientEvent::MediaFrame(_) => {
                        // `Why:` this arm is unreachable at runtime by
                        // construction, not dead code the compiler could
                        // prove unreachable itself. `RoomSession::event_rx`
                        // is never the raw channel `ScreenshareClient::connect`
                        // returns — `insert_room` always routes that raw
                        // channel through `spawn_media_ingestion_task` first,
                        // and that task intercepts every `MediaFrame` and
                        // feeds `watches`/`audio_watches` directly, in real
                        // time, without ever forwarding it here.
                    }
                    ClientEvent::Preview(preview) => match decode::decode_preview(&preview) {
                        Ok(image) => {
                            let key = (preview.conn_id, preview.stream_id);
                            // Upload/update the roster thumbnail texture right
                            // away, mirroring `WatchEntry`'s
                            // create-once-then-`set`-in-place pattern rather
                            // than reuploading an unchanged image every
                            // repaint.
                            match room.preview_textures.get_mut(&key) {
                                Some(tex) => tex.set(image.clone(), egui::TextureOptions::LINEAR),
                                None => {
                                    let name = format!("preview-{:016x}-{}", preview.conn_id, preview.stream_id);
                                    let tex = ctx.load_texture(name, image.clone(), egui::TextureOptions::LINEAR);
                                    room.preview_textures.insert(key, tex);
                                }
                            }
                            room.preview_cache.update(preview.conn_id, preview.stream_id, image);
                        }
                        Err(e) => {
                            tracing::warn!(
                                "failed to decode preview for stream {} on conn {:016x}: {e:#}",
                                preview.stream_id,
                                preview.conn_id
                            );
                        }
                    },
                    ClientEvent::ControlClosed(reason) => {
                        closed.push((handle, reason));
                        break;
                    }
                }
            }

            // Right after this room's `StreamStarted`/`StreamStopped` events
            // have been applied to `room.streams` — the only inputs that can
            // change the answer — make its auto-subscribed audio match. This
            // is what makes a sharer enabling Share Audio *while* someone is
            // already watching them become audible on the next tick, with no
            // action by the viewer. See `sync_audio_watches`' doc comment.
            room.sync_audio_watches();
            // Watches carried over a reconnect: pick up their streams as the
            // re-announces arrive (a no-op when there are none).
            room.apply_rewatch(Instant::now());
        }

        for (handle, reason) in closed {
            let cause = self.rooms.get(&handle).and_then(|r| r.client.close_cause()).unwrap_or(CloseCause::Other);
            self.suspend_room(handle, reason, cause);
        }
        self.poll_reconnects();
    }
}

// ---------------------------------------------------------------------
// Auto-reconnect and clean exit
// ---------------------------------------------------------------------
//
// A room whose connection drops unexpectedly (`ClientEvent::ControlClosed`:
// relay restart, network loss, idle timeout) is not torn down any more. It is
// *suspended*: the `RoomSession` goes away, but a `ReconnectState` takes its
// place in `AppState::reconnects`, holding the room's handle, the last-known
// roster, and what the user had running (watches, our own shares). Attempts
// then run through the ordinary connect slot (`pending_connect`) on a backoff
// until the room is back, the user leaves/forgets/cancels, or the relay
// rejects the join for good.
//
// `Why the room's handle is kept:` everything that refers to a room by
// `RoomHandle` (a destination's `room`, `active_room`, the UI's staged shares,
// open views and modals) then stays valid across the outage with no remapping.
// Handles must never alias a *different* room; reusing one for the same room
// is safe.
//
// `Why a dropped room has no `RoomSession` while reconnecting:` the UI derives
// "Reconnecting" from "no session for this saved room, but `is_reconnecting`"
// (`ui::room_entries`), so keeping a half-dead session around would show the
// room as joined. The last-known roster lives on the `ReconnectState` instead.
//
// `Why our shares are rebuilt, not re-pointed:` a branch is bound to the
// connection that existed when it was built (its client clone, nonce space,
// room key, announced `StreamId`), and a new connection has a fresh `ConnId`.
// Removing the branch and adding a new one on the new room uses only existing
// capture primitives (it is what `retry_destination` already does); the
// *capture* (the screen grab and its portal session) keeps running throughout,
// so there is no new picker prompt. The cost is one encoder re-init per share.

impl AppState {
    /// The handle of the reconnect entry for `join_string` (same room, any
    /// cert hash), if one exists.
    fn reconnect_handle_for(&self, join_string: &str) -> Option<RoomHandle> {
        self.reconnects.iter().find(|(_, r)| RoomStore::same_room(&r.join_string, join_string)).map(|(h, _)| *h)
    }

    /// The reconnect state of the room `join_string`, if it is reconnecting:
    /// attempt count, next retry, last error, last-known roster. `None` for a
    /// room that is joined, disconnected, or merely connecting for the first
    /// time.
    pub fn reconnect_info(&self, join_string: &str) -> Option<&ReconnectState> {
        self.reconnect_handle_for(join_string).and_then(|h| self.reconnects.get(&h))
    }

    /// Keep [`Status`] truthful: `Connecting` while a connect is in flight or a
    /// room waits to reconnect, back to `Idle` when neither (an `Error` stays
    /// until something replaces it).
    fn refresh_status(&mut self) {
        if self.pending_connect.is_some() || !self.reconnects.is_empty() {
            self.status = Status::Connecting;
        } else if matches!(self.status, Status::Connecting) {
            self.status = Status::Idle;
        }
    }

    /// The control connection of `handle` ended without the user asking for it
    /// (`reason` is the text of the `ControlClosed` event, `cause` the QUIC
    /// close code mapped by [`CloseCause`]). Suspends the room for
    /// reconnecting, or, when it must not reconnect, tears it down like a user
    /// leave.
    ///
    /// Not reconnected: the room is not in the saved list (reconnecting is for
    /// rooms that stay in it), the app is shutting down, or the relay called
    /// the join malformed ([`retry_delay`]).
    fn suspend_room(&mut self, handle: RoomHandle, reason: String, cause: CloseCause) {
        let Some(join_string) = self.rooms.get(&handle).map(|r| r.join_string.clone()) else { return };
        let reconnect = !self.shutting_down && retry_delay(cause, 0).is_some() && self.room_store.name_for(&join_string).is_some();
        if !reconnect {
            self.status = Status::Error(t!("errors.disconnected", reason = reason).to_string());
            self.disconnect_room(handle);
            return;
        }

        let Some(room) = self.rooms.shift_remove(&handle) else { return };
        // The connection is dead, or (if only the control stream died) at
        // least useless: make sure it is closed so the relay drops us.
        spawn_leave(room.client.clone());

        let me = room.client.conn_id();
        let wanted = room.wanted_streams();
        // Last-known roster for the People panel, without ourselves and our
        // own streams: those belong to the dead connection's `ConnId`.
        let members: Vec<Member> = room.members.iter().filter(|m| m.conn_id != me).cloned().collect();
        let streams: Vec<StreamInfo> = room.streams.iter().filter(|s| s.owner != me).cloned().collect();

        let mut shares = Vec::new();
        for destination_id in room.destinations.clone() {
            if let Some(share) = self.suspend_destination(destination_id) {
                shares.push(share);
            }
        }

        let was_active_room = self.active_room == Some(handle);
        if was_active_room {
            self.active_room = self.rooms.keys().next().copied();
        }
        // Dropping the session drops its decoders and ingestion task.
        drop(room);

        self.reconnects.insert(
            handle,
            ReconnectState {
                join_string,
                handle,
                attempts: 0,
                // `retry_delay(.., 0)` is always zero: the first retry is
                // immediate, whatever the cause.
                next_retry: Some(Instant::now()),
                last_error: Some(reason),
                cause,
                since: Instant::now(),
                members,
                streams,
                shares,
                wanted,
                was_active_room,
                cancelled: false,
            },
        );
        self.status = Status::Connecting;
    }

    /// Take one of our destinations off a room whose connection died, keeping
    /// the capture. Returns what to put back on the new connection, if
    /// anything. See the section comment for why this removes rather than
    /// re-points.
    fn suspend_destination(&mut self, destination_id: DestinationId) -> Option<SuspendedShare> {
        let (capture_id, capture, dest) = find_destination(&self.captures, destination_id)?;
        let kind = capture.kind;
        let audio_for = capture.audio_for_capture;
        let label = capture.label.clone();
        let pending = dest.pending;
        let params = dest.live_params();
        let active = matches!(capture.status, ShareStatus::Active(_));
        let starting = matches!(capture.status, ShareStatus::Starting(_));

        let share = match kind {
            StreamKind::Video if active => {
                if let Some(capture) = self.captures.get_mut(&capture_id)
                    && let ShareStatus::Active(ShareHandle::Video(handle)) = &mut capture.status
                {
                    // Removes the branch; its `StopStream` goes to a dead
                    // connection and fails harmlessly.
                    let rx = handle.spawn_remove_destination(DestKey(destination_id.0));
                    tokio::spawn(async move {
                        if let Ok(Err(e)) = rx.await {
                            tracing::debug!("removing a destination after a connection drop did not finish cleanly: {e}");
                        }
                    });
                }
                match pending {
                    // A transfer half cannot be resumed: the other half (in a
                    // room that is still up) carries on alone.
                    Some(PendingRole::TransferSource(_)) => None,
                    Some(role @ PendingRole::TransferTarget(_)) => {
                        self.clear_transfer_marker(capture_id, role);
                        None
                    }
                    _ => Some(SuspendedShare::Video { capture: capture_id, params, role: PendingRole::GoingLive }),
                }
            }
            StreamKind::Audio if active => {
                let source = self
                    .audio_share_sources
                    .get(&capture_id)
                    .cloned()
                    .or_else(|| self.audio_sources.iter().find(|s| s.name == label).cloned());
                // The whole audio capture is bound to the dead connection;
                // stop it and start a fresh one later.
                self.stop_share_one(capture_id);
                match (audio_for, source) {
                    (Some(for_capture), Some(source)) => Some(SuspendedShare::Audio { for_capture, source }),
                    _ => None,
                }
            }
            // Mid-start: no handle to take apart yet. Cancel it the way a user
            // stop would (torn down on arrival); the user starts it again.
            _ if starting => {
                self.stop_share_one(capture_id);
                None
            }
            _ => None,
        };
        // Gone from the local model either way, so a later reap of a stopping
        // capture cannot touch the reconnected room's (re-numbered) streams.
        self.forget_destination(capture_id, destination_id);
        share
    }

    /// "Retry now": make `join_string`'s reconnect attempt start right away,
    /// replacing a hung in-flight one and skipping the remaining wait. No-op
    /// for a room that is not reconnecting. (If another room's connect holds
    /// the single connect slot the attempt starts as soon as it frees up.)
    pub fn reconnect_now(&mut self, join_string: &str) {
        let Some(handle) = self.reconnect_handle_for(join_string) else { return };
        if self.pending_connect.as_ref().is_some_and(|p| p.reconnect == Some(handle))
            && let Some(pending) = self.pending_connect.take()
        {
            pending.abort.abort();
        }
        let now = Instant::now();
        if let Some(entry) = self.reconnects.get_mut(&handle) {
            entry.cancelled = false;
            entry.next_retry = Some(now);
        }
        self.poll_reconnects_at(now);
    }

    /// Stop reconnecting `join_string`: abort an attempt in flight, drop the
    /// wait, and stop the captures that only existed to feed that room. The
    /// room stays in the list, disconnected, with its Connect button. No-op for
    /// a room that is not reconnecting. This is "Cancel" on the Reconnecting
    /// banner; leaving or forgetting the room does it too.
    pub fn cancel_reconnect(&mut self, join_string: &str) {
        if let Some(handle) = self.reconnect_handle_for(join_string) {
            self.discard_reconnect(handle);
        }
    }

    /// Remove `handle`'s reconnect entry and everything hanging off it.
    fn discard_reconnect(&mut self, handle: RoomHandle) {
        let Some(entry) = self.reconnects.shift_remove(&handle) else { return };
        if self.pending_connect.as_ref().is_some_and(|p| p.reconnect == Some(handle))
            && let Some(pending) = self.pending_connect.take()
        {
            pending.abort.abort();
        }
        // A screen grab kept only for this room's sake has nothing to send to
        // now and nobody can see it: stop it (this cascades to its audio).
        for share in &entry.shares {
            if let SuspendedShare::Video { capture, .. } = share
                && self.captures.get(capture).is_some_and(|c| c.destinations.is_empty())
            {
                self.stop_share(*capture);
            }
        }
        self.refresh_status();
    }

    /// Drive the reconnect timers: drop entries that were cancelled or whose
    /// room left the list, then, if the connect slot is free, start the attempt
    /// that is due. Called from [`Self::poll_auto_connect_queue`] and
    /// [`Self::drain_events`], i.e. every frame/tick in both front-ends, so
    /// callers need nothing extra.
    pub fn poll_reconnects(&mut self) {
        self.poll_reconnects_at(Instant::now());
    }

    fn poll_reconnects_at(&mut self, now: Instant) {
        let doomed: Vec<RoomHandle> = self
            .reconnects
            .iter()
            .filter(|(_, r)| r.cancelled || self.shutting_down || self.room_store.name_for(&r.join_string).is_none())
            .map(|(h, _)| *h)
            .collect();
        for handle in doomed {
            self.discard_reconnect(handle);
        }
        if self.pending_connect.is_some() {
            return;
        }
        let due = self
            .reconnects
            .iter()
            .filter(|(_, r)| r.next_retry.is_some_and(|at| at <= now))
            .min_by_key(|(_, r)| r.next_retry)
            .map(|(h, _)| *h);
        if let Some(handle) = due {
            self.begin_reconnect_attempt(handle, now);
        }
    }

    fn begin_reconnect_attempt(&mut self, handle: RoomHandle, now: Instant) {
        let Some(entry) = self.reconnects.get_mut(&handle) else { return };
        entry.attempts += 1;
        entry.next_retry = None;
        let join_string = entry.join_string.clone();
        let failed = entry.attempts;
        if let Err(e) = self.spawn_connect(join_string, false, Some(handle))
            && let Some(entry) = self.reconnects.get_mut(&handle)
        {
            // The local endpoint itself is unusable; nothing to wait for, but
            // do not spin either.
            entry.last_error = Some(e);
            entry.next_retry = Some(now + backoff_delay(failed));
        }
    }

    /// Route the result of a reconnect attempt: success re-inserts the room
    /// under its old handle and restores the user's intent; failure schedules
    /// the next attempt per [`retry_delay`], or gives up on a malformed join.
    fn finish_reconnect_attempt(&mut self, handle: RoomHandle, result: ConnectResult) -> ConnectOutcome {
        match result {
            Ok((client, snapshot, event_rx)) => {
                let Some(entry) = self.reconnects.shift_remove(&handle) else { return ConnectOutcome::None };
                self.insert_room_at(handle, client, snapshot, event_rx, entry.join_string.clone());
                self.restore_after_reconnect(handle, entry);
                self.status = Status::Idle;
                self.refresh_status();
                ConnectOutcome::Joined(handle)
            }
            Err(e) => {
                let Some(entry) = self.reconnects.get_mut(&handle) else { return ConnectOutcome::None };
                entry.last_error = Some(e.message.clone());
                entry.cause = e.cause;
                match retry_delay(e.cause, entry.attempts) {
                    Some(wait) => {
                        entry.next_retry = Some(Instant::now() + wait);
                        self.refresh_status();
                        ConnectOutcome::None
                    }
                    None => {
                        // The relay will never accept this join: surface it
                        // and fall back to a plain disconnected room.
                        self.discard_reconnect(handle);
                        self.status = Status::Error(e.message.clone());
                        self.refresh_status();
                        ConnectOutcome::Failed { error: e.message, from_add_card: false }
                    }
                }
            }
        }
    }

    /// Put back what the user had before the drop, on the freshly inserted
    /// room: the active-room pointer, our own shares, and the watches (the
    /// latter resolve against the roster, now and as streams are re-announced).
    fn restore_after_reconnect(&mut self, handle: RoomHandle, entry: ReconnectState) {
        let now = Instant::now();
        if entry.was_active_room && self.active_room.is_none() {
            self.active_room = Some(handle);
        }
        if let Some(room) = self.rooms.get_mut(&handle) {
            room.rewatch = entry.wanted;
            room.rewatch_deadline = Some(now + REWATCH_GRACE);
            room.apply_rewatch(now);
        }
        // Video first: an audio share is only valid next to a live video one.
        for share in &entry.shares {
            if let SuspendedShare::Video { capture, params, role } = share
                && self.captures.get(capture).is_some_and(|c| matches!(c.status, ShareStatus::Active(ShareHandle::Video(_))))
            {
                self.add_destination_with_params(*capture, handle, *role, *params);
            }
        }
        for share in entry.shares {
            if let SuspendedShare::Audio { for_capture, source } = share
                && self.captures.get(&for_capture).is_some_and(|c| !c.destinations.is_empty())
                && let Err(e) = self.start_audio_share_with(handle, for_capture, source)
            {
                note_share_error(&mut self.share_error, e);
            }
        }
    }

    // -----------------------------------------------------------------
    // Clean exit
    // -----------------------------------------------------------------

    /// Start leaving everything and return the future that finishes the job
    /// (all of it bounded by `timeout`). Synchronous part: stop reconnecting,
    /// cancel connects, drop every room session, take every running capture.
    /// The future: send `Leave` and close each room's QUIC connection (so the
    /// relay and the other members drop us within a round trip, not after the
    /// relay's 60-75 s idle timeout), close the endpoint and give quinn up to
    /// 500 ms to flush the `CONNECTION_CLOSE` frames (`Endpoint::wait_idle`),
    /// while the captures are stopped alongside (a pipeline can take seconds
    /// to drain; it must not delay the leave).
    ///
    /// Idempotent: a second call finds nothing left to do. After it the state
    /// must not be used to connect again.
    ///
    /// Prefer [`Self::shutdown`] / [`Self::shutdown_blocking`]; this is the
    /// seam for a caller that wants to run the future itself.
    pub fn begin_shutdown(&mut self, timeout: Duration) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        self.shutting_down = true;
        if let Some(pending) = self.pending_connect.take() {
            pending.abort.abort();
        }
        self.reconnects.clear();
        self.auto_connect_queue.clear();
        self.status = Status::Idle;

        let clients: Vec<ScreenshareClient> = self.rooms.values().map(|r| r.client.clone()).collect();
        let endpoint = self.endpoint.as_ref().ok().cloned();
        let mut stops: Vec<ShareHandle> = Vec::new();
        for (_, capture) in std::mem::take(&mut self.captures) {
            // A capture still starting is dropped here; its start task's
            // result then has nowhere to go and the handle's `Drop` backstop
            // releases the pipeline.
            if let ShareStatus::Active(handle) = capture.status {
                stops.push(handle);
            }
        }
        self.pending_destinations.clear();
        self.rooms.clear();
        self.active_room = None;

        Box::pin(async move {
            let leave = async {
                futures_util::future::join_all(clients.iter().map(|c| c.leave_and_close())).await;
                if let Some(endpoint) = &endpoint {
                    endpoint.close(crate::net::CLOSE_CLIENT_LEAVE.into(), b"exit");
                    let _ = tokio::time::timeout(Duration::from_millis(500), endpoint.wait_idle()).await;
                }
            };
            let stop = futures_util::future::join_all(stops.into_iter().map(|handle| async move {
                let result = match handle {
                    ShareHandle::Video(h) => h.stop().await,
                    ShareHandle::Audio(h) => h.stop().await,
                };
                if let Err(e) = result {
                    tracing::warn!("stopping a capture during shutdown failed: {e:#}");
                }
            }));
            if tokio::time::timeout(timeout, futures_util::future::join(leave, stop)).await.is_err() {
                tracing::warn!("shutdown did not finish within {timeout:?}; exiting anyway");
                // Whatever the leave did not reach, the closes below still do.
                for client in &clients {
                    client.connection().close(crate::net::CLOSE_CLIENT_LEAVE.into(), b"exit");
                }
            }
        })
    }

    /// [`Self::begin_shutdown`], awaited. For callers already in async code.
    pub async fn shutdown(&mut self, timeout: Duration) {
        self.begin_shutdown(timeout).await;
    }

    /// [`Self::begin_shutdown`], blocking the calling thread until it is done
    /// or `timeout` (plus a small margin) has passed. This is what a
    /// synchronous exit hook calls: eframe's `App::on_exit`, a signal handler.
    ///
    /// Must run on a thread that is **not** a worker of the runtime it needs
    /// (the work is spawned onto it and waited for over a plain channel); the
    /// main thread of `main.rs`, where the multi-thread runtime is entered, is
    /// right. With no runtime at all it builds a throwaway one.
    pub fn shutdown_blocking(&mut self, timeout: Duration) {
        let fut = self.begin_shutdown(timeout);
        let margin = Duration::from_millis(250);
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                let (tx, rx) = std::sync::mpsc::channel();
                runtime.spawn(async move {
                    fut.await;
                    let _ = tx.send(());
                });
                let _ = rx.recv_timeout(timeout + margin);
            }
            Err(_) => {
                if let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    runtime.block_on(fut);
                }
            }
        }
    }
}

/// Send `Leave` and close `client`'s connection in the background; with no
/// runtime to spawn on, just close it.
fn spawn_leave(client: ScreenshareClient) {
    match tokio::runtime::Handle::try_current() {
        Ok(runtime) => {
            runtime.spawn(async move { client.leave_and_close().await });
        }
        Err(_) => client.connection().close(crate::net::CLOSE_CLIENT_LEAVE.into(), b"leave"),
    }
}

static SHUTDOWN_REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether SIGTERM/SIGINT/SIGHUP was received since
/// [`install_shutdown_signal_handlers`].
pub fn shutdown_requested() -> bool {
    SHUTDOWN_REQUESTED.load(std::sync::atomic::Ordering::SeqCst)
}

/// How long after the first signal the process force-exits if the UI never got
/// round to closing (a wedged frame loop must not make `kill` ineffective).
pub const SIGNAL_EXIT_GRACE: Duration = Duration::from_secs(4);

/// Turn SIGTERM, SIGINT and SIGHUP into a normal window close, so the app's
/// ordinary exit path (`eframe::App::on_exit` -> [`AppState::shutdown_blocking`])
/// runs and the relay is told we left. The first signal asks the root viewport
/// to close and wakes the frame loop; the second, or [`SIGNAL_EXIT_GRACE`]
/// without an exit, kills the process.
///
/// `Why via the window and not touching `AppState` here:` the state is owned by
/// the UI thread (`ui::App`), so the only safe way for a signal task to get a
/// clean shutdown is to ask that thread to run it.
///
/// Needs a Tokio runtime entered on the calling thread (`main.rs` has one);
/// without one it does nothing. Call once, from `ui::App::new`.
#[cfg(unix)]
pub fn install_shutdown_signal_handlers(ctx: egui::Context) {
    use tokio::signal::unix::{SignalKind, signal};
    let Ok(runtime) = tokio::runtime::Handle::try_current() else { return };
    runtime.spawn(async move {
        let (Ok(mut term), Ok(mut int), Ok(mut hup)) =
            (signal(SignalKind::terminate()), signal(SignalKind::interrupt()), signal(SignalKind::hangup()))
        else {
            tracing::warn!("could not install the SIGTERM/SIGINT handlers; the relay will only learn of an exit by timeout");
            return;
        };
        let mut seen = 0u32;
        loop {
            tokio::select! {
                _ = term.recv() => {}
                _ = int.recv() => {}
                _ = hup.recv() => {}
            }
            seen += 1;
            SHUTDOWN_REQUESTED.store(true, std::sync::atomic::Ordering::SeqCst);
            if seen > 1 {
                std::process::exit(130);
            }
            tracing::info!("termination signal received, closing the window");
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            ctx.request_repaint();
            std::thread::spawn(|| {
                std::thread::sleep(SIGNAL_EXIT_GRACE);
                std::process::exit(143);
            });
        }
    });
}

/// No signal handling off Unix (the client only runs on Linux today).
#[cfg(not(unix))]
pub fn install_shutdown_signal_handlers(_ctx: egui::Context) {}

// ---------------------------------------------------------------------
// UI-facing operations
// ---------------------------------------------------------------------
//
// Everything below used to live in the former (removed) Tauri sidecar's command layer
// or in the JS frontend. It is here so that the UI stays a thin view over
// `AppState` and never re-implements business logic.

/// A freshly minted room: the local nickname plus the join string (which
/// contains the room *key* — never log or display it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinLink {
    /// The LOCAL-only nickname the room was saved under (never sent over the
    /// wire).
    pub room_name: String,
    pub join_string: String,
}

/// List currently-audio-producing applications without blocking the async
/// executor: `audio::list_audio_sources` shells out to `pw-dump`
/// synchronously, so it runs on the blocking pool.
///
/// Deliberately bypasses [`AppState::refresh_audio_sources`]/
/// [`AppState::poll_audio_sources`] (the fire-then-poll-next-frame pattern):
/// a caller that can `.await` has a natural async return point and needs no
/// poll.
pub async fn list_audio_sources_async() -> Result<Vec<AudioSource>, String> {
    tokio::task::spawn_blocking(audio::list_audio_sources)
        .await
        .map_err(|e| format!("audio source listing task panicked: {e}"))?
        .map_err(|e| format!("{e:#}"))
}

/// Resolve the PipeWire `object.serial` to tap for `source_id` out of a
/// fresh listing. `Err` (the source vanished, or has no serial yet) just
/// means that one dropdown entry's meter stays unlit.
pub fn pick_level_tap_serial(sources: Vec<AudioSource>, source_id: u32) -> Result<u64, String> {
    let source =
        sources.into_iter().find(|s| s.id == source_id).ok_or_else(|| "audio source is no longer listed".to_string())?;
    source.serial.ok_or_else(|| format!("'{}' has no PipeWire object.serial to tap yet", source.name))
}

/// Build a non-announcing per-candidate level tap for the "Add audio"/"Switch"
/// dropdown's meters — see [`AppState::insert_audio_level_tap`] and
/// [`audio::AudioLevelTapHandle`]'s doc comments for why this is a separate,
/// much lighter thing than [`AppState::start_audio_share`]. Hand the result to
/// [`AppState::insert_audio_level_tap`] to get the id the UI polls/stops it
/// with.
///
/// Re-lists sources here (rather than trusting `source_id` against whatever
/// the UI already had) so the tap is built against `object.serial` for the
/// node as it exists right now.
///
/// `Why this is a free async fn that must run *before* `AppState` is ever
/// locked/borrowed, with its own `spawn_blocking`:` the doc comment of
/// `audio::start_level_tap` claims `pipeline.set_state(Playing)` returns as
/// soon as the state change is *requested*, without waiting for it to
/// complete — true in the common case, but confirmed live (real dropdown
/// open, real PipeWire session, one candidate among several) that
/// `set_state` can still block synchronously for several seconds on a node
/// PipeWire hasn't fully brought up yet (an idle/suspended stream in
/// particular). The old code built the *entire* GStreamer pipeline,
/// including that `set_state` call, while holding the old sidecar's `AppState`
/// mutex for the whole dropdown-open burst of concurrent taps. One slow/stuck
/// candidate then held that lock indefinitely, and every other command
/// needing it — including unrelated taps for perfectly healthy sources and,
/// worse, `start_audio_share` for the "Share" button on the very same
/// dropdown — queued behind it forever. That was the root cause of both the
/// "visualizer never lights up" and "clicking Share does nothing" bug
/// reports. Building the pipeline before taking the lock means a stuck
/// candidate only ever blocks its own request (which the UI's timeout already
/// treats as "leave this one meter unlit") instead of every other command in
/// the app.
pub async fn build_audio_level_tap(source_id: u32) -> Result<audio::AudioLevelTapHandle, String> {
    let serial = pick_level_tap_serial(list_audio_sources_async().await?, source_id)?;
    tokio::task::spawn_blocking(move || audio::start_level_tap(serial))
        .await
        .map_err(|e| format!("audio level-tap task panicked: {e}"))?
        .map_err(|e| format!("{e:#}"))
}

/// Room management: join / create / forget.
impl AppState {
    /// `Ok(())` unless a `RoomStore` save has failed (sticky — the error is
    /// never cleared, so once a save has failed every later call reports it).
    pub fn room_store_result(&self) -> Result<(), String> {
        match &self.room_store_error {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }

    /// Persist `room_store`, recording (not propagating) a failure in
    /// `room_store_error`.
    fn save_room_store(&mut self) {
        if let Err(e) = self.room_store.save() {
            self.room_store_error = Some(e.to_string());
        }
    }

    /// Save `join_string` under the local nickname `room_name` *before*
    /// connecting, so the room keeps the human-chosen name instead of
    /// `poll_connect_result`'s id-derived fallback (which only fires when
    /// `RoomStore::name_for` has no entry for this join string yet).
    ///
    /// `Why:` unconditional `upsert` (not "only if not already saved", which
    /// is `poll_connect_result`'s own guard): a name typed into the Join or
    /// Create form is a deliberate choice by the user for *this* join, so it
    /// wins over a name this room already had saved. `RoomStore::upsert` keys
    /// by room identity (join string minus cert hash), so a room that is
    /// already saved is updated in place (typed name, newest join string) and
    /// never duplicated; a blank `room_name` keeps the existing name and only
    /// refreshes the stored join string.
    fn seed_room_name(&mut self, room_name: String, join_string: String) {
        self.room_store.upsert(room_name, join_string);
        self.save_room_store();
    }

    /// Join `join_string`. `display_name` (if non-blank) replaces the
    /// session's display name; `room_name` (if non-blank after trimming) is
    /// the local nickname to save the room under before connecting — see
    /// [`Self::seed_room_name`]. Leaving it blank preserves the
    /// join-then-auto-name-from-id behaviour.
    ///
    /// Spawns the connect task, so it needs a Tokio runtime entered (see
    /// [`Self::start_connect`]).
    pub fn join_room(&mut self, join_string: String, display_name: String, room_name: String) -> Result<(), String> {
        if !display_name.trim().is_empty() {
            self.display_name = display_name;
        }
        let room_name = room_name.trim().to_string();
        // Also refresh an already-saved room's stored join string (e.g. a
        // fresh cert hash) when no name was typed; an unsaved room with a
        // blank name stays unsaved until the connect succeeds.
        if !room_name.is_empty() || self.room_store.get(&join_string).is_some() {
            self.seed_room_name(room_name, join_string.clone());
        }
        self.start_connect(join_string, true)
    }

    /// "Create a room", minus the connect: per the server's
    /// `Registry::get_or_create_room` (spec §10) there is no separate create
    /// verb on the wire — a room is auto-created the instant anyone joins a
    /// room-id that doesn't exist yet. So creating, from this client, is just:
    /// mint a fresh room-id + key nobody has used before, format them into a
    /// join string with `server`, and save that join string locally under
    /// `room_name`. Joining it ([`Self::create_and_join_room`], or
    /// [`Self::start_connect`] on the returned link) then makes the first
    /// joiner the room's effective creator.
    ///
    /// The room *key* is a real AES-256 AEAD secret (`net::ParsedJoinString`,
    /// and `net.rs`'s frame en/decryption), not a display label — it is
    /// generated here, in Rust, with the `rand` crate's default
    /// (CSPRNG-backed) generator, deliberately not in a web frontend (as the removed
    /// JS UI once did), where there is no equivalent secure-randomness primitive.
    ///
    /// `server` and `room_name` are trimmed and must be non-empty.
    pub fn create_room(&mut self, server: &str, room_name: &str) -> Result<JoinLink, String> {
        let server = server.trim();
        if server.is_empty() {
            return Err("server address must not be empty".to_string());
        }
        crate::net::parse_server_addr(server).map_err(|e| e.to_string())?;
        let room_name = room_name.trim().to_string();
        if room_name.is_empty() {
            return Err("room name must not be empty".to_string());
        }

        let room_id: crate::protocol::RoomId = rand::random();
        let room_key: [u8; 32] = rand::random();
        let join_string = crate::net::format_join_string(server, &room_id, &room_key);

        self.seed_room_name(room_name.clone(), join_string.clone());
        Ok(JoinLink { room_name, join_string })
    }

    /// [`Self::create_room`] followed by the same join flow
    /// [`Self::join_room`] uses.
    ///
    /// Display name is deliberately not a parameter: it is one global
    /// Settings value ([`Self::set_display_name`]), not asked per room, so
    /// this passes an empty one through — which leaves
    /// [`AppState::display_name`] untouched. The room name is already
    /// pre-seeded by `create_room`, so `join_room`'s own upsert is skipped
    /// too.
    pub fn create_and_join_room(&mut self, server: &str, room_name: &str) -> Result<(), String> {
        let link = self.create_room(server, room_name)?;
        self.join_room(link.join_string, String::new(), String::new())
    }

    /// Forget a saved room: drop its `RoomStore` entry and persist the
    /// removal immediately, so it doesn't reappear the next time the store is
    /// reloaded from disk. Keyed by join string rather than a room handle
    /// since "forget" only applies to a saved, disconnected row, which has no
    /// handle at all — mirrors [`Self::rename_saved_room`]'s lookup.
    ///
    /// A join string that isn't saved is a no-op that still re-saves (and
    /// reports any sticky store error).
    ///
    /// Also ends any reconnect of that room: reconnecting is only for rooms
    /// that stay in the list.
    pub fn forget_room(&mut self, join_string: &str) -> Result<(), String> {
        self.cancel_reconnect(join_string);
        self.room_store.remove(join_string);
        self.save_room_store();
        self.room_store_result()
    }
}

/// Persisted global settings.
impl AppState {
    /// Switch the UI language, persisting the choice and updating
    /// `rust_i18n`'s global locale.
    pub fn set_language(&mut self, language: &str) {
        rust_i18n::set_locale(language);
        self.language = language.to_string();
        save_language(language);
    }

    /// Persist the global display name (Settings tab; trimmed). Used for
    /// every room join/create from here on: the wire protocol sends a display
    /// name once, at connect, with no later way to change it for an
    /// already-joined room, so there was never a per-room distinction to
    /// preserve. See `rooms::AppSettings::display_name`.
    pub fn set_display_name(&mut self, display_name: &str) {
        let display_name = display_name.trim().to_string();
        save_display_name(&display_name);
        self.display_name_set = !display_name.is_empty();
        self.display_name = display_name;
    }

    /// Only takes effect on the *next* launch — see
    /// `superdoc/features/presentation-modes.md`.
    pub fn set_presentation_mode(&mut self, mode: PresentationMode) {
        self.presentation_mode = mode;
        save_presentation_mode(mode);
    }

    /// Clamped here too (defense in depth) even though the UI's range input
    /// already restricts it to 0..=5000 — see
    /// `rooms::AppSettings::clamp_max_jitter_buffer_ms`. Returns the value
    /// actually stored.
    pub fn set_max_jitter_buffer_ms(&mut self, ms: u32) -> u32 {
        let ms = AppSettings::clamp_max_jitter_buffer_ms(ms);
        self.max_jitter_buffer_ms = ms;
        save_max_jitter_buffer_ms(ms);
        ms
    }

    /// Clamped here too (defense in depth) even though the UI's range input
    /// already restricts it to 5..=10 — see
    /// `rooms::AppSettings::clamp_catch_up_speed_percent`. Returns the value
    /// actually stored.
    pub fn set_catch_up_speed_percent(&mut self, percent: u32) -> u32 {
        let percent = AppSettings::clamp_catch_up_speed_percent(percent);
        self.catch_up_speed_percent = percent;
        save_catch_up_speed_percent(percent);
        percent
    }

    /// Absolute-Maximum mode's per-client total, kbit/s (persisted along with
    /// the mode and floor — see [`save_bandwidth_settings`]).
    pub fn set_bandwidth_total(&mut self, total_kbps: u32) {
        self.absolute_total_kbps = total_kbps;
        save_bandwidth_settings(self.bandwidth_mode, self.absolute_total_kbps, self.destination_floor_kbps);
    }

    /// Absolute-Maximum mode's guaranteed per-destination floor, kbit/s
    /// (persisted along with the mode and total).
    pub fn set_bandwidth_floor(&mut self, floor_kbps: u32) {
        self.destination_floor_kbps = floor_kbps;
        save_bandwidth_settings(self.bandwidth_mode, self.absolute_total_kbps, self.destination_floor_kbps);
    }

    /// What the allocator currently assigns `destination_id`, kbit/s — the
    /// per-stream share of the Absolute-Maximum budget the destination card
    /// shows. `None` until the first allocation tick has run for a
    /// destination (Per-Stream mode, or the destination is still starting).
    ///
    /// The removed JS frontend showed a toy equal split here (`floor`-clamped
    /// `total / n`); the real allocator weights by `width*height*fps` and honours floors/ceilings,
    /// so this reads its actual result instead.
    pub fn allocated_kbps(&self, destination_id: DestinationId) -> Option<u32> {
        let id = DestId(destination_id.0);
        self.bandwidth.allocation()?.per_destination.iter().find(|(d, _)| *d == id).map(|(_, kbps)| *kbps)
    }

    /// How far the last allocation overshot the Absolute-Maximum total
    /// (floors alone not fitting the budget), kbit/s; `0` if it fit or no
    /// allocation has run. Drives the overshoot warning banner.
    pub fn bandwidth_overshoot_kbps(&self) -> u32 {
        self.bandwidth.allocation().map_or(0, |a| a.overshoot_kbps)
    }
}

/// The "Start a stream" form: persisted defaults, the live "next capture"
/// settings, and encoder presets.
impl AppState {
    /// The form's persisted last-used values, read straight from the
    /// settings file. Not `AppState`-backed like the bandwidth settings
    /// (nothing here has a live/in-session counterpart *as a saved
    /// default*), so this is a disk read.
    ///
    /// An associated function (no `&self`) because it needs no session
    /// state, so callers need not hold the `AppState`.
    pub fn start_stream_defaults() -> StartStreamDefaults {
        AppSettings::load().start_stream_defaults
    }

    /// Persist `defaults` as the form's last-used values. Does not touch the
    /// live "next capture" fields — see [`Self::apply_start_stream_defaults`]
    /// for the other direction.
    pub fn set_start_stream_defaults(defaults: StartStreamDefaults) {
        save_start_stream_defaults(
            defaults.framerate,
            defaults.bitrate_kbps,
            defaults.tune,
            defaults.quality_level,
            defaults.codec,
        );
    }

    /// The live "next capture" settings in persisted-defaults shape
    /// (everything the form saves; GOP is deliberately not part of it).
    pub fn current_start_stream_settings(&self) -> StartStreamDefaults {
        StartStreamDefaults {
            framerate: self.framerate,
            bitrate_kbps: self.bitrate_kbps,
            tune: self.tune,
            quality_level: self.quality_level,
            codec: self.codec,
        }
    }

    /// Persist the live "next capture" settings as the new last-used
    /// defaults.
    pub fn save_current_start_stream_defaults(&self) {
        Self::set_start_stream_defaults(self.current_start_stream_settings());
    }

    /// Push the persisted last-used values into the live "next capture"
    /// fields, and reset `gop_millis` to [`capture::DEFAULT_GOP_MILLIS`].
    ///
    /// `Why this exists:` [`Self::stage_video_share`] (like the
    /// `start_video_share` it replaced) reads those process-wide fields, not
    /// arguments of its own, and the capture's framerate in particular is
    /// fixed at pipeline-build time inside it. Without this, a staged card
    /// would open running whatever the fields last happened to be (the
    /// hardcoded first-run fallback on a fresh launch) instead of what "Go
    /// live" last actually used.
    ///
    /// `Why GOP is reset and not restored:` `gop_millis` is *not* one of the
    /// persisted fields — it is a preset-specific tradeoff (only Gaming moves
    /// it, see `presets.rs`), not a standalone setting worth remembering
    /// across restarts. It is always reset to the protocol default here so a
    /// previous staged card's Gaming preset (a shorter GOP) can never
    /// silently bleed into the next one.
    pub fn apply_start_stream_defaults(&mut self) {
        let d = Self::start_stream_defaults();
        self.framerate = d.framerate;
        self.bitrate_kbps = d.bitrate_kbps;
        self.tune = d.tune;
        self.quality_level = d.quality_level;
        self.codec = d.codec;
        self.gop_millis = capture::DEFAULT_GOP_MILLIS;
    }

    /// "Share Screen" clicked: [`Self::apply_start_stream_defaults`], then
    /// [`Self::stage_video_share`].
    pub fn stage_video_share_with_saved_defaults(&mut self) -> CaptureSourceId {
        self.apply_start_stream_defaults();
        self.stage_video_share()
    }

    /// "Go live" clicked: [`Self::go_live`], then persist the card's current
    /// encoder settings as the new last-used defaults.
    ///
    /// `Why the save happens here and not on every change:` per the project
    /// owner's explicit request, the defaults are persisted once, at "Go
    /// live", not on every keystroke while the card is merely staged.
    pub fn go_live_and_save_defaults(&mut self, capture_id: CaptureSourceId, room_handle: RoomHandle) {
        self.go_live(capture_id, room_handle);
        self.save_current_start_stream_defaults();
    }

    /// Which codecs are actually usable on this machine's current encoder
    /// backend — the mechanism the codec selector uses to grey out
    /// unavailable options. See `capture::supported_codecs` for why this is a
    /// static function of the backend rather than a live per-codec probe.
    /// (The backend detection itself is probed once and cached for the
    /// process's lifetime.)
    pub fn supported_codecs() -> &'static [capture::VideoCodec] {
        capture::supported_codecs(capture::detect_encoder_backend())
    }
}

/// Destination edits.
impl AppState {
    /// "Apply" clicked on a destination row's settings edit: replace the
    /// row's edit buffer with `edit`, then apply it.
    ///
    /// Any change to tune / quality level / rate control (like a
    /// resolution / framerate / backend change) is applied as a
    /// `ChangeTier::BranchSwap` by [`Self::apply_destination_edit`] →
    /// `capture::destination::classify_change` — never a live property write.
    /// No-op if the destination is already gone.
    pub fn edit_destination(&mut self, destination_id: DestinationId, edit: DestEdit) {
        self.with_destination(destination_id, |dest| dest.edit = edit);
        self.apply_destination_edit(destination_id);
    }
}

/// Viewer-side audio.
impl AppState {
    /// Set `owner`'s playback volume (0-200%) and mute state in `room`.
    /// `stream_id` may be the audio stream's id or the video stream's — see
    /// [`RoomSession::set_volume`]. No-op for an unknown room.
    pub fn set_volume(&mut self, room: RoomHandle, owner: ConnId, stream_id: StreamId, volume_percent: u32, muted: bool) {
        if let Some(room) = self.rooms.get_mut(&room) {
            room.set_volume(owner, stream_id, volume_percent, muted);
        }
    }

    /// `owner`'s current volume/mute state in `room` — see
    /// [`RoomSession::volume`]. Defaults (100%, unmuted) for an unknown room.
    pub fn volume(&self, room: RoomHandle, owner: ConnId, stream_id: StreamId) -> VolumeState {
        self.rooms.get(&room).map(|room| room.volume(owner, stream_id)).unwrap_or_default()
    }
}

#[cfg(test)]
mod connect_timeout_and_cancel_tests {
    use super::*;

    /// A syntactically valid join string pointing at TEST-NET-1
    /// (`192.0.2.0/24`, reserved by RFC 5737 for documentation/examples,
    /// guaranteed never to route anywhere real) — the QUIC handshake's
    /// `.await` inside `ScreenshareClient::connect` blocks on real network
    /// I/O against it and never resolves on its own, which is exactly the
    /// "unreachable server" shape both tests below need.
    fn unreachable_join_string() -> String {
        crate::net::format_join_string("192.0.2.1:4433", &[0u8; 16], &[0x42u8; 32])
    }

    /// The connect timeout fires and reports failure once `connect_timeout`
    /// elapses, without the test waiting that long for real: `start_paused =
    /// true` gives this test tokio's virtual clock, and `tokio::time::advance`
    /// jumps it forward directly — `tokio::time::timeout`'s internal sleep
    /// fires off that virtual clock, so this proves the same code path the
    /// real 15s default takes without a real 15s (or even a real fraction of
    /// a second) sleep. `connect_timeout` is shrunk first purely so the
    /// `advance` call is small and the intent ("past the deadline") reads
    /// clearly — the mechanism doesn't depend on the specific duration.
    #[tokio::test(start_paused = true)]
    async fn connect_times_out_and_reports_failure_without_a_real_wait() {
        let _home = super::test_support::isolated_config_home();
        let mut state = AppState::default();
        state.connect_timeout = Duration::from_millis(50);

        state.start_connect(unreachable_join_string(), false).expect("start_connect should not fail synchronously");
        assert!(state.pending_connect.is_some());

        // Let the spawned task actually start running (DNS-resolve the
        // literal IP, kick off the QUIC handshake) before advancing time —
        // otherwise `tokio::time::timeout`'s inner sleep hasn't even been
        // constructed yet for `advance` to fast-forward past.
        tokio::task::yield_now().await;

        // Nothing has resolved yet — the connect is still (virtually) in
        // flight.
        assert!(matches!(state.poll_connect_result(), ConnectOutcome::None));

        tokio::time::advance(Duration::from_millis(100)).await;
        // Let the now-fired timeout's task actually run and send its result;
        // may take a couple of scheduling rounds, so poll a few times rather
        // than assuming one `yield_now` is enough.
        let mut outcome = ConnectOutcome::None;
        for _ in 0..20 {
            tokio::task::yield_now().await;
            outcome = state.poll_connect_result();
            if !matches!(outcome, ConnectOutcome::None) {
                break;
            }
        }

        match outcome {
            ConnectOutcome::Failed { error, from_add_card } => {
                assert!(error.contains("timed out"), "expected a timeout error, got: {error}");
                assert!(!from_add_card);
            }
            other => panic!("expected ConnectOutcome::Failed, got a different outcome: {}", matches!(other, ConnectOutcome::None)),
        }
        assert!(state.pending_connect.is_none(), "pending_connect must be cleared once resolved");
    }

    /// `cancel_connect` aborts an in-flight attempt outright — the room
    /// panel's Disconnect button while a row shows "Reconnecting…". Checks
    /// both ends: `pending_connect` is cleared immediately (what makes the
    /// row flip back to disconnected), and the aborted task actually stops
    /// running rather than silently finishing in the background later (what
    /// "no leaked task" means here — see `PendingConnect::abort`'s doc
    /// comment for why aborting mid-handshake is safe).
    #[tokio::test(start_paused = true)]
    async fn cancel_connect_aborts_the_in_flight_attempt_without_leaking() {
        let _home = super::test_support::isolated_config_home();
        let mut state = AppState::default();
        // Timeout deliberately left far longer than this test needs — this
        // test is about the user-initiated cancel path, not the timeout, so
        // the timeout must not be the thing that resolves it first.
        state.connect_timeout = Duration::from_secs(60);

        state.start_connect(unreachable_join_string(), false).expect("start_connect should not fail synchronously");
        let pending = state.pending_connect.as_ref().expect("pending_connect must be set");
        let abort_handle = pending.abort.clone();
        assert!(!abort_handle.is_finished(), "connect task should still be in flight before cancelling");

        state.cancel_connect();
        assert!(state.pending_connect.is_none(), "cancel_connect must clear pending_connect immediately");
        assert!(matches!(state.status, Status::Idle));

        // Give the abort a chance to actually take effect on the task.
        tokio::task::yield_now().await;
        assert!(abort_handle.is_finished(), "the spawned connect task must actually stop running, not just be forgotten");

        // Advancing well past even the (irrelevantly long) timeout must not
        // resurrect a result — there's nothing left to resolve.
        tokio::time::advance(Duration::from_secs(120)).await;
        tokio::task::yield_now().await;
        assert!(matches!(state.poll_connect_result(), ConnectOutcome::None));
    }
}

/// Shared fixtures for tests that construct a real [`AppState`] (which reads —
/// and, for the persisted-settings operations, writes — the on-disk config).
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::PathBuf;
    use std::sync::MutexGuard;

    use super::*;

    /// Serializes every test that touches the process-global
    /// `XDG_CONFIG_HOME` env var: `cargo test` runs tests in parallel threads
    /// within one process, so without this two tests would stomp each other's
    /// config dir mid-test.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Points `XDG_CONFIG_HOME` at a private temp dir for the guard's
    /// lifetime (and removes it afterwards). Hold it for the whole test: the
    /// lock must stay held (or another test could repoint the env var out
    /// from under this one) and the dir must stay alive.
    pub(crate) struct IsolatedConfigHome {
        _guard: MutexGuard<'static, ()>,
        dir: PathBuf,
    }

    impl Drop for IsolatedConfigHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    pub(crate) fn isolated_config_home() -> IsolatedConfigHome {
        // Poisoning only happens if an earlier test panicked while holding the
        // lock; that failure is already reported, so recover rather than
        // cascading the panic into every later test.
        let guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = std::env::temp_dir().join(format!("simple-screensharing-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp config home");
        // SAFETY: held under `ENV_LOCK` for the guard's whole lifetime, so no
        // other test using this helper reads or writes the variable
        // concurrently. `dirs::config_dir()` is consulted lazily, at the point
        // `RoomStore`/`AppSettings` load/save — never cached.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", &dir);
            // `dirs` ignores XDG_CONFIG_HOME on Windows; this one is honoured everywhere.
            std::env::set_var("SSS_CONFIG_HOME", &dir);
        }
        // Fail loudly (rather than scribble over the developer's real config)
        // on a platform where `XDG_CONFIG_HOME` doesn't steer `dirs`.
        assert!(
            RoomStore::config_path().starts_with(&dir) && AppSettings::config_path().starts_with(&dir),
            "SSS_CONFIG_HOME did not redirect the config dir"
        );
        IsolatedConfigHome { _guard: guard, dir }
    }

    pub(crate) fn video_params() -> capture::destination::VideoParams {
        capture::destination::VideoParams {
            width: 1920,
            height: 1080,
            framerate: 60,
            bitrate_kbps: 8000,
            backend: capture::EncoderBackend::Software,
            tune: capture::TuneMode::BestQuality,
            quality_level: capture::QualityLevel::Balanced,
            gop_millis: capture::DEFAULT_GOP_MILLIS,
            codec: capture::VideoCodec::H264,
        }
    }

    /// Inserts a video capture (still `Starting`, so edits are stored but never
    /// reach an encoder) holding one destination, and returns their ids.
    pub(crate) fn insert_destination(state: &mut AppState) -> (CaptureSourceId, DestinationId) {
        let capture_id = state.alloc_capture_id();
        let destination_id = state.alloc_destination_id();
        let params = video_params();
        let mut destinations = IndexMap::new();
        destinations.insert(
            destination_id,
            Destination {
                room: RoomHandle(0),
                stream_id: 0,
                params,
                runtime: None,
                pending: None,
                edit: Destination::edit_from(params),
                apply_rx: None,
            },
        );
        let (_tx, rx) = oneshot::channel();
        state.captures.insert(
            capture_id,
            LocalCapture {
                kind: StreamKind::Video,
                label: "test".to_string(),
                status: ShareStatus::Starting(rx),
                audio_for_capture: None,
                destinations,
            },
        );
        (capture_id, destination_id)
    }
}

/// Tests for the UI-facing operations above.
/// Each needs a real `AppState` (so `#[tokio::test]`: its constructor binds a
/// QUIC endpoint) pointed at a private config dir, but no network and no
/// GStreamer.
#[cfg(test)]
mod ui_ops_tests {
    use super::test_support::*;
    use super::*;

    fn stream(owner: ConnId, stream_id: StreamId, kind: StreamKind) -> StreamInfo {
        StreamInfo { owner, stream_id, kind, label: String::new(), width: 0, height: 0 }
    }

    // ---- rooms ----

    #[tokio::test]
    async fn create_room_mints_a_fresh_valid_join_string_and_persists_the_name() {
        let _home = isolated_config_home();
        let mut state = AppState::default();

        let a = state.create_room("relay.example:4433", "Movie Night").unwrap();
        let b = state.create_room("relay.example:4433", "Game Night").unwrap();

        assert_eq!(a.room_name, "Movie Night");
        assert_ne!(a.join_string, b.join_string, "every created room must get its own id/key");
        let parsed = crate::net::parse_join_string(&a.join_string).expect("minted join string must parse");
        assert_eq!(parsed.server, "relay.example:4433");
        let parsed_b = crate::net::parse_join_string(&b.join_string).unwrap();
        assert_ne!(parsed.room_id, parsed_b.room_id);
        assert_ne!(parsed.room_key, parsed_b.room_key);

        // Pre-seeded in memory *and* on disk.
        assert_eq!(state.room_store.name_for(&a.join_string), Some("Movie Night"));
        let reloaded = RoomStore::load().unwrap();
        assert_eq!(reloaded.rooms.len(), 2);
        assert_eq!(reloaded.name_for(&b.join_string), Some("Game Night"));
        assert!(state.room_store_result().is_ok());
    }

    #[tokio::test]
    async fn create_room_trims_and_rejects_blank_input_without_touching_the_store() {
        let _home = isolated_config_home();
        let mut state = AppState::default();

        assert_eq!(state.create_room("   ", "Movie Night").unwrap_err(), "server address must not be empty");
        assert_eq!(state.create_room("relay:4433", " \t").unwrap_err(), "room name must not be empty");
        assert!(state.room_store.rooms.is_empty());

        let link = state.create_room("  relay:4433 ", "  Movie Night  ").unwrap();
        assert_eq!(link.room_name, "Movie Night");
        assert_eq!(crate::net::parse_join_string(&link.join_string).unwrap().server, "relay:4433");
    }

    /// The reported bug at the lib level: forgetting a saved room must remove it
    /// from the on-disk store immediately, not just from memory, so a later
    /// `RoomStore::load()` doesn't resurrect it.
    #[tokio::test]
    async fn forget_room_persists_the_removal_to_disk() {
        let _home = isolated_config_home();
        let mut store = RoomStore::default();
        store.upsert("Movie Night".to_string(), "server|00112233|deadbeef".to_string());
        store.upsert("Gaming Night".to_string(), "server|44556677|c0ffee".to_string());
        store.save().unwrap();

        let mut state = AppState::default();
        assert_eq!(state.room_store.rooms.len(), 2);

        state.forget_room("server|00112233|deadbeef").expect("forget should succeed");

        assert_eq!(state.room_store.rooms.len(), 1);
        assert_eq!(state.room_store.rooms[0].name, "Gaming Night");
        let reloaded = RoomStore::load().unwrap();
        assert_eq!(reloaded.rooms.len(), 1, "deletion must be persisted to disk immediately");
        assert_eq!(reloaded.rooms[0].name, "Gaming Night");
    }

    fn room_link(id: u8, hash: Option<&[u8]>) -> String {
        let hash = hash.map(crate::protocol::cert_hash_of_der);
        crate::net::format_join_string_with_hash("192.0.2.1:4433", &[id; 16], &[0x42u8; 32], hash.as_ref())
    }

    /// Bug #31 at the `AppState` level: the same link joined under two names
    /// is one saved room (the second name wins), and rename / forget /
    /// auto-connect then act on that one entry, persisted to disk.
    #[tokio::test(start_paused = true)]
    async fn one_saved_room_per_link_across_join_rename_autoconnect_and_forget() {
        let _home = isolated_config_home();
        let mut state = AppState::default();
        let js = room_link(1, None);

        state.join_room(js.clone(), String::new(), "MovieNight".to_string()).unwrap();
        state.cancel_connect();
        state.join_room(js.clone(), String::new(), "Films".to_string()).unwrap();
        state.cancel_connect();
        assert_eq!(state.room_store.rooms.len(), 1);
        assert_eq!(state.room_store.rooms[0].name, "Films");

        // A rejoin with a fresh cert hash and no typed name: same entry, same
        // name, newest link stored.
        let rotated = room_link(1, Some(b"rotated cert"));
        state.join_room(rotated.clone(), String::new(), String::new()).unwrap();
        state.cancel_connect();
        assert_eq!(state.room_store.rooms.len(), 1);
        assert_eq!(state.room_store.rooms[0].name, "Films");
        assert_eq!(state.room_store.rooms[0].join_string, rotated);

        // A second, unrelated room so "exactly one" is observable.
        state.rename_saved_room(&room_link(2, None), "Other".to_string());
        assert_eq!(state.room_store.rooms.len(), 2);

        // Any spelling of the link addresses the same single entry.
        state.rename_saved_room(&js, "Cinema".to_string());
        state.set_room_auto_connect(&js, true);
        assert_eq!(state.room_store.rooms[0].name, "Cinema");
        assert!(state.room_store.rooms[0].auto_connect);
        assert_eq!(state.room_store.rooms[1].name, "Other");
        assert!(!state.room_store.rooms[1].auto_connect);
        let on_disk = RoomStore::load().unwrap();
        assert_eq!(on_disk.rooms.len(), 2);
        assert_eq!(on_disk.rooms[0].name, "Cinema");

        state.forget_room(&js).unwrap();
        assert_eq!(state.room_store.rooms.len(), 1);
        assert_eq!(state.room_store.rooms[0].name, "Other");
        assert_eq!(RoomStore::load().unwrap().rooms.len(), 1, "forget must be persisted");
    }

    /// The `RoomKey` guarantee (key = stored join string): joining, renaming
    /// or re-joining under another name never changes it.
    #[tokio::test(start_paused = true)]
    async fn saved_room_key_is_stable_across_name_only_changes() {
        let _home = isolated_config_home();
        let mut state = AppState::default();
        let js = room_link(1, Some(b"cert"));

        state.join_room(js.clone(), String::new(), "A".to_string()).unwrap();
        state.cancel_connect();
        state.join_room(js.clone(), String::new(), "B".to_string()).unwrap();
        state.cancel_connect();
        state.rename_saved_room(&js, "C".to_string());

        assert_eq!(state.room_store.rooms.len(), 1);
        assert_eq!(state.room_store.rooms[0].join_string, js);
        assert_eq!(state.room_store.rooms[0].name, "C");
    }

    /// A `rooms.toml` that already has duplicates is merged on load and the
    /// cleaned file is written back.
    #[tokio::test]
    async fn loading_a_rooms_file_with_duplicates_merges_and_saves_them() {
        let _home = isolated_config_home();
        let old = room_link(1, Some(b"old"));
        let new = room_link(1, Some(b"new"));
        let store = RoomStore {
            rooms: vec![
                crate::rooms::SavedRoom { name: "MovieNight".to_string(), join_string: old, auto_connect: true },
                crate::rooms::SavedRoom { name: "Films".to_string(), join_string: new.clone(), auto_connect: false },
            ],
        };
        store.save().unwrap();

        let state = AppState::default();
        assert_eq!(state.room_store.rooms.len(), 1);
        assert_eq!(state.room_store.rooms[0].name, "Films");
        assert_eq!(state.room_store.rooms[0].join_string, new);
        assert!(state.room_store.rooms[0].auto_connect);

        // Written back: a raw read of the file sees one entry.
        let raw = std::fs::read_to_string(RoomStore::config_path()).unwrap();
        assert_eq!(raw.matches("[[rooms]]").count(), 1);
    }

    #[tokio::test]
    async fn forget_room_on_an_unknown_join_string_is_a_harmless_ok() {
        let _home = isolated_config_home();
        let mut state = AppState::default();
        state.room_store.upsert("Keep".to_string(), "a|b|c".to_string());
        assert!(state.forget_room("x|y|z").is_ok());
        assert_eq!(state.room_store.rooms.len(), 1);
    }

    /// A failed store save is sticky: reported by `forget_room` and by
    /// `room_store_result` for every later call (behaviour carried over from
    /// the command layer).
    #[tokio::test]
    async fn room_store_errors_are_sticky_and_reported() {
        let _home = isolated_config_home();
        let mut state = AppState::default();
        assert!(state.room_store_result().is_ok());
        state.room_store_error = Some("disk full".to_string());
        assert_eq!(state.room_store_result().unwrap_err(), "disk full");
        assert_eq!(state.forget_room("a|b|c").unwrap_err(), "disk full");
    }

    /// `join_room` pre-seeds the typed name *before* connecting, applies a
    /// non-blank display name, and kicks off the connect.
    #[tokio::test(start_paused = true)]
    async fn join_room_seeds_the_name_and_display_name_then_starts_connecting() {
        let _home = isolated_config_home();
        let mut state = AppState::default();

        state.join_room("not-a-valid-join-string".to_string(), "Alice".to_string(), "  Movie Night  ".to_string()).unwrap();

        assert_eq!(state.display_name, "Alice");
        assert_eq!(state.room_store.name_for("not-a-valid-join-string"), Some("Movie Night"));
        assert_eq!(RoomStore::load().unwrap().rooms.len(), 1, "the name must be saved to disk too");
        assert!(state.pending_connect.is_some());
        assert!(state.pending_connect.as_ref().unwrap().from_add_card);
        state.cancel_connect();

        // Blank display name and blank room name change nothing.
        state.join_room("other".to_string(), "  ".to_string(), "   ".to_string()).unwrap();
        assert_eq!(state.display_name, "Alice");
        assert_eq!(state.room_store.rooms.len(), 1, "a blank room name must not create a saved room");
        state.cancel_connect();
    }

    #[tokio::test(start_paused = true)]
    async fn create_and_join_room_saves_the_room_and_connects_without_touching_the_display_name() {
        let _home = isolated_config_home();
        let mut state = AppState::default();
        let display_name = state.display_name.clone();

        state.create_and_join_room("192.0.2.1:4433", "Movie Night").unwrap();

        assert_eq!(state.display_name, display_name);
        assert_eq!(state.room_store.rooms.len(), 1);
        assert_eq!(state.room_store.rooms[0].name, "Movie Night");
        let pending = state.pending_connect.as_ref().expect("create must start connecting");
        assert_eq!(pending.join_string, state.room_store.rooms[0].join_string);
        state.cancel_connect();

        // Validation errors surface before anything is saved or connected.
        assert!(state.create_and_join_room("", "x").is_err());
        assert!(state.pending_connect.is_none());
        assert_eq!(state.room_store.rooms.len(), 1);
    }

    // ---- persisted settings ----

    #[tokio::test]
    async fn display_name_is_trimmed_and_persisted() {
        let _home = isolated_config_home();
        let mut state = AppState::default();
        state.set_display_name("  Mo  ");
        assert_eq!(state.display_name, "Mo");
        assert_eq!(AppSettings::load().display_name, "Mo");
    }

    #[tokio::test]
    async fn jitter_buffer_and_catch_up_speed_are_clamped_then_persisted() {
        let _home = isolated_config_home();
        let mut state = AppState::default();

        assert_eq!(state.set_max_jitter_buffer_ms(99_999), 5000);
        assert_eq!(state.max_jitter_buffer_ms, 5000);
        assert_eq!(AppSettings::load().max_jitter_buffer_ms, 5000);
        assert_eq!(state.set_max_jitter_buffer_ms(1500), 1500);

        assert_eq!(state.set_catch_up_speed_percent(0), 5);
        assert_eq!(state.set_catch_up_speed_percent(50), 10);
        assert_eq!(state.catch_up_speed_percent, 10);
        let saved = AppSettings::load();
        assert_eq!(saved.catch_up_speed_percent, 10);
        assert_eq!(saved.max_jitter_buffer_ms, 1500, "saving one setting must not clobber its siblings");
    }

    #[tokio::test]
    async fn bandwidth_total_and_floor_persist_together_with_the_mode() {
        let _home = isolated_config_home();
        let mut state = AppState::default();
        state.bandwidth_mode = BandwidthMode::Absolute;

        state.set_bandwidth_total(12_345);
        state.set_bandwidth_floor(750);

        assert_eq!((state.absolute_total_kbps, state.destination_floor_kbps), (12_345, 750));
        let saved = AppSettings::load();
        assert_eq!(saved.absolute_total_kbps, 12_345);
        assert_eq!(saved.destination_floor_kbps, 750);
        assert_eq!(saved.bandwidth_mode, BandwidthMode::Absolute);
    }

    #[tokio::test]
    async fn language_and_presentation_mode_are_persisted() {
        let _home = isolated_config_home();
        let mut state = AppState::default();
        // "en" only: `rust_i18n`'s locale is process-global and other tests
        // assert on English messages.
        state.set_language("en");
        state.set_presentation_mode(PresentationMode::Mailbox);
        assert_eq!(state.language, "en");
        assert_eq!(state.presentation_mode, PresentationMode::Mailbox);
        let saved = AppSettings::load();
        assert_eq!(saved.language, "en");
        assert_eq!(saved.presentation_mode, PresentationMode::Mailbox);
    }

    // ---- start-a-stream form ----

    fn sample_defaults() -> StartStreamDefaults {
        StartStreamDefaults {
            framerate: 120,
            bitrate_kbps: 15_000,
            tune: capture::TuneMode::LowLatency,
            quality_level: capture::QualityLevel::Quality,
            codec: capture::VideoCodec::Av1,
        }
    }

    #[tokio::test]
    async fn start_stream_defaults_round_trip_without_clobbering_siblings() {
        let _home = isolated_config_home();
        let mut state = AppState::default();
        state.set_bandwidth_total(31_000);

        assert_eq!(AppState::start_stream_defaults(), StartStreamDefaults::default());
        AppState::set_start_stream_defaults(sample_defaults());
        assert_eq!(AppState::start_stream_defaults(), sample_defaults());
        assert_eq!(AppSettings::load().absolute_total_kbps, 31_000);
    }

    /// Staging a share must open with the *saved* last-used values, and must
    /// always reset the (never-persisted) GOP so a previous Gaming preset
    /// can't bleed into the next card.
    #[tokio::test]
    async fn apply_start_stream_defaults_loads_saved_values_and_resets_gop() {
        let _home = isolated_config_home();
        AppState::set_start_stream_defaults(sample_defaults());
        let mut state = AppState::default();
        state.gop_millis = 750;
        state.framerate = 30;

        state.apply_start_stream_defaults();

        assert_eq!(state.current_start_stream_settings(), sample_defaults());
        assert_eq!(state.gop_millis, capture::DEFAULT_GOP_MILLIS);
    }

    #[tokio::test]
    async fn save_current_start_stream_defaults_persists_the_live_fields() {
        let _home = isolated_config_home();
        let mut state = AppState::default();
        state.framerate = 144;
        state.bitrate_kbps = 22_000;
        state.codec = capture::VideoCodec::H265;
        state.gop_millis = 750;

        state.save_current_start_stream_defaults();

        let saved = AppState::start_stream_defaults();
        assert_eq!(saved, state.current_start_stream_settings());
        assert_eq!((saved.framerate, saved.bitrate_kbps, saved.codec), (144, 22_000, capture::VideoCodec::H265));
    }

    #[tokio::test]
    async fn supported_codecs_always_offers_h264_first() {
        let codecs = AppState::supported_codecs();
        assert_eq!(codecs.first(), Some(&capture::VideoCodec::H264));
    }

    // ---- cancelling a capture whose start is still in flight ----

    #[tokio::test]
    async fn stopping_a_starting_capture_cancels_it_and_never_goes_live() {
        let _home = isolated_config_home();
        let mut state = AppState::default();
        let (capture_id, _dest) = insert_destination(&mut state);
        // Replace the inert receiver with one we control (the "picker").
        let (tx, rx) = oneshot::channel();
        state.captures.get_mut(&capture_id).unwrap().status = ShareStatus::Starting(rx);

        state.stop_share(capture_id);
        assert!(matches!(state.captures[&capture_id].status, ShareStatus::Stopping(_)));

        // The user picks nothing / the start fails after the discard.
        assert!(tx.send(Err("picker closed".to_string())).is_ok());
        for _ in 0..100 {
            state.poll_capture_states();
            if !state.captures.contains_key(&capture_id) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(!state.captures.contains_key(&capture_id), "cancelled capture must be removed");
        assert!(state.share_error.is_none(), "a cancelled start is not an error");
        assert!(state.pending_destinations.is_empty());
    }

    /// The user dismissing the OS screen picker (nobody pressed Stop) must
    /// drop the capture silently — no `share_error`, so no error toast.
    #[tokio::test]
    async fn a_dismissed_picker_removes_the_capture_without_an_error() {
        let _home = isolated_config_home();
        let mut state = AppState::default();
        let (capture_id, _dest) = insert_destination(&mut state);
        let (tx, rx) = oneshot::channel();
        state.captures.get_mut(&capture_id).unwrap().status = ShareStatus::Starting(rx);

        let cancelled = anyhow::Error::new(capture::PickerCancelled);
        assert!(tx.send(Err(start_error_string(&cancelled))).is_ok());
        state.poll_capture_states();

        assert!(!state.captures.contains_key(&capture_id), "a cancelled capture must be removed");
        assert!(state.share_error.is_none(), "dismissing the picker is not an error: {:?}", state.share_error);
    }

    /// …while a real failure still reaches the user, wrapped in context or not.
    #[tokio::test]
    async fn a_real_start_failure_still_surfaces_as_a_share_error() {
        let _home = isolated_config_home();
        let mut state = AppState::default();
        let (capture_id, _dest) = insert_destination(&mut state);
        let (tx, rx) = oneshot::channel();
        state.captures.get_mut(&capture_id).unwrap().status = ShareStatus::Starting(rx);

        let failure = anyhow::anyhow!("the portal exploded").context("screencast portal 'Start' request failed");
        assert!(tx.send(Err(start_error_string(&failure))).is_ok());
        state.poll_capture_states();

        assert!(!state.captures.contains_key(&capture_id));
        let shown = state.share_error.expect("a real failure must be reported");
        assert!(shown.contains("the portal exploded"), "{shown}");
    }

    /// A cancel that has been wrapped in `.context(..)` further up the stack is
    /// still a cancel.
    #[test]
    fn a_context_wrapped_cancel_is_still_recognised() {
        let wrapped = anyhow::Error::new(capture::PickerCancelled).context("starting capture");
        assert_eq!(start_error_string(&wrapped), SHARE_START_CANCELLED);
        assert_ne!(start_error_string(&anyhow::anyhow!("boom")), SHARE_START_CANCELLED);
    }

    #[tokio::test]
    async fn stopping_a_starting_capture_whose_start_task_died_is_reaped() {
        let _home = isolated_config_home();
        let mut state = AppState::default();
        let (capture_id, _dest) = insert_destination(&mut state); // sender already dropped
        state.stop_share(capture_id);
        for _ in 0..100 {
            state.poll_capture_states();
            if !state.captures.contains_key(&capture_id) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(!state.captures.contains_key(&capture_id));
        assert!(state.share_error.is_none());
    }

    // ---- destination edits ----

    #[tokio::test]
    async fn edit_destination_replaces_the_whole_edit_buffer() {
        let _home = isolated_config_home();
        let mut state = AppState::default();
        let (capture_id, destination_id) = insert_destination(&mut state);
        let edit = DestEdit {
            scale_percent: 50,
            framerate: 144,
            bitrate_kbps: 12_000,
            tune: capture::TuneMode::LowLatency,
            quality_level: capture::QualityLevel::Performance,
            gop_millis: 750,
            codec: capture::VideoCodec::Av1,
        };

        state.edit_destination(destination_id, edit);

        let dest = &state.captures[&capture_id].destinations[&destination_id];
        assert_eq!(dest.edit, edit);
        // Not active yet, so nothing was applied to an encoder or reported.
        assert!(state.share_error.is_none());
        assert_eq!(dest.params, video_params(), "live params only change when the capture applies the edit");
    }

    #[tokio::test]
    async fn edit_destination_on_a_missing_destination_is_a_no_op() {
        let _home = isolated_config_home();
        let mut state = AppState::default();
        let (_, destination_id) = insert_destination(&mut state);
        let edit = Destination::edit_from(video_params());
        state.edit_destination(DestinationId(destination_id.0 + 99), edit);
    }

    // ---- bandwidth display ----

    fn spec(id: u32, floor_kbps: u32) -> bandwidth::DestSpec {
        bandwidth::DestSpec { id: DestId(id), width: 1920, height: 1080, fps: 60, floor_kbps, ceiling_kbps: Some(MAX_BITRATE_KBPS) }
    }

    #[tokio::test]
    async fn allocated_kbps_reads_the_last_allocation() {
        let _home = isolated_config_home();
        let mut state = AppState::default();
        assert_eq!(state.allocated_kbps(DestinationId(7)), None, "nothing allocated yet");
        assert_eq!(state.bandwidth_overshoot_kbps(), 0);

        let input = bandwidth::AllocInput { total_kbps: 10_000, audio_reserved_kbps: 0, destinations: vec![spec(7, 500), spec(8, 500)] };
        state.bandwidth.tick(Instant::now(), &input).expect("first tick is never rate limited");

        assert_eq!(state.allocated_kbps(DestinationId(7)), Some(5000));
        assert_eq!(state.allocated_kbps(DestinationId(8)), Some(5000));
        assert_eq!(state.allocated_kbps(DestinationId(9)), None, "unknown destination");
        assert_eq!(state.bandwidth_overshoot_kbps(), 0);
    }

    #[tokio::test]
    async fn bandwidth_overshoot_is_reported_when_floors_do_not_fit() {
        let _home = isolated_config_home();
        let mut state = AppState::default();
        let input = bandwidth::AllocInput { total_kbps: 10_000, audio_reserved_kbps: 0, destinations: vec![spec(1, 6000), spec(2, 6000)] };
        state.bandwidth.tick(Instant::now(), &input).unwrap();
        assert_eq!(state.bandwidth_overshoot_kbps(), 2000);
    }

    // ---- viewer-side audio ----

    #[test]
    fn effective_gain_is_zero_while_muted_otherwise_percent_over_100() {
        assert_eq!(effective_gain(100, false), 1.0);
        assert_eq!(effective_gain(200, false), 2.0);
        assert_eq!(effective_gain(40, false), 0.4);
        assert_eq!(effective_gain(40, true), 0.0, "muted silences the decoder regardless of the slider value");
    }

    #[test]
    fn volume_state_defaults_to_full_and_unmuted() {
        assert_eq!(VolumeState::default(), VolumeState { volume_percent: 100, muted: false });
    }

    #[test]
    fn sibling_audio_stream_finds_the_owners_audio_only() {
        let streams = [stream(1, 0, StreamKind::Video), stream(2, 3, StreamKind::Audio), stream(1, 1, StreamKind::Audio)];
        assert_eq!(sibling_audio_stream_in(&streams, 1), Some(1));
        assert_eq!(sibling_audio_stream_in(&streams, 2), Some(3));
        assert_eq!(sibling_audio_stream_in(&streams, 9), None);
        assert_eq!(sibling_audio_stream_in(&[stream(1, 0, StreamKind::Video)], 1), None, "video-only sender has no sibling");
    }

    /// The volume control is handed either the audio stream's own id (a direct
    /// hit — the standalone "Listen" case) or the *video* stream id the tile
    /// was rendered from (resolved to the owner's sibling audio stream).
    #[test]
    fn volume_calls_accept_the_audio_id_or_the_video_id() {
        let streams = [stream(1, 0, StreamKind::Video), stream(1, 1, StreamKind::Audio)];
        let watching_audio_1 = |id: StreamId| id == 1;

        // Direct hit on the audio watch.
        assert_eq!(resolve_audio_stream_id(&streams, 1, 1, watching_audio_1), Some(1));
        // Video id -> sibling audio stream.
        assert_eq!(resolve_audio_stream_id(&streams, 1, 0, watching_audio_1), Some(1));
        // A direct hit wins even when the owner also has a (different) sibling.
        let two_audio = [stream(1, 1, StreamKind::Audio), stream(1, 2, StreamKind::Audio)];
        assert_eq!(resolve_audio_stream_id(&two_audio, 1, 2, |id| id == 2), Some(2));
        // No watch and no announced audio stream: nothing to resolve.
        assert_eq!(resolve_audio_stream_id(&[stream(1, 0, StreamKind::Video)], 1, 0, |_| false), None);
    }

    #[tokio::test]
    async fn volume_calls_for_an_unknown_room_default_and_do_not_panic() {
        let _home = isolated_config_home();
        let mut state = AppState::default();
        state.set_volume(RoomHandle(42), 1, 0, 150, true);
        assert_eq!(state.volume(RoomHandle(42), 1, 0), VolumeState { volume_percent: 100, muted: false });
    }

    #[test]
    fn level_tap_serial_is_picked_from_a_fresh_listing() {
        let src = |id, serial| AudioSource { id, name: format!("app{id}"), serial };
        let sources = vec![src(1, Some(100)), src(2, None)];
        assert_eq!(pick_level_tap_serial(sources.clone(), 1), Ok(100));
        assert_eq!(pick_level_tap_serial(sources.clone(), 3), Err("audio source is no longer listed".to_string()));
        assert_eq!(
            pick_level_tap_serial(sources, 2),
            Err("'app2' has no PipeWire object.serial to tap yet".to_string())
        );
    }

    #[tokio::test]
    async fn unknown_audio_level_taps_are_inert() {
        let _home = isolated_config_home();
        let mut state = AppState::default();
        assert_eq!(state.audio_level_tap_level(5), None);
        state.stop_audio_level_tap(5);
    }
}

#[cfg(test)]
mod reconnect_tests {
    use super::test_support::*;
    use super::*;

    fn member(conn_id: ConnId, name: &str) -> Member {
        Member { conn_id, display_name: name.to_string(), stream_count: 0 }
    }

    fn stream(owner: ConnId, stream_id: StreamId, kind: StreamKind, label: &str) -> StreamInfo {
        StreamInfo { owner, stream_id, kind, label: label.to_string(), width: 0, height: 0 }
    }

    fn wanted(name: &str, kind: StreamKind, label: &str) -> WantedStream {
        WantedStream { owner_name: name.to_string(), kind, label: label.to_string(), volume_percent: None }
    }

    // ---- policy ----------------------------------------------------------

    #[test]
    fn backoff_is_immediate_then_doubles_from_half_a_second_to_the_cap() {
        let ms = Duration::from_millis;
        let schedule: Vec<Duration> = (0..9).map(backoff_delay).collect();
        assert_eq!(schedule, vec![ms(0), ms(500), ms(1000), ms(2000), ms(4000), ms(8000), ms(15_000), ms(15_000), ms(15_000)]);
        // Far past any realistic outage: no overflow, still capped.
        assert_eq!(backoff_delay(u32::MAX), RECONNECT_BACKOFF_CAP);
    }

    #[test]
    fn close_causes_map_to_retry_policy() {
        // Relay restart: immediate first retry, then ordinary backoff.
        assert_eq!(retry_delay(CloseCause::RelayShutdown, 0), Some(Duration::ZERO));
        assert_eq!(retry_delay(CloseCause::RelayShutdown, 2), Some(Duration::from_secs(1)));
        // Transport drops back off the same way.
        assert_eq!(retry_delay(CloseCause::Other, 0), Some(Duration::ZERO));
        assert_eq!(retry_delay(CloseCause::Other, 3), Some(Duration::from_secs(2)));
        // Room full: never faster than the slow floor, the long backoff once it exceeds it.
        assert_eq!(retry_delay(CloseCause::RoomFull, 0), Some(ROOM_FULL_RETRY));
        assert_eq!(retry_delay(CloseCause::RoomFull, 1), Some(ROOM_FULL_RETRY));
        assert_eq!(retry_delay(CloseCause::RoomFull, 6), Some(RECONNECT_BACKOFF_CAP));
        // Malformed join: give up.
        assert_eq!(retry_delay(CloseCause::Malformed, 0), None);
        assert_eq!(retry_delay(CloseCause::Malformed, 5), None);
    }

    // ---- re-matching watches ---------------------------------------------

    #[test]
    fn a_watch_is_rematched_by_owner_name_kind_and_label_not_by_ids() {
        let members = [member(10, "alice"), member(11, "bob")];
        let streams = [stream(10, 3, StreamKind::Video, "Screen"), stream(11, 0, StreamKind::Video, "Screen")];
        let w = wanted("bob", StreamKind::Video, "Screen");
        assert_eq!(match_wanted(&w, 99, &members, &streams, |_, _| false), Some((11, 0)));
        // A different label, kind or name is a different stream.
        assert_eq!(match_wanted(&wanted("bob", StreamKind::Video, "Other"), 99, &members, &streams, |_, _| false), None);
        assert_eq!(match_wanted(&wanted("bob", StreamKind::Audio, "Screen"), 99, &members, &streams, |_, _| false), None);
        assert_eq!(match_wanted(&wanted("carol", StreamKind::Video, "Screen"), 99, &members, &streams, |_, _| false), None);
        // Our own streams never match.
        assert_eq!(match_wanted(&w, 11, &members, &streams, |_, _| false), None);
    }

    #[test]
    fn two_wanted_streams_with_the_same_key_pick_different_streams() {
        let members = [member(10, "alice")];
        let streams = [stream(10, 5, StreamKind::Video, "Screen"), stream(10, 2, StreamKind::Video, "Screen")];
        let w = wanted("alice", StreamKind::Video, "Screen");
        // Lowest stream id first, then the next one once that is taken.
        assert_eq!(match_wanted(&w, 99, &members, &streams, |_, _| false), Some((10, 2)));
        assert_eq!(match_wanted(&w, 99, &members, &streams, |o, s| (o, s) == (10, 2)), Some((10, 5)));
        assert_eq!(match_wanted(&w, 99, &members, &streams, |_, _| true), None);
    }

    // ---- the reconnect state machine, without a network --------------------

    fn saved_state(tag: &str) -> (IsolatedConfigHome, AppState, String) {
        let home = isolated_config_home();
        let mut state = AppState::default();
        let js = state.create_room("127.0.0.1:9", tag).expect("create_room").join_string;
        (home, state, js)
    }

    fn fake_entry(handle: RoomHandle, join_string: &str) -> ReconnectState {
        ReconnectState {
            join_string: join_string.to_string(),
            handle,
            attempts: 0,
            next_retry: Some(Instant::now()),
            last_error: Some("dropped".to_string()),
            cause: CloseCause::Other,
            since: Instant::now(),
            members: vec![member(1, "alice")],
            streams: Vec::new(),
            shares: Vec::new(),
            wanted: Vec::new(),
            was_active_room: false,
            cancelled: false,
        }
    }

    fn connect_err(cause: CloseCause) -> ConnectResult {
        Err(ConnectError { message: format!("{cause:?}"), cause })
    }

    #[tokio::test]
    async fn a_waiting_room_reads_as_reconnecting_and_keeps_the_ui_ticking() {
        let (_home, mut state, js) = saved_state("waiting");
        let mut entry = fake_entry(RoomHandle(7), &js);
        entry.next_retry = Some(Instant::now() + Duration::from_secs(60));
        state.reconnects.insert(RoomHandle(7), entry);
        state.refresh_status();
        assert!(state.is_reconnecting(&js), "a room between attempts must not fall back to Disconnected");
        assert!(matches!(state.status, Status::Connecting), "the UI only repaints on Connecting");
        let info = state.reconnect_info(&js).expect("info");
        assert_eq!(info.attempts, 0);
        assert!(info.retry_in(Instant::now()).unwrap() > Duration::from_secs(50));
        assert_eq!(info.members.len(), 1, "the last-known roster is exposed");
    }

    #[tokio::test]
    async fn an_attempt_starts_when_due_and_a_failure_schedules_the_next_one_by_backoff() {
        let (_home, mut state, js) = saved_state("due");
        state.reconnect_attempt_timeout = Duration::from_millis(150);
        state.reconnects.insert(RoomHandle(3), fake_entry(RoomHandle(3), &js));

        state.poll_reconnects();
        let pending = state.pending_connect.as_ref().expect("a due entry starts an attempt");
        assert_eq!(pending.reconnect, Some(RoomHandle(3)));
        assert!(!pending.from_add_card);
        assert_eq!(state.reconnects[&RoomHandle(3)].attempts, 1);
        assert!(state.reconnects[&RoomHandle(3)].attempt_in_flight());

        // Nothing listens on port 9, so the attempt times out and counts as a failure.
        let outcome = loop {
            tokio::time::sleep(Duration::from_millis(20)).await;
            match state.poll_connect_result() {
                ConnectOutcome::None if state.pending_connect.is_some() => continue,
                other => break other,
            }
        };
        assert!(matches!(outcome, ConnectOutcome::None), "a failed attempt is not a UI-visible failure");
        let entry = &state.reconnects[&RoomHandle(3)];
        let wait = entry.retry_in(Instant::now()).expect("next attempt scheduled");
        // 1 failed attempt -> 0.5 s.
        assert!(wait <= Duration::from_millis(500) && wait > Duration::from_millis(300), "wait = {wait:?}");
        assert!(entry.last_error.is_some());
        assert!(state.is_reconnecting(&js));
    }

    #[tokio::test]
    async fn room_full_keeps_retrying_slowly_and_says_so() {
        let (_home, mut state, js) = saved_state("full");
        let mut entry = fake_entry(RoomHandle(1), &js);
        entry.attempts = 1;
        entry.next_retry = None;
        state.reconnects.insert(RoomHandle(1), entry);
        let outcome = state.finish_reconnect_attempt(RoomHandle(1), connect_err(CloseCause::RoomFull));
        assert!(matches!(outcome, ConnectOutcome::None));
        let info = state.reconnect_info(&js).expect("still reconnecting");
        assert!(info.room_full());
        assert!(info.retry_in(Instant::now()).unwrap() >= Duration::from_secs(4));
        assert!(info.last_error.as_deref().unwrap().contains("RoomFull"));
    }

    #[tokio::test]
    async fn a_malformed_join_stops_and_surfaces_the_error() {
        let (_home, mut state, js) = saved_state("malformed");
        let mut entry = fake_entry(RoomHandle(1), &js);
        entry.attempts = 1;
        entry.next_retry = None;
        state.reconnects.insert(RoomHandle(1), entry);
        let outcome = state.finish_reconnect_attempt(RoomHandle(1), connect_err(CloseCause::Malformed));
        match outcome {
            ConnectOutcome::Failed { error, from_add_card } => {
                assert!(error.contains("Malformed"));
                assert!(!from_add_card);
            }
            _ => panic!("a terminal failure must be reported"),
        }
        assert!(!state.is_reconnecting(&js));
        assert!(state.reconnects.is_empty());
        assert!(matches!(state.status, Status::Error(_)), "status = surfaced error, not Connecting");
    }

    #[tokio::test]
    async fn cancel_reconnect_names_the_room_and_leaves_the_others_alone() {
        let (_home, mut state, js_a) = saved_state("cancel-a");
        let js_b = state.create_room("127.0.0.1:9", "cancel-b").unwrap().join_string;
        for (h, js) in [(1, &js_a), (2, &js_b)] {
            let mut e = fake_entry(RoomHandle(h), js);
            e.next_retry = Some(Instant::now() + Duration::from_secs(30));
            state.reconnects.insert(RoomHandle(h), e);
        }
        state.cancel_reconnect(&js_a);
        assert!(!state.is_reconnecting(&js_a));
        assert!(state.is_reconnecting(&js_b));
        state.cancel_reconnect("not|a|room");
        assert_eq!(state.reconnects.len(), 1);
    }

    #[tokio::test]
    async fn forgetting_a_room_ends_its_reconnect() {
        let (_home, mut state, js) = saved_state("forget");
        let mut e = fake_entry(RoomHandle(1), &js);
        e.next_retry = Some(Instant::now() + Duration::from_secs(30));
        state.reconnects.insert(RoomHandle(1), e);
        state.forget_room(&js).unwrap();
        assert!(state.reconnects.is_empty());
        assert!(!state.is_reconnecting(&js));
    }

    #[tokio::test]
    async fn a_room_that_left_the_saved_list_is_dropped_by_the_poll() {
        let (_home, mut state, js) = saved_state("delisted");
        state.reconnects.insert(RoomHandle(1), fake_entry(RoomHandle(1), &js));
        state.room_store.remove(&js);
        state.poll_reconnects();
        assert!(state.reconnects.is_empty());
        assert!(state.pending_connect.is_none(), "no attempt for a room that is not in the list");
    }

    /// The current UI's "Retry now" is `CancelConnect` then `Connect(key)` in one
    /// frame, and its "Cancel" is `CancelConnect` alone. Both must work without
    /// UI changes for the single-room case.
    #[tokio::test]
    async fn legacy_cancel_then_connect_is_retry_now_and_cancel_alone_cancels() {
        let (_home, mut state, js) = saved_state("legacy");
        state.reconnect_attempt_timeout = Duration::from_secs(5);
        let mut e = fake_entry(RoomHandle(1), &js);
        e.next_retry = Some(Instant::now() + Duration::from_secs(30));
        state.reconnects.insert(RoomHandle(1), e);

        // Retry now: cancel, then connect, then the next poll.
        state.cancel_connect();
        assert!(state.reconnects[&RoomHandle(1)].cancelled);
        state.start_connect(js.clone(), false).unwrap();
        assert!(!state.reconnects[&RoomHandle(1)].cancelled, "connecting revives the parked reconnect");
        let pending = state.pending_connect.as_ref().expect("retry now starts an attempt at once");
        assert_eq!(pending.reconnect, Some(RoomHandle(1)), "and it stays an automatic reconnect");
        state.poll_reconnects();
        assert!(state.is_reconnecting(&js));

        // Cancel alone: the in-flight attempt is aborted and the entry dropped at the next poll.
        state.cancel_connect();
        assert!(state.pending_connect.is_none());
        assert!(state.is_reconnecting(&js), "still listed until the next poll, so a Connect can revive it");
        state.poll_reconnects();
        assert!(!state.is_reconnecting(&js));
        assert!(matches!(state.status, Status::Idle));
    }

    #[tokio::test]
    async fn legacy_cancel_never_guesses_between_several_reconnecting_rooms() {
        let (_home, mut state, js_a) = saved_state("legacy-multi-a");
        let js_b = state.create_room("127.0.0.1:9", "legacy-multi-b").unwrap().join_string;
        for (h, js) in [(1, &js_a), (2, &js_b)] {
            let mut e = fake_entry(RoomHandle(h), js);
            e.next_retry = Some(Instant::now() + Duration::from_secs(30));
            state.reconnects.insert(RoomHandle(h), e);
        }
        state.cancel_connect();
        state.poll_reconnects();
        assert!(state.is_reconnecting(&js_a) && state.is_reconnecting(&js_b), "no room may be given up on by a key-less cancel");
        assert!(state.reconnects.values().all(|e| !e.cancelled));
    }

    #[tokio::test]
    async fn reconnect_now_replaces_a_hung_attempt_and_skips_the_wait() {
        let (_home, mut state, js) = saved_state("now");
        let mut e = fake_entry(RoomHandle(1), &js);
        e.next_retry = Some(Instant::now() + Duration::from_secs(30));
        state.reconnects.insert(RoomHandle(1), e);
        state.reconnect_now(&js);
        assert_eq!(state.reconnects[&RoomHandle(1)].attempts, 1);
        assert!(state.pending_connect.is_some());
        state.reconnect_now(&js);
        assert_eq!(state.reconnects[&RoomHandle(1)].attempts, 2, "a second Retry now restarts the attempt");
        state.cancel_reconnect(&js);
        assert!(state.pending_connect.is_none(), "cancelling aborts the attempt that belonged to the room");
    }

    #[tokio::test]
    async fn shutdown_stops_reconnecting_and_is_idempotent() {
        let (_home, mut state, js) = saved_state("shutdown");
        state.reconnects.insert(RoomHandle(1), fake_entry(RoomHandle(1), &js));
        state.shutdown(Duration::from_millis(500)).await;
        assert!(state.reconnects.is_empty());
        assert!(state.pending_connect.is_none());
        state.poll_reconnects();
        assert!(state.pending_connect.is_none());
        state.shutdown(Duration::from_millis(500)).await;
    }
}

/// Auto-reconnect and clean exit against a real relay (child process) and, for
/// the share tests, a real GStreamer capture (`videotestsrc`; the portal cannot
/// be driven unattended). All `#[ignore]`d like the rest of the end-to-end
/// suite:
///
/// ```text
/// cargo test -p simple-screensharing-client --lib -- --ignored session::reconnect_e2e:: --test-threads=1 --nocapture
/// ```
#[cfg(test)]
mod reconnect_e2e {
    use std::net::UdpSocket;
    use std::process::Command;
    use std::time::{Duration, Instant};

    use indexmap::IndexMap;

    use super::test_support::*;
    use super::*;
    use crate::capture::{self, DestKey};
    use crate::end_to_end_test::{ServerGuard, connect_with_retry, pick_free_udp_port, server_binary_path, spawn_relay_server};
    use crate::net::ClientEvent;
    use crate::protocol::PresenceEvent;

    const TEST_FPS: u32 = 30;

    /// One front-end frame for `state`, as `ui::App::poll` runs it.
    fn tick(state: &mut AppState, ctx: &egui::Context) {
        state.poll_auto_connect_queue();
        state.poll_connect_result();
        state.drain_events(ctx);
        state.poll_capture_states();
        state.poll_pending_destinations();
        state.poll_destination_edits();
    }

    /// Tick every state until `done` or the deadline; panics with `what`.
    async fn drive(states: &mut [&mut AppState], timeout: Duration, what: &str, mut done: impl FnMut(&mut [&mut AppState]) -> bool) -> Duration {
        let ctx = egui::Context::default();
        let started = Instant::now();
        while started.elapsed() < timeout {
            for state in states.iter_mut() {
                tick(state, &ctx);
            }
            if done(states) {
                return started.elapsed();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("{what}: not met within {timeout:?}");
    }

    /// Tick every state for `duration` (the "nothing must happen" wait).
    async fn run_for(states: &mut [&mut AppState], duration: Duration) {
        let ctx = egui::Context::default();
        let started = Instant::now();
        while started.elapsed() < duration {
            for state in states.iter_mut() {
                tick(state, &ctx);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Waits until the relay answers, so a test's first connect is not racing
    /// its start-up.
    async fn await_relay(join_string: &str) {
        let (probe, _snapshot, _events) = connect_with_retry(join_string, "probe").await.expect("relay never came up");
        probe.leave_and_close().await;
    }

    /// Stop the relay the way an operator does (SIGINT -> `relay::shutdown`, so
    /// connected clients get `CONNECTION_CLOSE 0x200`), wait for its port, and
    /// start a fresh one on the same port.
    async fn restart_relay(old: ServerGuard, port: u16) -> ServerGuard {
        let binary = server_binary_path().expect("relay binary");
        let _ = Command::new("pkill").args(["-INT", "-f", &format!("^{} {port}$", binary.display())]).status();
        let mut freed = false;
        for _ in 0..60 {
            if UdpSocket::bind(("127.0.0.1", port)).is_ok() {
                freed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(freed, "the relay did not exit on SIGINT");
        // Dropping the old guard runs `pkill -f "simple-screensharing-server <port>"`:
        // it must be gone before the new relay exists, or it would kill that one.
        drop(old);
        spawn_relay_server(port).expect("respawn relay")
    }

    fn frames_decoded(state: &AppState, room: RoomHandle, key: (ConnId, StreamId)) -> u64 {
        let session = &state.rooms[&room];
        let watches = session.watches.lock().unwrap();
        watches.get(&key).map(|w| w.decoder.lock().unwrap().frames().frames_decoded()).unwrap_or(0)
    }

    /// Put a test-source capture into `room` as the first destination of a new
    /// capture (what `start_video_share` does, minus the interactive portal).
    async fn share_test_source(state: &mut AppState, room: RoomHandle) -> CaptureSourceId {
        let capture_id = state.alloc_capture_id();
        let destination_id = state.alloc_destination_id();
        assert_eq!(destination_id, DestinationId(0), "the test source's DestKey(0) must match the first destination id");
        let session = state.rooms.get_mut(&room).expect("room");
        let stream_id = session.next_stream_id;
        session.next_stream_id += 1;
        session.destinations.push(destination_id);
        let client = session.client.clone();
        let fragment = format!("videotestsrc is-live=true pattern=ball ! video/x-raw,width=320,height=240,framerate={TEST_FPS}/1");
        let handle = capture::start_sharing_from_test_source(&client, stream_id, 2_000, TEST_FPS, &fragment)
            .await
            .expect("start_sharing_from_test_source");
        let runtime = handle.destination_runtime(DestKey(destination_id.0));
        let params = runtime.as_ref().expect("first destination").lock().unwrap().params();
        state.captures.insert(
            capture_id,
            LocalCapture {
                kind: StreamKind::Video,
                label: "test capture".to_string(),
                status: ShareStatus::Active(ShareHandle::Video(handle)),
                audio_for_capture: None,
                destinations: IndexMap::from([(
                    destination_id,
                    Destination { room, stream_id, params, runtime, pending: None, edit: Destination::edit_from(params), apply_rx: None },
                )]),
            },
        );
        capture_id
    }

    fn stream_of(state: &AppState, room: RoomHandle, owner_name: &str) -> Option<(ConnId, StreamId)> {
        let session = state.rooms.get(&room)?;
        let owner = session.members.iter().find(|m| m.display_name == owner_name)?.conn_id;
        session.streams.iter().find(|s| s.owner == owner && s.kind == StreamKind::Video).map(|s| (s.owner, s.stream_id))
    }

    /// The headline case: the relay restarts (SIGINT, `0x200`). The watcher
    /// rejoins by itself under the same handle, its watch resumes on the
    /// re-announced stream (new owner `ConnId`, found by name + kind + label),
    /// and the sharer's capture keeps running and re-announces into the new
    /// connection.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "spawns a real relay server child process (and a real GStreamer capture)"]
    async fn relay_restart_is_survived_the_watch_and_the_own_share_come_back() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        gstreamer::init().expect("gstreamer init");
        let _home = isolated_config_home();
        let port = pick_free_udp_port().expect("free port");
        let relay = spawn_relay_server(port).expect("relay");

        let mut watcher = AppState::default();
        let mut sharer = AppState::default();
        let join_string = watcher.create_room(&format!("127.0.0.1:{port}"), "restart").expect("create_room").join_string;
        await_relay(&join_string).await;
        watcher.join_room(join_string.clone(), "watcher".into(), String::new()).expect("join");
        sharer.join_room(join_string.clone(), "sharer".into(), "restart".into()).expect("join");
        drive(&mut [&mut watcher, &mut sharer], Duration::from_secs(10), "both joined", |s| {
            !s[0].rooms.is_empty() && !s[1].rooms.is_empty()
        })
        .await;
        let wh = *watcher.rooms.keys().next().unwrap();
        let sh = *sharer.rooms.keys().next().unwrap();

        let capture_id = share_test_source(&mut sharer, sh).await;
        drive(&mut [&mut watcher, &mut sharer], Duration::from_secs(10), "watcher sees the stream", |s| {
            stream_of(s[0], wh, "sharer").is_some()
        })
        .await;
        let (old_owner, old_stream) = stream_of(&watcher, wh, "sharer").unwrap();
        watcher.rooms.get_mut(&wh).unwrap().toggle_watch(old_owner, old_stream);
        drive(&mut [&mut watcher, &mut sharer], Duration::from_secs(15), "first frames decoded", |s| {
            frames_decoded(s[0], wh, (old_owner, old_stream)) > 5
        })
        .await;

        // --- restart ---
        let relay = restart_relay(relay, port).await;
        let restarted = Instant::now();

        drive(&mut [&mut watcher, &mut sharer], Duration::from_secs(5), "watcher notices the drop", |s| !s[0].rooms.contains_key(&wh))
            .await;
        // The outage is visible as a truthful Reconnecting state, with the roster kept.
        assert!(watcher.is_reconnecting(&join_string), "a dropped room must read as reconnecting, not disconnected");
        assert!(matches!(watcher.status, Status::Connecting));
        {
            let info = watcher.reconnect_info(&join_string).expect("reconnect info");
            assert_eq!(info.cause, CloseCause::RelayShutdown, "the relay's 0x200 must be read from the QUIC close");
            assert!(info.members.iter().any(|m| m.display_name == "sharer"), "last-known roster is kept for the People panel");
            assert!(!info.members.iter().any(|m| m.display_name == "watcher"), "we are not in our own last-known roster");
        }

        let back = drive(&mut [&mut watcher, &mut sharer], Duration::from_secs(15), "both rooms back", |s| {
            s[0].rooms.contains_key(&wh) && s[1].rooms.contains_key(&sh)
        })
        .await;
        eprintln!("[restart] both clients rejoined {:.2}s after the relay was restarted (loop waited {back:?})", restarted.elapsed().as_secs_f64());
        assert!(restarted.elapsed() < Duration::from_secs(8), "reconnect took {:?}", restarted.elapsed());
        assert!(!watcher.is_reconnecting(&join_string));
        assert!(watcher.reconnects.is_empty() && sharer.reconnects.is_empty());
        assert!(matches!(watcher.status, Status::Idle));

        // The sharer's capture survived and its destination was rebuilt on the new connection.
        drive(&mut [&mut watcher, &mut sharer], Duration::from_secs(15), "own destination live again", |s| {
            let capture = &s[1].captures[&capture_id];
            capture.destinations.len() == 1
                && capture.destinations.values().all(|d| d.room == sh && d.pending.is_none() && d.state() == capture::destination::DestinationState::Live)
        })
        .await;
        assert!(sharer.share_error.is_none(), "{:?}", sharer.share_error);
        assert!(matches!(sharer.captures[&capture_id].status, ShareStatus::Active(_)), "the capture itself was never restarted");

        // The watch resumed on the re-announced stream.
        drive(&mut [&mut watcher, &mut sharer], Duration::from_secs(15), "watch resumed", |s| {
            stream_of(s[0], wh, "sharer").is_some_and(|key| frames_decoded(s[0], wh, key) > 5)
        })
        .await;
        let (new_owner, new_stream) = stream_of(&watcher, wh, "sharer").unwrap();
        assert_ne!(new_owner, old_owner, "a reconnect gives the sharer a fresh ConnId, so this proves the name-based re-match");
        assert!(watcher.rooms[&wh].watches.lock().unwrap().contains_key(&(new_owner, new_stream)));
        assert!(!watcher.rooms[&wh].watches.lock().unwrap().contains_key(&(old_owner, old_stream)));
        eprintln!("[restart] watch resumed {:.2}s after the restart", restarted.elapsed().as_secs_f64());

        watcher.shutdown(Duration::from_secs(2)).await;
        sharer.shutdown(Duration::from_secs(3)).await;
        drop(relay);
    }

    /// The sharer stops (the relay announces `StreamStopped`): the viewer's
    /// watch ends within a second, no frozen tile is left behind (the watch
    /// entry, which owns the decoder, is gone, and `ui::video::VideoViews::sync`
    /// drops the GPU view with it). The same sharer going live again within
    /// `RESUME_AFTER_STOP` takes the dead tile's place without a click.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "spawns a real relay server child process (and a real GStreamer capture)"]
    async fn sharer_stop_ends_the_watch_and_a_reshare_resumes_it() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        gstreamer::init().expect("gstreamer init");
        let _home = isolated_config_home();
        let port = pick_free_udp_port().expect("free port");
        let relay = spawn_relay_server(port).expect("relay");

        let mut watcher = AppState::default();
        let mut sharer = AppState::default();
        let join_string = watcher.create_room(&format!("127.0.0.1:{port}"), "stop").expect("create_room").join_string;
        await_relay(&join_string).await;
        watcher.join_room(join_string.clone(), "watcher".into(), String::new()).expect("join");
        sharer.join_room(join_string.clone(), "sharer".into(), "stop".into()).expect("join");
        drive(&mut [&mut watcher, &mut sharer], Duration::from_secs(10), "both joined", |s| {
            !s[0].rooms.is_empty() && !s[1].rooms.is_empty()
        })
        .await;
        let wh = *watcher.rooms.keys().next().unwrap();
        let sh = *sharer.rooms.keys().next().unwrap();

        let capture_id = share_test_source(&mut sharer, sh).await;
        drive(&mut [&mut watcher, &mut sharer], Duration::from_secs(10), "watcher sees the stream", |s| stream_of(s[0], wh, "sharer").is_some())
            .await;
        let (owner, first_stream) = stream_of(&watcher, wh, "sharer").unwrap();
        watcher.rooms.get_mut(&wh).unwrap().toggle_watch(owner, first_stream);
        drive(&mut [&mut watcher, &mut sharer], Duration::from_secs(15), "first frames decoded", |s| {
            frames_decoded(s[0], wh, (owner, first_stream)) > 5
        })
        .await;

        // --- the sharer stops: the watch must go, fast ---
        sharer.stop_share(capture_id);
        let stopped = Instant::now();
        drive(&mut [&mut watcher, &mut sharer], Duration::from_secs(5), "watch removed after the stop", |s| {
            s[0].rooms[&wh].watches.lock().unwrap().is_empty()
        })
        .await;
        let took = stopped.elapsed();
        eprintln!("[stop] the viewer's watch was gone {took:?} after stop_share returned");
        assert!(took < Duration::from_secs(1), "watch lingered {took:?} after the sharer stopped");
        {
            let room = &watcher.rooms[&wh];
            assert!(room.streams.iter().all(|s| s.owner != owner), "the roster forgot the stream");
            assert_eq!(room.resume_after_stop.len(), 1, "the stopped watch is remembered for a re-share");
        }
        // Nothing comes back by itself while the sharer is not sharing.
        run_for(&mut [&mut watcher, &mut sharer], Duration::from_millis(500)).await;
        assert!(watcher.rooms[&wh].watches.lock().unwrap().is_empty());

        // --- the same sharer goes live again: the new stream replaces the dead tile ---
        // (the test source is bound to DestKey(0), the first destination)
        sharer.next_destination_id = 0;
        share_test_source(&mut sharer, sh).await;
        drive(&mut [&mut watcher, &mut sharer], Duration::from_secs(15), "watch resumed on the new stream", |s| {
            stream_of(s[0], wh, "sharer").is_some_and(|key| frames_decoded(s[0], wh, key) > 5)
        })
        .await;
        let (new_owner, second_stream) = stream_of(&watcher, wh, "sharer").unwrap();
        assert_eq!(new_owner, owner, "same connection, new stream");
        assert_ne!(second_stream, first_stream);
        let room = &watcher.rooms[&wh];
        assert_eq!(room.watches.lock().unwrap().len(), 1, "exactly one watch: the new stream, no stale one");
        assert!(room.resume_after_stop.is_empty());

        watcher.shutdown(Duration::from_secs(2)).await;
        sharer.shutdown(Duration::from_secs(3)).await;
        drop(relay);
    }

    /// Reconnecting is for unexpected drops only: a user Leave, a Forget, and a
    /// Cancel must leave the room down even though the relay is fine. A Leave
    /// is also seen by the other members at once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "spawns a real relay server child process"]
    async fn user_leave_forget_and_cancel_do_not_reconnect() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        let _home = isolated_config_home();
        let port = pick_free_udp_port().expect("free port");
        let relay = spawn_relay_server(port).expect("relay");
        let mut app = AppState::default();
        let join_string = app.create_room(&format!("127.0.0.1:{port}"), "leave").expect("create_room").join_string;
        await_relay(&join_string).await;
        let (peer, _snapshot, mut peer_events) = connect_with_retry(&join_string, "peer").await.expect("peer");

        app.join_room(join_string.clone(), "app".into(), String::new()).expect("join");
        drive(&mut [&mut app], Duration::from_secs(10), "joined", |s| !s[0].rooms.is_empty()).await;
        let handle = *app.rooms.keys().next().unwrap();
        let app_conn = app.rooms[&handle].client.conn_id();

        // --- user Leave: peer sees it within a second, nothing reconnects ---
        let left_at = tokio::spawn(async move {
            loop {
                match peer_events.recv().await {
                    Some(ClientEvent::PresenceDelta(PresenceEvent::MemberLeft { conn_id })) if conn_id == app_conn => return Instant::now(),
                    Some(_) => {}
                    None => panic!("peer event channel closed before MemberLeft"),
                }
            }
        });
        let leave_called = Instant::now();
        app.disconnect_room(handle);
        let seen = left_at.await.expect("peer task");
        let took = seen.duration_since(leave_called);
        eprintln!("[leave] peer saw MemberLeft {took:?} after disconnect_room");
        assert!(took < Duration::from_secs(1), "peer saw the leave after {took:?}");
        run_for(&mut [&mut app], Duration::from_secs(3)).await;
        assert!(app.rooms.is_empty() && app.reconnects.is_empty() && app.pending_connect.is_none(), "a user Leave must not reconnect");
        assert!(!app.is_reconnecting(&join_string));

        // --- Cancel while reconnecting: join again, drop the relay under it, cancel, bring the relay back ---
        app.start_connect(join_string.clone(), false).expect("connect");
        drive(&mut [&mut app], Duration::from_secs(10), "rejoined", |s| !s[0].rooms.is_empty()).await;
        let handle = *app.rooms.keys().next().unwrap();
        let relay = restart_relay(relay, port).await;
        drive(&mut [&mut app], Duration::from_secs(5), "drop noticed", |s| !s[0].rooms.contains_key(&handle)).await;
        assert!(app.is_reconnecting(&join_string));
        app.cancel_reconnect(&join_string);
        assert!(!app.is_reconnecting(&join_string));
        run_for(&mut [&mut app], Duration::from_secs(3)).await;
        assert!(app.rooms.is_empty() && app.pending_connect.is_none(), "a cancelled reconnect must not come back by itself");

        // --- Forget while reconnecting (and the saved row going away) ---
        app.start_connect(join_string.clone(), false).expect("connect");
        drive(&mut [&mut app], Duration::from_secs(10), "rejoined again", |s| !s[0].rooms.is_empty()).await;
        let handle = *app.rooms.keys().next().unwrap();
        let relay = restart_relay(relay, port).await;
        drive(&mut [&mut app], Duration::from_secs(5), "drop noticed again", |s| !s[0].rooms.contains_key(&handle)).await;
        app.forget_room(&join_string).expect("forget");
        run_for(&mut [&mut app], Duration::from_secs(3)).await;
        assert!(app.rooms.is_empty() && app.reconnects.is_empty() && app.pending_connect.is_none(), "a forgotten room must not reconnect");

        peer.leave_and_close().await;
        drop(relay);
    }

    /// Exit (`shutdown_blocking`, what `on_exit` and the signal path call) tells
    /// the relay we left within a round trip, not after its 60-75 s idle
    /// timeout, even while sharing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "spawns a real relay server child process (and a real GStreamer capture)"]
    async fn clean_exit_is_seen_by_a_peer_in_under_a_second() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        gstreamer::init().expect("gstreamer init");
        let _home = isolated_config_home();
        let port = pick_free_udp_port().expect("free port");
        let relay = spawn_relay_server(port).expect("relay");
        let mut app = AppState::default();
        let join_string = app.create_room(&format!("127.0.0.1:{port}"), "exit").expect("create_room").join_string;
        await_relay(&join_string).await;
        let (peer, _snapshot, mut peer_events) = connect_with_retry(&join_string, "peer").await.expect("peer");

        app.join_room(join_string.clone(), "app".into(), String::new()).expect("join");
        drive(&mut [&mut app], Duration::from_secs(10), "joined", |s| !s[0].rooms.is_empty()).await;
        let handle = *app.rooms.keys().next().unwrap();
        let app_conn = app.rooms[&handle].client.conn_id();
        share_test_source(&mut app, handle).await;
        // The peer has seen us (and our stream) before we leave.
        let seen_left = tokio::spawn(async move {
            loop {
                match peer_events.recv().await {
                    Some(ClientEvent::PresenceDelta(PresenceEvent::MemberLeft { conn_id })) if conn_id == app_conn => return Instant::now(),
                    Some(_) => {}
                    None => panic!("peer event channel closed before MemberLeft"),
                }
            }
        });
        tokio::time::sleep(Duration::from_secs(1)).await;

        let called = Instant::now();
        app.shutdown_blocking(Duration::from_secs(5));
        let returned = called.elapsed();
        let seen = seen_left.await.expect("peer task").duration_since(called);
        eprintln!("[exit] peer saw MemberLeft {seen:?} after shutdown_blocking was called; the call itself took {returned:?}");
        assert!(seen < Duration::from_secs(1), "peer saw the exit after {seen:?}");
        assert!(app.rooms.is_empty() && app.captures.is_empty());

        peer.leave_and_close().await;
        drop(relay);
    }

    /// A full room is retried slowly, not given up on, and not hammered: the
    /// error is kept for the UI ("room full, still trying"), and the room is
    /// joined as soon as a slot frees up and the user presses Retry now.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "spawns a real relay server child process"]
    async fn a_full_room_keeps_being_retried_slowly_and_is_joined_when_a_slot_frees() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        let _home = isolated_config_home();
        let port = pick_free_udp_port().expect("free port");
        let relay = spawn_relay_server(port).expect("relay");
        let mut app = AppState::default();
        let join_string = app.create_room(&format!("127.0.0.1:{port}"), "full").expect("create_room").join_string;
        await_relay(&join_string).await;
        app.join_room(join_string.clone(), "app".into(), String::new()).expect("join");
        drive(&mut [&mut app], Duration::from_secs(10), "joined", |s| !s[0].rooms.is_empty()).await;
        let handle = *app.rooms.keys().next().unwrap();

        // Restart the relay without ticking `app`, and fill the new room to the brim
        // before it gets a chance to rejoin.
        let relay = restart_relay(relay, port).await;
        let mut others = Vec::new();
        for i in 0..crate::protocol::MAX_ROOM_MEMBERS {
            others.push(connect_with_retry(&join_string, &format!("p{i}")).await.expect("filler"));
        }

        drive(&mut [&mut app], Duration::from_secs(10), "room-full noticed", |s| {
            s[0].reconnect_info(&join_string).is_some_and(|i| i.room_full())
        })
        .await;
        {
            let info = app.reconnect_info(&join_string).unwrap();
            assert!(info.last_error.as_deref().unwrap().contains("RoomFull"), "error kept for the UI: {:?}", info.last_error);
            assert!(info.retry_in(Instant::now()).unwrap() > Duration::from_secs(3), "slow retry, not a tight loop");
        }
        let attempts = app.reconnect_info(&join_string).unwrap().attempts;
        run_for(&mut [&mut app], Duration::from_secs(2)).await;
        assert_eq!(app.reconnect_info(&join_string).unwrap().attempts, attempts, "no attempts while the slow wait runs");
        assert!(app.is_reconnecting(&join_string), "still trying");

        // A slot frees; Retry now gets in.
        let (leaver, _, _) = others.pop().unwrap();
        leaver.leave_and_close().await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        app.reconnect_now(&join_string);
        drive(&mut [&mut app], Duration::from_secs(10), "joined after a slot freed", |s| s[0].rooms.contains_key(&handle)).await;
        assert!(app.reconnects.is_empty());

        for (client, _, _) in others {
            client.leave_and_close().await;
        }
        drop(relay);
    }
}
