//! egui UI: the always-on app shell (left sidebar + right content area),
//! presence roster, screen sharing, and stream watching.
//!
//! # App shell (sidebar + nav tabs)
//!
//! There is no join *screen* any more — no one-time gate the app passes
//! through once and never shows again. The window is permanently split into a
//! left sidebar (`ui_sidebar`) and a right content area (`ui_content`), and
//! joining a room is just one affordance inside the sidebar's ROOMS section
//! rather than a mode the whole app is in. See
//! `superdoc/ui/APP-SHELL.md` for the layout, its `Why:` notes, and which
//! parts are deliberately minimal mount points for later stages.
//!
//! - **Sidebar**, top to bottom: wordmark / nav rail (three
//!   [`NavTab`] pills) / ROOMS (scrollable, greedy, with "+ Add Room" pinned
//!   to its bottom) / STREAMS (capped height, own scroll).
//! - **Content area** renders whichever [`NavTab`] is selected.
//!
//! **The nav is a pure view-switcher.** Nothing in `set_nav_tab` or in any
//! `ui_*_tab` function starts or stops a capture, a subscription, or a
//! decoder; every one of those transitions is an explicit user action
//! (`toggle_watch`, `start_video_share`, …) reachable from the sidebar, which
//! is rendered on every tab. `eframe::App::update` likewise polls *every*
//! room's watches and re-declares *every* popout viewport before the tab is
//! even consulted, so a stream keeps decoding, playing and repainting while
//! its tab isn't showing.
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
//! `DestinationState` (see `Destination::state`).
//!
//! The **sidebar** is genuinely multi-room: ROOMS lists every joined room at
//! once and STREAMS lists this client's outgoing destinations across *all* of
//! them. The **content area** is not yet: `active_room` names the one
//! `RoomSession` the Stream Control and View tabs drive, re-pointed by
//! clicking a room row. Retiring it belongs to the stages that add
//! multi-destination fan-out (Stream Control) and the one global tile grid
//! across every room (View); nothing in the data model depends on it.

use std::collections::{HashMap, HashSet};
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
// App-shell layout constants
// ---------------------------------------------------------------------

/// Fixed width of the always-on left sidebar. Not resizable: every element in
/// it (nav pills, room rows, stream rows) is designed against one measure, and
/// a draggable splitter is user-facing complexity with nothing behind it.
const SIDEBAR_WIDTH: f32 = 268.0;
/// Ceiling on the STREAMS section's height, so a long list of outgoing
/// destinations scrolls inside its own well instead of squeezing ROOMS out of
/// the sidebar.
///
/// `Why` a cap rather than pinning STREAMS to the bottom and letting it grow:
/// multi-destination fan-out (a later stage) makes STREAMS' length a function
/// of how many rooms each capture feeds, so an uncapped section could take
/// over the whole rail. ROOMS is the sidebar's primary navigation and keeps
/// the greedy middle; STREAMS gets a bounded footer and its own scroll.
const STREAMS_MAX_HEIGHT: f32 = 176.0;
/// Period of the reconnecting dot's opacity pulse, in seconds.
const RECONNECT_PULSE_PERIOD: f64 = 1.2;

// Icon glyphs. Deliberately drawn from Latin-1 / Geometric Shapes / the
// emoji-icon-font subset egui's own widgets rely on (`🗙` is what
// `egui::Window`'s close button uses), because this project has not installed
// the Geist Mono Nerd Font icon fallback its styling guide calls for — see
// STYLING-GUIDE.md's "Fonts" status note. Anything from a richer icon set
// would render as tofu today.
const ICON_WATCH: &str = "\u{25b6}"; // ▶
const ICON_STOP: &str = "\u{25a0}"; // ■
const ICON_CARET_COLLAPSED: &str = "\u{25b8}"; // ▸
const ICON_CARET_EXPANDED: &str = "\u{25be}"; // ▾
const ICON_CLOSE: &str = "\u{1f5d9}"; // 🗙
const ICON_AUDIO: &str = "\u{1f50a}"; // 🔊

/// Which pill of the sidebar's nav rail is selected — i.e. what the right-hand
/// content area renders.
///
/// Purely a *view* selection: see this module's docs on why switching tabs
/// cannot start or stop any share or watch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NavTab {
    StreamControl,
    View,
    Settings,
}

impl NavTab {
    const ALL: [NavTab; 3] = [Self::StreamControl, Self::View, Self::Settings];

    fn label(self) -> String {
        match self {
            Self::StreamControl => t!("nav.stream_control").to_string(),
            Self::View => t!("nav.view").to_string(),
            Self::Settings => t!("nav.settings").to_string(),
        }
    }
}

impl Default for NavTab {
    /// `Why` Stream Control and not View or Settings on a zero-room first
    /// launch: with nothing joined, all three tabs are empty, so the default
    /// should be whichever empty state best answers "what do I do now?".
    /// Sharing your screen is what this app is *for* — Stream Control's empty
    /// state points straight at the sidebar's "+ Add Room" and is the same
    /// place the STREAMS section's "+ Start a Stream" button lands you, so
    /// first launch and first share both converge on one screen. View's empty
    /// state ("nobody's on screen") reads as a dead end on a fresh launch, and
    /// opening into Settings suggests configuration is a prerequisite, which
    /// it isn't.
    fn default() -> Self {
        Self::StreamControl
    }
}

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
    /// The `server|room-id|key` string this session was connected with.
    ///
    /// `Why` it lives on the session (it used to be a single `App::join_string`
    /// bound to the join form's text field): the sidebar renders `room_store ⟕
    /// rooms`, and the join string is the only key a `SavedRoom` and a live
    /// session both carry (see `RoomStore::name_for`). A single `App`-level
    /// field could only ever describe the *most recently used* join string,
    /// which stops meaning anything the moment two rooms are joined.
    join_string: String,
    /// Rename field for this room's `RoomStore` entry, bound to the Stream
    /// Control tab's "save this room as" row.
    save_name: String,
    /// Set once the user has saved/renamed this room from that row, so the
    /// confirmation shows exactly once per edit.
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

/// A connect attempt in flight, tagged with what it was for so its result can
/// be routed back to the row (or the "+ Add Room" card) that started it.
///
/// `Why` a struct instead of the bare `oneshot::Receiver` this used to be:
/// with an always-on ROOMS list there is no longer one join form owning the
/// only connect in the app. A saved row's `[Reconnect]` and the Add Room
/// card's `[Join]` both land here, and the sidebar has to know *which room row*
/// is the one currently showing the amber reconnecting dot — which needs the
/// join string, since a not-yet-connected room has no `RoomHandle`.
struct PendingConnect {
    rx: oneshot::Receiver<ConnectResult>,
    /// The room being connected to. Also what the reconnecting-dot lookup
    /// matches a `SavedRoom` row against.
    join_string: String,
    /// Whether this attempt came from the inline "+ Add Room" card, so a
    /// failure can be reported inside that card (which stays open) rather than
    /// as a detached error under the ROOMS list.
    from_add_card: bool,
}

pub struct App {
    display_name: String,
    status: Status,
    pending_connect: Option<PendingConnect>,

