//! Session state and business logic for the desktop client, split out of
//! `ui.rs` as the first step of the egui -> Tauri UI port (see
//! `superdoc/ui/APP-SHELL.md`'s in-progress-port note).
//!
//! Everything in this module is rendering-agnostic: it owns the room/capture/
//! destination/bandwidth data model and every state transition (connect,
//! join, share, watch, fan-out, transfer, per-destination edits, bandwidth
//! allocation, ...), spawns the same background tasks the egui frontend
//! always has, and depends on `egui` only for the handful of types
//! (`egui::TextureHandle`) that already lived on this data before this split
//! — no *new* egui dependency is introduced here. The egui-specific
//! rendering functions (`ui_*`, `update_watch_textures`, `show_popouts`,
//! `update_self_preview_textures`, `eframe::App::update`, ...) stay in
//! `ui.rs`, calling into [`AppState`]/[`RoomSession`]'s methods here instead
//! of owning any new logic themselves. This is what lets `src-tauri` depend
//! on this crate as a library (see `lib.rs`) without pulling in eframe/egui
//! at all.
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
use crate::net::{ClientEvent, DecryptedMediaFrame, PresenceSnapshotState, ScreenshareClient};
use crate::protocol::{ConnId, Member, PresenceEvent, StreamId, StreamInfo, StreamKind};
use crate::rooms::{AppSettings, BandwidthMode, PresentationMode, RoomStore};

pub type ConnectResult = Result<(ScreenshareClient, PresenceSnapshotState, mpsc::UnboundedReceiver<ClientEvent>), String>;

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
    Starting(oneshot::Receiver<Result<ShareHandle, String>>),
    Active(ShareHandle),
    Stopping(oneshot::Receiver<Result<(), String>>),
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
    rate_control: capture::RateControlMode::Constant,
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
    /// Constant/Adaptive/Quality rate control — `capture::RateControlMode`.
    pub rate_control: capture::RateControlMode,
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
            rate_control: params.rate_control,
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
            rate_control: self.edit.rate_control,
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
    /// Live self-preview texture for this capture (video only), created
    /// lazily on the first captured frame then updated in place. Populated by
    /// `ui::App::update_self_preview_textures` from
    /// `CaptureHandle::self_preview_frame`. See
    /// `superdoc/features/self-preview.md`.
    ///
    /// `Why:` (one texture on the capture, rather than a `HashMap<StreamId, _>`
    /// on the room) a self-preview shows what *this machine is capturing*,
    /// which is a property of the capture and identical for every room it's
    /// sent to. Keying it by `stream_id` — as this did while there was only
    /// ever one connection — collides the moment two rooms exist, since
    /// `stream_id` is only unique within one connection.
    pub self_preview_texture: Option<egui::TextureHandle>,
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

/// One stream currently being watched: its decoder, plus the texture handle
/// (created lazily on the first decoded frame, then updated in place).
///
/// The decoder is behind an `Arc<Mutex<_>>` rather than owned outright
/// because, once popped out (see `PopoutState`), it needs to be polled from
/// inside a `show_viewport_deferred` closure — those closures are `Fn`
/// (callable repeatedly, possibly from the egui backend's own bookkeeping)
/// rather than `FnMut`, so any state they mutate has to live behind shared
/// interior mutability rather than being captured by unique reference. The
/// same `Arc` is cloned into the popout closure while this struct keeps its
/// own handle, so `drain_events`' `feed()` calls keep working unchanged
/// regardless of popped-out state.
pub struct WatchEntry {
    pub decoder: Arc<Mutex<StreamDecoder>>,
    pub texture: Option<egui::TextureHandle>,
    /// `Some` while this stream is displayed in its own native window instead
    /// of the central panel.
    pub popout: Option<PopoutState>,
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
    /// Optional second consumer of every raw `MediaFrame` fed to this watch,
    /// set (from outside `session.rs`) by the Tauri front-end's
    /// `commands::toggle_watch` right after this entry is created, so
    /// `src-tauri/src/video_server.rs`'s H.264 relay can tap the same raw
    /// access units this entry's `decoder` gets, without `session.rs` (this
    /// crate's front-end-agnostic library) depending on the `src-tauri`
    /// binary crate that owns the relay. `None` for the plain egui front-end
    /// (`ui.rs`), which never sets it and only ever reads through `decoder`.
    /// A plain `std::sync::mpsc::Sender` (not the `tokio::sync::mpsc` used
    /// elsewhere in this struct) because the relay's producer thread is a
    /// plain `std::thread`, not a Tokio task.
    pub raw_tx: Option<std::sync::mpsc::Sender<DecryptedMediaFrame>>,
}

