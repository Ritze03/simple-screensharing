//! egui UI: room join screen, presence roster, screen sharing, and stream
//! watching.
//!
//! eframe's `update()` closure is synchronous and can't `.await`, so every
//! async call (connecting, starting/stopping capture, subscribing) is
//! spawned onto the ambient tokio runtime (entered in `main.rs`) and results
//! are delivered back via a `tokio::sync::oneshot` channel that the UI polls
//! with `try_recv()` each frame — the same pattern `start_connect`/
//! `poll_connect_result` established for `ScreenshareClient::connect`.
//! Incoming presence/media events are drained the same way from the
//! `mpsc::UnboundedReceiver<ClientEvent>` that `ScreenshareClient::connect`
//! returns.
//!
//! # Session data model (rooms, captures, destinations)
//!
//! [`App`] owns three collections that together describe "what this client is
//! doing right now". None of it is persisted — every launch starts empty
//! (only `rooms.rs`'s `RoomStore`/`AppSettings` survive a restart):
//!
//! - `rooms: IndexMap<RoomHandle, RoomSession>` — one entry per joined room.
//!   A room *is* one QUIC connection: the protocol welds a `ScreenshareClient`
//!   to exactly one room, so N rooms means N connections (all riding the one
//!   shared `quinn::Endpoint` in `App::endpoint`). Everything genuinely
//!   room-scoped — roster, announced streams, watches, audio watches, preview
//!   cache/textures, the `stream_id` allocator — lives here, which is what
//!   keeps two rooms from colliding on a shared key space.
//! - `captures: IndexMap<CaptureSourceId, LocalCapture>` — one entry per local
//!   GStreamer capture pipeline (a screen share, or an audio source). **All**
//!   capture/pipeline state lives here, not under a room.
//! - each `LocalCapture` owns `destinations: IndexMap<DestinationId, Destination>`
//!   — one entry per room this capture is being sent into.
//!
//! `Why:` (captures are `App`-owned, and own their destinations rather than
//! the other way round) one capture is eventually meant to feed several rooms
//! at once — one real screen grab, N independently encoded and independently
//! encrypted outgoing streams, because encryption keys and nonce spaces are
//! room-bound and can never be shared across rooms. If a room owned its
//! captures, leaving that room would tear down a pipeline other rooms are
//! still using. With captures owned by `App` and destinations owned by the
//! capture, leaving a room only removes that room's destinations; the capture
//! survives as long as any destination remains. `RoomSession` keeps only a
//! `Vec<DestinationId>` back-reference so a room can still answer "what am I
//! sending here?" in its own order.
//!
//! **This stage ships exactly one destination per capture** — the fan-out
//! machinery (`capture::branch`'s dynamic GStreamer tee branches) and the
//! per-destination lifecycle machinery (`capture::destination`) are both built
//! and tested, but `capture::start_sharing` still builds one *fixed* encoder
//! branch rather than one branch per destination; the ids and ownership are
//! already in their final shape so nothing has to be re-keyed when it lands.
//! Each `Destination` does already carry its own `VideoParams` and a live
//! `DestinationState` (see `Destination::state`). The visible UI is likewise
//! still single-room: `active_room` names the one `RoomSession` the
//! join/roster/share/watch screens drive.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use indexmap::IndexMap;
use tokio::sync::{mpsc, oneshot};

use crate::audio::{self, AudioCaptureHandle, AudioSource};
use crate::capture::{self, CaptureHandle};
use crate::decode::{self, AudioDecoder, PreviewCache, StreamDecoder};
use crate::net::{ClientEvent, PresenceSnapshotState, ScreenshareClient};
use crate::protocol::{ConnId, Member, PresenceEvent, StreamId, StreamInfo, StreamKind};
use crate::rooms::{AppSettings, PresentationMode, RoomStore};
use crate::theme;

type ConnectResult = Result<(ScreenshareClient, PresenceSnapshotState, mpsc::UnboundedReceiver<ClientEvent>), String>;

/// A reasonable default encode bitrate, preselected on the bitrate slider.
const DEFAULT_BITRATE_KBPS: u32 = 8000;
/// Slider bounds, matching the project's 40 Mbit/s ceiling.
const MIN_BITRATE_KBPS: u32 = 1000;
const MAX_BITRATE_KBPS: u32 = 40000;

/// Idle-fallback repaint cadence while connecting/connected but nothing
/// needs a faster one (see `eframe::App::update`'s use of this constant).
const IDLE_REPAINT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);
/// Repaint cadence used instead of `IDLE_REPAINT_INTERVAL` — and instead of
/// a watched stream's full-rate `ctx.request_repaint()` — whenever the
/// window has lost OS focus, regardless of the selected `PresentationMode`.
/// See `superdoc/features/presentation-modes.md`'s "Why" note on this.
const UNFOCUSED_REPAINT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// Default capture framerate, preselected on the framerate dropdown —
/// mirrors `capture::DEFAULT_FRAMERATE` (kept as a local constant, like
/// `DEFAULT_BITRATE_KBPS` above, rather than importing it, for consistency
/// with the existing bitrate constants' pattern in this module). Deliberately
/// conservative: see `capture::DEFAULT_FRAMERATE`'s doc comment for why 30
/// (matching this pipeline's framerate before it became configurable) beats
/// defaulting straight to the 240Hz ceiling.
const DEFAULT_FRAMERATE: u32 = 30;
/// Common refresh rates offered on the framerate dropdown, up to the
/// project's 240Hz desktop-client ceiling (`capture::MAX_FRAMERATE`) — more
/// natural as discrete choices than a continuous slider, since these are the
/// values that actually correspond to real monitor refresh rates.
const FRAMERATE_OPTIONS: [u32; 7] = [30, 60, 90, 120, 144, 165, 240];

// ---------------------------------------------------------------------
// Session ids
// ---------------------------------------------------------------------
//
// `Why:` (opaque `u32` newtypes with a monotonic per-`App` counter, not
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
pub struct RoomHandle(u32);

/// Identifies one local capture pipeline (one screen grab, or one audio
/// source) for this session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CaptureSourceId(u32);

/// Identifies one outgoing stream: one capture, encoded and encrypted for one
/// room.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DestinationId(u32);

/// Either kind of running outbound share this client can hold onto.
enum ShareHandle {
    Video(CaptureHandle),
    Audio(AudioCaptureHandle),
}

/// Per-share state machine. Starting/stopping are async
/// (`capture::start_sharing`/`audio::start_audio_sharing`, and each handle's
/// `.stop()`), so each transition is spawned and its result delivered back
/// via a oneshot, mirroring `Status`/`connect_rx` above.
enum ShareStatus {
    Starting(oneshot::Receiver<Result<ShareHandle, String>>),
    Active(ShareHandle),
    Stopping(oneshot::Receiver<Result<(), String>>),
}

/// One room this capture is being sent into: the room, plus the `stream_id`
/// announced on *that* room's connection.
///
/// `Why:` (no back-pointer to the owning capture, no copy of its own
/// `DestinationId`) a `Destination` is only ever reachable by walking
/// `App::captures` → `LocalCapture::destinations`, so both the owning
/// `CaptureSourceId` and this destination's own `DestinationId` are already
/// in hand at every use site. Storing them here would be a second copy to
/// keep in sync, with nothing to gain.
struct Destination {
    room: RoomHandle,
    /// This destination's `stream_id` on `room`'s connection — allocated from
    /// that `RoomSession`'s own `next_stream_id`, so the same capture fanned
    /// out to two rooms can (and generally will) carry a different
    /// `stream_id` in each.
    stream_id: StreamId,
    /// This destination's own encode settings. Genuinely per-destination: two
    /// rooms fed by the same capture can run different resolutions, framerates
    /// and bitrates, each served by its own tee branch's
    /// `videoscale`/`videorate`/encoder chain (`capture::branch::add_branch`).
    /// Set at creation from the app-level defaults; a later UI stage exposes
    /// per-destination editing on top of `capture::destination::classify_change`.
    params: capture::destination::VideoParams,
    /// Shared handle onto this destination's live lifecycle state — the
    /// `DestinationState` machine, its monotonic wire-PTS clock, and the stall
    /// protocol's cached IDR. See `capture::destination::DestinationRuntime`.
    ///
    /// `None` only while the capture is still `ShareStatus::Starting` (the
    /// runtime is created inside `capture::start_sharing`, so it doesn't exist
    /// until the handle comes back) and for audio destinations, which have no
    /// video lifecycle. `poll_capture_states` attaches it the moment the
    /// handle lands; `Destination::state` reports `Starting` until then, which
    /// is exactly what a not-yet-started destination is.
    runtime: Option<capture::destination::SharedDestination>,
}

impl Destination {
    /// This destination's current lifecycle state — `Starting` before the
    /// capture handle exists (see `runtime`), otherwise whatever the pump task
    /// has most recently recorded.
    fn state(&self) -> capture::destination::DestinationState {
        match &self.runtime {
            Some(runtime) => runtime.lock().unwrap().state(),
            None => capture::destination::DestinationState::Starting,
        }
    }
}

/// One local capture pipeline (video or audio) and every room it feeds.
/// Several can run at once — e.g. two monitors, or a screen share plus an
/// audio share. Owned by `App::captures`, never by a room: see this module's
/// "Session data model" docs for why.
struct LocalCapture {
    kind: StreamKind,
    /// Human-readable label for this capture's row: "Screen share" for video,
    /// the source's name for audio.
    label: String,
    status: ShareStatus,
    /// Rooms this capture is being sent into. Exactly one entry for now (see
    /// the module docs); the map — rather than a single field — is what later
    /// fan-out work grows into without re-keying anything.
    destinations: IndexMap<DestinationId, Destination>,
    /// Live self-preview texture for this capture (video only), created
    /// lazily on the first captured frame then updated in place. Populated by
    /// `App::update_self_preview_textures` from
    /// `CaptureHandle::self_preview_frame`. See
    /// `superdoc/features/self-preview.md`.
    ///
    /// `Why:` (one texture on the capture, rather than a `HashMap<StreamId, _>`
    /// on the room) a self-preview shows what *this machine is capturing*,
    /// which is a property of the capture and identical for every room it's
    /// sent to. Keying it by `stream_id` — as this did while there was only
    /// ever one connection — collides the moment two rooms exist, since
    /// `stream_id` is only unique within one connection.
    self_preview_texture: Option<egui::TextureHandle>,
}

/// One stream currently being watched: its decoder, plus the texture handle
/// (created lazily on the first decoded frame, then updated in place).
///
/// The decoder is behind an `Arc<Mutex<_>>` rather than owned outright
/// because, once popped out (see `popout`), it needs to be polled from
/// inside a `show_viewport_deferred` closure — those closures are `Fn`
/// (callable repeatedly, possibly from the egui backend's own bookkeeping)
/// rather than `FnMut`, so any state they mutate has to live behind shared
/// interior mutability rather than being captured by unique reference. The
/// same `Arc` is cloned into the popout closure while this struct keeps its
/// own handle, so `drain_events`' `feed()` calls keep working unchanged
/// regardless of popped-out state.
struct WatchEntry {
    decoder: Arc<Mutex<StreamDecoder>>,
    texture: Option<egui::TextureHandle>,
    /// `Some` while this stream is displayed in its own native window
    /// instead of the central panel.
    popout: Option<PopoutState>,
    /// The stream_id of the same sender's audio stream, if one was active
    /// when this video watch started (see `RoomSession::toggle_watch`'s
    /// auto-subscribe-audio behavior) and got auto-subscribed alongside it.
    /// Looked up in `RoomSession::audio_watches` to render the volume slider
    /// next to the video panel and to tear the audio watch down together
    /// with this one. `None` if the sender had no audio stream announced at
    /// watch time.
    audio_stream_id: Option<StreamId>,
}

/// One audio stream currently being decoded/played, whether auto-subscribed
/// alongside a video `WatchEntry` (see `WatchEntry::audio_stream_id`) or
/// started standalone via a "Listen" click on an audio-only stream (no
/// video from the same sender). Keyed the same way as `watches`, in
/// `RoomSession::audio_watches`.
struct AudioWatch {
    decoder: AudioDecoder,
    /// 0-200, the value bound to this stream's volume slider. Applied to
    /// `decoder` (as `volume_percent / 100.0`) whenever the slider changes.
    volume_percent: u32,
}

/// State for a `WatchEntry` currently rendered in its own
/// `show_viewport_deferred` window rather than the central panel.
struct PopoutState {
    /// The popout window's own texture, decoupled from `WatchEntry::texture`
    /// so the inline central panel and the popout window can each hold the
    /// most recent frame decoded at their own repaint cadence without
    /// stepping on each other. Wrapped for the same `Fn`-closure reason as
    /// `WatchEntry::decoder`.
    texture: Arc<Mutex<Option<egui::TextureHandle>>>,
    /// Set from inside the popout's `viewport_ui_cb` when it observes
    /// `ViewportInfo::close_requested()` (OS close button, Alt+F4, etc.).
    /// Polled once per main-thread frame in `show_popouts` so the entry can
    /// be un-popped-out (returned to the central panel) instead of just
    /// leaking into a window that no longer repaints.
    close_requested: Arc<AtomicBool>,
}

enum Status {
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
struct RoomSession {
    client: ScreenshareClient,
    event_rx: mpsc::UnboundedReceiver<ClientEvent>,
    members: Vec<Member>,
    streams: Vec<StreamInfo>,
    save_name: String,
    /// Set once the user has saved this room, so we don't offer to re-save.
    saved: bool,