    /// Which nav pill is selected — i.e. what `ui_content` renders. Real,
    /// frame-persistent state on `App`, deliberately *not* derived from
    /// whether anything is joined/watching: a view selection the app silently
    /// overrode would make the rail's accent fill lie about where you are.
    nav_tab: NavTab,
    /// Whether the inline "+ Add Room" mini-card is expanded under the ROOMS
    /// section. Inline, not a modal (per the plan doc): joining is the most
    /// routine thing this app does and shouldn't blank the rest of the UI.
    add_room_open: bool,
    /// The join link typed into that card.
    add_room_join_string: String,
    /// Last failure from that card's Join, shown inline inside it.
    add_room_error: Option<String>,
    /// Rooms whose row is expanded to show their per-user stream rows.
    /// A set (not a single `Option`) so several rooms can be open at once —
    /// the point of the sidebar is seeing every room at a glance.
    expanded_rooms: HashSet<RoomHandle>,
    /// Room awaiting leave confirmation, if the confirm dialog is up. See
    /// `leave_needs_confirm`.
    leave_confirm: Option<RoomHandle>,
    /// Whether `theme::install` has run for this context yet. Once per
    /// process, not once per frame: it rewrites the whole `Visuals` struct.
    theme_installed: bool,

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
    /// Which room the (still single-room) Stream Control and View tabs drive.
    ///
    /// `Why:` the sidebar is multi-room, but the two content tabs' *contents*
    /// are a later stage (multi-destination fan-out; one global tile grid), so
    /// until then they need one designated room to render. Deliberately a
    /// view-level pointer that nothing in the data model depends on — deleting
    /// it is all that's needed once both tabs can show every room at once.
    /// Kept in sync by `poll_connect_result` (set on join), a click on a room
    /// row (re-pointed), and `disconnect_room` (re-pointed on leave).
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
            display_name: whoami_fallback(),
            status: Status::Idle,
            pending_connect: None,
            nav_tab: NavTab::default(),
            add_room_open: false,
            add_room_join_string: String::new(),
            add_room_error: None,
            expanded_rooms: HashSet::new(),
            leave_confirm: None,
            theme_installed: false,
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

/// One deferred sidebar interaction. Collected while the ROOMS list renders
/// (which holds shared borrows of `rooms`/`room_store`/`expanded_rooms`) and
/// applied by `App::apply_room_action` afterwards.
enum RoomAction {
    /// Point the content tabs at this room.
    Select(RoomHandle),
    /// Expand/collapse this room's per-stream rows.
    ToggleExpand(RoomHandle),
    /// Leave — possibly via the confirm dialog, see `leave_needs_confirm`.
    Leave(RoomHandle),
    /// Connect to a saved-but-disconnected room's join string.
    Reconnect(String),
    /// Forget a saved room, by its `RoomStore` name.
    Forget(String),
    Watch(RoomHandle, ConnId, StreamId),
    Listen(RoomHandle, ConnId, StreamId),
}

/// How many *other* people are streaming into this room right now — the
/// "N Active Users" in a joined room's collapsed row.
///
/// Distinct owners, not stream count: someone sharing two monitors is one
/// active user. Our own streams are excluded — "who else is on" is the
/// question the row answers.
fn active_user_count(room: &RoomSession) -> usize {
    let me = room.client.conn_id();
    room.streams
        .iter()
        .filter(|s| s.owner != me)
        .map(|s| s.owner)
        .collect::<HashSet<_>>()
        .len()
}

/// `WxH@FPS` for a *remote* stream, as announced by its sender.
///
/// `Why` the framerate is always `—`: `StreamInfo`/`AnnounceStream` carry
/// width and height but no `fps` field, and a subscriber genuinely cannot
/// derive one before subscribing and decoding. Adding that protocol field is
/// tracked in the plan doc's gap list and has to be coordinated with the relay
/// and the browser viewer; rendering the em-dash placeholder keeps this row
/// honest in the meantime rather than inventing a number or omitting the
/// column (which would make the sidebar's alignment change the day the field
/// lands).
fn remote_stream_geometry(info: &StreamInfo) -> String {
    if info.width == 0 || info.height == 0 {
        return t!("streams.geometry_unknown").to_string();
    }
    t!("streams.geometry", width = info.width, height = info.height, fps = "\u{2014}").to_string()
}

/// `WxH@FPS` for one of *our* outgoing destinations.
///
/// Prefers the live `DestinationRuntime`'s params over the `Destination`'s
/// own: the runtime's width/height are the pipeline's real negotiated caps
/// (read off the encoder appsink in `capture::start_sharing`), whereas the
/// destination's are the *requested* ones, which are `0` for "whatever the
/// source is" — the only value this stage ever sets.
fn destination_geometry(dest: &Destination) -> String {
    let params = match &dest.runtime {
        Some(runtime) => runtime.lock().unwrap().params(),
        None => dest.params,
    };
    if params.width == 0 || params.height == 0 {
        return t!("streams.geometry_unknown").to_string();
    }
    t!("streams.geometry", width = params.width, height = params.height, fps = params.framerate).to_string()
}

/// The label a room row shows: its saved name if it has one, otherwise a
/// stable short id derived from the join string's room-id field.
///
/// `Why` an id-derived fallback rather than showing the raw join string: the
/// join string contains the room *key*, i.e. the secret that grants access.
/// A sidebar is exactly the sort of thing that ends up in a screenshot or a
/// screen share, so it must never render one.
fn room_label(room_store: &RoomStore, join_string: &str) -> String {
    if let Some(name) = room_store.name_for(join_string) {
        return name.to_string();
    }
    t!("rooms.unnamed", id = room_id_prefix(join_string)).to_string()
}

/// First 4 bytes of the room id, hex — short enough to read in a sidebar row,
/// long enough to tell two rooms apart. Falls back to a placeholder for a
/// join string that doesn't parse (which can't have connected, so this is only
/// ever reachable for a corrupt `rooms.toml` entry).
fn room_id_prefix(join_string: &str) -> String {
    match crate::net::parse_join_string(join_string) {
        Ok(parsed) => parsed.room_id.iter().take(4).map(|b| format!("{b:02x}")).collect(),
        Err(_) => t!("rooms.unknown_id").to_string(),
    }
}

impl App {
    /// Kick off a connect to `join_string`. `from_add_card` routes a failure
    /// back into the inline "+ Add Room" card instead of the ROOMS list's own
    /// error line.
    fn start_connect(&mut self, join_string: String, from_add_card: bool) {
        let endpoint = match &self.endpoint {
            Ok(endpoint) => endpoint.clone(),
            Err(e) => {
                self.status = Status::Error(e.clone());
                if from_add_card {
                    self.add_room_error = Some(e.clone());
                }
                return;
            }
        };
        let display_name = self.display_name.clone();
        let (tx, rx) = oneshot::channel();
        self.pending_connect = Some(PendingConnect { rx, join_string: join_string.clone(), from_add_card });
        self.status = Status::Connecting;
        self.add_room_error = None;

        tokio::spawn(async move {
            let result = ScreenshareClient::connect(&endpoint, &join_string, &display_name)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
        });
    }