/// One audio stream currently being decoded/played, whether auto-subscribed
/// alongside a video `WatchEntry` (see `WatchEntry::audio_stream_id`) or
/// started standalone via a "Listen" click on an audio-only stream (no video
/// from the same sender). Keyed the same way as `watches`, in
/// `RoomSession::audio_watches`.
pub struct AudioWatch {
    pub decoder: AudioDecoder,
    /// 0-200, the value bound to this stream's volume slider. Applied to
    /// `decoder` (as `volume_percent / 100.0`) whenever the slider changes.
    pub volume_percent: u32,
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

/// State for a `WatchEntry` currently rendered in its own
/// `show_viewport_deferred` window rather than the central panel.
pub struct PopoutState {
    /// The popout window's own texture, decoupled from `WatchEntry::texture`
    /// so the inline central panel and the popout window can each hold the
    /// most recent frame decoded at their own repaint cadence without
    /// stepping on each other. Wrapped for the same `Fn`-closure reason as
    /// `WatchEntry::decoder`.
    pub texture: Arc<Mutex<Option<egui::TextureHandle>>>,
    /// Set from inside the popout's `viewport_ui_cb` when it observes
    /// `ViewportInfo::close_requested()` (OS close button, Alt+F4, etc.).
    /// Polled once per main-thread frame in `show_popouts` so the entry can
    /// be un-popped-out (returned to the central panel) instead of just
    /// leaking into a window that no longer repaints.
    pub close_requested: Arc<std::sync::atomic::AtomicBool>,
}

pub enum Status {
    Idle,
    Connecting,
    Error(String),
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
    /// Which watched stream of this room's the central video panel displays.
    pub selected_watch: Option<(ConnId, StreamId)>,

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
}

impl RoomSession {
    /// The `stream_id` of `owner`'s currently-announced audio stream, if
    /// any — used by `toggle_watch` to auto-subscribe audio alongside video
    /// from the same sender.
    pub fn sibling_audio_stream(&self, owner: ConnId) -> Option<StreamId> {
        self.streams.iter().find(|s| s.owner == owner && s.kind == StreamKind::Audio).map(|s| s.stream_id)
    }

    /// Whether `owner` currently has an announced video stream — used to
    /// decide whether an audio stream gets its own "Listen" row in the
    /// Streams list (only audio-only senders do; a sender with video has
    /// their audio auto-subscribed via `toggle_watch` instead).
    pub fn owner_has_video(&self, owner: ConnId) -> bool {
        self.streams.iter().any(|s| s.owner == owner && s.kind == StreamKind::Video)
    }