    /// What this client is currently sending *into this room*, in the order
    /// the user started them. Back-references only: the `Destination`s
    /// themselves are owned by their `LocalCapture` in `App::captures` (see
    /// the module docs), because a capture outlives any single room it feeds.
    destinations: Vec<DestinationId>,
    /// Single per-room counter for allocating our own `stream_id`s on *this*
    /// connection, shared across video and audio (protocol nonce-domain
    /// separation is per stream_id, not per media kind). Per room rather than
    /// global: `stream_id` is scoped to a connection, so every room starts
    /// from 0 independently and two rooms handing out the same number is
    /// correct, not a collision.
    next_stream_id: StreamId,

    /// Streams we're currently subscribed to and decoding in this room, keyed
    /// by (owner conn_id, stream_id).
    ///
    /// `Why` `Arc<Mutex<_>>` around the whole map (not just per-entry, though
    /// `WatchEntry::decoder` is separately `Arc<Mutex<_>>` too, for the
    /// popout-closure reason documented there): the dedicated per-connection
    /// media-ingestion task spawned by `insert_room` (see
    /// `spawn_media_ingestion_task`) needs to look decoders up by
    /// `(ConnId, StreamId)` the instant a `MediaFrame` event arrives, from a
    /// different task than whatever's currently running `App::update()` —
    /// this is what lets media delivery bypass `App::update()`'s throttled
    /// repaint cadence entirely (see that function's doc comment for the
    /// backlog bug this fixes). Sharing the *actual* map, rather than a
    /// separate shadow registry the UI methods below would also have to keep
    /// in sync, means a stream started/stopped via `toggle_watch` is visible
    /// to the ingestion task the instant the same lock that guards every
    /// other access here is released — nothing to duplicate, nothing that can
    /// drift out of sync.
    watches: Arc<Mutex<HashMap<(ConnId, StreamId), WatchEntry>>>,
    watch_error: Option<String>,
    /// Which watched stream of this room's the central video panel displays.
    selected_watch: Option<(ConnId, StreamId)>,

    /// Audio streams we're currently subscribed to and playing, keyed by
    /// (owner conn_id, audio stream_id) — the audio stream's own id, not the
    /// paired video's. Populated both by watching a video stream whose
    /// sender also has an active audio stream (auto-subscribe, see
    /// `toggle_watch`) and by clicking "Listen" on an audio-only stream (see
    /// `toggle_listen`).
    ///
    /// Shared with the media-ingestion task the same way and for the same
    /// reason as `watches` above.
    audio_watches: Arc<Mutex<HashMap<(ConnId, StreamId), AudioWatch>>>,

    /// Latest decoded preview still per active stream in this room,
    /// regardless of whether it's being watched — previews arrive for every
    /// active stream (spec design), so the "Streams" list row for any of them
    /// can show a thumbnail once one has arrived.
    preview_cache: PreviewCache,
    /// Lazily created/updated texture per stream for the "Streams" list
    /// thumbnail, keyed the same way as `preview_cache`/`watches`. Mirrors
    /// `WatchEntry::texture`'s create-once/`set`-in-place pattern.
    preview_textures: HashMap<(ConnId, StreamId), egui::TextureHandle>,
}

pub struct App {
    join_string: String,
    display_name: String,
    status: Status,
    connect_rx: Option<oneshot::Receiver<ConnectResult>>,

    /// The one local QUIC endpoint every room connection rides (see
    /// `net.rs`'s "One endpoint, N connections"). Built once here, at `App`
    /// construction, rather than per `connect()` call. `Err` carries the bind
    /// failure so the join screen can report it instead of the app panicking
    /// at startup over a socket it couldn't open.
    endpoint: Result<quinn::Endpoint, String>,

    /// Every room currently joined, in join order. See the module docs.
    rooms: IndexMap<RoomHandle, RoomSession>,
    /// Every local capture pipeline currently running, in start order, with
    /// its destinations. See the module docs.
    captures: IndexMap<CaptureSourceId, LocalCapture>,
    /// Which room the (still single-room) connected screen drives.
    ///
    /// `Why:` the multi-room *sidebar* is a later stage; until it exists the
    /// screens need one designated room to render, and this is deliberately a
    /// view-level pointer rather than anything the data model depends on —
    /// deleting it is all that's needed once the UI can show every room at
    /// once. Kept in sync by `poll_connect_result` (set on join) and
    /// `disconnect_room` (re-pointed on leave).
    active_room: Option<RoomHandle>,

    /// Monotonic id allocators — never rewind, never reuse (see the "Session
    /// ids" note above).
    next_room_handle: u32,
    next_capture_id: u32,
    next_destination_id: u32,

    /// Last error from starting/stopping a capture. `App`-level, not
    /// per-room: a capture is not owned by a room.
    share_error: Option<String>,
    /// Bitrate (kbps) to use for the *next* video capture started.
    bitrate_kbps: u32,
    /// Capture framerate cap (fps) to use for the *next* video capture
    /// started — see `capture::build_pipeline`'s `videorate` cap and
    /// `key-int-max` scaling.
    framerate: u32,

    /// Audio sources discovered by the last `list_audio_sources()` call.
    /// `App`-level: this is what the *machine* can capture, which has nothing
    /// to do with which rooms are joined.
    audio_sources: Vec<AudioSource>,
    audio_sources_error: Option<String>,
    /// Set once we've kicked off the first automatic listing, so we don't
    /// re-trigger it every frame.
    audio_sources_requested: bool,
    /// In-flight `list_audio_sources()` call, if any.
    audio_sources_rx: Option<oneshot::Receiver<Result<Vec<AudioSource>, String>>>,
    selected_audio_source: Option<AudioSource>,

    room_store: RoomStore,
    room_store_error: Option<String>,
    /// Current UI language ("en" or "de"), mirrored into `rust_i18n`'s
    /// global locale on startup and on every toggle. Kept here too (rather
    /// than re-reading `rust_i18n::locale()` each frame) so the language
    /// `ComboBox` has a plain `&mut String` to bind to.
    language: String,
    /// Current `presentation_mode` selection, bound to the presentation-mode
    /// dropdown and persisted immediately on change. Only takes effect on
    /// the *next* launch (`main.rs` reads it once at startup and passes it
    /// into `NativeOptions.wgpu_options`) — see
    /// `superdoc/features/presentation-modes.md`.
    presentation_mode: PresentationMode,
    /// The presentation mode that was actually active when this window was
    /// created (i.e. what was persisted at launch, before any in-session
    /// change). Compared against `presentation_mode` to show a "restart to
    /// apply" hint only once the two diverge.
    launch_presentation_mode: PresentationMode,
}

impl Default for App {
    fn default() -> Self {
        let (room_store, room_store_error) = match RoomStore::load() {
            Ok(store) => (store, None),
            Err(e) => (RoomStore::default(), Some(e.to_string())),
        };
        let settings = AppSettings::load();
        rust_i18n::set_locale(&settings.language);
        Self {
            join_string: String::new(),
            display_name: whoami_fallback(),
            status: Status::Idle,
            connect_rx: None,
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
            audio_sources: Vec::new(),
            audio_sources_error: None,
            audio_sources_requested: false,
            audio_sources_rx: None,
            selected_audio_source: None,
            room_store,
            room_store_error,
            language: settings.language,
            presentation_mode: settings.presentation_mode,
            launch_presentation_mode: settings.presentation_mode,
        }
    }
}

/// Persist the current language choice, logging (not panicking) on failure —
/// losing a settings preference across restarts is a minor inconvenience,
/// not worth surfacing as a UI error.
///
/// Loads the existing settings file first rather than constructing a fresh
/// `AppSettings` from just `language`: `AppSettings` now also carries
/// `presentation_mode`, and building a literal with only `language` set
/// would silently reset that field to its default every time the language
/// toggle is touched.
fn save_language(language: &str) {
    let mut settings = AppSettings::load();
    settings.language = language.to_string();
    if let Err(e) = settings.save() {
        tracing::warn!("failed to save language preference: {e:#}");
    }
}

/// Persist the current presentation-mode choice. Mirrors `save_language`'s
/// load-modify-save shape for the same reason: don't clobber the sibling
/// setting.
fn save_presentation_mode(mode: PresentationMode) {
    let mut settings = AppSettings::load();
    settings.presentation_mode = mode;
    if let Err(e) = settings.save() {
        tracing::warn!("failed to save presentation mode preference: {e:#}");
    }
}

/// A small local fallback so the display-name field isn't empty by default.
/// Not a real identity system — just a nicer starting point than "".
fn whoami_fallback() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| t!("common.default_display_name").to_string())
}

/// Resolve a `RoomSession::destinations` back-reference to the capture that
/// owns it. `None` if the capture is already gone (a stale back-reference is
/// harmless — ids are never reused, so a miss can only ever be a miss).
fn find_destination(
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
fn capture_feeding(
    captures: &IndexMap<CaptureSourceId, LocalCapture>,
    room: RoomHandle,
    stream_id: StreamId,
) -> Option<&LocalCapture> {
    captures
        .values()
        .find(|capture| capture.destinations.values().any(|d| d.room == room && d.stream_id == stream_id))
}

/// Always-on per-connection task that reads `raw_events` — the exact
/// `mpsc::UnboundedReceiver<ClientEvent>` `ScreenshareClient::connect`
/// returns — as fast as events arrive, independent of `App::update()`'s
/// repaint-driven cadence, and forwards everything except `MediaFrame` on to
/// `forward_tx` (what `RoomSession::event_rx` actually is) for `drain_events`
/// to keep handling exactly as before, at its normal throttled pace.
///
/// # Why this exists (the "fast-forward catch-up" bug)
///
/// `Why:` before this task existed, every `ClientEvent` — including
/// `MediaFrame` — sat in that same unbounded channel until `App::update()`
/// next ran and `drain_events` drained it with `try_recv()`. While the window
/// is unfocused, `update()` only runs every `UNFOCUSED_REPAINT_INTERVAL`
/// (500ms), so a sustained stream of incoming `MediaFrame`s piled up
/// unconsumed in the channel for up to 500ms at a stretch — not stale in
/// decode, not stale in `PacingState`'s budget math, just sitting
/// undelivered. The moment `update()` next ran, the whole backlog drained in
/// one tight `while let Ok(event) = ... try_recv()` burst, handing
/// `StreamDecoder`/`AudioDecoder` a rapid-fire sequence of now-stale access
/// units with almost no wall-clock time between `feed()` calls.
/// `PacingState::decide` — correctly, given what it was fed — read that as
/// "decode finished early" for every one of them and paced each out with a
/// real-time sleep: a literal real-time replay of however much history had
/// piled up, the "fast-forward catch-up" the user reported (see
/// `superdoc/brainstorm/ui-redesign-multiroom-streaming.md`,
/// "Performance/Rendering" round 2, and `decode.rs`'s `PacingState::epoch`
/// doc comment for the *other*, already-merged half of this fix — a
/// defensive wall-clock staleness backstop inside the pacing math itself,
/// which only ever mitigates a burst after the fact; it cannot stop one from
/// forming).
///
/// This task is the fix for the actual backlog: it calls `.recv().await` in
/// a plain loop, which the tokio scheduler wakes the instant a new event is
/// queued — not on any repaint-driven cadence — so a `MediaFrame` is fed to
/// its decoder within a scheduler tick of arriving, whether the window is
/// focused, unfocused, minimized, or the UI thread is busy laying out a
/// frame. Nothing at this layer ever has 500ms (or even one repaint
/// interval's worth) of frames to burst-deliver, so `PacingState` never sees
/// the bursty-delivery signature that caused it to fast-forward.
///
/// # Design choice: intercept-and-forward, not a `net.rs`-side split
///
/// Two shapes were considered for *where* `MediaFrame` peels off from
/// everything else `ClientEvent` carries (membership changes, stream
/// announce/stop, subscriber counts — none of it latency-sensitive, all of
/// it fine staying on `drain_events`'s throttled cadence):
///
/// 1. Split inside `net.rs` itself, before a `MediaFrame` ever reaches the
///    UI-facing channel — e.g. datagram_reader_loop looks up a decoder
///    registry directly and calls `feed()` inline, never constructing a
///    `ClientEvent::MediaFrame` for it at all.
/// 2. Keep `net.rs` and `ClientEvent` exactly as they are — one channel out
///    of `ScreenshareClient::connect` — and put a dedicated always-on drain
///    task between that channel and the UI, forwarding everything that isn't
///    a `MediaFrame`.
///
/// This function is option 2. `net.rs` has no notion of a decoder registry
/// today (by design — see its module docs' "Public API shape": it hands back
/// a plain event channel and stays ignorant of what the caller does with
/// `MediaFrame`s). Reaching into `net.rs` to add a `ui.rs`-specific registry
/// would either leak `ui.rs` types into `net.rs` or force a generic registry
/// trait for a single caller — real complexity for no behavioral difference, since
/// `mpsc::UnboundedReceiver::recv().await` already wakes immediately on
/// arrival with nothing throttling it between `net.rs` and here. Option 2
/// gets the identical latency characteristics (nothing sits in a channel
/// waiting on `App::update()`) with a change confined entirely to `ui.rs`.
///
/// # Why the registry is `Arc<Mutex<HashMap<...>>>`, not a channel back to the UI thread
///
/// Sending "please feed this decoder" messages back to the UI thread would
/// just recreate the same kind of queue this fix removes, one level higher.
/// The decoders themselves already tolerate being driven from any thread —
/// `StreamDecoder::feed`/`AudioDecoder::feed` are non-blocking, cheap, and
/// exist precisely so an arbitrary caller can hand off an access unit without
/// synchronizing with whatever else that decoder is doing — so the simplest
/// correct shape is to give this task direct read access to the same
/// `watches`/`audio_watches` maps `RoomSession`'s own methods
/// (`toggle_watch`, `toggle_listen`, ...) already mutate, behind the
/// `std::sync::Mutex` those fields are now typed with. Lock hold times here
/// are a single hash lookup plus (for video) cloning an `Arc` — the same
/// order of magnitude as the UI thread's own per-repaint texture-upload pass
/// over the same map, so contention between this task and `App::update()` is
/// not a real concern.
async fn spawn_media_ingestion_task(
    mut raw_events: mpsc::UnboundedReceiver<ClientEvent>,
    forward_tx: mpsc::UnboundedSender<ClientEvent>,
    watches: Arc<Mutex<HashMap<(ConnId, StreamId), WatchEntry>>>,
    audio_watches: Arc<Mutex<HashMap<(ConnId, StreamId), AudioWatch>>>,
) {
    loop {
        // `Why` racing `raw_events.recv()` against `forward_tx.closed()`,
        // rather than only detecting a dropped `RoomSession` via a failed
        // `forward_tx.send()` below: this task now owns `raw_events` (see
        // this function's doc comment), so — unlike before this fix, when
        // `RoomSession` held that receiver directly — dropping the
        // `RoomSession` (`disconnect_room`) no longer has any *direct*
        // effect on this loop. Relying solely on a future failed
        // `forward_tx.send()` to notice would leave this task (and every
        // decoder/GStreamer pipeline it keeps feeding, and the
        // `datagram_reader_loop`/etc. background tasks in `net.rs`, which
        // only exit when *their* send into `raw_events` fails) running
        // indefinitely after the user leaves a room, for as long as the
        // remote side keeps sending `MediaFrame`s and nothing else — the
        // common case for an active share — since every one of those frames
        // is consumed here without ever reaching the `other` arm below.
        // `forward_tx.closed()` resolves the instant `RoomSession::event_rx`
        // (`forward_rx`) is dropped, independent of whether any event is in
        // flight, so this task — and transitively, once `raw_events` itself
        // drops with it, the `net.rs` background tasks — tears down
        // immediately on disconnect, matching the old teardown promptness.
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
                // needing to know the frame's `StreamKind` up front. Mirrors
                // the lookup `drain_events` used to do directly.
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
                // belongs on `drain_events`'s normal throttled cadence — see
                // this function's doc comment. A send failure here means the
                // room's `RoomSession` (and its `forward_rx`) has already
                // been dropped (room left / app shutting down), so this task
                // has nothing left to do. In practice the `forward_tx.closed()`
                // branch above should always win that race first, but this
                // stays as a belt-and-suspenders backstop.
                if forward_tx.send(other).is_err() {
                    return;
                }
            }
        }
    }
}