    /// Whether `join_string` is the room a connect is currently in flight for
    /// — what puts a saved row into the amber "reconnecting" state.
    fn is_reconnecting(&self, join_string: &str) -> bool {
        self.pending_connect.as_ref().is_some_and(|p| p.join_string == join_string)
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
        // Purely view state, but ids never rewind, so a stale entry would
        // linger for the process's lifetime rather than ever being reused.
        self.expanded_rooms.remove(&handle);
        if self.leave_confirm == Some(handle) {
            self.leave_confirm = None;
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

    /// Whether leaving `handle` should raise a confirm dialog first.
    ///
    /// `Why` both conditions and not just watching: the plan's final verify
    /// pass resolved that leaving a room you're *streaming to* gets the same
    /// treatment as leaving one you're watching — "you're killing an outgoing
    /// broadcast, not just navigating". A plain leave with neither is
    /// immediate: there is nothing to lose and a confirm would just be a
    /// speed bump on the most common case.
    ///
    /// Watching counts whether the tile is inline or popped out into its own
    /// OS window — `watches` holds both (`WatchEntry::popout` only says
    /// *where* it renders), and a popped-out window vanishing unannounced is
    /// if anything the more startling of the two.
    fn leave_needs_confirm(&self, handle: RoomHandle) -> bool {
        let Some(room) = self.rooms.get(&handle) else { return false };
        !room.destinations.is_empty()
            || !room.watches.lock().unwrap().is_empty()
            || !room.audio_watches.lock().unwrap().is_empty()
    }

    /// "Leave" clicked on a joined room row: tear it down now, or raise the
    /// confirm dialog first if there's active work in it.
    fn request_leave_room(&mut self, handle: RoomHandle) {
        if self.leave_needs_confirm(handle) {
            self.leave_confirm = Some(handle);
        } else {
            self.disconnect_room(handle);
        }
    }

    fn poll_connect_result(&mut self) {
        let Some(pending) = &mut self.pending_connect else { return };
        let result = match pending.rx.try_recv() {
            Ok(result) => result,
            Err(oneshot::error::TryRecvError::Empty) => return,
            Err(oneshot::error::TryRecvError::Closed) => Err(t!("errors.connect_task_ended").to_string()),
        };
        let Some(pending) = self.pending_connect.take() else { return };

        match result {
            Ok((client, snapshot, event_rx)) => {
                self.status = Status::Idle;
                let handle = self.insert_room(client, snapshot, event_rx, pending.join_string.clone());
                // A successful join *is* the save: `RoomStore` is this app's
                // only persistent "rooms I care about" state, and the plan's
                // Add Room flow deliberately has no separate staging step —
                // "on success the room appears live in the list, on failure
                // nothing is saved". Saved under an id-derived name, which the
                // Stream Control tab's rename row can replace; a room already
                // saved keeps whatever name it has.
                if self.room_store.name_for(&pending.join_string).is_none() {
                    let name = room_label(&self.room_store, &pending.join_string);
                    self.room_store.upsert(name, pending.join_string.clone());
                    if let Err(e) = self.room_store.save() {
                        self.room_store_error = Some(e.to_string());
                    }
                }
                // Newly joined room becomes what the content tabs drive, and
                // the Add Room card has served its purpose.
                self.active_room = Some(handle);
                self.add_room_open = false;
                self.add_room_join_string.clear();
                self.add_room_error = None;
            }
            Err(e) => {
                if pending.from_add_card {
                    // Keep the card open with the error inline, so the typed
                    // link is still there to fix rather than retype.
                    self.add_room_error = Some(e.clone());
                }
                self.status = Status::Error(e);
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

    /// Global settings: the "English"/"Deutsch" language switcher and the
    /// render presentation-mode dropdown. Language names are deliberately
    /// left untranslated (a language selector conventionally shows each
    /// option in its own language, e.g. "Deutsch" rather than "German" while
    /// in English mode) so a user can always find their way back regardless
    /// of the current locale.
    ///
    /// Both settings are global (not per-stream/per-room), which is why this
    /// card lives on the Settings nav tab rather than anywhere room-scoped.
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

    // -----------------------------------------------------------------
    // App shell — sidebar
    // -----------------------------------------------------------------

    /// The always-on left sidebar: wordmark / nav rail / ROOMS / STREAMS.
    ///
    /// `Why` nested egui panels rather than one vertical layout with
    /// hand-computed spacers: the plan's layout is "ROOMS consumes the greedy
    /// middle; STREAMS is capped and never squeezes it out". `TopBottomPanel`
    /// expresses exactly that — the head sizes to its content, the STREAMS
    /// footer to its content up to `STREAMS_MAX_HEIGHT`, and whatever is left
    /// goes to the central region. Spacers would have to re-derive the same
    /// split from `available_height()` every frame, and would get it wrong the
    /// moment either section's content changed height.
    fn ui_sidebar(&mut self, ui: &mut egui::Ui) {
        egui::TopBottomPanel::top("sidebar_head")
            .frame(egui::Frame::NONE)
            .show_separator_line(false)
            .show_inside(ui, |ui| {
                ui.add_space(theme::SPACING);
                ui.label(egui::RichText::new(t!("app.title").to_string()).heading().strong().color(theme::TEXT));
                ui.add_space(theme::SPACING * 2.0);
                self.ui_nav(ui);
                ui.add_space(theme::SPACING);
            });

        egui::TopBottomPanel::bottom("sidebar_streams")
            .frame(egui::Frame::NONE)
            .show_separator_line(false)
            .max_height(STREAMS_MAX_HEIGHT)
            .show_inside(ui, |ui| {
                ui.add_space(theme::SPACING);
                self.ui_streams_section(ui);
            });

        egui::CentralPanel::default().frame(egui::Frame::NONE).show_inside(ui, |ui| {
            // "+ Add Room" (and its inline card) is pinned to the *bottom of
            // the ROOMS section*, not the bottom of the sidebar — STREAMS
            // lives below it — so it's a bottom panel nested inside the rooms
            // region rather than a sibling of it.
            egui::TopBottomPanel::bottom("sidebar_add_room")
                .frame(egui::Frame::NONE)
                .show_separator_line(false)
                .show_inside(ui, |ui| {
                    self.ui_add_room(ui);
                });
            egui::CentralPanel::default().frame(egui::Frame::NONE).show_inside(ui, |ui| {
                self.ui_rooms_section(ui);
            });
        });
    }

    /// The three-pill nav rail. Sets `nav_tab` and nothing else — see this
    /// module's docs on the nav being a pure view-switcher.
    fn ui_nav(&mut self, ui: &mut egui::Ui) {
        for tab in NavTab::ALL {
            let active = self.nav_tab == tab;
            let response =
                ui.add_sized([ui.available_width(), theme::NAV_BUTTON_HEIGHT], theme::nav_button(tab.label(), active));
            if response.clicked() {
                self.nav_tab = tab;
            }
            ui.add_space(theme::SPACING * 0.5);
        }
    }

    /// The ROOMS section: every saved room, with the ones that have a live
    /// `RoomSession` rendered as connected.
    ///
    /// `Why` iterate `room_store` and look the session up (rather than the
    /// reverse): the plan unifies "rooms" and the old saved "lounges" into one
    /// list, i.e. a left join `room_store ⟕ rooms`. Driving the loop from the
    /// persistent side is what makes a disconnected saved room a *row* (with
    /// `[Reconnect]`) instead of something that vanishes when its connection
    /// drops. A live room with no store entry can't normally exist — a
    /// successful join saves it — but is rendered afterwards anyway rather
    /// than being silently invisible.
    fn ui_rooms_section(&mut self, ui: &mut egui::Ui) {
        theme::section_label(ui, t!("rooms.section").as_ref());

        if let Some(err) = &self.room_store_error {
            ui.colored_label(theme::DANGER, t!("join.load_error", error = err).to_string());
        }
        // A connect failure that didn't come from the Add Room card (i.e. a
        // `[Reconnect]`, or a dropped connection) has no card to live in, so
        // it reports here, above the list it concerns.
        if let Status::Error(e) = &self.status
            && !self.add_room_open
        {
            ui.colored_label(theme::DANGER, t!("join.error", error = e).to_string());
        }

        let mut actions: Vec<RoomAction> = Vec::new();
        egui::ScrollArea::vertical()
            .id_salt("rooms_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for saved in &self.room_store.rooms {
                    let handle = self
                        .rooms
                        .iter()
                        .find(|(_, room)| room.join_string == saved.join_string)
                        .map(|(handle, _)| *handle);
                    self.ui_room_row(ui, &saved.name, &saved.join_string, handle, &mut actions);
                }
                for (handle, room) in &self.rooms {
                    if self.room_store.name_for(&room.join_string).is_some() {
                        continue;
                    }
                    let name = room_label(&self.room_store, &room.join_string);
                    self.ui_room_row(ui, &name, &room.join_string, Some(*handle), &mut actions);
                }
                if self.room_store.rooms.is_empty() && self.rooms.is_empty() {
                    ui.label(egui::RichText::new(t!("rooms.none_yet").to_string()).color(theme::TEXT_FAINT).small());
                }
            });

        for action in actions {
            self.apply_room_action(action);
        }
    }

    /// One ROOMS row, in whichever of the three states applies: joined (green
    /// dot, expandable), reconnecting (amber pulsing dot), or saved-but-
    /// disconnected (hollow dot, `[Reconnect]`).
    ///
    /// Takes `&self` and collects into `actions` rather than acting directly:
    /// every affordance here mutates something (`rooms`, `room_store`,
    /// `expanded_rooms`) that this function is simultaneously reading to
    /// render the list.
    fn ui_room_row(
        &self,
        ui: &mut egui::Ui,
        name: &str,
        join_string: &str,
        handle: Option<RoomHandle>,
        actions: &mut Vec<RoomAction>,
    ) {
        let selected = handle.is_some() && self.active_room == handle;
        let expanded = handle.is_some_and(|h| self.expanded_rooms.contains(&h));
        let reconnecting = handle.is_none() && self.is_reconnecting(join_string);

        let fill = if selected { theme::tint(theme::ACCENT, 40) } else { egui::Color32::TRANSPARENT };
        egui::Frame::new()
            .fill(fill)
            .corner_radius(egui::CornerRadius::same(theme::ROUNDING_SM))
            .inner_margin(4)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    match (handle, reconnecting) {
                        (Some(_), _) => theme::status_dot(ui, theme::SUCCESS, true),
                        // A simple sine pulse on the dot's alpha, per the
                        // plan's "don't over-build this" note. Driven off
                        // egui's own frame clock; `update()` already keeps
                        // repainting while a connect is in flight, so this
                        // needs no repaint request of its own.
                        (None, true) => {
                            let phase = (ui.input(|i| i.time) / RECONNECT_PULSE_PERIOD * std::f64::consts::TAU).sin();
                            let alpha = (140.0 + 115.0 * phase) as u8;
                            theme::status_dot(ui, theme::tint(theme::WARNING, alpha), true);
                        }
                        (None, false) => theme::status_dot(ui, theme::TEXT_FAINT, false),
                    }

                    if let Some(h) = handle {
                        let glyph = if expanded { ICON_CARET_EXPANDED } else { ICON_CARET_COLLAPSED };
                        if ui.add(theme::icon_button(glyph)).clicked() {
                            actions.push(RoomAction::ToggleExpand(h));
                        }
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        match handle {
                            Some(h) => {
                                if ui
                                    .add(theme::icon_button(ICON_CLOSE))
                                    .on_hover_text(t!("rooms.leave_tooltip").to_string())
                                    .clicked()
                                {
                                    actions.push(RoomAction::Leave(h));
                                }
                                let room = &self.rooms[&h];
                                ui.label(
                                    egui::RichText::new(
                                        t!("rooms.active_users", count = active_user_count(room)).to_string(),
                                    )
                                    .small()
                                    .color(theme::TEXT_DIM),
                                );
                            }
                            None => {
                                if ui
                                    .add(theme::icon_button(ICON_CLOSE))
                                    .on_hover_text(t!("rooms.forget_tooltip").to_string())
                                    .clicked()
                                {
                                    actions.push(RoomAction::Forget(name.to_string()));
                                }
                                ui.add_enabled_ui(!reconnecting, |ui| {
                                    if ui.add(theme::secondary_button(t!("rooms.reconnect_button").to_string())).clicked() {
                                        actions.push(RoomAction::Reconnect(join_string.to_string()));
                                    }
                                });
                                if !reconnecting {
                                    ui.label(
                                        egui::RichText::new(t!("rooms.not_connected").to_string())
                                            .small()
                                            .color(theme::TEXT_FAINT),
                                    );
                                }
                            }
                        }

                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            let label = egui::Label::new(
                                egui::RichText::new(name).strong().color(if handle.is_some() {
                                    theme::TEXT
                                } else {
                                    theme::TEXT_DIM
                                }),
                            )
                            .truncate()
                            .sense(egui::Sense::click());
                            if ui.add(label).clicked()
                                && let Some(h) = handle
                            {
                                actions.push(RoomAction::Select(h));
                            }
                        });
                    });
                });
            });

        if !expanded {
            return;
        }
        let Some(handle) = handle else { return };
        let Some(room) = self.rooms.get(&handle) else { return };
        let me = room.client.conn_id();
        let unknown_owner = t!("streams.unknown_owner").to_string();
        for info in &room.streams {
            if info.owner == me {
                continue;
            }
            // A sender who also has video gets their audio auto-subscribed by
            // the video row's action (see `RoomSession::toggle_watch`), so
            // only an audio-only sender gets a row of their own.
            if info.kind == StreamKind::Audio && room.owner_has_video(info.owner) {
                continue;
            }
            let owner_name = room
                .members
                .iter()
                .find(|m| m.conn_id == info.owner)
                .map(|m| m.display_name.as_str())
                .unwrap_or(unknown_owner.as_str());
            let key = (info.owner, info.stream_id);
            let active = if info.kind == StreamKind::Audio {
                room.audio_watches.lock().unwrap().contains_key(&key)
            } else {
                room.watches.lock().unwrap().contains_key(&key)
            };

            ui.horizontal(|ui| {
                ui.add_space(theme::SPACING * 2.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let glyph = if active { ICON_STOP } else { ICON_WATCH };
                    let tooltip = match (info.kind, active) {
                        (StreamKind::Audio, false) => t!("streams.listen_button"),
                        (StreamKind::Audio, true) => t!("streams.stop_listening_button"),
                        (_, false) => t!("streams.watch_button"),
                        (_, true) => t!("streams.stop_watching_button"),
                    };
                    if ui.add(theme::icon_button(glyph)).on_hover_text(tooltip.to_string()).clicked() {
                        actions.push(match info.kind {
                            StreamKind::Audio => RoomAction::Listen(handle, key.0, key.1),
                            _ => RoomAction::Watch(handle, key.0, key.1),
                        });
                    }
                    ui.label(
                        egui::RichText::new(remote_stream_geometry(info))
                            .small()
                            .color(theme::TEXT_FAINT),
                    );
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(owner_name)
                                    .small()
                                    .color(if active { theme::SUCCESS } else { theme::TEXT_DIM }),
                            )
                            .truncate(),
                        );
                    });
                });
            });
        }
    }

    /// The "+ Add Room" button pinned to the bottom of the ROOMS section, and
    /// the inline mini-card it expands into.
    ///
    /// `Why` inline and not a modal: joining is the most routine action in the
    /// app, and a modal would blank out the very list the new room is about to
    /// appear in. There is deliberately no staging step either — Join connects
    /// immediately, and success *is* the save (see `poll_connect_result`),
    /// because `RoomStore` already is this app's persistent "rooms I care
    /// about" state and a second "saved but never joined" concept would be a
    /// third thing to reconcile with the two that already exist.
    fn ui_add_room(&mut self, ui: &mut egui::Ui) {
        if self.add_room_open {
            theme::card(ui, t!("rooms.add_header").as_ref(), |ui| {
                ui.label(egui::RichText::new(t!("join.display_name_label").to_string()).small().color(theme::TEXT_DIM));
                ui.add(egui::TextEdit::singleline(&mut self.display_name).desired_width(ui.available_width()));
                ui.label(egui::RichText::new(t!("join.link_label").to_string()).small().color(theme::TEXT_DIM));
                ui.add(egui::TextEdit::singleline(&mut self.add_room_join_string).desired_width(ui.available_width()));

                let connecting = matches!(self.status, Status::Connecting);
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(!connecting && !self.add_room_join_string.trim().is_empty(), |ui| {
                        if ui.add(theme::primary_button(t!("join.button").to_string())).clicked() {
                            let join_string = self.add_room_join_string.trim().to_string();
                            self.start_connect(join_string, true);
                        }
                    });
                    if ui.add(theme::secondary_button(t!("common.cancel").to_string())).clicked() {
                        self.add_room_open = false;
                        self.add_room_error = None;
                    }
                    if connecting {
                        ui.spinner();
                    }
                });
                if let Some(err) = &self.add_room_error {
                    ui.colored_label(theme::DANGER, t!("join.error", error = err).to_string());
                }
            });
        }

        if ui
            .add_sized(
                [ui.available_width(), ui.spacing().interact_size.y],
                theme::secondary_button(t!("rooms.add_button").to_string()),
            )
            .clicked()
        {
            self.add_room_open = !self.add_room_open;
            self.add_room_error = None;
        }
    }

    /// The STREAMS section: this client's own outgoing destinations across
    /// *every* room, not just the one the content tabs happen to be showing.
    ///
    /// `Why` cross-room: a destination is "a thing I am broadcasting right
    /// now", and the whole point of the multi-room model is that several can
    /// be live in different rooms at once. Scoping this list to `active_room`
    /// would hide outgoing streams the user is still paying bandwidth for.
    fn ui_streams_section(&mut self, ui: &mut egui::Ui) {
        theme::section_label(ui, t!("streams_section.header").as_ref());

        if self.captures.is_empty() {
            // The common case on every launch: nothing is persisted across
            // sessions, so an inviting empty state is the *normal* state here,
            // not an edge case.
            ui.label(egui::RichText::new(t!("streams_section.empty").to_string()).color(theme::TEXT_FAINT).small());
            ui.add_space(theme::SPACING * 0.5);
            if ui
                .add_sized(
                    [ui.available_width(), ui.spacing().interact_size.y],
                    theme::primary_button(t!("streams_section.start_button").to_string()),
                )
                .clicked()
            {
                self.nav_tab = NavTab::StreamControl;
            }
            return;
        }

        let mut to_stop: Vec<CaptureSourceId> = Vec::new();
        egui::ScrollArea::vertical()
            .id_salt("streams_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (capture_id, capture) in &self.captures {
                    for dest in capture.destinations.values() {
                        let room_name = match self.rooms.get(&dest.room) {
                            Some(room) => room_label(&self.room_store, &room.join_string),
                            None => t!("streams_section.unknown_room").to_string(),
                        };
                        ui.horizontal(|ui| {
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                let stoppable = matches!(capture.status, ShareStatus::Active(_));
                                ui.add_enabled_ui(stoppable, |ui| {
                                    if ui
                                        .add(theme::icon_button(ICON_CLOSE))
                                        .on_hover_text(t!("share.stop_button").to_string())
                                        .clicked()
                                    {
                                        to_stop.push(*capture_id);
                                    }
                                });
                                // Audio is still its own capture at this
                                // stage, so the speaker icon marks an
                                // audio *row*. Once audio becomes an optional
                                // attachment on a video `Destination` (a later
                                // stage, see the plan doc), it moves onto that
                                // video row instead and this row disappears.
                                if capture.kind == StreamKind::Audio {
                                    ui.label(egui::RichText::new(ICON_AUDIO).small().color(theme::TEXT_DIM));
                                }
                                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                    let detail = if capture.kind == StreamKind::Audio {
                                        capture.label.clone()
                                    } else {
                                        destination_geometry(dest)
                                    };
                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(t!(
                                                "streams_section.line",
                                                room = room_name,
                                                detail = detail
                                            ).to_string())
                                            .small()
                                            .color(theme::TEXT),
                                        )
                                        .truncate(),
                                    );
                                });
                            });
                        });
                        if !matches!(capture.status, ShareStatus::Active(_)) {
                            let state = match capture.status {
                                ShareStatus::Starting(_) => t!("share.state_starting"),
                                _ => t!("share.state_stopping"),
                            };
                            ui.label(egui::RichText::new(state.to_string()).small().color(theme::TEXT_FAINT));
                        }
                    }
                }
            });

        for capture_id in to_stop {
            self.stop_share(capture_id);
        }
    }

    /// Apply one deferred sidebar action, now that the render borrows are
    /// gone. See `ui_room_row`.
    fn apply_room_action(&mut self, action: RoomAction) {
        match action {
            RoomAction::Select(handle) => self.active_room = Some(handle),
            RoomAction::ToggleExpand(handle) => {
                if !self.expanded_rooms.remove(&handle) {
                    self.expanded_rooms.insert(handle);
                }
            }
            RoomAction::Leave(handle) => self.request_leave_room(handle),
            RoomAction::Reconnect(join_string) => self.start_connect(join_string, false),
            RoomAction::Forget(name) => {
                self.room_store.remove(&name);
                if let Err(e) = self.room_store.save() {
                    self.room_store_error = Some(e.to_string());
                }
            }
            RoomAction::Watch(handle, owner, stream_id) => {
                if let Some(room) = self.rooms.get_mut(&handle) {
                    room.toggle_watch(owner, stream_id);
                    // Watching from a room's row is also a statement about
                    // which room you're looking at — otherwise the View tab
                    // would keep showing a different room's stream and the
                    // click would look like it did nothing.
                    self.active_room = Some(handle);
                    self.nav_tab = NavTab::View;
                }
            }
            RoomAction::Listen(handle, owner, stream_id) => {
                if let Some(room) = self.rooms.get_mut(&handle) {
                    room.toggle_listen(owner, stream_id);
                }
            }
        }
    }

    /// Confirm dialog for leaving a room with active work in it. See
    /// `leave_needs_confirm` for when this is raised at all.
    fn ui_leave_confirm(&mut self, ctx: &egui::Context) {
        let Some(handle) = self.leave_confirm else { return };
        let Some(room) = self.rooms.get(&handle) else {
            self.leave_confirm = None;
            return;
        };
        let name = room_label(&self.room_store, &room.join_string);
        let watching = room.watches.lock().unwrap().len() + room.audio_watches.lock().unwrap().len();
        let sending = room.destinations.len();

        let mut confirmed = false;
        let mut cancelled = false;
        let response = egui::Modal::new(egui::Id::new("leave_room_confirm")).show(ctx, |ui| {
            ui.set_max_width(theme::PANEL_MAX_WIDTH);
            theme::section_label(ui, t!("rooms.leave_confirm_title").as_ref());
            ui.add_space(theme::SPACING);
            ui.label(egui::RichText::new(t!("rooms.leave_confirm_body", room = name).to_string()).color(theme::TEXT));
            if sending > 0 {
                ui.label(
                    egui::RichText::new(t!("rooms.leave_confirm_sending", count = sending).to_string())
                        .color(theme::WARNING),
                );
            }
            if watching > 0 {
                ui.label(
                    egui::RichText::new(t!("rooms.leave_confirm_watching", count = watching).to_string())
                        .color(theme::WARNING),
                );
            }
            ui.add_space(theme::SPACING);
            ui.horizontal(|ui| {
                if ui.add(theme::danger_button(t!("rooms.leave_confirm_button").to_string())).clicked() {
                    confirmed = true;
                }
                if ui.add(theme::secondary_button(t!("common.cancel").to_string())).clicked() {
                    cancelled = true;
                }
            });
        });

        if confirmed {
            self.leave_confirm = None;
            self.disconnect_room(handle);
        } else if cancelled || response.should_close() {
            self.leave_confirm = None;
        }
    }

    // -----------------------------------------------------------------
    // App shell — content area
    // -----------------------------------------------------------------

    /// Render whichever nav tab is selected. Nothing here (or in the tab
    /// functions it dispatches to) starts or stops a share, a subscription or
    /// a decoder as a side effect of *which* tab is showing — see this
    /// module's docs.
    fn ui_content(&mut self, ui: &mut egui::Ui) {
        match self.nav_tab {
            NavTab::StreamControl => self.ui_stream_control_tab(ui),
            NavTab::View => self.ui_view_tab(ui),
            NavTab::Settings => self.ui_settings_tab(ui),
        }
    }

    /// A centered "there's nothing here yet, here's what to do" message —
    /// shared by every tab's empty state so they read as one family.
    fn ui_empty_state(ui: &mut egui::Ui, message: &str) {
        ui.centered_and_justified(|ui| {
            ui.label(egui::RichText::new(message).color(theme::TEXT_FAINT));
        });
    }

    fn ui_settings_tab(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.set_max_width(theme::PANEL_MAX_WIDTH);
            ui.spacing_mut().item_spacing.y = 0.0;
            self.ui_settings_card(ui);
        });
    }

    /// Stream Control: the sharing controls for `active_room`.
    ///
    /// **Deliberately minimal at this stage.** This tab is the mount point for
    /// the multi-destination fan-out UI, the Absolute-Maximum /
    /// Per-Stream-Maximum bandwidth-mode toggle (and `bandwidth::allocate`'s
    /// wiring), live transfer between rooms, and the overshoot warning banner
    /// — none of which are built here. What it holds today is exactly the
    /// controls the old connected screen had, relocated and restyled: one
    /// bitrate/framerate setting pair applied to the *next* capture started,
    /// share screen / share audio, and the list of what's already running.
    ///
    /// `self` is destructured up front rather than accessed field-by-field:
    /// the tab mixes room-scoped state (`rooms[active_room]`) with `App`-scoped
    /// capture state (`captures`, `share_error`, the audio-source list), and
    /// taking them as independent bindings is what lets both be live at once.
    /// The capture-side *actions* still need `&mut self` whole, so they're
    /// recorded and applied after the borrow ends.
    fn ui_stream_control_tab(&mut self, ui: &mut egui::Ui) {
        let Some(room_handle) = self.active_room else {
            Self::ui_empty_state(ui, t!("stream_control.empty").as_ref());
            return;
        };

        let mut start_video = false;
        let mut start_audio = false;
        let mut refresh_audio = false;
        let mut to_stop: Vec<CaptureSourceId> = Vec::new();
        let mut rename: Option<(String, String)> = None;

        let App {
            rooms,
            captures,
            room_store,
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

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.set_max_width(theme::PANEL_MAX_WIDTH);
            ui.spacing_mut().item_spacing.y = 0.0;

            ui.label(
                egui::RichText::new(room_label(room_store, &room.join_string))
                    .heading()
                    .color(theme::TEXT),
            );
            ui.label(
                egui::RichText::new(
                    t!("connected.status", conn_id = format!("{:016x}", room.client.conn_id())).to_string(),
                )
                .small()
                .color(theme::TEXT_DIM),
            );
            ui.add_space(theme::SPACING);

            // Rename this room's saved entry. A single field plus one button
            // doesn't need card ceremony (STYLING-GUIDE.md: cards are for
            // *grouping* related controls).
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(t!("rooms.save_label").to_string()).color(theme::TEXT));
                ui.text_edit_singleline(&mut room.save_name);
                let new_name = room.save_name.trim().to_string();
                ui.add_enabled_ui(!new_name.is_empty(), |ui| {
                    if ui.add(theme::secondary_button(t!("rooms.save_button").to_string())).clicked() {
                        rename = Some((room.join_string.clone(), new_name.clone()));
                    }
                });
            });
            if room.saved {
                ui.colored_label(theme::SUCCESS, t!("rooms.saved_confirmation").to_string());
            }
            ui.add_space(theme::SPACING);

            theme::card(ui, t!("members.header", count = room.members.len()).as_ref(), |ui| {
                egui::ScrollArea::vertical().id_salt("members_scroll").max_height(120.0).show(ui, |ui| {
                    // Show all members, not just streamers (spec §8/§4.2: a
                    // lurker with stream_count == 0 still appears in the
                    // roster).
                    for member in &room.members {
                        ui.horizontal(|ui| {
                            if member.stream_count > 0 {
                                theme::status_dot(ui, theme::SUCCESS, true);
                                let key = if member.stream_count == 1 { "members.stream_singular" } else { "members.stream_plural" };
                                ui.label(
                                    egui::RichText::new(t!(key, name = member.display_name, count = member.stream_count).to_string())
                                        .color(theme::TEXT),
                                );
                            } else {
                                theme::status_dot(ui, theme::TEXT_FAINT, false);
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
            // room's own `destinations` back-references. That ordering (the
            // order the user started them) replaces the old sort-by-stream_id:
            // with several rooms, `stream_id` is no longer a session-wide
            // ordering at all, just a per-connection label.
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
                        // machine's own captured video, so the person sharing
                        // can confirm what viewers actually see. Video shares
                        // only, and only once actually `Active` (a
                        // `CaptureHandle` — and therefore a self-preview frame
                        // source — only exists at that point). See
                        // `superdoc/features/self-preview.md`.
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
                                    ui.painter().rect_filled(rect, theme::ROUNDING_SM, theme::FIELD);
                                }
                            }
                        }
                    }
                }
                if let Some(err) = share_error.as_ref() {
                    ui.colored_label(theme::DANGER, t!("share.error", error = err).to_string());
                }
            });
        });

        // Deferred actions, applied now that the field borrows are gone.
        if let Some((join_string, new_name)) = rename {
            self.rename_saved_room(&join_string, new_name);
            if let Some(room) = self.rooms.get_mut(&room_handle) {
                room.saved = true;
            }
        }
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

    /// Rename the `RoomStore` entry for `join_string` to `new_name`.
    ///
    /// `Why` remove-then-upsert rather than a plain `upsert`: `RoomStore` is
    /// keyed by *name*, so upserting under a new name would leave the old
    /// entry behind and the sidebar would show the same room twice — once
    /// connected, once as a stale saved row.
    fn rename_saved_room(&mut self, join_string: &str, new_name: String) {
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

    /// View: the watched-stream pane for `active_room`.
    ///
    /// **Deliberately minimal at this stage.** This tab is the mount point for
    /// the one global auto-tiled grid across every room, the capture-restart
    /// placeholder tiers, per-tile hover overlays and live audio-toggle
    /// affordances — none of which are built here. What it holds today is the
    /// existing single-selected-watch display, relocated and restyled onto
    /// `theme::media_frame` (chrome-free, per the plan: a video surface must
    /// never read as a settings card), plus the same stream picker the old
    /// connected screen's Streams card was, moved to the bottom of this tab
    /// where the previews it renders are actually useful.
    ///
    /// Pop-out is unchanged: it still marks the entry and lets
    /// `RoomSession::show_popouts` give it a real OS viewport. Only the
    /// *button* moved.
    fn ui_view_tab(&mut self, ui: &mut egui::Ui) {
        let Some(room_handle) = self.active_room else {
            Self::ui_empty_state(ui, t!("view.empty").as_ref());
            return;
        };
        let Some(room) = self.rooms.get_mut(&room_handle) else { return };

        let mut to_toggle: Option<(ConnId, StreamId)> = None;
        let mut to_pop_out: Option<(ConnId, StreamId)> = None;
        let mut to_toggle_listen: Option<(ConnId, StreamId)> = None;

        // The stream picker, pinned below the video. Rendered first so it
        // reserves its height before the media surface claims "the rest".
        egui::TopBottomPanel::bottom("view_stream_picker")
            .frame(egui::Frame::NONE)
            .show_separator_line(false)
            .max_height(160.0)
            .show_inside(ui, |ui| {
                theme::section_label(ui, t!("streams.header").as_ref());
                if let Some(err) = &room.watch_error {
                    ui.colored_label(theme::DANGER, t!("streams.watch_error", error = err).to_string());
                }
                let me = room.client.conn_id();
                let visible: Vec<&StreamInfo> = room
                    .streams
                    .iter()
                    .filter(|s| s.owner != me)
                    .filter(|s| !(s.kind == StreamKind::Audio && room.owner_has_video(s.owner)))
                    .collect();
                if visible.is_empty() {
                    ui.label(egui::RichText::new(t!("streams.none_active").to_string()).color(theme::TEXT_FAINT));
                    return;
                }
                let unknown_owner = t!("streams.unknown_owner").to_string();
                egui::ScrollArea::horizontal().id_salt("view_picker_scroll").show(ui, |ui| {
                    ui.horizontal(|ui| {
                        for info in visible {
                            let key = (info.owner, info.stream_id);
                            let owner_name = room
                                .members
                                .iter()
                                .find(|m| m.conn_id == info.owner)
                                .map(|m| m.display_name.as_str())
                                .unwrap_or(unknown_owner.as_str());
                            ui.vertical(|ui| {
                                const THUMB_WIDTH: f32 = 120.0;
                                const THUMB_HEIGHT: f32 = THUMB_WIDTH * 9.0 / 16.0;
                                match room.preview_textures.get(&key) {
                                    Some(tex) => {
                                        let tex_size = tex.size_vec2();
                                        let scale = if tex_size.x > 0.0 { THUMB_WIDTH / tex_size.x } else { 1.0 };
                                        ui.image((tex.id(), tex_size * scale));
                                    }
                                    None => {
                                        let (rect, _) = ui.allocate_exact_size(
                                            egui::vec2(THUMB_WIDTH, THUMB_HEIGHT),
                                            egui::Sense::hover(),
                                        );
                                        ui.painter().rect_filled(rect, theme::ROUNDING_SM, theme::PANEL_SUNKEN);
                                    }
                                }
                                ui.add(
                                    egui::Label::new(egui::RichText::new(owner_name).small().color(theme::TEXT))
                                        .truncate(),
                                );
                                if info.kind == StreamKind::Audio {
                                    let listening = room.audio_watches.lock().unwrap().contains_key(&key);
                                    let label = if listening {
                                        t!("streams.stop_listening_button")
                                    } else {
                                        t!("streams.listen_button")
                                    };
                                    if ui.add(theme::secondary_button(label.to_string())).clicked() {
                                        to_toggle_listen = Some(key);
                                    }
                                    return;
                                }
                                let watching = room.watches.lock().unwrap().contains_key(&key);
                                let popped_out =
                                    room.watches.lock().unwrap().get(&key).is_some_and(|e| e.popout.is_some());
                                ui.horizontal(|ui| {
                                    let label = if watching {
                                        t!("streams.stop_watching_button")
                                    } else {
                                        t!("streams.watch_button")
                                    };
                                    if ui.add(theme::secondary_button(label.to_string())).clicked() {
                                        to_toggle = Some(key);
                                    }
                                    if watching {
                                        ui.add_enabled_ui(!popped_out, |ui| {
                                            if ui.add(theme::secondary_button(t!("streams.pop_out_button").to_string())).clicked() {
                                                to_pop_out = Some(key);
                                            }
                                        });
                                    }
                                });
                                if popped_out {
                                    ui.colored_label(theme::ACCENT, t!("streams.popped_out").to_string());
                                }
                            });
                        }
                    });
                });
            });

        egui::CentralPanel::default().frame(egui::Frame::NONE).show_inside(ui, |ui| {
            // Which watch the pane shows. `selected_watch` is set by watching
            // something; the chips let you get back to an earlier one without
            // stopping and restarting it. Not the tiled grid — that's a later
            // stage; this is just a selector over the same single surface.
            let keys: Vec<(ConnId, StreamId)> = room.watches.lock().unwrap().keys().copied().collect();
            if keys.len() > 1 {
                ui.horizontal_wrapped(|ui| {
                    for key in &keys {
                        let owner_name = room
                            .members
                            .iter()
                            .find(|m| m.conn_id == key.0)
                            .map(|m| m.display_name.clone())
                            .unwrap_or_else(|| t!("streams.unknown_owner").to_string());
                        if ui.selectable_label(room.selected_watch == Some(*key), owner_name).clicked() {
                            room.selected_watch = Some(*key);
                        }
                    }
                });
            }

            ui.horizontal(|ui| {
                // Viewer-side live FPS: how often the currently displayed
                // watch's decoder actually yields a *new* frame (see
                // `StreamDecoder::current_fps`), not just the UI's own repaint
                // rate — the most directly useful FPS readout, since it tells
                // you whether playback of what's on screen right now is
                // actually smooth. Reads through the same
                // `Arc<Mutex<StreamDecoder>>` `update_watch_textures`/the
                // popout closure poll `latest_frame()` through, so this
                // reflects real decode activity regardless of whether the
                // stream is currently shown inline or popped out.
                if let Some(key) = room.selected_watch {
                    let watches = room.watches.lock().unwrap();
                    if let Some(entry) = watches.get(&key) {
                        let fps = entry.decoder.lock().unwrap().current_fps();
                        ui.label(
                            egui::RichText::new(t!("video.fps_label", fps = format!("{fps:.1}")).to_string())
                                .small()
                                .color(theme::TEXT_DIM),
                        );
                    }
                }
            });

            // If the currently displayed video watch also auto-subscribed the
            // same sender's audio (see `RoomSession::toggle_watch`), show its
            // volume slider right above the video surface.
            let linked_audio_key: Option<(ConnId, StreamId)> = room.selected_watch.and_then(|key| {
                let audio_stream_id = room.watches.lock().unwrap().get(&key)?.audio_stream_id?;
                Some((key.0, audio_stream_id))
            });
            let standalone_audio_keys: Vec<(ConnId, StreamId)> = room
                .audio_watches
                .lock()
                .unwrap()
                .keys()
                .copied()
                .filter(|k| Some(*k) != linked_audio_key)
                .collect();
            for audio_key in linked_audio_key.into_iter().chain(standalone_audio_keys) {
                let mut audio_watches = room.audio_watches.lock().unwrap();
                if let Some(audio_watch) = audio_watches.get_mut(&audio_key)
                    && theme::slider_row(ui, t!("audio.volume_label").as_ref(), &mut audio_watch.volume_percent, 0..=200, 1.0, "%")
                {
                    audio_watch.decoder.set_volume(audio_watch.volume_percent as f64 / 100.0);
                }
            }

            // Chrome-free media surface: no card border, no title, letterboxed
            // against `PANEL_SUNKEN`. See `theme::media_frame`.
            theme::media_frame(ui, |ui| {
                let available = ui.available_size();
                ui.allocate_ui_with_layout(
                    available,
                    egui::Layout::centered_and_justified(egui::Direction::TopDown),
                    |ui| {
                        let watches = room.watches.lock().unwrap();
                        match room.selected_watch.and_then(|key| watches.get(&key)) {
                            Some(entry) if entry.popout.is_some() => {
                                ui.label(egui::RichText::new(t!("video.popped_out_note").to_string()).color(theme::TEXT_DIM));
                            }
                            Some(entry) => match &entry.texture {
                                Some(tex) => {
                                    let tex_size = tex.size_vec2();
                                    let scale = if tex_size.x > 0.0 && tex_size.y > 0.0 {
                                        (available.x / tex_size.x).min(available.y / tex_size.y)
                                    } else {
                                        1.0
                                    };
                                    let display_size = tex_size * scale.max(0.01);
                                    ui.add(
                                        egui::Image::new((tex.id(), display_size))
                                            .corner_radius(theme::ROUNDING),
                                    );
                                }
                                None => {
                                    ui.label(egui::RichText::new(t!("video.waiting_frame").to_string()).color(theme::TEXT_DIM));
                                }
                            },
                            None => {
                                ui.label(egui::RichText::new(t!("view.empty").to_string()).color(theme::TEXT_FAINT));
                            }
                        }
                    },
                );
            });
        });

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
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.theme_installed {
            theme::install(ctx);
            self.theme_installed = true;
        }
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

        // The always-on shell. Both panels render on every frame regardless of
        // what's joined or watched — there is no join-gate mode any more.
        egui::SidePanel::left("sidebar")
            .resizable(false)
            .exact_width(SIDEBAR_WIDTH)
            .frame(
                egui::Frame::new()
                    .fill(theme::PANEL_SUNKEN)
                    .inner_margin(theme::SPACING),
            )
            .show(ctx, |ui| self.ui_sidebar(ui));

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::PANEL_BASE)
                    .inner_margin(theme::SPACING),
            )
            .show(ctx, |ui| self.ui_content(ui));

        // Drawn last so it floats above both panels.
        self.ui_leave_confirm(ctx);

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

    /// The zero-room first-launch default. Pinned because it's a real
    /// product decision (see `NavTab::default`'s `Why`), not an arbitrary
    /// first-variant fallthrough.
    #[test]
    fn first_launch_opens_on_stream_control() {
        assert_eq!(NavTab::default(), NavTab::StreamControl);
    }

    /// A room row must never render the join string itself — it carries the
    /// room *key*, and a sidebar is exactly the kind of thing that ends up in
    /// a screenshot or a screen share.
    #[test]
    fn room_label_falls_back_to_an_id_prefix_and_never_leaks_the_key() {
        let room_id = "00112233445566778899aabbccddeeff";
        // A real, parseable 32-byte base64url key — `room_label` falls back to
        // a bare placeholder for a join string that doesn't parse, which would
        // pass the "no key in the label" assertion for the wrong reason.
        let key = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8";
        let join_string = format!("relay.example:4433|{room_id}|{key}");

        let store = RoomStore::default();
        let label = room_label(&store, &join_string);
        assert!(label.contains("00112233"), "expected the room-id prefix in {label:?}");
        assert!(!label.contains(key), "the room key must never reach a rendered label");
        assert!(!label.contains("relay.example"), "the label is an identifier, not a connection string");

        // A saved room wins over the fallback.
        let mut store = RoomStore::default();
        store.upsert("movie night".to_string(), join_string.clone());
        assert_eq!(room_label(&store, &join_string), "movie night");
    }

    /// Remote streams have no announced framerate (`StreamInfo` carries none),
    /// so the sidebar renders the placeholder rather than inventing a number —
    /// and degrades to a bare placeholder when even the size is unknown,
    /// instead of printing `0x0`.
    #[test]
    fn remote_stream_geometry_renders_an_em_dash_for_the_missing_framerate() {
        let info = StreamInfo {
            owner: 1,
            stream_id: 0,
            kind: StreamKind::Video,
            label: "Screen".to_string(),
            width: 1920,
            height: 1080,
        };
        assert_eq!(remote_stream_geometry(&info), "1920x1080@\u{2014}");
        assert_eq!(remote_stream_geometry(&StreamInfo { width: 0, height: 0, ..info }), "\u{2014}");
    }

    /// Our *own* destinations do know their real geometry — but only once the
    /// capture handle has landed and its runtime is attached, because the
    /// requested params are `0` ("whatever the source is") until then.
    #[test]
    fn destination_geometry_prefers_the_live_runtime_over_the_requested_params() {
        let mut dest = destination(RoomHandle(0), 0);
        assert_eq!(destination_geometry(&dest), "\u{2014}", "no runtime yet, and 0x0 was requested");

        dest.runtime = Some(capture::destination::DestinationRuntime::shared(
            std::time::Instant::now(),
            capture::destination::VideoParams { width: 2560, height: 1440, framerate: 60, ..dest.params },
        ));
        assert_eq!(destination_geometry(&dest), "2560x1440@60");
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
        let room_handle = app.insert_room(client_b, snapshot_b, events_b, join_string.clone());
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
                // Also the regression test for the app shell's hard rule that
                // **the nav is a pure view-switcher**: flip to a different tab
                // on every single pass, so for most of this run the View tab
                // (the only one that renders this watch) is *not* the selected
                // one. If switching tabs stopped, unsubscribed, or dropped a
                // watch, the fps assertion below would collapse to ~0.
                app.nav_tab = match app.nav_tab {
                    NavTab::StreamControl => NavTab::View,
                    NavTab::View => NavTab::Settings,
                    NavTab::Settings => NavTab::StreamControl,
                };
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
             either the repaint-cadence fix is not keeping pace with the real decode rate, or \
             switching nav tabs (which this loop does every pass) is interfering with an active watch"
        );
        // The watch survived hundreds of tab switches, still subscribed and
        // still decoding — the nav really is a pure view-switcher.
        assert!(
            app.rooms[&room_handle].watches.lock().unwrap().contains_key(&(owner, TEST_STREAM_ID)),
            "the watch was torn down at some point during nav-tab switching"
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
        let room_handle = app.insert_room(client_b, snapshot_b, events_b, join_string.clone());
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