    /// How many *other* people are streaming into this room right now — the
    /// "N active" sidebar count both `ui.rs`'s egui ROOMS row and the Tauri
    /// shell's `SavedRoomRow::active_stream_count` show.
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
            Ok(decoder) => {
                self.audio_watches.lock().unwrap().insert(key, AudioWatch { decoder, volume_percent: 100, auto });
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
            if self.selected_watch == Some(key) {
                self.selected_watch = self.watches.lock().unwrap().keys().next().copied();
            }
            let client = self.client.clone();
            tokio::spawn(async move {
                if let Err(e) = client.unsubscribe(owner, stream_id).await {
                    tracing::warn!("unsubscribe({owner:016x}, {stream_id}) failed: {e:#}");
                }
            });
        } else {
            match StreamDecoder::new(stream_id) {
                Ok(decoder) => {
                    self.watches.lock().unwrap().insert(
                        key,
                        WatchEntry {
                            decoder: Arc::new(Mutex::new(decoder)),
                            texture: None,
                            popout: None,
                            audio_stream_id: None,
                            raw_tx: None,
                        },
                    );
                    self.selected_watch = Some(key);
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
    /// Encoder rate-control mode to use for the *next* video capture
    /// started. See `tune`'s `Why:` note and `capture::RateControlMode`.
    pub rate_control: capture::RateControlMode,
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
}

impl Default for AppState {
    fn default() -> Self {
        let (room_store, room_store_error) = match RoomStore::load() {
            Ok(store) => (store, None),
            Err(e) => (RoomStore::default(), Some(e.to_string())),
        };
        let settings = AppSettings::load();
        rust_i18n::set_locale(&settings.language);
        Self {
            display_name: whoami_fallback(),
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
            rate_control: capture::RateControlMode::default(),
            new_stream_room: None,
            bandwidth_mode: settings.bandwidth_mode,
            absolute_total_kbps: settings.absolute_total_kbps,
            destination_floor_kbps: settings.destination_floor_kbps,
            max_jitter_buffer_ms: settings.max_jitter_buffer_ms,
            bandwidth: BandwidthController::new(),
            pending_destinations: Vec::new(),
            audio_sources: Vec::new(),
            audio_sources_error: None,
            audio_sources_requested: false,
            audio_sources_rx: None,
            selected_audio_source: None,
            room_store,
            room_store_error,
            auto_connect_queue: Vec::new(),
            language: settings.language,
            presentation_mode: settings.presentation_mode,
            launch_presentation_mode: settings.presentation_mode,
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

/// Persist the "START A STREAM" form's current values. Same load-modify-save
/// shape as `save_bandwidth_settings`, and for the same reason all five are
/// written together: they're one coherent form, saved on every change to
/// that form (`src-tauri/src/commands.rs`'s `set_start_stream_defaults`),
/// so a mid-edit crash never leaves the persisted defaults half-updated.
pub fn save_start_stream_defaults(
    framerate: u32,
    bitrate_kbps: u32,
    tune: crate::capture::TuneMode,
    quality_level: crate::capture::QualityLevel,
    rate_control: crate::capture::RateControlMode,
) {
    let mut settings = AppSettings::load();
    settings.start_stream_defaults = crate::rooms::StartStreamDefaults { framerate, bitrate_kbps, tune, quality_level, rate_control };
    if let Err(e) = settings.save() {
        tracing::warn!("failed to save start-stream defaults: {e:#}");
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
                    // Tap the same raw access unit for the Tauri front-end's
                    // H.264 relay (see `WatchEntry::raw_tx`'s doc comment).
                    // `None` for the plain egui front-end, which never sets
                    // it.
                    if let Some(tx) = &entry.raw_tx {
                        let _ = tx.send(frame.clone());
                    }
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
            // `self.tune`/`self.quality_level`/`self.rate_control`, not
            // `TuneMode::default()`/etc.: these are the same "process-wide,
            // set via a pre-start command" setting as `bitrate_kbps`/
            // `framerate` above (see their own doc comments on `AppState`) —
            // the "START A STREAM" form's Preset/Advanced-tuning controls
            // (`stream-control.js`) set them via `set_tune`/`set_quality_
            // level`/`set_rate_control` before calling this.
            tune: self.tune,
            quality_level: self.quality_level,
            rate_control: self.rate_control,
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
                self_preview_texture: None,
            },
        );
        self.share_error = None;

        tokio::spawn(async move {
            let result = capture::start_sharing(&client, stream_id, DestKey(destination_id.0), bitrate_kbps, framerate)
                .await
                .map(ShareHandle::Video)
                .map_err(|e| format!("{e:#}"));
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
    /// so the caller (a Tauri command) can hand it back to the frontend for
    /// the staged card's "Go live"/"Cancel" buttons and `list_staged_
    /// captures` to find it by.
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
                self_preview_texture: None,
            },
        );
        self.share_error = None;

        tokio::spawn(async move {
            let result = capture::start_capture(framerate).await.map(ShareHandle::Video).map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
        });
        capture_id
    }

    /// "Go live" clicked on a staged card: attach `capture_id`'s first
    /// destination into `room_handle`, using the *current* process-wide
    /// framerate/bitrate/tune/quality-level/rate-control settings — the same
    /// ones the staged card's Preset/Advanced-tuning/Framerate/Bitrate
    /// controls already write via `set_framerate`/`set_bitrate`/`set_tune`/
    /// `set_quality_level`/`set_rate_control` (mirroring `start_video_share`'s
    /// own "applied before the call, not passed as arguments" shape — see
    /// that method's doc comment). The caller (`stream-control.js`'s
    /// `saveStartFormDefaults`) persists those same values as the new
    /// defaults at this exact moment, not while the card is merely staged.
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
            rate_control: self.rate_control,
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
                rate_control: capture::RateControlMode::default(),
            }
        });
        self.add_destination_with_params(capture_id, room_handle, role, params);
    }