/// Capture-side operations: everything that owns or drives a local GStreamer
/// pipeline. Deliberately on `App`, not on `RoomSession` — see the module
/// docs' "Session data model".
impl App {
    fn alloc_capture_id(&mut self) -> CaptureSourceId {
        let id = CaptureSourceId(self.next_capture_id);
        self.next_capture_id = self.next_capture_id.wrapping_add(1);
        id
    }

    fn alloc_destination_id(&mut self) -> DestinationId {
        let id = DestinationId(self.next_destination_id);
        self.next_destination_id = self.next_destination_id.wrapping_add(1);
        id
    }

    /// "Share Screen" clicked: allocate a capture + one destination into
    /// `room_handle` (with a fresh `stream_id` drawn from *that room's*
    /// counter) and spawn `capture::start_sharing` onto the ambient tokio
    /// runtime. Can be called again while other captures (video or audio) are
    /// already running — each call prompts the OS portal picker again, so a
    /// second click lets the user pick a different monitor/window.
    fn start_video_share(&mut self, room_handle: RoomHandle) {
        let capture_id = self.alloc_capture_id();
        let destination_id = self.alloc_destination_id();

        let Some(room) = self.rooms.get_mut(&room_handle) else { return };
        let stream_id = room.next_stream_id;
        room.next_stream_id = room.next_stream_id.wrapping_add(1);
        room.destinations.push(destination_id);
        let client = room.client.clone();

        let bitrate_kbps = self.bitrate_kbps;
        let framerate = self.framerate;
        let (tx, rx) = oneshot::channel();
        self.captures.insert(
            capture_id,
            LocalCapture {
                kind: StreamKind::Video,
                label: t!("share.video_label").to_string(),
                status: ShareStatus::Starting(rx),
                destinations: IndexMap::from([(
                    destination_id,
                    Destination {
                        room: room_handle,
                        stream_id,
                        // Width/height 0 = "whatever the source is": this
                        // destination doesn't downscale, so there is nothing to
                        // clamp against yet (the capture hasn't negotiated a
                        // size). `capture::destination::clamp_to_source` treats
                        // 0 as "no constraint" for exactly this case.
                        params: capture::destination::VideoParams {
                            width: 0,
                            height: 0,
                            framerate,
                            bitrate_kbps,
                            backend: capture::EncoderBackend::default(),
                        },
                        runtime: None,
                    },
                )]),
                self_preview_texture: None,
            },
        );
        self.share_error = None;

        tokio::spawn(async move {
            let result = capture::start_sharing(&client, stream_id, bitrate_kbps, framerate)
                .await
                .map(ShareHandle::Video)
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
        });
    }