    /// The guts of [`Self::add_destination`], taking the new destination's
    /// encode params explicitly rather than deriving them from a sibling
    /// destination. Split out for [`Self::retry_destination`], which needs to
    /// hand in the failed destination's *own* last-known params (its
    /// bitrate/tune/quality_level/rate_control) instead of the "copy a
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
                    self.share_error = Some(e);
                    self.forget_destination(pending.capture_id, pending.destination_id);
                    self.clear_transfer_marker(pending.capture_id, pending.role);
                }
                Err(oneshot::error::TryRecvError::Empty) => {
                    still_pending.push(pending);
                }
                Err(oneshot::error::TryRecvError::Closed) => {
                    self.share_error = Some(t!("errors.share_start_task_ended").to_string());
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
        if let Some(capture) = self.captures.get_mut(&capture_id) {
            capture.destinations.shift_remove(&destination_id);
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

    /// "Retry" clicked on a Failed destination card: tear the dead branch
    /// down and bring a fresh one right back up in the same room, off the
    /// same capture — preserving whatever encoder settings
    /// (bitrate/tune/quality_level/rate_control) this destination had, not
    /// the capture's/session's defaults.
    ///
    /// `Why this doesn't just do `stop_destination` + `add_destination`
    /// (which is what the front-end used to do by calling the
    /// `stop_destination`/`start_destination` commands back to back):` two
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
            Err(e) => self.share_error = Some(format!("{e:#}")),
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
            self.share_error = Some(e);
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
    /// currently selected source. No-op if no source is selected (the button
    /// is disabled in that case).
    ///
    /// `for_capture` is the video capture this audio is being attached to, so
    /// its row can be rendered inside that capture's card — see
    /// [`LocalCapture::audio_for_capture`]. `None` starts a standalone audio
    /// share, which still works and simply has no card to nest into.
    pub fn start_audio_share(&mut self, room_handle: RoomHandle, for_capture: Option<CaptureSourceId>) {
        let Some(source) = self.selected_audio_source.clone() else {
            return;
        };
        let capture_id = self.alloc_capture_id();
        let destination_id = self.alloc_destination_id();

        let Some(room) = self.rooms.get_mut(&room_handle) else { return };
        let stream_id = room.next_stream_id;
        room.next_stream_id = room.next_stream_id.wrapping_add(1);
        room.destinations.push(destination_id);
        let client = room.client.clone();

        let (tx, rx) = oneshot::channel();
        self.captures.insert(
            capture_id,
            LocalCapture {
                kind: StreamKind::Audio,
                label: source.name.clone(),
                status: ShareStatus::Starting(rx),
                audio_for_capture: for_capture,
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
                self_preview_texture: None,
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

    /// "Stop"/"Stop Sharing Audio" clicked for `capture_id`: hand the
    /// underlying handle off to a spawned task that tears it down
    /// (`.stop()` is async on both `CaptureHandle` and `AudioCaptureHandle`).
    /// The `LocalCapture` stays in the map, in `Stopping`, so the UI can show
    /// that state; `poll_capture_states` reaps it once the stop lands.
    pub fn stop_share(&mut self, capture_id: CaptureSourceId) {
        let Some(capture) = self.captures.get_mut(&capture_id) else {
            return;
        };
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
                        capture.status = ShareStatus::Active(handle);
                    }
                    Ok(Err(e)) => {
                        self.share_error = Some(e);
                        to_remove.push(*capture_id);
                    }
                    Err(oneshot::error::TryRecvError::Empty) => {}
                    Err(oneshot::error::TryRecvError::Closed) => {
                        self.share_error = Some(t!("errors.share_start_task_ended").to_string());
                        to_remove.push(*capture_id);
                    }
                },
                ShareStatus::Stopping(rx) => match rx.try_recv() {
                    Ok(Ok(())) => to_remove.push(*capture_id),
                    Ok(Err(e)) => {
                        self.share_error = Some(e);
                        to_remove.push(*capture_id);
                    }
                    Err(oneshot::error::TryRecvError::Empty) => {}
                    Err(oneshot::error::TryRecvError::Closed) => {
                        self.share_error = Some(t!("errors.share_stop_task_ended").to_string());
                        to_remove.push(*capture_id);
                    }
                },
                ShareStatus::Active(_) => {}
            }
        }
        for capture_id in to_remove {
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
    /// `Why` remove-then-upsert rather than a plain `upsert`: `RoomStore` is
    /// keyed by *name*, so upserting under a new name would leave the old
    /// entry behind and the sidebar would show the same room twice — once
    /// connected, once as a stale saved row.
    pub fn rename_saved_room(&mut self, join_string: &str, new_name: String) {
        if let Some(old) = self.room_store.name_for(join_string).map(str::to_string)
            && old != new_name
        {
            self.room_store.remove(&old);
        }
        self.room_store.upsert(new_name, join_string.to_string());
        if let Err(e) = self.room_store.save() {
            self.room_store_error = Some(e.to_string());
        }
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
        let endpoint = match &self.endpoint {
            Ok(endpoint) => endpoint.clone(),
            Err(e) => {
                self.status = Status::Error(e.clone());
                return Err(e.clone());
            }
        };
        let display_name = self.display_name.clone();
        let (tx, rx) = oneshot::channel();
        self.pending_connect = Some(PendingConnect { rx, join_string: join_string.clone(), from_add_card });
        self.status = Status::Connecting;

        tokio::spawn(async move {
            let result = ScreenshareClient::connect(&endpoint, &join_string, &display_name)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
        });
        Ok(())
    }

    /// Whether `join_string` is the room a connect is currently in flight for
    /// — what puts a saved row into the amber "reconnecting" state.
    pub fn is_reconnecting(&self, join_string: &str) -> bool {
        self.pending_connect.as_ref().is_some_and(|p| p.join_string == join_string)
    }

    /// Drain one entry off `auto_connect_queue` into `pending_connect`, if
    /// the slot is free. Called every driver tick (`driver.rs`'s `tick`,
    /// right before `poll_connect_result`) so a startup batch of
    /// auto-connect rooms gets kicked off one at a time as each previous one
    /// finishes, instead of all at once — see `auto_connect_queue`'s doc
    /// comment for why one-at-a-time is required, not just simpler.
    pub fn poll_auto_connect_queue(&mut self) {
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
                selected_watch: None,
                audio_watches,
                preview_cache: PreviewCache::new(),
                preview_textures: HashMap::new(),
            },
        );
        handle
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
    pub fn disconnect_room(&mut self, handle: RoomHandle) {
        let Some(room) = self.rooms.shift_remove(&handle) else { return };
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
            Err(oneshot::error::TryRecvError::Closed) => Err(t!("errors.connect_task_ended").to_string()),
        };
        let Some(pending) = self.pending_connect.take() else { return ConnectOutcome::None };

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
                ConnectOutcome::Joined(handle)
            }
            Err(e) => {
                self.status = Status::Error(e.clone());
                ConnectOutcome::Failed { error: e, from_add_card: pending.from_add_card }
            }
        }
    }

    /// Drain every joined room's event channel, not just one. Each room owns
    /// its own connection and therefore its own channel; nothing here is
    /// shared between rooms except `captures`, which is why `rooms` and
    /// `captures` are borrowed as separate fields rather than through `self`.
    ///
    /// Rooms whose control connection closed are collected and torn down
    /// after the loop, since removing from `rooms` mid-iteration isn't
    /// allowed.
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
                    }
                    ClientEvent::PresenceDelta(PresenceEvent::StreamStarted(info)) => {
                        if let Some(m) = room.members.iter_mut().find(|m| m.conn_id == info.owner) {
                            m.stream_count = m.stream_count.saturating_add(1);
                        }
                        room.streams.retain(|s| !(s.owner == info.owner && s.stream_id == info.stream_id));
                        room.streams.push(info);
                    }
                    ClientEvent::PresenceDelta(PresenceEvent::StreamStopped { conn_id, stream_id }) => {
                        if let Some(m) = room.members.iter_mut().find(|m| m.conn_id == conn_id) {
                            m.stream_count = m.stream_count.saturating_sub(1);
                        }
                        room.streams.retain(|s| !(s.owner == conn_id && s.stream_id == stream_id));
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
        }

        for (handle, reason) in closed {
            self.status = Status::Error(t!("errors.disconnected", reason = reason).to_string());
            self.disconnect_room(handle);
        }
    }
}