    /// "Share Audio" clicked: same shape as `start_video_share`, for the
    /// currently selected source. No-op if no source is selected (the button
    /// is disabled in that case).
    fn start_audio_share(&mut self, room_handle: RoomHandle) {
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
                destinations: IndexMap::from([(
                    destination_id,
                    Destination {
                        room: room_handle,
                        stream_id,
                        // Audio destinations carry the video params struct
                        // inertly — there is no per-destination audio geometry
                        // to describe, and `runtime` stays `None` because the
                        // video `DestinationState`/stall machinery has no audio
                        // counterpart (audio's own always-on-mixer design makes
                        // toggling invisible by construction; see the plan doc).
                        params: capture::destination::VideoParams {
                            width: 0,
                            height: 0,
                            framerate: 0,
                            bitrate_kbps: 0,
                            backend: capture::EncoderBackend::default(),
                        },
                        runtime: None,
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

    /// "Stop"/"Stop Sharing Audio" clicked for `capture_id`: hand the
    /// underlying handle off to a spawned task that tears it down
    /// (`.stop()` is async on both `CaptureHandle` and `AudioCaptureHandle`).
    /// The `LocalCapture` stays in the map, in `Stopping`, so the UI can show
    /// that state; `poll_capture_states` reaps it once the stop lands.
    fn stop_share(&mut self, capture_id: CaptureSourceId) {
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
    /// synchronously when `App` goes away right after this call.
    ///
    /// Mirrors `stop_share`'s spawn pattern, but without waiting to see the
    /// result — the caller is discarding this capture (or the whole app is
    /// exiting) regardless of whether the stop finishes, so there is nothing
    /// meaningful left to poll it into. `Starting`/`Stopping` captures are
    /// left to their in-flight task: a `Starting` one has no handle yet to
    /// stop, and a `Stopping` one is already being torn down.
    fn drop_capture_fire_and_forget(&mut self, capture_id: CaptureSourceId) {
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
    fn stop_all_captures_fire_and_forget(&mut self) {
        for capture_id in self.captures.keys().copied().collect::<Vec<_>>() {
            self.drop_capture_fire_and_forget(capture_id);
        }
    }

    /// Drop `capture`'s destinations out of the back-reference lists of every
    /// room they pointed into. Call whenever a `LocalCapture` leaves
    /// `self.captures`, so no room is left advertising an outgoing stream
    /// that no longer exists.
    fn forget_destinations_of(&mut self, capture: &LocalCapture) {
        for (destination_id, dest) in capture.destinations.iter() {
            if let Some(room) = self.rooms.get_mut(&dest.room) {
                room.destinations.retain(|d| d != destination_id);
            }
        }
    }

    /// Poll every in-flight capture start/stop, if any. Call once per frame.
    fn poll_capture_states(&mut self) {
        let mut to_remove = Vec::new();
        for (capture_id, capture) in self.captures.iter_mut() {
            match &mut capture.status {
                ShareStatus::Starting(rx) => match rx.try_recv() {
                    Ok(Ok(handle)) => {
                        // Attach the destination lifecycle runtime the capture
                        // just created, so `Destination::state` starts
                        // reporting the real state machine (`Starting` until
                        // the pump sends the first IDR, then `Live`) instead of
                        // its not-yet-started placeholder.
                        if let ShareHandle::Video(video) = &handle {
                            let runtime = video.destination();
                            for dest in capture.destinations.values_mut() {
                                dest.runtime = Some(runtime.clone());
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

    /// Upload any freshly captured self-preview frames (see
    /// `capture::CaptureHandle::self_preview_frame` and
    /// `superdoc/features/self-preview.md`) to each running video capture's
    /// own texture. Call once per repaint alongside
    /// `RoomSession::update_watch_textures`; cheap no-op when nothing new has
    /// arrived.
    ///
    /// Unlike `update_watch_textures`, this doesn't need to drive the repaint
    /// cadence itself: the self-preview branch is capped to a fixed, modest
    /// rate (`capture::SELF_PREVIEW_BRANCH_FRAMERATE`), well within what
    /// `update()`'s existing idle-fallback repaint interval already keeps up
    /// with, so no extra `ctx.request_repaint()` is needed for this alone.
    ///
    /// No map cleanup pass any more: the texture lives on the `LocalCapture`,
    /// so it dies exactly when the capture does.
    fn update_self_preview_textures(&mut self, ctx: &egui::Context) {
        for (capture_id, capture) in self.captures.iter_mut() {
            let ShareStatus::Active(ShareHandle::Video(handle)) = &capture.status else {
                continue;
            };
            let Some(image) = handle.self_preview_frame() else {
                continue;
            };
            match &mut capture.self_preview_texture {
                Some(tex) => tex.set(image, egui::TextureOptions::LINEAR),
                None => {
                    let name = format!("self-preview-{}", capture_id.0);
                    capture.self_preview_texture = Some(ctx.load_texture(name, image, egui::TextureOptions::LINEAR));
                }
            }
        }
    }

    /// Kick off (or re-kick-off) an `audio::list_audio_sources()` call on
    /// the ambient tokio runtime. That function shells out to `pw-dump`
    /// synchronously, so it's run via `spawn_blocking` rather than directly
    /// in the async task, matching the non-blocking intent of the rest of
    /// this module's spawn pattern.
    fn refresh_audio_sources(&mut self) {
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
    fn poll_audio_sources(&mut self) {
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
}

/// Room-scoped operations: presence, watching, and the room's own view state.
/// Everything in here was already correctly room-scoped as `Connected`; the
/// only change is that there can now be several of them.
impl RoomSession {
    /// The `stream_id` of `owner`'s currently-announced audio stream, if
    /// any — used by `toggle_watch` to auto-subscribe audio alongside video
    /// from the same sender. `announced-streams` state (`self.streams`) is
    /// exactly the `StreamInfo`/`ANNOUNCE_STREAM` state described in this
    /// task's instructions, already tracked here from
    /// `PresenceSnapshot`/`PresenceDelta`.
    fn sibling_audio_stream(&self, owner: ConnId) -> Option<StreamId> {
        self.streams.iter().find(|s| s.owner == owner && s.kind == StreamKind::Audio).map(|s| s.stream_id)
    }

    /// Whether `owner` currently has an announced video stream — used to
    /// decide whether an audio stream gets its own "Listen" row in the
    /// Streams list (only audio-only senders do; a sender with video has
    /// their audio auto-subscribed via `toggle_watch` instead).
    fn owner_has_video(&self, owner: ConnId) -> bool {
        self.streams.iter().any(|s| s.owner == owner && s.kind == StreamKind::Video)
    }

    /// Start decoding/playing `(owner, stream_id)`'s audio if not already
    /// doing so, and subscribe to it. Shared by `toggle_watch`'s
    /// auto-subscribe-audio-alongside-video path and `toggle_listen`'s
    /// standalone audio-only path — both just need "make sure this audio
    /// stream is playing", so a click on "Listen" for a stream already
    /// auto-subscribed via its video watch (or vice versa) is a safe no-op
    /// rather than a second decoder.
    fn start_audio_watch(&mut self, owner: ConnId, stream_id: StreamId) {
        let key = (owner, stream_id);
        if self.audio_watches.lock().unwrap().contains_key(&key) {
            return;
        }
        match AudioDecoder::new(stream_id) {
            Ok(decoder) => {
                self.audio_watches.lock().unwrap().insert(key, AudioWatch { decoder, volume_percent: 100 });
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
    fn teardown_audio_watch(&mut self, owner: ConnId, stream_id: StreamId) {
        if self.audio_watches.lock().unwrap().remove(&(owner, stream_id)).is_some() {
            let client = self.client.clone();
            tokio::spawn(async move {
                if let Err(e) = client.unsubscribe(owner, stream_id).await {
                    tracing::warn!("unsubscribe (audio) ({owner:016x}, {stream_id}) failed: {e:#}");
                }
            });
        }
    }

    /// "Watch"/"Stop Watching" clicked for `(owner, stream_id)`. Subscribing
    /// and unsubscribing are async (control-plane round trip) so they're
    /// fired onto the runtime and not awaited here; the decoder itself is
    /// created/dropped synchronously since that's cheap (spawns/joins a
    /// local decode thread, no network wait).
    ///
    /// If `owner` also has an active audio stream, it's auto-subscribed and
    /// played alongside the video (see `start_audio_watch`) — matching the
    /// natural expectation that "watching someone's screen" includes
    /// hearing their audio, without a separate manual step — and torn down
    /// together with the video watch when watching stops.
    fn toggle_watch(&mut self, owner: ConnId, stream_id: StreamId) {
        let key = (owner, stream_id);
        let removed = self.watches.lock().unwrap().remove(&key);
        if let Some(entry) = removed {
            if self.selected_watch == Some(key) {
                self.selected_watch = self.watches.lock().unwrap().keys().next().copied();
            }
            let client = self.client.clone();
            tokio::spawn(async move {
                if let Err(e) = client.unsubscribe(owner, stream_id).await {
                    tracing::warn!("unsubscribe({owner:016x}, {stream_id}) failed: {e:#}");
                }
            });
            if let Some(audio_stream_id) = entry.audio_stream_id {
                self.teardown_audio_watch(owner, audio_stream_id);
            }
        } else {
            match StreamDecoder::new(stream_id) {
                Ok(decoder) => {
                    let audio_stream_id = self.sibling_audio_stream(owner);
                    self.watches.lock().unwrap().insert(
                        key,
                        WatchEntry {
                            decoder: Arc::new(Mutex::new(decoder)),
                            texture: None,
                            popout: None,
                            audio_stream_id,
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

                    if let Some(audio_stream_id) = audio_stream_id {
                        self.start_audio_watch(owner, audio_stream_id);
                    }
                }
                Err(e) => {
                    self.watch_error = Some(format!("{e:#}"));
                }
            }
        }
    }

    /// "Listen"/"Stop Listening" clicked for an audio-only stream (a sender
    /// with no video announced — see `owner_has_video`). Just toggles that
    /// stream's `AudioWatch` via the same start/teardown helpers
    /// `toggle_watch` uses for its auto-subscribed audio.
    fn toggle_listen(&mut self, owner: ConnId, stream_id: StreamId) {
        if self.audio_watches.lock().unwrap().contains_key(&(owner, stream_id)) {
            self.teardown_audio_watch(owner, stream_id);
        } else {
            self.start_audio_watch(owner, stream_id);
        }
    }

    /// Upload any freshly decoded frames to their stream's texture. Call
    /// once per repaint; cheap no-op when nothing new has decoded.
    ///
    /// Skips popped-out entries: once a stream is popped out, its own
    /// viewport closure (`show_popouts`) is the sole `latest_frame()`
    /// poller for that stream, so the main-thread pass here doesn't steal
    /// frames out from under it.
    ///
    /// Returns whether at least one non-popped-out watch is active, which
    /// `update()` uses to decide whether to keep requesting repaints on the
    /// root context every frame: `latest_frame()` is non-blocking and only
    /// yields `Some` at the decoder's real (possibly >30fps, up to 240Hz)
    /// output rate, so the repaint cadence has to be driven explicitly here
    /// rather than left to egui's default input-triggered repaints — without
    /// this, the panel only actually refreshes on the side effect of other
    /// input (e.g. mouse movement) or the coarse idle-fallback interval at
    /// the bottom of `update()`, capping perceived playback well below the
    /// decoder's real framerate whenever the mouse is idle.
    fn update_watch_textures(&mut self, ctx: &egui::Context) -> bool {
        let mut any_active = false;
        let mut watches = self.watches.lock().unwrap();
        for ((owner, stream_id), entry) in watches.iter_mut() {
            if entry.popout.is_some() {
                continue;
            }
            any_active = true;
            let image = entry.decoder.lock().unwrap().latest_frame();
            if let Some(image) = image {
                match &mut entry.texture {
                    Some(tex) => tex.set(image, egui::TextureOptions::LINEAR),
                    None => {
                        let name = format!("stream-{owner:016x}-{stream_id}");
                        entry.texture = Some(ctx.load_texture(name, image, egui::TextureOptions::LINEAR));
                    }
                }
            }
        }
        any_active
    }

    /// "Pop out" clicked for `(owner, stream_id)`: mark that `WatchEntry` as
    /// popped-out so the central panel stops rendering it and `show_popouts`
    /// starts giving it its own native window instead.
    fn pop_out(&mut self, owner: ConnId, stream_id: StreamId) {
        let key = (owner, stream_id);
        if self.selected_watch == Some(key) {
            // Free the central panel to show something else immediately —
            // otherwise it'd sit on "(popped out)" with nothing to fall
            // back to for a lone watched stream.
            self.selected_watch = self.watches.lock().unwrap().keys().find(|k| **k != key).copied();
        }
        if let Some(entry) = self.watches.lock().unwrap().get_mut(&key) {
            entry.popout = Some(PopoutState {
                texture: Arc::new(Mutex::new(None)),
                close_requested: Arc::new(AtomicBool::new(false)),
            });
        }
    }

    /// For every currently popped-out `WatchEntry`, (re)declare its deferred
    /// viewport — `show_viewport_deferred` must be called every frame a
    /// viewport should keep existing, per egui's viewport API. Call once per
    /// repaint, alongside `update_watch_textures`.
    ///
    /// `handle` is this session's own `RoomHandle`, mixed into each
    /// viewport's id: `(owner, stream_id)` alone is only unique within one
    /// connection, so two rooms could otherwise hash to the same
    /// `ViewportId` and fight over one OS window.
    fn show_popouts(&mut self, ctx: &egui::Context, handle: RoomHandle) {
        // Locked only for the duration of this loop, which just clones out
        // `Arc`s to move into each viewport closure — released before
        // `ctx.show_viewport_deferred` runs (that closure never needs this
        // lock, since it captured its own `decoder`/`texture` handles) and
        // well before the second loop below re-locks for the unpop pass.
        let watches = self.watches.lock().unwrap();
        for ((owner, stream_id), entry) in watches.iter() {
            let Some(popout) = &entry.popout else { continue };
            let owner = *owner;
            let stream_id = *stream_id;
            let viewport_id = egui::ViewportId::from_hash_of((handle.0, owner, stream_id));
            let decoder = entry.decoder.clone();
            let texture = popout.texture.clone();
            let close_requested = popout.close_requested.clone();
            let title = t!("popout.title", stream_id = stream_id, owner = format!("{owner:016x}")).to_string();

            ctx.show_viewport_deferred(
                viewport_id,
                egui::ViewportBuilder::default().with_decorations(false).with_title(title),
                move |ctx, _class| {
                    // Poll this stream's decoder on the popout's own
                    // repaint cadence rather than the root viewport's —
                    // `show_viewport_deferred`'s closure is driven
                    // independently once the viewport is a real native
                    // window, which is exactly what lets a high-refresh-rate
                    // popped-out stream repaint faster than a slower main
                    // window.
                    if let Some(image) = decoder.lock().unwrap().latest_frame() {
                        let mut tex_guard = texture.lock().unwrap();
                        match &mut *tex_guard {
                            Some(tex) => tex.set(image, egui::TextureOptions::LINEAR),
                            None => {
                                let name = format!("popout-{owner:016x}-{stream_id}");
                                *tex_guard = Some(ctx.load_texture(name, image, egui::TextureOptions::LINEAR));
                            }
                        }
                    }

                    egui::CentralPanel::default().frame(egui::Frame::NONE).show(ctx, |ui| {
                        let tex_guard = texture.lock().unwrap();
                        match &*tex_guard {
                            Some(tex) => {
                                let available = ui.available_size();
                                let tex_size = tex.size_vec2();
                                let scale = if tex_size.x > 0.0 && tex_size.y > 0.0 {
                                    (available.x / tex_size.x).min(available.y / tex_size.y)
                                } else {
                                    1.0
                                };
                                let display_size = tex_size * scale.max(0.01);
                                ui.centered_and_justified(|ui| ui.image((tex.id(), display_size)));
                            }
                            None => {
                                ui.centered_and_justified(|ui| ui.label(t!("video.waiting_frame").to_string()));
                            }
                        }
                    });

                    if ctx.input(|i| i.key_pressed(egui::Key::F11)) {
                        let is_fullscreen = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!is_fullscreen));
                    }

                    if ctx.input(|i| i.viewport().close_requested()) {
                        close_requested.store(true, Ordering::Relaxed);
                    }

                    // Keep this viewport polling `latest_frame()` every
                    // frame while it's popped out, regardless of whether
                    // this particular poll found a new one: `latest_frame()`
                    // only returns `Some` at the decoder's real output rate,
                    // so requesting a repaint solely on the polls that *did*
                    // find something new would let the chain die the moment
                    // the decoder's real framerate falls behind this
                    // viewport's repaint rate (the common case), stalling
                    // this window until unrelated input (e.g. a mouse move
                    // over it) kicks it again. Only this viewport, not the
                    // whole app, is affected by this request.
                    ctx.request_repaint();
                },
            );
        }

        // Re-embed any popouts whose native window the user closed, instead
        // of leaving them marked popped-out with nothing left repainting
        // them.
        let mut to_unpop: Vec<(ConnId, StreamId)> = Vec::new();
        for (key, entry) in watches.iter() {
            if let Some(popout) = &entry.popout
                && popout.close_requested.load(Ordering::Relaxed)
            {
                to_unpop.push(*key);
            }
        }
        drop(watches);
        for key in to_unpop {
            if let Some(entry) = self.watches.lock().unwrap().get_mut(&key) {
                if let Some(popout) = entry.popout.take() {
                    // Hand the popout's last frame over as the inline
                    // texture so returning to the central panel doesn't
                    // flash blank until the next decoded frame.
                    if let Some(tex) = popout.texture.lock().unwrap().take() {
                        entry.texture = Some(tex);
                    }
                }
                if self.selected_watch.is_none() {
                    self.selected_watch = Some(key);
                }
            }
        }
    }
}

impl App {
    fn start_connect(&mut self) {
        let endpoint = match &self.endpoint {
            Ok(endpoint) => endpoint.clone(),
            Err(e) => {
                self.status = Status::Error(e.clone());
                return;
            }
        };
        let join_string = self.join_string.clone();
        let display_name = self.display_name.clone();
        let (tx, rx) = oneshot::channel();
        self.connect_rx = Some(rx);
        self.status = Status::Connecting;

        tokio::spawn(async move {
            let result = ScreenshareClient::connect(&endpoint, &join_string, &display_name)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
        });
    }

    /// Insert a freshly connected room and return its handle. The one place
    /// a `RoomSession` is born — used by `poll_connect_result` and, in tests,
    /// to stand a session up directly.
    ///
    /// This is also where the always-on media-ingestion task is spawned (see
    /// [`spawn_media_ingestion_task`]) — every `RoomSession` is created here,
    /// so this is the one choke point that guarantees no room's `event_rx`
    /// ever bypasses it.
    fn insert_room(
        &mut self,
        client: ScreenshareClient,
        snapshot: PresenceSnapshotState,
        event_rx: mpsc::UnboundedReceiver<ClientEvent>,
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
                save_name: String::new(),
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

    /// Tear one room down: drop every capture feeding it (stage 1 has exactly
    /// one destination per capture, so a room's destinations *are* whole
    /// captures), drop the session — which drops its decoders — and re-point
    /// `active_room` at whatever is left.
    ///
    /// Only this room's captures: a capture with no destination in `handle`
    /// is untouched, which is the whole point of `App` owning captures.
    ///
    /// **Invariant this depends on:** dropping the *whole* capture is right
    /// only while each capture feeds exactly one room. Once fan-out lands, this
    /// must remove just `handle`'s destinations and drop the capture only when
    /// its `destinations` map is left empty — otherwise leaving one room tears
    /// down a pipeline another room is still using, the exact failure that
    /// `App`-owned (rather than room-owned) captures exist to prevent. The
    /// `debug_assert!` below fails loudly in tests if a later stage adds a
    /// second destination without revisiting this function.
    fn disconnect_room(&mut self, handle: RoomHandle) {
        let Some(room) = self.rooms.shift_remove(&handle) else { return };
        for destination_id in room.destinations {
            let Some((capture_id, capture, _)) = find_destination(&self.captures, destination_id) else {
                continue;
            };
            debug_assert_eq!(
                capture.destinations.len(),
                1,
                "disconnect_room drops whole captures; capture {capture_id:?} feeds several rooms, \
                 so it must instead lose only this room's destination (see this method's docs)"
            );
            self.drop_capture_fire_and_forget(capture_id);
        }
        if self.active_room == Some(handle) {
            self.active_room = self.rooms.keys().next().copied();
        }
        // Back on the join screen with nothing joined: reset the two pieces
        // of `App`-level capture state that used to die with the singleton
        // `Connected`, so a rejoin looks exactly like a first join (a fresh
        // audio-source listing, no stale error banner) rather than inheriting
        // the previous session's leftovers. Deliberately *only* when the last
        // room goes: with several rooms joined, one leaving must not disturb
        // what the others are showing.
        if self.rooms.is_empty() {
            self.audio_sources_requested = false;
            self.share_error = None;
        }
    }

    fn poll_connect_result(&mut self) {
        let Some(rx) = &mut self.connect_rx else { return };
        match rx.try_recv() {
            Ok(Ok((client, snapshot, event_rx))) => {
                self.status = Status::Idle;
                self.connect_rx = None;
                let handle = self.insert_room(client, snapshot, event_rx);
                // Single-room UI for now: the newest join is what the
                // connected screen shows. The multi-room sidebar replaces
                // this with an explicit selection.
                self.active_room = Some(handle);
            }
            Ok(Err(e)) => {
                self.status = Status::Error(e);
                self.connect_rx = None;
            }
            Err(oneshot::error::TryRecvError::Empty) => {}
            Err(oneshot::error::TryRecvError::Closed) => {
                self.status = Status::Error(t!("errors.connect_task_ended").to_string());
                self.connect_rx = None;
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
    fn drain_events(&mut self, ctx: &egui::Context) {
        let mut closed: Vec<(RoomHandle, String)> = Vec::new();
        let App { rooms, captures, .. } = self;

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
                        if let Some(LocalCapture { status: ShareStatus::Active(ShareHandle::Video(h)), .. }) =
                            capture_feeding(captures, handle, stream_id)
                        {
                            h.notify_subscriber_count(count);
                        }
                    }
                    ClientEvent::KeyframeRequest { stream_id } => {
                        if let Some(LocalCapture { status: ShareStatus::Active(ShareHandle::Video(h)), .. }) =
                            capture_feeding(captures, handle, stream_id)
                        {
                            h.notify_keyframe_request();
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
                        // time, without ever forwarding it here. This keeps
                        // media delivery fully decoupled from this function's
                        // (and `App::update()`'s) throttled repaint cadence,
                        // closing the "fast-forward catch-up" backlog bug —
                        // see `spawn_media_ingestion_task`'s doc comment for
                        // the full root-cause writeup. Kept as an explicit
                        // no-op arm (rather than folded into a wildcard, or
                        // `unreachable!()`) purely so this `match` stays
                        // exhaustive against `ClientEvent` and self-documents
                        // where the frames actually went.
                    }
                    ClientEvent::Preview(preview) => match decode::decode_preview(&preview) {
                        Ok(image) => {
                            let key = (preview.conn_id, preview.stream_id);
                            // Upload/update the roster thumbnail texture right
                            // away, mirroring `WatchEntry`'s
                            // create-once-then-`set`-in-place pattern (see
                            // `update_watch_textures`) rather than reuploading
                            // an unchanged image every repaint.
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
        }

        for (handle, reason) in closed {
            self.status = Status::Error(t!("errors.disconnected", reason = reason).to_string());
            self.disconnect_room(handle);
        }
    }

    /// Global settings card: the "English"/"Deutsch" language switcher and
    /// the render presentation-mode dropdown. Language names are
    /// deliberately left untranslated (a language selector conventionally
    /// shows each option in its own language, e.g. "Deutsch" rather than
    /// "German" while in English mode) so a user can always find their way
    /// back regardless of the current locale.
    ///
    /// Both settings are global (not per-stream/per-room), which is why
    /// this card lives on the join screen rather than anywhere inside a
    /// connected session.
    fn ui_settings_card(&mut self, ui: &mut egui::Ui) {
        theme::card(ui, t!("settings.header").as_ref(), |ui| {
            theme::setting_row(ui, t!("settings.language_label").as_ref(), |ui| {
                let mut changed = false;
                if ui.selectable_label(self.language == "en", "English").clicked() && self.language != "en" {
                    self.language = "en".to_string();
                    changed = true;
                }
                if ui.selectable_label(self.language == "de", "Deutsch").clicked() && self.language != "de" {
                    self.language = "de".to_string();
                    changed = true;
                }
                if changed {
                    rust_i18n::set_locale(&self.language);
                    save_language(&self.language);
                }
            });

            theme::setting_row(ui, t!("settings.presentation_mode_label").as_ref(), |ui| {
                let mut changed = false;
                egui::ComboBox::from_id_salt("presentation_mode_combo")
                    .selected_text(self.presentation_mode.label())
                    .show_ui(ui, |ui| {
                        for mode in PresentationMode::ALL {
                            if ui.selectable_value(&mut self.presentation_mode, mode, mode.label()).changed() {
                                changed = true;
                            }
                        }
                    });
                if changed {
                    save_presentation_mode(self.presentation_mode);
                }
                // Runtime present-mode swap isn't available with this pinned
                // eframe/egui-wgpu (see superdoc/features/presentation-modes.md):
                // the surface is only ever configured once, at window
                // creation. Surface this as an inline hint rather than
                // silently doing nothing until the next launch.
                if self.presentation_mode != self.launch_presentation_mode {
                    ui.label(egui::RichText::new(t!("settings.presentation_mode_restart_hint").to_string()).color(theme::TEXT_FAINT).italics());
                }
            });
        });
    }

    fn ui_join_screen(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().auto_shrink([false, true]).show(ui, |ui| {
            ui.set_max_width(theme::PANEL_MAX_WIDTH);
            ui.spacing_mut().item_spacing.y = 0.0;

            ui.heading(egui::RichText::new(t!("app.title").to_string()).color(theme::TEXT));
            ui.add_space(theme::SPACING);

            self.ui_settings_card(ui);

            theme::card(ui, t!("join.button").as_ref(), |ui| {
                theme::setting_row(ui, t!("join.display_name_label").as_ref(), |ui| {
                    ui.text_edit_singleline(&mut self.display_name);
                });
                theme::setting_row(ui, t!("join.link_label").as_ref(), |ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.join_string).desired_width(ui.available_width()));
                });

                let connecting = matches!(self.status, Status::Connecting);
                ui.add_enabled_ui(!connecting && !self.join_string.trim().is_empty(), |ui| {
                    if ui.add(theme::primary_button(t!("join.button").to_string())).clicked() {
                        self.start_connect();
                    }
                });

                match &self.status {
                    Status::Idle => {}
                    Status::Connecting => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(t!("join.connecting").to_string());
                        });
                    }
                    Status::Error(e) => {
                        ui.colored_label(theme::DANGER, t!("join.error", error = e).to_string());
                    }
                }
            });

            theme::card(ui, t!("join.saved_lounges").as_ref(), |ui| {
                if let Some(err) = &self.room_store_error {
                    ui.colored_label(theme::DANGER, t!("join.load_error", error = err).to_string());
                }
                if self.room_store.rooms.is_empty() {
                    ui.label(egui::RichText::new(t!("join.no_saved_rooms").to_string()).color(theme::TEXT_FAINT));
                } else {
                    let mut to_join: Option<String> = None;
                    let mut to_remove: Option<String> = None;
                    for room in &self.room_store.rooms {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(&room.name).color(theme::TEXT));
                            if ui.add(theme::secondary_button(t!("join.button").to_string())).clicked() {
                                to_join = Some(room.join_string.clone());
                            }
                            if ui.add(theme::danger_button(t!("rooms.remove_button").to_string())).clicked() {
                                to_remove = Some(room.name.clone());
                            }
                        });
                    }
                    if let Some(join_string) = to_join {
                        self.join_string = join_string;
                        self.start_connect();
                    }
                    if let Some(name) = to_remove {
                        self.room_store.remove(&name);
                        if let Err(e) = self.room_store.save() {
                            self.room_store_error = Some(e.to_string());
                        }
                    }
                }
            });
        });
    }

    /// The connected screen, rendered for `active_room`.
    ///
    /// `self` is destructured up front rather than accessed field-by-field:
    /// the screen mixes room-scoped state (`rooms[active_room]`) with
    /// `App`-scoped capture state (`captures`, `share_error`, the audio-source
    /// list), and taking them as independent bindings is what lets both be
    /// live at once. The capture-side *actions* still can't run here — they
    /// need `&mut self` whole — so they're recorded and applied after the
    /// borrow ends, the same collect-then-apply idiom the stream list already
    /// used for its watch toggles.
    fn ui_connected_screen(&mut self, ui: &mut egui::Ui) {
        let Some(room_handle) = self.active_room else { return };

        let mut start_video = false;
        let mut start_audio = false;
        let mut refresh_audio = false;
        let mut to_stop: Vec<CaptureSourceId> = Vec::new();

        // Borrows of these fields end at their last use below (NLL), which is
        // what lets the deferred `&mut self` actions run at the bottom.
        let App {
            rooms,
            captures,
            join_string,
            display_name,
            room_store,
            room_store_error,
            share_error,
            bitrate_kbps,
            framerate,
            audio_sources,
            audio_sources_error,
            audio_sources_requested,
            audio_sources_rx,
            selected_audio_source,
            ..
        } = self;
        let Some(room) = rooms.get_mut(&room_handle) else { return };

        ui.heading(egui::RichText::new(t!("app.title").to_string()).color(theme::TEXT));
        ui.label(
            egui::RichText::new(
                t!(
                    "connected.status",
                    name = display_name.clone(),
                    conn_id = format!("{:016x}", room.client.conn_id())
                )
                .to_string(),
            )
            .color(theme::TEXT_DIM),
        );
        ui.add_space(theme::SPACING);

        // A single control row, not its own card: one field plus one button
        // doesn't need card ceremony (see STYLING-GUIDE.md's card guidance —
        // cards are for *grouping* related controls).
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(t!("rooms.save_label").to_string()).color(theme::TEXT));
            ui.text_edit_singleline(&mut room.save_name);
            let can_save = !room.save_name.trim().is_empty();
            ui.add_enabled_ui(can_save, |ui| {
                if ui.add(theme::secondary_button(t!("rooms.save_button").to_string())).clicked() {
                    room_store.upsert(room.save_name.clone(), join_string.clone());
                    if let Err(e) = room_store.save() {
                        *room_store_error = Some(e.to_string());
                    } else {
                        room.saved = true;
                    }
                }
            });
        });
        if room.saved {
            ui.colored_label(theme::SUCCESS, t!("rooms.saved_confirmation").to_string());
        }
        ui.add_space(theme::SPACING);

        theme::card(ui, t!("members.header", count = room.members.len()).as_ref(), |ui| {
            egui::ScrollArea::vertical().max_height(120.0).show(ui, |ui| {
                // Show all members, not just streamers (spec §8/§4.2: a
                // lurker with stream_count == 0 still appears in the
                // roster).
                for member in &room.members {
                    ui.horizontal(|ui| {
                        if member.stream_count > 0 {
                            ui.colored_label(theme::SUCCESS, "\u{25cf}");
                            let key = if member.stream_count == 1 { "members.stream_singular" } else { "members.stream_plural" };
                            ui.label(
                                egui::RichText::new(t!(key, name = member.display_name, count = member.stream_count).to_string())
                                    .color(theme::TEXT),
                            );
                        } else {
                            ui.colored_label(theme::TEXT_FAINT, "\u{25cb}");
                            ui.label(egui::RichText::new(&member.display_name).color(theme::TEXT_DIM));
                        }
                    });
                }
            });
        });

        theme::card(ui, t!("share.video_header").as_ref(), |ui| {
            theme::slider_row(ui, t!("share.bitrate_label").as_ref(), bitrate_kbps, MIN_BITRATE_KBPS..=MAX_BITRATE_KBPS, 50.0, " kbps");
            theme::setting_row(ui, t!("share.framerate_label").as_ref(), |ui| {
                egui::ComboBox::from_id_salt("framerate_combo")
                    .selected_text(format!("{} fps", *framerate))
                    .show_ui(ui, |ui| {
                        for &fps in &FRAMERATE_OPTIONS {
                            ui.selectable_value(framerate, fps, format!("{fps} fps"));
                        }
                    });
            });
            ui.horizontal(|ui| {
                // Always enabled: clicking again while a video share is
                // already active starts *another* capture (its own
                // destination and stream_id), re-prompting the OS portal
                // picker so the user can pick a different monitor/window.
                if ui.add(theme::primary_button(t!("share.screen_button").to_string())).clicked() {
                    start_video = true;
                }
                let starting_video = captures
                    .values()
                    .any(|c| c.kind == StreamKind::Video && matches!(c.status, ShareStatus::Starting(_)));
                if starting_video {
                    ui.spinner();
                    ui.label(egui::RichText::new(t!("share.waiting_permission").to_string()).color(theme::TEXT_DIM));
                }
            });
        });

        theme::card(ui, t!("share.audio_header").as_ref(), |ui| {
            ui.horizontal(|ui| {
                if !*audio_sources_requested {
                    *audio_sources_requested = true;
                    refresh_audio = true;
                }
                if ui.add(theme::secondary_button(t!("share.refresh_button").to_string())).clicked() {
                    refresh_audio = true;
                }
                if audio_sources_rx.is_some() {
                    ui.spinner();
                }
            });
            ui.horizontal(|ui| {
                let placeholder = t!("share.select_source_placeholder").to_string();
                let selected_text = selected_audio_source
                    .as_ref()
                    .map(|s| s.name.as_str())
                    .unwrap_or(placeholder.as_str());
                egui::ComboBox::from_label(t!("share.source_label").to_string())
                    .selected_text(selected_text)
                    .show_ui(ui, |ui| {
                        for source in audio_sources.iter() {
                            let selected = selected_audio_source.as_ref() == Some(source);
                            if ui.selectable_label(selected, &source.name).clicked() {
                                *selected_audio_source = Some(source.clone());
                            }
                        }
                    });
                let can_share_audio = selected_audio_source.is_some();
                ui.add_enabled_ui(can_share_audio, |ui| {
                    if ui.add(theme::primary_button(t!("share.audio_button").to_string())).clicked() {
                        start_audio = true;
                    }
                });
            });
            if audio_sources.is_empty() && audio_sources_rx.is_none() {
                ui.label(egui::RichText::new(t!("share.no_audio_sources").to_string()).color(theme::TEXT_FAINT));
            }
            if let Some(err) = audio_sources_error.as_ref() {
                ui.colored_label(theme::DANGER, t!("share.audio_sources_error", error = err).to_string());
            }
        });

        // What this client is sending *into this room*, walked through the
        // room's own `destinations` back-references. That ordering (the order
        // the user started them) replaces the old sort-by-stream_id: with
        // several rooms, `stream_id` is no longer a session-wide ordering at
        // all, just a per-connection label.
        theme::card(ui, t!("share.active_header").as_ref(), |ui| {
            if room.destinations.is_empty() {
                ui.label(egui::RichText::new(t!("share.none").to_string()).color(theme::TEXT_FAINT));
            } else {
                for destination_id in room.destinations.iter() {
                    let Some((capture_id, capture, dest)) = find_destination(captures, *destination_id) else {
                        continue;
                    };
                    let kind_label = match capture.kind {
                        StreamKind::Video => t!("share.kind_video").to_string(),
                        StreamKind::Audio => t!("share.kind_audio").to_string(),
                    };
                    let (state_label, stoppable) = match &capture.status {
                        ShareStatus::Starting(_) => (t!("share.state_starting").to_string(), false),
                        ShareStatus::Active(_) => (t!("share.state_active").to_string(), true),
                        ShareStatus::Stopping(_) => (t!("share.state_stopping").to_string(), false),
                    };
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(
                                t!(
                                    "share.active_line",
                                    stream_id = dest.stream_id,
                                    kind = kind_label,
                                    label = capture.label.clone(),
                                    state = state_label
                                )
                                .to_string(),
                            )
                            .color(theme::TEXT),
                        );
                        let stop_label = if capture.kind == StreamKind::Audio {
                            t!("share.stop_audio_button").to_string()
                        } else {
                            t!("share.stop_button").to_string()
                        };
                        ui.add_enabled_ui(stoppable, |ui| {
                            if ui.add(theme::danger_button(stop_label)).clicked() {
                                to_stop.push(capture_id);
                            }
                        });
                        // Sharer-side live encode rate: the encoder
                        // branch's actual output framerate (see
                        // `CaptureHandle::encoder_fps`), confirming the
                        // configured framerate is really being hit rather
                        // than just selected in the UI. Video shares
                        // only — audio has no comparable per-frame rate.
                        if let ShareStatus::Active(ShareHandle::Video(handle)) = &capture.status {
                            ui.label(egui::RichText::new(t!("share.fps_label", fps = handle.encoder_fps()).to_string()).color(theme::TEXT_DIM));
                        }
                    });
                    // Sender-side self-preview: a small live view of this
                    // machine's own captured video, so the person sharing can
                    // confirm what viewers actually see. Video shares only,
                    // and only once actually `Active` (a `CaptureHandle` — and
                    // therefore a self-preview frame source — only exists at
                    // that point). See `superdoc/features/self-preview.md`.
                    if capture.kind == StreamKind::Video && matches!(capture.status, ShareStatus::Active(_)) {
                        const SELF_PREVIEW_WIDTH: f32 = 160.0;
                        const SELF_PREVIEW_HEIGHT: f32 = SELF_PREVIEW_WIDTH * 9.0 / 16.0;
                        theme::section_label(ui, t!("share.self_preview_label").as_ref());
                        match &capture.self_preview_texture {
                            Some(tex) => {
                                let tex_size = tex.size_vec2();
                                let scale = if tex_size.x > 0.0 { SELF_PREVIEW_WIDTH / tex_size.x } else { 1.0 };
                                ui.image((tex.id(), tex_size * scale));
                            }
                            None => {
                                let (rect, _) = ui
                                    .allocate_exact_size(egui::vec2(SELF_PREVIEW_WIDTH, SELF_PREVIEW_HEIGHT), egui::Sense::hover());
                                ui.painter().rect_filled(rect, 4.0, theme::FIELD);
                            }
                        }
                    }
                }
            }
            if let Some(err) = share_error.as_ref() {
                ui.colored_label(theme::DANGER, t!("share.error", error = err).to_string());
            }
        });

        theme::card(ui, t!("streams.header").as_ref(), |ui| {
            if let Some(err) = &room.watch_error {
                ui.colored_label(theme::DANGER, t!("streams.watch_error", error = err).to_string());
            }
            if room.streams.is_empty() {
                ui.label(egui::RichText::new(t!("streams.none_active").to_string()).color(theme::TEXT_FAINT));
            } else {
                let mut to_toggle: Option<(ConnId, StreamId)> = None;
                let mut to_pop_out: Option<(ConnId, StreamId)> = None;
                let mut to_toggle_listen: Option<(ConnId, StreamId)> = None;
                let unknown_owner = t!("streams.unknown_owner").to_string();
                for info in &room.streams {
                    let key = (info.owner, info.stream_id);
                    let owner_name = room
                        .members
                        .iter()
                        .find(|m| m.conn_id == info.owner)
                        .map(|m| m.display_name.as_str())
                        .unwrap_or(unknown_owner.as_str());

                    if info.kind == StreamKind::Audio {
                        // A sender with video gets their audio
                        // auto-subscribed via the video row's Watch button
                        // (see `RoomSession::toggle_watch`) — only an
                        // audio-only sender (no video announced) gets its
                        // own "Listen" row here.
                        if room.owner_has_video(info.owner) {
                            continue;
                        }
                        let listening = room.audio_watches.lock().unwrap().contains_key(&key);
                        ui.horizontal(|ui| {
                            ui.vertical(|ui| {
                                ui.label(
                                    egui::RichText::new(
                                        t!(
                                            "streams.audio_line",
                                            owner = owner_name,
                                            label = info.label.clone()
                                        )
                                        .to_string(),
                                    )
                                    .color(theme::TEXT),
                                );
                                ui.horizontal(|ui| {
                                    let button_label = if listening {
                                        t!("streams.stop_listening_button").to_string()
                                    } else {
                                        t!("streams.listen_button").to_string()
                                    };
                                    if ui.small_button(button_label).clicked() {
                                        to_toggle_listen = Some(key);
                                    }
                                    if listening {
                                        ui.colored_label(theme::SUCCESS, t!("streams.listening").to_string());
                                    }
                                });
                                let mut audio_watches = room.audio_watches.lock().unwrap();
                                if let Some(audio_watch) = audio_watches.get_mut(&key)
                                    && theme::slider_row(ui, t!("audio.volume_label").as_ref(), &mut audio_watch.volume_percent, 0..=200, 1.0, "%")
                                {
                                    audio_watch.decoder.set_volume(audio_watch.volume_percent as f64 / 100.0);
                                }
                            });
                        });
                        continue;
                    }

                    let watching = room.watches.lock().unwrap().contains_key(&key);
                    ui.horizontal(|ui| {
                        // Thumbnail (or a placeholder box until the first
                        // preview for this stream arrives) — previews
                        // arrive for every active stream regardless of
                        // whether we're watching it, so this shows up
                        // independent of the Watch/Stop Watching toggle
                        // below.
                        const THUMB_WIDTH: f32 = 120.0;
                        const THUMB_HEIGHT: f32 = THUMB_WIDTH * 9.0 / 16.0;
                        match room.preview_textures.get(&key) {
                            Some(tex) => {
                                let tex_size = tex.size_vec2();
                                let scale = if tex_size.x > 0.0 { THUMB_WIDTH / tex_size.x } else { 1.0 };
                                ui.image((tex.id(), tex_size * scale));
                            }
                            None => {
                                let (rect, _) =
                                    ui.allocate_exact_size(egui::vec2(THUMB_WIDTH, THUMB_HEIGHT), egui::Sense::hover());
                                ui.painter().rect_filled(rect, 4.0, theme::FIELD);
                            }
                        }

                        let popped_out = room.watches.lock().unwrap().get(&key).is_some_and(|e| e.popout.is_some());

                        ui.vertical(|ui| {
                            ui.label(
                                egui::RichText::new(
                                    t!(
                                        "streams.line",
                                        owner = owner_name,
                                        label = info.label.clone(),
                                        width = info.width,
                                        height = info.height
                                    )
                                    .to_string(),
                                )
                                .color(theme::TEXT),
                            );
                            ui.horizontal(|ui| {
                                let button_label = if watching {
                                    t!("streams.stop_watching_button").to_string()
                                } else {
                                    t!("streams.watch_button").to_string()
                                };
                                if ui.small_button(button_label).clicked() {
                                    to_toggle = Some(key);
                                }
                                if watching {
                                    // Pop out into its own native window
                                    // (or, once popped out, this just
                                    // reports state — closing that window
                                    // is how it comes back).
                                    ui.add_enabled_ui(!popped_out, |ui| {
                                        if ui.small_button(t!("streams.pop_out_button").to_string()).clicked() {
                                            to_pop_out = Some(key);
                                        }
                                    });
                                }
                                if popped_out {
                                    ui.colored_label(theme::ACCENT, t!("streams.popped_out").to_string());
                                } else if room.selected_watch == Some(key) {
                                    ui.colored_label(theme::SUCCESS, t!("streams.viewing").to_string());
                                }
                            });
                        });
                    });
                }
                if let Some((owner, stream_id)) = to_toggle {
                    room.toggle_watch(owner, stream_id);
                }
                if let Some((owner, stream_id)) = to_pop_out {
                    room.pop_out(owner, stream_id);
                }
                if let Some((owner, stream_id)) = to_toggle_listen {
                    room.toggle_listen(owner, stream_id);
                }
            }
        });

        theme::card(ui, t!("video.header").as_ref(), |ui| {
            // Viewer-side live FPS: how often the currently displayed
            // watch's decoder actually yields a *new* frame (see
            // `StreamDecoder::current_fps`), not just the UI's own repaint
            // rate — the most directly useful FPS readout, since it tells
            // you whether playback of what's on screen right now is
            // actually smooth. Reads through the same
            // `Arc<Mutex<StreamDecoder>>` `update_watch_textures`/the
            // popout closure poll `latest_frame()` through, so this
            // reflects real decode activity regardless of whether the
            // stream is currently shown inline or popped out into its own
            // window.
            if let Some(key) = room.selected_watch {
                let watches = room.watches.lock().unwrap();
                if let Some(entry) = watches.get(&key) {
                    let fps = entry.decoder.lock().unwrap().current_fps();
                    ui.label(egui::RichText::new(t!("video.fps_label", fps = format!("{fps:.1}")).to_string()).color(theme::TEXT_DIM));
                }
            }

            // If the currently displayed video watch also auto-subscribed
            // the same sender's audio (see `RoomSession::toggle_watch`), show
            // its volume slider here, right above the video panel —
            // "somewhere sensible in the watched-stream UI", per this
            // task's brief.
            let linked_audio_key: Option<(ConnId, StreamId)> = room.selected_watch.and_then(|key| {
                let audio_stream_id = room.watches.lock().unwrap().get(&key)?.audio_stream_id?;
                Some((key.0, audio_stream_id))
            });
            if let Some(audio_key) = linked_audio_key {
                let mut audio_watches = room.audio_watches.lock().unwrap();
                if let Some(audio_watch) = audio_watches.get_mut(&audio_key)
                    && theme::slider_row(ui, t!("audio.volume_label").as_ref(), &mut audio_watch.volume_percent, 0..=200, 1.0, "%")
                {
                    audio_watch.decoder.set_volume(audio_watch.volume_percent as f64 / 100.0);
                }
            }

            let watches = room.watches.lock().unwrap();
            match room.selected_watch.and_then(|key| watches.get(&key)) {
                Some(entry) if entry.popout.is_some() => {
                    ui.label(egui::RichText::new(t!("video.popped_out_note").to_string()).color(theme::TEXT_DIM));
                }
                Some(entry) => match &entry.texture {
                    Some(tex) => {
                        let available = ui.available_size();
                        let tex_size = tex.size_vec2();
                        let scale = if tex_size.x > 0.0 && tex_size.y > 0.0 {
                            (available.x / tex_size.x).min(available.y.max(60.0) / tex_size.y).min(1.0)
                        } else {
                            1.0
                        };
                        let display_size = tex_size * scale.max(0.01);
                        ui.image((tex.id(), display_size));
                    }
                    None => {
                        ui.label(egui::RichText::new(t!("video.waiting_frame").to_string()).color(theme::TEXT_DIM));
                    }
                },
                None => {
                    ui.label(egui::RichText::new(t!("video.not_watching").to_string()).color(theme::TEXT_FAINT));
                }
            }
        });

        // Capture-side actions, applied now that the room borrow is gone.
        if refresh_audio {
            self.refresh_audio_sources();
        }
        if start_video {
            self.start_video_share(room_handle);
        }
        if start_audio {
            self.start_audio_share(room_handle);
        }
        for capture_id in to_stop {
            self.stop_share(capture_id);
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_connect_result();
        self.drain_events(ctx);
        self.poll_capture_states();
        self.poll_audio_sources();
        self.update_self_preview_textures(ctx);

        // Every room's watches are polled and repainted, not just the one the
        // UI happens to be showing: a `StreamDecoder` that stops being fed
        // (or stops being drained) stalls, and a room being off-screen is a
        // view decision, not a reason to starve its decoders.
        let mut has_inline_watch = false;
        for (handle, room) in self.rooms.iter_mut() {
            has_inline_watch |= room.update_watch_textures(ctx);
            room.show_popouts(ctx, *handle);
        }

        // Whether the OS reports this window as focused right now. `None`
        // (backend doesn't report focus) is treated as focused, so a
        // platform that can't answer this question keeps today's behavior
        // rather than silently throttling forever.
        let focused = ctx.input(|i| i.viewport().focused).unwrap_or(true);

        // Keep the root context repainting every frame while at least one
        // stream is being watched inline (not popped out) *and* the window
        // is focused: `latest_frame()` is non-blocking and only yields a new
        // frame at the decoder's real output rate (up to 240fps per the
        // framerate-control feature, and no longer vsync-capped once a
        // faster `PresentationMode` is selected — see
        // `superdoc/features/presentation-modes.md`), so without an explicit
        // continuous repaint request here the panel would only actually
        // refresh on the side effect of unrelated input (e.g. mouse movement
        // over it) or the coarse idle-fallback interval below — capping
        // perceived playback well under the real decode rate whenever the
        // mouse sits still. Scoped to "currently watching something inline"
        // rather than requested unconditionally on every `update()` call, so
        // idle/disconnected sessions don't burn CPU on needless repaints.
        //
        // The focus check is what keeps an uncapped `PresentationMode` from
        // burning full GPU/CPU while the window sits in the background: it
        // only relaxes the *ceiling* wgpu imposes on render rate, so
        // something still has to stop asking for repaints when there's no
        // point rendering them, and that's this check, not the present mode.
        if has_inline_watch && focused {
            ctx.request_repaint();
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            if self.active_room.is_some() {
                self.ui_connected_screen(ui);
            } else {
                self.ui_join_screen(ui);
            }
        });

        // Keep polling the connect future / event channel even though egui
        // has no input to react to yet. Falls back to a much coarser
        // interval while unfocused (also covers the has_inline_watch case
        // above when the window is backgrounded) — see `focused` above.
        if matches!(self.status, Status::Connecting) || !self.rooms.is_empty() {
            let interval = if focused { IDLE_REPAINT_INTERVAL } else { UNFOCUSED_REPAINT_INTERVAL };
            ctx.request_repaint_after(interval);
        }
    }

    /// Called once on shutdown (window closed, app quit) — covers the same
    /// "abrupt teardown while sharing" gap as `ClientEvent::ControlClosed`,
    /// just triggered by the user closing the app instead of the connection
    /// dying underneath them.
    ///
    /// `eframe::App::on_exit` is a synchronous callback with no way to block
    /// on (or even poll) async work before eframe tears the window down, so
    /// this can't `.await` `.stop()` to completion the way the voluntary
    /// `stop_share` path does. Best effort: spawn `.stop()` for every running
    /// capture onto the ambient tokio runtime (mirroring `ControlClosed`'s
    /// `drop_capture_fire_and_forget`) so graceful teardown (portal
    /// session close, relay stop-stream notification) at least gets
    /// *attempted*, rather than relying solely on the handles' `Drop` impls
    /// once this process starts exiting.
    fn on_exit(&mut self) {
        self.stop_all_captures_fire_and_forget();
    }
}

#[cfg(test)]
mod session_model_tests {
    //! Pure (no I/O, no GStreamer, no network) coverage for the session
    //! model's cross-room lookups.
    //!
    //! The whole point of the rooms/captures/destinations split is that a
    //! protocol `StreamId` is only unique *within one connection*: two rooms
    //! will both hand out `stream_id` 0 for their first outgoing stream, and
    //! the old singleton `active_shares: HashMap<StreamId, _>` therefore had
    //! one entry where two were needed. These tests pin the replacement
    //! behaviour with that collision deliberately constructed.
    use super::*;

    /// A `LocalCapture` with the given destinations and a never-resolving
    /// `Starting` status — enough for the lookup functions, which never touch
    /// the pipeline handle.
    fn capture_with(destinations: IndexMap<DestinationId, Destination>) -> LocalCapture {
        let (_tx, rx) = oneshot::channel();
        LocalCapture {
            kind: StreamKind::Video,
            label: "test".to_string(),
            status: ShareStatus::Starting(rx),
            destinations,
            self_preview_texture: None,
        }
    }

    /// A `Destination` with default params and no attached runtime — the
    /// shape a destination has between "the user clicked Share" and "the
    /// capture handle came back" (see `Destination::runtime`).
    fn destination(room: RoomHandle, stream_id: StreamId) -> Destination {
        Destination {
            room,
            stream_id,
            params: capture::destination::VideoParams {
                width: 0,
                height: 0,
                framerate: DEFAULT_FRAMERATE,
                bitrate_kbps: DEFAULT_BITRATE_KBPS,
                backend: capture::EncoderBackend::default(),
            },
            runtime: None,
        }
    }

    /// Two rooms, each fed by its own capture, both using `stream_id` 0 —
    /// exactly the collision the old single-map keying could not represent.
    fn two_rooms_same_stream_id() -> (IndexMap<CaptureSourceId, LocalCapture>, RoomHandle, RoomHandle) {
        let (room_a, room_b) = (RoomHandle(0), RoomHandle(1));
        let mut captures = IndexMap::new();
        captures.insert(
            CaptureSourceId(10),
            capture_with(IndexMap::from([(DestinationId(100), destination(room_a, 0))])),
        );
        captures.insert(
            CaptureSourceId(11),
            capture_with(IndexMap::from([(DestinationId(101), destination(room_b, 0))])),
        );
        (captures, room_a, room_b)
    }

    /// A destination with no runtime attached yet reports `Starting` — the
    /// state a not-yet-started destination is genuinely in, rather than
    /// needing a separate "unknown" variant.
    #[test]
    fn destination_without_a_runtime_reports_starting() {
        let dest = destination(RoomHandle(0), 0);
        assert_eq!(dest.state(), capture::destination::DestinationState::Starting);
    }

    /// Once the capture handle lands, `poll_capture_states` attaches its
    /// runtime and the destination starts reporting the real state machine.
    #[test]
    fn attaching_a_runtime_makes_the_destination_report_the_real_state() {
        let mut dest = destination(RoomHandle(0), 0);
        let runtime = capture::destination::DestinationRuntime::shared(
            std::time::Instant::now(),
            dest.params,
        );
        dest.runtime = Some(runtime.clone());
        assert_eq!(dest.state(), capture::destination::DestinationState::Starting);

        // The pump sends the first IDR: Starting -> Live, visible through the
        // shared handle without the UI polling the capture at all.
        runtime
            .lock()
            .unwrap()
            .note_encoded_frame(true, b"idr", std::time::Instant::now());
        assert_eq!(dest.state(), capture::destination::DestinationState::Live);
    }

    #[test]
    fn capture_feeding_disambiguates_the_same_stream_id_in_two_rooms() {
        let (captures, room_a, room_b) = two_rooms_same_stream_id();

        // Each room's stream 0 must resolve to *its own* capture. Identity is
        // checked by pointer comparison against the map entry, since
        // `LocalCapture` holds a channel receiver and isn't `PartialEq`.
        let a = capture_feeding(&captures, room_a, 0).expect("room A's stream 0 must resolve");
        let b = capture_feeding(&captures, room_b, 0).expect("room B's stream 0 must resolve");
        assert!(std::ptr::eq(a, &captures[&CaptureSourceId(10)]), "room A's stream 0 resolved to the wrong capture");
        assert!(std::ptr::eq(b, &captures[&CaptureSourceId(11)]), "room B's stream 0 resolved to the wrong capture");
        assert!(!std::ptr::eq(a, b), "the two rooms' stream 0 must not resolve to the same capture");

        // A stream_id nobody announced in that room is a clean miss, never a
        // wrong hit borrowed from the other room.
        assert!(capture_feeding(&captures, room_a, 1).is_none());
        assert!(capture_feeding(&captures, RoomHandle(2), 0).is_none(), "an unknown room must never match");
    }

    #[test]
    fn find_destination_resolves_a_room_back_reference_to_its_owning_capture() {
        let (captures, room_a, room_b) = two_rooms_same_stream_id();

        let (capture_id, _, dest) = find_destination(&captures, DestinationId(101)).expect("destination must resolve");
        assert_eq!(capture_id, CaptureSourceId(11));
        assert_eq!(dest.room, room_b);
        assert_eq!(dest.stream_id, 0);

        let (capture_id, _, dest) = find_destination(&captures, DestinationId(100)).expect("destination must resolve");
        assert_eq!(capture_id, CaptureSourceId(10));
        assert_eq!(dest.room, room_a);

        // Ids come from a counter that never rewinds, so a back-reference to
        // something already dropped can only ever miss.
        assert!(find_destination(&captures, DestinationId(999)).is_none());
    }

    #[tokio::test]
    async fn session_ids_are_monotonic_and_allocated_from_separate_spaces() {
        // `App::default()` binds a real UDP socket for the shared endpoint,
        // so this needs a runtime entered — hence `#[tokio::test]`.
        let mut app = App::default();
        assert_eq!(app.alloc_capture_id(), CaptureSourceId(0));
        assert_eq!(app.alloc_capture_id(), CaptureSourceId(1));
        // Allocating a destination id must not disturb the capture counter,
        // and vice versa — they are separate spaces on purpose.
        assert_eq!(app.alloc_destination_id(), DestinationId(0));
        assert_eq!(app.alloc_capture_id(), CaptureSourceId(2));
        assert_eq!(app.alloc_destination_id(), DestinationId(1));
    }
}

#[cfg(test)]
mod tests {
    //! Live regression coverage for the "watched-video panel only updates at
    //! full rate while the mouse moves" bug: `update_watch_textures` used to
    //! upload freshly decoded frames without ever telling egui a repaint was
    //! worth scheduling, so the panel's real refresh cadence was left to
    //! whatever repaints happened to occur for unrelated reasons (input
    //! events, or `App::update`'s coarse idle-fallback
    //! `request_repaint_after`) rather than tracking the decoder's actual
    //! output rate.
    //!
    //! This drives the exact same `RoomSession`/`App` types and methods
    //! `eframe` calls in production (`drain_events`, `update_watch_textures`)
    //! against a real relay server child process, a real
    //! `videotestsrc ! clockoverlay ! x264enc` capture pipeline (see
    //! `end_to_end_test`'s module docs for why this is the right level of
    //! "real" for a headless, unattended test), and a real `StreamDecoder`
    //! — with **zero synthetic input ever fed into the `egui::Context`** —
    //! and asserts that over a several-second run, the decoder's real
    //! observed output rate (`StreamDecoder::current_fps`) comfortably
    //! clears the old ~30fps cap this bug imposed, proving repeated
    //! `update_watch_textures` polling (the mechanism `update()`'s new
    //! `ctx.request_repaint()` call keeps alive) actually keeps pace with a
    //! fast source rather than idling at the old fallback rate.
    //!
    //! [`media_ingestion_keeps_feeding_the_decoder_while_drain_events_is_starved`]
    //! below is the sibling regression test for the *other* fps bug fixed in
    //! the same stage — "fast-forward catch-up" — using the same real
    //! relay/capture/decoder infrastructure; see that test's own doc comment
    //! for why it starves `drain_events` specifically rather than the whole
    //! `App::update()` call.
    use super::*;
    use crate::end_to_end_test::{connect_with_retry, pick_free_udp_port, spawn_relay_server};
    use crate::net::format_join_string;
    use crate::protocol::RoomId;
    use std::time::{Duration, Instant};

    const TEST_STREAM_ID: StreamId = 1;
    /// Comfortably above the ~20-30fps this bug capped playback at while the
    /// mouse was idle, and well within what a software x264enc/avdec_h264
    /// round trip can sustain on ordinary test hardware — high enough to
    /// make the regression obvious without flaking on slower CI-like boxes.
    const TEST_FPS: u32 = 100;

    #[tokio::test]
    #[ignore = "spawns a real relay server child process and a real GStreamer capture/decode \
                pipeline; run explicitly with `cargo test ui:: -- --ignored --nocapture`"]
    async fn watched_panel_repaints_continuously_without_input_at_real_decode_rate() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        gstreamer::init().expect("gstreamer init");

        let port = pick_free_udp_port().expect("failed to pick a free UDP port for the test relay");
        eprintln!("[ui-repaint] starting relay server on 127.0.0.1:{port}");
        let _server = spawn_relay_server(port).expect("failed to spawn relay server child process");

        let server_addr = format!("127.0.0.1:{port}");
        let room_id: RoomId = rand::random();
        let room_key: [u8; 32] = rand::random();
        let join_string = format_join_string(&server_addr, &room_id, &room_key);

        eprintln!("[ui-repaint] connecting sharer + watcher");
        let (client_a, _snapshot_a, events_a) = connect_with_retry(&join_string, "sharer")
            .await
            .expect("sharer failed to connect");
        let (client_b, snapshot_b, events_b) = connect_with_retry(&join_string, "watcher")
            .await
            .expect("watcher failed to connect");
        // Keep client A's event channel alive (dropping it makes net.rs's
        // reader loop treat sends as errors and tear the connection down),
        // exactly as `end_to_end_test` does.
        let _events_a_keepalive = events_a;

        // A real `clockoverlay`-stamped high-framerate source: visually
        // exactly what a human would eyeball for this same check (a ticking
        // timestamp that should keep advancing smoothly), and a real
        // pipeline the fix has to keep pace with.
        let source_fragment = format!(
            "videotestsrc is-live=true pattern=ball \
             ! clockoverlay time-format=\"%H:%M:%S%.3f\" \
             ! video/x-raw,width=320,height=240,framerate={TEST_FPS}/1"
        );
        eprintln!("[ui-repaint] starting real capture pipeline on the sharer at {TEST_FPS}fps");
        let capture_handle = capture::start_sharing_from_test_source(&client_a, TEST_STREAM_ID, 4000, TEST_FPS, &source_fragment)
            .await
            .expect("start_sharing_from_test_source failed");
        let owner = client_a.conn_id();

        // Stand the room up through the exact same `App::insert_room` call
        // `poll_connect_result` uses in production, then wire up a watch on
        // the sharer's stream the same way `toggle_watch` does — this is the
        // real code under test, not a re-implementation of it.
        let mut app = App { framerate: TEST_FPS, ..App::default() };
        let room_handle = app.insert_room(client_b, snapshot_b, events_b);
        app.active_room = Some(room_handle);
        let decoder = StreamDecoder::new(TEST_STREAM_ID).expect("failed to build StreamDecoder");
        app.rooms[&room_handle].watches.lock().unwrap().insert(
            (owner, TEST_STREAM_ID),
            WatchEntry { decoder: Arc::new(Mutex::new(decoder)), texture: None, popout: None, audio_stream_id: None },
        );

        room_client_b_subscribe(&mut app, room_handle, owner).await;

        // Drive `App::drain_events` + `RoomSession::update_watch_textures` —
        // the real production methods — in a tight loop with a bare
        // `egui::Context` fed `RawInput::default()` every pass: no mouse
        // position, no clicks, no keys, ever. `eframe::Frame` has no public
        // constructor outside the real winit/wgpu integration, so this
        // can't invoke the `eframe::App::update` trait method itself;
        // instead it reproduces `update()`'s one-line repaint decision
        // (`if has_inline_watch { ctx.request_repaint(); }`) around the same
        // real `update_watch_textures` call `update()` makes. Since this
        // loop supplies its own next iteration unconditionally (there's no
        // real winit event loop here honoring egui's repaint scheduling),
        // this can't by itself prove eframe would keep calling `update()`
        // this fast purely from the repaint request — that half of the
        // fix's mechanism is confirmed by code review (`ctx.request_repaint`
        // is exactly what `eframe`'s integration uses to decide when to
        // wake and redraw). What this loop *does* prove for real: with
        // nothing but repeated `update_watch_textures` polls (no simulated
        // mouse motion, no artificial delay), decode output actually keeps
        // flowing at a rate the fix's fallback interval alone could never
        // have delivered.
        let ctx = egui::Context::default();
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut polls_with_watch = 0u64;
        while Instant::now() < deadline {
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                app.drain_events(ctx);
                let Some(room) = app.rooms.get_mut(&room_handle) else { return };
                let has_inline_watch = room.update_watch_textures(ctx);
                if has_inline_watch {
                    polls_with_watch += 1;
                    ctx.request_repaint();
                }
            });
            // Yield back to the tokio runtime between passes: this test's
            // `#[tokio::test]` defaults to a current-thread executor, and a
            // purely synchronous loop body (no `.await` anywhere in it)
            // would otherwise starve every other task on that same thread
            // forever — including the QUIC connection's own background
            // receive task and, since `insert_room` above, the dedicated
            // `spawn_media_ingestion_task` for this room, which is what
            // actually turns incoming media datagrams into `feed()` calls on
            // this stream's `StreamDecoder` now (see that function's doc
            // comment) — `drain_events` above no longer sees `MediaFrame`
            // events at all, only presence/subscriber-count traffic. Not
            // part of the fix itself: purely this test harness's own
            // concession to sharing one OS thread with the networking (and
            // now ingestion task) it's exercising.
            tokio::task::yield_now().await;
        }

        let observed_fps = app.rooms[&room_handle]
            .watches
            .lock()
            .unwrap()
            .get(&(owner, TEST_STREAM_ID))
            .unwrap()
            .decoder
            .lock()
            .unwrap()
            .current_fps();

        eprintln!("[ui-repaint] {polls_with_watch} polls observed an active watch, decoder-observed fps = {observed_fps:.1}");

        if let Err(e) = capture_handle.stop().await {
            tracing::warn!("[ui-repaint] capture_handle.stop() reported an error (non-fatal): {e:#}");
        }

        assert!(polls_with_watch > 0, "the watch loop never observed an active inline watch at all");
        assert!(
            observed_fps > 40.0,
            "decoder-observed fps ({observed_fps:.1}) did not clear the old ~30fps idle cap — \
             the repaint-cadence fix does not appear to be keeping pace with the real decode rate"
        );
    }

    /// Regression test for the "fast-forward catch-up" bug (see
    /// `superdoc/brainstorm/ui-redesign-multiroom-streaming.md`,
    /// "Performance/Rendering" round 2, and `spawn_media_ingestion_task`'s
    /// doc comment for the full root-cause writeup).
    ///
    /// # What this isolates, and why
    ///
    /// Before this stage's fix, `ClientEvent::MediaFrame` -> `feed()` only
    /// ever happened *inside* `drain_events`, so `drain_events`'s own call
    /// frequency was, byte for byte, the decoder's *input* rate ceiling —
    /// and in production `drain_events` only runs as often as `App::update()`
    /// does, which drops to one call every `UNFOCUSED_REPAINT_INTERVAL`
    /// (500ms) whenever the OS reports the window unfocused. Reproducing
    /// that exact OS-focus-driven throttle end-to-end would need a real
    /// winit event loop and a real window manager to unfocus it — not
    /// practical in a headless test (the sibling
    /// `watched_panel_repaints_continuously_without_input_at_real_decode_rate`
    /// test's own doc comment notes the same limit for the *other* half of
    /// `App::update()`'s scheduling). What *is* directly testable, and is
    /// exactly the mechanism this stage's fix changed, is the dependency
    /// itself: does new media reach a decoder's `feed()` at all without
    /// `drain_events` running? So this test drives that dependency to its
    /// most extreme case — `drain_events` is **never called even once**
    /// during the timed portion of the run — while a separate, tight loop
    /// keeps polling `update_watch_textures` (decoded *output*) the whole
    /// time, exactly the way `App::update()` always does both together in
    /// production. Splitting them apart like this only requires trusting
    /// that a real production `App::update()` call always invokes
    /// `drain_events` and `update_watch_textures` in the same pass (true by
    /// inspection of `eframe::App::update` above) — this test just proves
    /// the ingestion side no longer needs that pass to happen at all, let
    /// alone at a full-focus rate.
    ///
    /// Before the fix, this would starve `StreamDecoder::feed()` completely
    /// for the whole run (nothing else ever called it), so `current_fps()`
    /// would sit at ~0 the entire time. After the fix,
    /// `insert_room`'s `spawn_media_ingestion_task` feeds the decoder
    /// straight off the raw connection channel regardless, so `current_fps()`
    /// tracks the real sender rate even with `drain_events` fully starved —
    /// proving media delivery no longer depends on it.
    #[tokio::test]
    #[ignore = "spawns a real relay server child process and a real GStreamer capture/decode \
                pipeline; run explicitly with `cargo test ui:: -- --ignored --nocapture`"]
    async fn media_ingestion_keeps_feeding_the_decoder_while_drain_events_is_starved() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        gstreamer::init().expect("gstreamer init");

        let port = pick_free_udp_port().expect("failed to pick a free UDP port for the test relay");
        eprintln!("[ingestion] starting relay server on 127.0.0.1:{port}");
        let _server = spawn_relay_server(port).expect("failed to spawn relay server child process");

        let server_addr = format!("127.0.0.1:{port}");
        let room_id: RoomId = rand::random();
        let room_key: [u8; 32] = rand::random();
        let join_string = format_join_string(&server_addr, &room_id, &room_key);

        eprintln!("[ingestion] connecting sharer + watcher");
        let (client_a, _snapshot_a, events_a) = connect_with_retry(&join_string, "sharer")
            .await
            .expect("sharer failed to connect");
        let (client_b, snapshot_b, events_b) = connect_with_retry(&join_string, "watcher")
            .await
            .expect("watcher failed to connect");
        let _events_a_keepalive = events_a;

        let source_fragment = format!(
            "videotestsrc is-live=true pattern=ball \
             ! clockoverlay time-format=\"%H:%M:%S%.3f\" \
             ! video/x-raw,width=320,height=240,framerate={TEST_FPS}/1"
        );
        eprintln!("[ingestion] starting real capture pipeline on the sharer at {TEST_FPS}fps");
        let capture_handle = capture::start_sharing_from_test_source(&client_a, TEST_STREAM_ID, 4000, TEST_FPS, &source_fragment)
            .await
            .expect("start_sharing_from_test_source failed");
        let owner = client_a.conn_id();

        // `insert_room` is where `spawn_media_ingestion_task` actually gets
        // spawned in production — using it here (not a hand-rolled
        // `RoomSession`) means this test exercises the real wiring, not a
        // re-implementation of it.
        let mut app = App { framerate: TEST_FPS, ..App::default() };
        let room_handle = app.insert_room(client_b, snapshot_b, events_b);
        app.active_room = Some(room_handle);
        let decoder = StreamDecoder::new(TEST_STREAM_ID).expect("failed to build StreamDecoder");
        app.rooms[&room_handle].watches.lock().unwrap().insert(
            (owner, TEST_STREAM_ID),
            WatchEntry { decoder: Arc::new(Mutex::new(decoder)), texture: None, popout: None, audio_stream_id: None },
        );

        // The one and only `drain_events` call in this whole test — needed
        // to observe the `StreamStarted` presence delta before subscribing
        // (the relay silently drops a `Subscribe` for an unknown stream).
        // Everything from here to the fps check runs with `drain_events`
        // never called again.
        room_client_b_subscribe(&mut app, room_handle, owner).await;

        eprintln!(
            "[ingestion] drain_events will not be called again for the rest of this test \
             (simulating it being starved by App::update()'s unfocused-window throttle)"
        );
        let ctx = egui::Context::default();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut polls_with_watch = 0u64;
        while Instant::now() < deadline {
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                // Deliberately *not* calling `app.drain_events(ctx)` here —
                // see this test's doc comment. Only decoded-output polling
                // happens in this loop.
                let Some(room) = app.rooms.get_mut(&room_handle) else { return };
                let has_inline_watch = room.update_watch_textures(ctx);
                if has_inline_watch {
                    polls_with_watch += 1;
                    ctx.request_repaint();
                }
            });
            // Same rationale as the sibling test: yield to the tokio
            // runtime so the connection's background receive task *and* the
            // per-room media-ingestion task this fix added both keep
            // running on this test's single-threaded executor.
            tokio::task::yield_now().await;
        }

        let observed_fps = app.rooms[&room_handle]
            .watches
            .lock()
            .unwrap()
            .get(&(owner, TEST_STREAM_ID))
            .unwrap()
            .decoder
            .lock()
            .unwrap()
            .current_fps();

        eprintln!(
            "[ingestion] {polls_with_watch} polls observed an active watch (drain_events called 0 times \
             during this loop), decoder-observed fps = {observed_fps:.1}"
        );

        if let Err(e) = capture_handle.stop().await {
            tracing::warn!("[ingestion] capture_handle.stop() reported an error (non-fatal): {e:#}");
        }

        assert!(polls_with_watch > 0, "the watch loop never observed an active inline watch at all");
        assert!(
            observed_fps > 40.0,
            "decoder-observed fps ({observed_fps:.1}) stayed near zero with drain_events starved — \
             media delivery still appears to depend on drain_events being called, i.e. the \
             'fast-forward catch-up' backlog bug is not actually fixed"
        );
    }

    /// Waits for the watcher to see the sharer's `StreamStarted` presence
    /// delta (the relay silently drops a `Subscribe` for a stream it
    /// doesn't know about yet) and then subscribes — mirrors
    /// `end_to_end_test`'s same wait, just against `RoomSession::event_rx`
    /// directly instead of a bare receiver.
    async fn room_client_b_subscribe(app: &mut App, room_handle: RoomHandle, owner: ConnId) {
        let connected = app.rooms.get_mut(&room_handle).expect("room must be present");
        let saw_stream_started = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match connected.event_rx.recv().await {
                    Some(ClientEvent::PresenceDelta(PresenceEvent::StreamStarted(info)))
                        if info.owner == owner && info.stream_id == TEST_STREAM_ID =>
                    {
                        return true;
                    }
                    Some(_) => continue,
                    None => return false,
                }
            }
        })
        .await;
        assert_eq!(saw_stream_started, Ok(true), "watcher never observed the sharer's StreamStarted delta");

        connected.client.subscribe(owner, TEST_STREAM_ID).await.expect("subscribe() failed");
    }
}
