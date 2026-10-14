//! The native egui UI: one window, room-centric, Discord-like (the approved
//! mockup's "Proposed" tab; container tree in the plan's `ui-tree.md`).
//!
//! # Shell (this file)
//!
//! [`App::update`] polls the session, then lays the window out with egui
//! panels at fixed sizes. Each region is handed to exactly one screen module:
//!
//! ```text
//! ┌ SidePanel::left 272 (SIDEBAR_RAIL, 1 px PANEL_RAISED right border) ┐┌ TopBottomPanel::top 56: room header band ────────────────┐
//! │ brand band 56 ............. sidebar::show_brand                    ││ room::show_header (band, DIVIDER bottom, pad 16)           │
//! │ ScrollArea (pad 0/12/12, vbox gap 8) .. sidebar::show_body         │├──────────────────────────────────────────┬─────────────────┤
//! │                                                                    ││ CentralPanel: ScrollArea (pad 16, vbox   │ SidePanel::right│
//! │                                                                    ││ gap 16) .. room::show_body               │ 232 (people)    │
//! │                                                                    ││                                          │ room::show_people
//! │ user band 56 (ELEV_14, WELL_BORDER top) .. sidebar::show_user_band ││ TopBottomPanel::bottom 56: action_bar::show (PANEL_SUNKEN) │
//! └────────────────────────────────────────────────────────────────────┘└────────────────────────────────────────────────────────────┘
//! ```
//!
//! The People panel and the action bar exist only while a room view shows a
//! joined or reconnecting room ([`Cx::shows_room_panels`]); a disconnected
//! room, the "Watched streams" view and the welcome view have neither. The
//! user band and the action band are both 56 px tall at the window bottom,
//! so their top border lines are one line.
//!
//! Modals ([`Modal`]) render after the panels: [`stream_settings::show`] for
//! [`Modal::StreamSettings`], [`modals::show`] for every other kind. Pop-out
//! viewports are re-declared every frame by [`popout::show`]. Toasts are
//! painted last, by this file.
//!
//! # The contract every screen follows
//!
//! Each screen module owns **one** `pub struct <X>State` (its private UI
//! state — text buffers, scroll offsets, edit buffers, hover state; add fields
//! freely) and one or more `pub fn show...` with these exact signatures:
//!
//! ```ignore
//! sidebar::show_brand(ui: &mut Ui, cx: &mut Cx, st: &mut SidebarState)
//! sidebar::show_body(ui: &mut Ui, cx: &mut Cx, st: &mut SidebarState)
//! sidebar::show_user_band(ui: &mut Ui, cx: &mut Cx, st: &mut SidebarState)
//! room::show_header(ui: &mut Ui, cx: &mut Cx, st: &mut RoomState)
//! room::show_body(ui: &mut Ui, cx: &mut Cx, st: &mut RoomState)
//! room::show_people(ui: &mut Ui, cx: &mut Cx, st: &mut RoomState)
//! action_bar::show(ui: &mut Ui, cx: &mut Cx, st: &mut ActionBarState)
//! stream_settings::show(ctx: &egui::Context, cx: &mut Cx, st: &mut StreamSettingsState,
//!                       capture: CaptureSourceId, room: RoomHandle)
//! modals::show(ctx: &egui::Context, cx: &mut Cx, st: &mut ModalsState, modal: &Modal)
//! popout::show(ctx: &egui::Context, cx: &mut Cx, st: &mut PopoutState)
//! ```
//!
//! **Data in, actions out.** A screen reads everything through [`Cx`]:
//! `cx.state` (`&AppState`, read-only), `cx.nav`, `cx.shell`, `cx.videos`,
//! plus the selector helpers on `Cx` (room list, share state, names, …).
//! It **never mutates `AppState`, `Nav` or `Shell`**: every change is a
//! [`UiAction`] pushed with [`Cx::act`], applied by [`App::apply`] (in
//! `apply.rs`) after all screens have drawn. Local editing state (a text
//! field, a `DestEdit` copy being dragged in the stream-settings modal) lives
//! in the screen's own `State` and is pushed as one action when it should
//! take effect (e.g. on `drag_stopped`, not every frame of a drag).
//! `Why:` the borrow checker can't hand four screens `&mut AppState` while
//! they also read it, and one apply site keeps every state transition in one
//! reviewable place (the old `RoomAction`/`StreamAction` deferral, generalised).
//!
//! **Where a screen draws.** The shell allocates the region, paints its
//! background and borders, and passes a `Ui` that is exactly the region (band
//! functions get the padded band, left-to-right, vertically centred; body
//! functions get a top-down vbox inside the panel's ScrollArea, fixed width,
//! item spacing already set to the spec gap). A screen must not paint outside
//! its `Ui`'s `max_rect`; build rows with `widgets::HBox` / the row helpers,
//! never `ui.horizontal` + `add_space`.
//!
//! **Popovers** ([`Popover`]) are anchored to a button rect. The screen that
//! owns the button pushes [`UiAction::OpenPopover`] with `response.rect` and
//! draws the popover itself (with [`popover`]) while `cx.nav.popover` names
//! its kind; the shell only guarantees there's at most one open, that opening
//! a modal closes it, and that Escape closes it before it closes a modal.
//!
//! **Video.** [`VideoViews`] (`cx.videos`) holds one `VideoView` per watched
//! stream, keyed by [`StreamKey`], created and dropped by the shell as
//! watches start and stop. Draw a tile with
//! `cx.videos.show(key, ui, size, Fit::Contain)` (returns the response; paint
//! overlays with `ui.painter_at(resp.rect)`). Never `request_repaint()` for
//! video: the view repaints at the stream's frame rate on its own.

use std::collections::HashMap;

use egui::{Align, Color32, CornerRadius, Frame, Id, Layout, Margin, Rect, Stroke, Ui, UiBuilder, pos2, vec2};
use simple_screensharing_client::audio::{AudioLevelTapHandle, AudioSource};
use simple_screensharing_client::capture::destination::DestinationState;
use simple_screensharing_client::presets::EncoderPreset;
use simple_screensharing_client::protocol::{ConnId, StreamInfo, StreamKind};
use simple_screensharing_client::rooms::{BandwidthMode, PresentationMode};
use simple_screensharing_client::session::{
    AppState, CaptureSourceId, ConnectOutcome, DestEdit, Destination, DestinationId, JoinLink, LocalCapture,
    RoomHandle, RoomSession, ShareStatus, Status, room_label,
};
use tokio::sync::oneshot;

use crate::theme::{self, Weight, font};
use crate::widgets::{self, BandEdge};

pub mod action_bar;
mod apply;
pub mod modals;
pub mod popout;
pub mod room;
pub mod sidebar;
pub mod stream_settings;
#[cfg(test)]
mod tests;
mod video;

pub use video::{StreamKey, VideoViews};

/// Idle-fallback repaint cadence while connecting/connected, so presence
/// events and connect results are polled with no input. Video does not need
/// this (its views repaint per decoded frame on their own).
const IDLE_REPAINT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);
/// [`IDLE_REPAINT_INTERVAL`] while the window has lost focus. `Why:` an
/// uncapped present mode (Mailbox is the default) has no vsync ceiling, so
/// nothing but this keeps an idle background window from polling at full rate.
const UNFOCUSED_REPAINT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
/// How long a toast stays up, seconds (mockup: 2.6 s).
const TOAST_SECS: f64 = 2.6;

// ---------------------------------------------------------------------------
// Shared navigation types
// ---------------------------------------------------------------------------

/// A room's UI identity: its **join string**. Stable across reconnects (a
/// [`RoomHandle`] is minted per connection) and present for saved rooms that
/// aren't connected at all. **Contains the room key: never render or log it**
/// — show [`RoomEntry::name`] instead.
pub type RoomKey = String;

/// What the central area shows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum View {
    /// No room selected (fresh install, or the selected room was forgotten).
    #[default]
    Welcome,
    /// "Watched streams": every watched stream from every room.
    Watching,
    /// One room (joined, reconnecting, or disconnected).
    Room(RoomKey),
}

/// The open modal, if any. Only one at a time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Modal {
    /// Stream settings for `capture`, as seen from `room`: staged ("Set up
    /// your stream", footer Discard/Later/Go live) when the capture has no
    /// destination yet, else live ("Stream settings", Stop sharing in/Done).
    /// Rendered by [`stream_settings::show`].
    StreamSettings { capture: CaptureSourceId, room: RoomHandle },
    /// "Create or join a room" (tabs Join / Create). Form state lives in
    /// `ModalsState`; results come back in `Shell::join_error` /
    /// `Shell::create_result`.
    CreateJoin,
    /// App settings (profile, language, presentation mode, playback).
    Settings,
    /// "Leave X?" — raised by [`UiAction::Leave`] when the room has active
    /// shares or watches; confirm with [`UiAction::LeaveConfirmed`].
    LeaveConfirm { room: RoomHandle },
    /// "Forget X?" — confirm with [`UiAction::Forget`].
    ForgetConfirm { room: RoomKey },
    /// "Rename room" — confirm with [`UiAction::Rename`].
    Rename { room: RoomKey },
}

/// Which popover is open. Drawn by the screen that owns its anchor button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PopoverKind {
    /// Room header `[⋯]`: Rename / Copy invite link / Forget room. (room.rs)
    RoomMenu { room: RoomKey },
    /// User band "who" click: "You're sharing" list + Stop all sharing. (sidebar.rs)
    Outgoing,
    /// Idle bar "Share screen ▾" when another capture is live: add here /
    /// move here / share a different screen. (action_bar.rs)
    ShareMenu { room: RoomHandle },
    /// Live bar "N rooms ▾": Also send to + Move to. (action_bar.rs)
    Rooms { capture: CaptureSourceId, room: RoomHandle },
    /// Audio source picker for `capture` (the video capture the audio rides
    /// on), with per-source meters. (action_bar.rs)
    AudioSource { capture: CaptureSourceId },
    /// Stream settings "Also send to…". (stream_settings.rs)
    AlsoSendTo { capture: CaptureSourceId, room: RoomHandle },
    /// Stream settings "Move to…". (stream_settings.rs)
    MoveTo { capture: CaptureSourceId, room: RoomHandle },
}

/// An open popover and the screen rect of the button it hangs off.
#[derive(Debug, Clone, PartialEq)]
pub struct Popover {
    pub kind: PopoverKind,
    pub anchor: Rect,
}

/// Navigation state: what is shown, which modal / popover is open.
#[derive(Debug, Clone, Default)]
pub struct Nav {
    pub view: View,
    pub modal: Option<Modal>,
    pub popover: Option<Popover>,
}

// ---------------------------------------------------------------------------
// Cross-screen actions
// ---------------------------------------------------------------------------

/// Encoder settings for the *next* capture (what a staged stream goes live
/// with): `AppState`'s process-wide `framerate`/`bitrate_kbps`/`tune`/
/// `quality_level`/`rate_control`/`gop_millis`/`codec` fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NextStream {
    pub framerate: u32,
    pub bitrate_kbps: u32,
    pub tune: simple_screensharing_client::capture::TuneMode,
    pub quality_level: simple_screensharing_client::capture::QualityLevel,
    pub rate_control: simple_screensharing_client::capture::RateControlMode,
    pub gop_millis: u32,
    pub codec: simple_screensharing_client::capture::VideoCodec,
}

impl NextStream {
    /// The current next-capture settings.
    pub fn of(state: &AppState) -> Self {
        Self {
            framerate: state.framerate,
            bitrate_kbps: state.bitrate_kbps,
            tune: state.tune,
            quality_level: state.quality_level,
            rate_control: state.rate_control,
            gop_millis: state.gop_millis,
            codec: state.codec,
        }
    }
}

/// Every cross-screen action. Pushed by screens with [`Cx::act`], applied in
/// order by [`App::apply`] after the frame's screens have drawn (so an action
/// takes visible effect next frame). Handlers call the lib (`session.rs`);
/// see `apply.rs` for exactly what each does.
#[derive(Debug, Clone)]
pub enum UiAction {
    // --- navigation -------------------------------------------------------
    /// Show a room (any connection state). Also sets `AppState::active_room`.
    SelectRoom(RoomKey),
    /// Show "Watched streams".
    ShowWatching,
    /// Open a modal (closes any popover).
    OpenModal(Modal),
    CloseModal,
    /// Open a popover (replaces any open one).
    OpenPopover(Popover),
    ClosePopover,

    // --- rooms ------------------------------------------------------------
    /// Connect a saved room ("Connect").
    Connect(RoomKey),
    /// "Retry now" on the Reconnecting banner: start this room's next
    /// reconnect attempt immediately (`AppState::reconnect_now`).
    RetryNow(RoomKey),
    /// "Cancel" on the Reconnecting banner / "Leave" while reconnecting: stop
    /// reconnecting this room (`AppState::cancel_reconnect`), or abort a
    /// first-time connect to it. The room stays in the list, disconnected.
    CancelReconnect(RoomKey),
    /// Join a pasted link; `room_name` (may be empty) is the local nickname.
    /// On success the room is selected and a `CreateJoin` modal closes; on
    /// failure `Shell::join_error` is set.
    JoinRoom { join_string: String, room_name: String },
    /// Mint a new room on `server` and save it as `room_name` (no connect);
    /// result in `Shell::create_result`. Join it with `JoinRoom`.
    CreateRoom { server: String, room_name: String },
    /// "Leave": leave now, or open [`Modal::LeaveConfirm`] if the room has
    /// shares/watches.
    Leave(RoomHandle),
    /// Leave without asking (the confirm modal's button). Closes the modal.
    LeaveConfirmed(RoomHandle),
    /// Leave (if connected) and remove from the saved list. Closes the modal.
    Forget(RoomKey),
    Rename { room: RoomKey, name: String },
    SetAutoConnect { room: RoomKey, on: bool },
    /// Copy the live connection's invite link (with cert hash) + toast.
    CopyInviteLink(RoomHandle),

    // --- watching ---------------------------------------------------------
    /// Start watching (idempotent). Audio from the same sender follows.
    Watch(StreamKey),
    /// Watch straight into a pop-out window ("Pop out" on a share card).
    WatchPoppedOut(StreamKey),
    /// Stop watching (idempotent); closes its pop-out.
    Unwatch(StreamKey),
    PopOut(StreamKey),
    /// Return a popped-out stream to its tile ("Bring back", window closed).
    BringBack(StreamKey),
    /// Toggle a standalone listen on an audio-only sender.
    ToggleListen(StreamKey),
    /// Viewer volume 0-200 % + mute for a stream (video or audio stream id).
    SetVolume { stream: StreamKey, volume_percent: u32, muted: bool },

    // --- sharing ----------------------------------------------------------
    /// "Share screen": stage a capture for `room` (opens the OS picker).
    /// The stream-settings modal opens by itself once the picker resolves.
    StageShare(RoomHandle),
    /// Go live with a staged capture in `room` (saves the settings as
    /// defaults, attaches any staged audio, closes its settings modal).
    GoLive { capture: CaptureSourceId, room: RoomHandle },
    /// Throw a staged capture away (closes its settings modal).
    Discard(CaptureSourceId),
    /// Stop one destination (the × on a dest row, "Stop sharing" in a room).
    /// The capture stops too if it was its last destination.
    StopDestination(DestinationId),
    /// Stop every capture ("Stop all sharing").
    StopAllSharing,
    /// Also send `capture` to `room` (fan-out).
    AddDestination { capture: CaptureSourceId, room: RoomHandle },
    /// Move one destination to `room` (make-before-break).
    MoveDestination { destination: DestinationId, room: RoomHandle },
    /// Move every destination of `capture` into `room` ("Move X here").
    MoveCaptureHere { capture: CaptureSourceId, room: RoomHandle },
    /// Replace a live destination's settings and apply them now.
    EditDestination { destination: DestinationId, edit: DestEdit },
    ApplyPresetToDestination { destination: DestinationId, preset: EncoderPreset },
    /// Settings a staged capture goes live with.
    SetNextStream(NextStream),
    ApplyPresetToNextStream(EncoderPreset),
    SetBandwidthMode(BandwidthMode),
    SetBandwidthTotal(u32),
    SetBandwidthFloor(u32),

    // --- shared audio -----------------------------------------------------
    /// Share (or switch to) `source` alongside video `capture`. For a staged
    /// capture it is remembered and attached at Go live.
    ShareAudio { capture: CaptureSourceId, source: AudioSource },
    StopSharingAudio { capture: CaptureSourceId },
    SetSharedAudioMuted { capture: CaptureSourceId, muted: bool },
    /// Re-list audio sources (async; result in `cx.state.audio_sources`).
    RefreshAudioSources,
    /// Start level meters for these audio source ids (a picker opened).
    /// Read them with [`Cx::source_level`]. Replaces any running taps.
    StartLevelTaps(Vec<u32>),
    /// Stop every source meter (the picker closed).
    StopLevelTaps,

    // --- settings ---------------------------------------------------------
    SetDisplayName(String),
    SetLanguage(String),
    SetPresentationMode(PresentationMode),
    SetJitterBuffer(u32),
    SetCatchUpSpeed(u32),

    // --- misc -------------------------------------------------------------
}

// ---------------------------------------------------------------------------
// Shell state shared by screens (read-only to them)
// ---------------------------------------------------------------------------

/// A capture staged for one room (picker open, or picked but not live).
#[derive(Debug, Clone)]
pub struct StagedShare {
    pub capture: CaptureSourceId,
    /// Audio chosen before going live; attached at Go live (the lib only
    /// shares audio next to a live capture).
    pub audio: Option<AudioSource>,
    /// Whether the settings modal was auto-opened when the picker resolved.
    pub modal_opened: bool,
}

/// A level tap being built: (generation, audio source id, result).
type PendingTap = (u64, u32, oneshot::Receiver<Result<AudioLevelTapHandle, String>>);

/// A transient message at the bottom centre.
#[derive(Debug, Clone)]
pub struct Toast {
    pub text: String,
    pub icon: &'static str,
    /// `ctx.input(|i| i.time)` after which it disappears.
    pub until: f64,
}

/// UI-level state that more than one screen reads, owned by the shell.
#[derive(Default)]
pub struct Shell {
    /// Staged captures by room. `Why` here and not in the lib: a staged
    /// capture has no destination, so nothing in `AppState` says which room
    /// it is for.
    pub staged: indexmap::IndexMap<RoomHandle, StagedShare>,
    pub toasts: Vec<Toast>,
    /// Result of the last [`UiAction::CreateRoom`]; the create/join modal
    /// shows the minted link from here.
    pub create_result: Option<Result<JoinLink, String>>,
    /// Last failure of a [`UiAction::JoinRoom`].
    pub join_error: Option<String>,
    /// The join string a `JoinRoom` is waiting on (select it on success).
    pub awaiting_join: Option<RoomKey>,
    /// Running source meters: audio source id → level tap id.
    pub level_taps: HashMap<u32, u32>,
    /// Level taps being built (async), tagged with a generation so a stale
    /// build after `StopLevelTaps` is discarded.
    pending_taps: Vec<PendingTap>,
    tap_generation: u64,
}

// ---------------------------------------------------------------------------
// Screen context + selectors
// ---------------------------------------------------------------------------

/// A room's connection state as the UI shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnState {
    Joined,
    /// A connect for this room is in flight (first connect or reconnect).
    Reconnecting,
    Disconnected,
}

/// One row of the room list: saved rooms in saved order, then live rooms
/// that aren't saved (and an in-flight connect to an unsaved link).
#[derive(Debug, Clone)]
pub struct RoomEntry {
    pub key: RoomKey,
    /// Display name (saved nickname or id-derived fallback; never the key).
    pub name: String,
    pub handle: Option<RoomHandle>,
    pub conn: ConnState,
    pub auto_connect: bool,
}

/// What the action bar shows for a room (also used for "Live in N rooms").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareState {
    /// Nothing of ours in this room.
    Idle,
    /// Staged, OS picker still open (capture `Starting`).
    Picking { capture: CaptureSourceId },
    /// Staged, picked, not live.
    Staged { capture: CaptureSourceId },
    /// One of our video captures has a destination here.
    Live { capture: CaptureSourceId, destination: DestinationId },
    /// The room is reconnecting.
    Paused,
}

/// Our share's phase in one room ([`Cx::my_phase`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MyPhase {
    /// Sending (Live, Rebuilding or Stalled).
    OnAir,
    /// No destination there is on air yet; at least one is Starting.
    Starting,
}

/// What every screen gets: read access to all shared state, and the action
/// queue. See the module docs for the contract.
pub struct Cx<'a> {
    pub state: &'a AppState,
    pub nav: &'a Nav,
    pub shell: &'a Shell,
    pub videos: &'a VideoViews,
    actions: &'a mut Vec<UiAction>,
}

impl<'a> Cx<'a> {
    pub fn new(state: &'a AppState, nav: &'a Nav, shell: &'a Shell, videos: &'a VideoViews, actions: &'a mut Vec<UiAction>) -> Self {
        Self { state, nav, shell, videos, actions }
    }

    /// Queue an action (applied after this frame's screens).
    pub fn act(&mut self, action: UiAction) {
        self.actions.push(action);
    }

    /// Every room, in list order.
    pub fn rooms(&self) -> Vec<RoomEntry> {
        room_entries(self.state)
    }

    /// One room by key.
    pub fn room(&self, key: &str) -> Option<RoomEntry> {
        self.rooms().into_iter().find(|r| same_room(&r.key, key))
    }

    /// The room the central area shows, if the view is a room view.
    pub fn selected_room(&self) -> Option<RoomEntry> {
        match &self.nav.view {
            View::Room(key) => self.room(key),
            _ => None,
        }
    }

    /// Whether the People panel and the action bar are shown this frame.
    pub fn shows_room_panels(&self) -> bool {
        self.selected_room().is_some_and(|r| r.conn != ConnState::Disconnected)
    }

    /// A joined room's session.
    pub fn session(&self, handle: RoomHandle) -> Option<&'a RoomSession> {
        self.state.rooms.get(&handle)
    }

    /// Display name of a room handle (never the key).
    pub fn room_name(&self, handle: RoomHandle) -> String {
        match self.state.rooms.get(&handle) {
            Some(room) => room_label(&self.state.room_store, &room.join_string),
            None => tr("ui.room.unknown", "Unknown room").to_string(),
        }
    }

    /// Display name of `owner` in `room` ("?" if unknown).
    pub fn member_name(&self, room: RoomHandle, owner: ConnId) -> String {
        self.session(room)
            .and_then(|s| s.members.iter().find(|m| m.conn_id == owner))
            .map(|m| m.display_name.clone())
            .unwrap_or_else(|| "?".to_string())
    }

    /// Our own connection id in `room`.
    pub fn my_conn(&self, room: RoomHandle) -> Option<ConnId> {
        self.session(room).map(|s| s.client.conn_id())
    }

    /// Other people's shares in `room` that get a card: video streams, plus
    /// audio streams of senders with no video (audio of a video sender rides
    /// along with the video watch).
    pub fn shares_in(&self, room: RoomHandle) -> Vec<&'a StreamInfo> {
        let Some(s) = self.session(room) else { return Vec::new() };
        let me = s.client.conn_id();
        s.streams
            .iter()
            .filter(|i| i.owner != me)
            .filter(|i| !(i.kind == StreamKind::Audio && s.owner_has_video(i.owner)))
            .collect()
    }

    /// Whether `key` is being watched (video) or listened to (audio-only).
    pub fn is_watching(&self, key: StreamKey) -> bool {
        let Some(s) = self.session(key.0) else { return false };
        s.watches.lock().unwrap().contains_key(&(key.1, key.2)) || s.audio_watches.lock().unwrap().contains_key(&(key.1, key.2))
    }

    /// Whether `key` is shown in a pop-out window.
    pub fn is_popped(&self, key: StreamKey) -> bool {
        self.videos.get(key).is_some_and(|e| e.popped)
    }

    /// Every watched video stream, across rooms (for "Watched streams").
    pub fn watched(&self) -> Vec<StreamKey> {
        self.videos.keys()
    }

    /// The share state of a room entry for the action bar.
    pub fn share_state(&self, room: &RoomEntry) -> ShareState {
        if room.conn == ConnState::Reconnecting {
            return ShareState::Paused;
        }
        let Some(handle) = room.handle else { return ShareState::Idle };
        share_state(self.state, self.shell, handle)
    }

    /// Rooms one of our video captures is live in **or still starting in**
    /// (any destination). For "is something of ours in that room"; use
    /// [`Self::on_air_rooms`] for "Live in N rooms".
    pub fn live_rooms(&self) -> Vec<RoomHandle> {
        let mut out: Vec<RoomHandle> = Vec::new();
        for c in self.state.captures.values().filter(|c| c.kind == StreamKind::Video) {
            for d in c.destinations.values() {
                if !out.contains(&d.room) {
                    out.push(d.room);
                }
            }
        }
        out
    }

    /// Where our share in `room` stands: `None` if nothing of ours is there.
    /// `Why:` a destination stays `Starting` until its first frame (an idle
    /// screen can take a long time); showing it as Live meanwhile was wrong.
    pub fn my_phase(&self, room: RoomHandle) -> Option<MyPhase> {
        let mut out = None;
        for c in self.state.captures.values().filter(|c| c.kind == StreamKind::Video) {
            for d in c.destinations.values().filter(|d| d.room == room) {
                match c.destination_state(d) {
                    DestinationState::Live | DestinationState::Rebuilding { .. } | DestinationState::Stalled { .. } => return Some(MyPhase::OnAir),
                    DestinationState::Starting => out = Some(MyPhase::Starting),
                    DestinationState::Failed(_) => {}
                }
            }
        }
        out
    }

    /// Rooms our screen is actually on air in (Live, Rebuilding or Stalled;
    /// not Starting or Failed). For "Live in N rooms" and live badges.
    pub fn on_air_rooms(&self) -> Vec<RoomHandle> {
        self.live_rooms().into_iter().filter(|h| self.my_phase(*h) == Some(MyPhase::OnAir)).collect()
    }

    /// Our video captures that have at least one destination.
    pub fn live_captures(&self) -> Vec<(CaptureSourceId, &'a LocalCapture)> {
        self.state
            .captures
            .iter()
            .filter(|(_, c)| c.kind == StreamKind::Video && !c.destinations.is_empty())
            .map(|(id, c)| (*id, c))
            .collect()
    }

    /// A capture by id.
    pub fn capture(&self, id: CaptureSourceId) -> Option<&'a LocalCapture> {
        self.state.captures.get(&id)
    }

    /// The audio capture paired with video capture `capture`, if any.
    pub fn paired_audio(&self, capture: CaptureSourceId) -> Option<(CaptureSourceId, &'a LocalCapture)> {
        paired_audio(self.state, capture).and_then(|id| self.state.captures.get(&id).map(|c| (id, c)))
    }

    /// Live state of a destination as one value for pills.
    pub fn destination_state(&self, capture: &LocalCapture, dest: &Destination) -> DestinationState {
        capture.destination_state(dest)
    }

    /// Level 0..1 of a source meter started with [`UiAction::StartLevelTaps`].
    pub fn source_level(&self, source_id: u32) -> Option<f32> {
        self.shell.level_taps.get(&source_id).and_then(|tap| self.state.audio_level_tap_level(*tap))
    }


    /// The open popover if it is `kind`.
    pub fn popover_of(&self, kind: &PopoverKind) -> Option<&'a Popover> {
        self.nav.popover.as_ref().filter(|p| &p.kind == kind)
    }

    /// Push the action that toggles popover `kind` anchored at `anchor`.
    pub fn toggle_popover(&mut self, kind: PopoverKind, anchor: Rect) {
        if self.popover_of(&kind).is_some() {
            self.act(UiAction::ClosePopover);
        } else {
            self.act(UiAction::OpenPopover(Popover { kind, anchor }));
        }
    }
}

/// Whether two join strings name the same room (server + room id + key; a
/// different relay cert hash is the same room). `Why` never `==`: a saved
/// spelling and a live session's spelling can differ only in the cert hash
/// (#31), and `==` then shows one room twice or connects it twice.
pub fn same_room(a: &str, b: &str) -> bool {
    simple_screensharing_client::rooms::RoomStore::same_room(a, b)
}

/// Whether `view` shows the room `key` names.
pub fn view_is_room(view: &View, key: &str) -> bool {
    matches!(view, View::Room(k) if same_room(k, key))
}

/// All rooms, in list order. See [`RoomEntry`].
pub fn room_entries(state: &AppState) -> Vec<RoomEntry> {
    let handle_of = |js: &str| state.rooms.iter().find(|(_, r)| same_room(&r.join_string, js)).map(|(h, _)| *h);
    let conn_of = |js: &str, handle: Option<RoomHandle>| match handle {
        Some(_) => ConnState::Joined,
        None if state.is_reconnecting(js) => ConnState::Reconnecting,
        None => ConnState::Disconnected,
    };
    let mut out: Vec<RoomEntry> = state
        .room_store
        .rooms
        .iter()
        .map(|saved| {
            let handle = handle_of(&saved.join_string);
            RoomEntry {
                key: saved.join_string.clone(),
                name: saved.name.clone(),
                handle,
                conn: conn_of(&saved.join_string, handle),
                auto_connect: saved.auto_connect,
            }
        })
        .collect();
    for (handle, room) in &state.rooms {
        if out.iter().any(|e| same_room(&e.key, &room.join_string)) {
            continue;
        }
        out.push(RoomEntry {
            key: room.join_string.clone(),
            name: room_label(&state.room_store, &room.join_string),
            handle: Some(*handle),
            conn: ConnState::Joined,
            auto_connect: false,
        });
    }
    if let Some(p) = &state.pending_connect
        && !out.iter().any(|e| same_room(&e.key, &p.join_string))
    {
        out.push(RoomEntry {
            key: p.join_string.clone(),
            name: room_label(&state.room_store, &p.join_string),
            handle: None,
            conn: ConnState::Reconnecting,
            auto_connect: false,
        });
    }
    out
}

/// [`Cx::share_state`] for a joined room.
pub fn share_state(state: &AppState, shell: &Shell, room: RoomHandle) -> ShareState {
    for (id, c) in &state.captures {
        if c.kind != StreamKind::Video {
            continue;
        }
        if let Some((dest_id, _)) = c.destinations.iter().find(|(_, d)| d.room == room) {
            return ShareState::Live { capture: *id, destination: *dest_id };
        }
    }
    if let Some(staged) = shell.staged.get(&room)
        && let Some(c) = state.captures.get(&staged.capture)
    {
        return match c.status {
            ShareStatus::Starting(_) => ShareState::Picking { capture: staged.capture },
            _ => ShareState::Staged { capture: staged.capture },
        };
    }
    ShareState::Idle
}

/// The audio capture riding on video capture `capture`.
pub fn paired_audio(state: &AppState, capture: CaptureSourceId) -> Option<CaptureSourceId> {
    state.captures.iter().find(|(_, c)| c.audio_for_capture == Some(capture)).map(|(id, _)| *id)
}



// ---------------------------------------------------------------------------
// Strings
// ---------------------------------------------------------------------------

/// `t!(key)` in the current locale, or `en` when the locale files have no
/// entry for `key` (rust-i18n then returns the key itself, or
/// `<locale>.<key>`). Every UI string goes through this or [`tr_with`].
/// `Why` a fallback at all: a missing key still reads as English at its real
/// width instead of `ui.sidebar.rooms`, and the English source text sits next
/// to each key for `client/locales/check_keys.py` and translators.
pub fn tr(key: &str, en: &'static str) -> String {
    let s = t!(key);
    if s.as_ref() == key || s.ends_with(key) { en.to_string() } else { s.into_owned() }
}

/// [`tr`] with `%{name}` placeholders filled from `args`.
pub fn tr_with(key: &str, en: &'static str, args: &[(&str, &str)]) -> String {
    let mut s = tr(key, en);
    for (k, v) in args {
        s = s.replace(&format!("%{{{k}}}"), v);
    }
    s
}

// ---------------------------------------------------------------------------
// Shared drawing helpers (popover chrome, placeholders)
// ---------------------------------------------------------------------------

/// Popover chrome (mockup `.popup`): `PANEL_RAISED`, 1 px border, radius 10,
/// padding 6, drop shadow, `width` wide (mockup 340), below the anchor
/// (above when there's no room), clamped to the window. Rows inside are
/// `widgets::pop_item` / `heading`. Returns `false` when it should close
/// (click outside); the caller then pushes [`UiAction::ClosePopover`].
pub fn popover(ctx: &egui::Context, pop: &Popover, width: f32, body: impl FnOnce(&mut Ui)) -> bool {
    let id = Id::new(("ui_popover", format!("{:?}", pop.kind)));
    let mut open = true;
    let frame = Frame::new()
        .fill(theme::PANEL_RAISED)
        .stroke(Stroke::new(1.0_f32, theme::BORDER))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(6))
        .shadow(egui::Shadow { offset: [0, 14], blur: 40, spread: 0, color: Color32::from_black_alpha(153) });
    // Last frame's size (the first frame assumes a zero height and corrects
    // itself on the next).
    let size = egui::AreaState::load(ctx, id).and_then(|s| s.size).unwrap_or(vec2(width, 0.0));
    let pos = popover_pos(ctx.content_rect(), pop.anchor, size);
    let top_left = egui::RectAlign { parent: egui::Align2::LEFT_TOP, child: egui::Align2::LEFT_TOP };
    egui::Popup::new(id, ctx.clone(), pos, egui::LayerId::new(egui::Order::Foreground, id))
        .open_bool(&mut open)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .align(top_left)
        .align_alternatives(&[])
        .gap(0.0)
        .width(width)
        .frame(frame)
        .show(|ui| {
            ui.set_width(width - 12.0);
            ui.spacing_mut().item_spacing.y = 0.0;
            body(ui)
        });
    open
}

/// Where a popover of `size` anchored at `anchor` goes in `screen` (the
/// mockup's `positionPopup`): left edges aligned, 6 px below the anchor, or
/// above it when it would cross the window bottom; pushed left to stay 8 px
/// inside the right edge, never closer than 8 px to the top/left.
/// `Why` not egui's `RectAlign` alternatives: those flip to right-aligned
/// (`*_END`) near the right edge, so the bottom bar's "N rooms ▾" popover
/// jumped to the button's left instead of hanging off it like the mockup.
pub fn popover_pos(screen: Rect, anchor: Rect, size: egui::Vec2) -> egui::Pos2 {
    const GAP: f32 = 6.0;
    const EDGE: f32 = 8.0;
    let mut x = anchor.left();
    if x + size.x > screen.right() - EDGE {
        x = screen.right() - size.x - EDGE;
    }
    let mut y = anchor.bottom() + GAP;
    if y + size.y > screen.bottom() - EDGE {
        y = anchor.top() - size.y - GAP;
    }
    pos2(x.max(screen.left() + EDGE), y.max(screen.top() + EDGE))
}



// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

/// The app: session state + navigation + per-screen state.
pub struct App {
    pub state: AppState,
    pub nav: Nav,
    pub shell: Shell,
    pub videos: VideoViews,
    pub sidebar: sidebar::SidebarState,
    pub room: room::RoomState,
    pub action_bar: action_bar::ActionBarState,
    pub stream_settings: stream_settings::StreamSettingsState,
    pub modals: modals::ModalsState,
    pub popout: popout::PopoutState,
    /// `ctx.input(|i| i.time)` at the start of this frame (toast clock).
    now: f64,
}

impl App {
    /// Build the app. Needs eframe's wgpu renderer (registers the GPU video
    /// renderer once) and an entered Tokio runtime (`AppState::default`
    /// binds the QUIC endpoint).
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theme::apply(&cc.egui_ctx);
        match cc.wgpu_render_state.as_ref() {
            Some(rs) => simple_screensharing_client::decode::render::VideoRenderer::register(rs),
            None => tracing::error!("no wgpu render state: video tiles will stay black"),
        }
        // SIGTERM/SIGINT/SIGHUP close the window, so `on_exit` leaves every
        // room cleanly and peers drop us at once instead of on a timeout.
        simple_screensharing_client::session::install_shutdown_signal_handlers(cc.egui_ctx.clone());
        Self::with_state(AppState::default())
    }

    /// Build around an existing `AppState` (tests). Queues every saved room
    /// flagged auto-connect, and selects the first saved room.
    pub fn with_state(mut state: AppState) -> Self {
        let auto: Vec<String> = state.room_store.rooms.iter().filter(|r| r.auto_connect).map(|r| r.join_string.clone()).collect();
        state.auto_connect_queue.extend(auto);
        let view = state.room_store.rooms.first().map(|r| View::Room(r.join_string.clone())).unwrap_or_default();
        Self {
            state,
            nav: Nav { view, modal: None, popover: None },
            shell: Shell::default(),
            videos: VideoViews::default(),
            sidebar: Default::default(),
            room: Default::default(),
            action_bar: Default::default(),
            stream_settings: Default::default(),
            modals: Default::default(),
            popout: Default::default(),
            now: 0.0,
        }
    }

    /// Per-frame session polling, revived from the old `ui.rs` `update()`:
    /// auto-connect queue, connect result, events, capture/destination/edit
    /// state, audio sources, bandwidth allocation; then UI bookkeeping
    /// (video views, staged captures, level taps, toasts).
    fn poll(&mut self, ctx: &egui::Context) {
        self.state.poll_auto_connect_queue();
        match self.state.poll_connect_result() {
            ConnectOutcome::None => {}
            ConnectOutcome::Joined(handle) => self.on_joined(handle),
            ConnectOutcome::Failed { error, from_add_card } => {
                if from_add_card {
                    self.shell.join_error = Some(error);
                }
                self.shell.awaiting_join = None;
            }
        }
        self.state.drain_events(ctx);
        self.state.poll_capture_states();
        self.state.poll_pending_destinations();
        self.state.poll_destination_edits();
        self.state.poll_audio_sources();
        // Rate-limited + hysteresis-filtered internally; cheap most frames.
        self.state.apply_bandwidth_allocation();

        // A failed share start/stop would otherwise vanish without a word
        // (nothing else in the new UI shows `share_error`).
        if let Some(e) = self.state.share_error.take() {
            self.now = ctx.input(|i| i.time);
            self.toast(tr_with("share.error", "Share error: %{error}", &[("error", &e)]), "error");
        }
        self.videos.sync(&self.state);
        self.poll_staged();
        self.poll_level_taps();
        let now = ctx.input(|i| i.time);
        self.shell.toasts.retain(|t| t.until > now);
        // A modal or view pointing at something that's gone falls back.
        if let Some(Modal::StreamSettings { capture, .. }) = &self.nav.modal
            && !self.state.captures.contains_key(capture)
        {
            self.nav.modal = None;
        }
        if let Some(Modal::LeaveConfirm { room }) = &self.nav.modal
            && !self.state.rooms.contains_key(room)
        {
            self.nav.modal = None;
        }
    }

    fn on_joined(&mut self, handle: RoomHandle) {
        let Some(js) = self.state.rooms.get(&handle).map(|r| r.join_string.clone()) else { return };
        let awaited = self.shell.awaiting_join.as_deref() == Some(js.as_str());
        if awaited {
            self.shell.awaiting_join = None;
            self.shell.join_error = None;
            // "Join now" after Create keeps the modal open: it then offers the
            // live connection's invite link (with the relay cert hash).
            let minted = matches!(&self.shell.create_result, Some(Ok(l)) if same_room(&l.join_string, &js));
            if self.nav.modal == Some(Modal::CreateJoin) && !minted {
                self.nav.modal = None;
            }
        }
        if awaited || self.nav.view == View::Welcome {
            self.nav.view = View::Room(js);
            self.state.active_room = Some(handle);
        }
        let name = self.state.rooms.get(&handle).map(|r| room_label(&self.state.room_store, &r.join_string)).unwrap_or_default();
        self.toast(tr_with("ui.toast.connected", "Connected to %{room}", &[("room", &name.to_string())]), "check_circle");
    }

    /// Drop staged entries whose capture is gone or whose room is gone (and
    /// stop that capture: nothing else would); auto-open the settings modal
    /// once the picker resolves (mockup `stage()`).
    ///
    /// `Why a reconnecting room keeps its staged share:` the handle is stable
    /// across the outage (`ReconnectState::handle`), so the staged card simply
    /// reappears when the room is back instead of re-prompting the OS picker.
    fn poll_staged(&mut self) {
        let state = &self.state;
        let mut orphaned = Vec::new();
        self.shell.staged.retain(|room, s| {
            let capture_ok = state.captures.get(&s.capture).is_some_and(|c| c.destinations.is_empty() && !matches!(c.status, ShareStatus::Stopping(_)));
            let room_ok = state.rooms.contains_key(room) || state.reconnects.contains_key(room);
            if capture_ok && !room_ok {
                orphaned.push(s.capture);
            }
            capture_ok && room_ok
        });
        for capture in orphaned {
            self.state.stop_share(capture);
        }
        let selected = match &self.nav.view {
            View::Room(key) => self.state.rooms.iter().find(|(_, r)| same_room(&r.join_string, key)).map(|(h, _)| *h),
            _ => None,
        };
        for (room, s) in self.shell.staged.iter_mut() {
            let ready = self.state.captures.get(&s.capture).is_some_and(|c| matches!(c.status, ShareStatus::Active(_)));
            if ready && !s.modal_opened {
                s.modal_opened = true;
                if self.nav.modal.is_none() && selected == Some(*room) {
                    self.nav.modal = Some(Modal::StreamSettings { capture: s.capture, room: *room });
                    self.nav.popover = None;
                }
            }
        }
    }

    fn poll_level_taps(&mut self) {
        let generation = self.shell.tap_generation;
        let mut done = Vec::new();
        self.shell.pending_taps.retain_mut(|(g, source, rx)| match rx.try_recv() {
            Ok(result) => {
                done.push((*g, *source, result));
                false
            }
            Err(oneshot::error::TryRecvError::Empty) => true,
            Err(oneshot::error::TryRecvError::Closed) => false,
        });
        for (g, source, result) in done {
            match result {
                Ok(handle) => {
                    let tap = self.state.insert_audio_level_tap(handle);
                    if g == generation {
                        self.shell.level_taps.insert(source, tap);
                    } else {
                        self.state.stop_audio_level_tap(tap);
                    }
                }
                Err(e) => tracing::debug!("level tap for audio source {source} failed: {e}"),
            }
        }
    }

    pub(crate) fn toast(&mut self, text: String, icon: &'static str) {
        self.shell.toasts.push(Toast { text, icon, until: self.now + TOAST_SECS });
    }

    /// Lay the window out and run every screen. Returns the actions they pushed.
    fn draw(&mut self, ctx: &egui::Context) -> Vec<UiAction> {
        let mut actions = Vec::new();
        let App { state, nav, shell, videos, sidebar, room, action_bar, stream_settings, modals, popout, .. } = self;
        let mut cx = Cx::new(state, nav, shell, videos, &mut actions);
        let room_panels = cx.shows_room_panels();

        // Left sidebar: brand band / scroll body / user band, split by hand
        // so each region is exactly its spec size whatever its content does.
        egui::SidePanel::left("sidebar")
            .resizable(false)
            .exact_width(theme::SIDEBAR_W)
            .show_separator_line(false)
            .frame(Frame::NONE.fill(theme::SIDEBAR_RAIL))
            .show(ctx, |ui| {
                let full = ui.max_rect();
                ui.painter().vline(full.right() - 0.5, full.y_range(), Stroke::new(1.0_f32, theme::PANEL_RAISED));
                let inner = Rect::from_min_max(full.min, pos2(full.right() - 1.0, full.bottom()));
                let brand = Rect::from_min_size(inner.min, vec2(inner.width(), theme::BAND_H));
                let user = Rect::from_min_max(pos2(inner.left(), inner.bottom() - theme::BAND_H), inner.max);
                let body = Rect::from_min_max(pos2(inner.left(), brand.bottom()), pos2(inner.right(), user.top()));
                region(ui, brand, "brand", |ui| {
                    widgets::band(ui, theme::SIDEBAR_RAIL, BandEdge::None, theme::S4, |ui| sidebar::show_brand(ui, &mut cx, sidebar))
                });
                region(ui, body, "sidebar_body", |ui| {
                    egui::ScrollArea::vertical().id_salt("sidebar_scroll").auto_shrink([false, false]).show(ui, |ui| {
                        Frame::NONE.inner_margin(Margin { left: theme::S3 as i8, right: theme::S3 as i8, top: 0, bottom: theme::S3 as i8 }).show(
                            ui,
                            |ui| vbox(ui, theme::S2, |ui| sidebar::show_body(ui, &mut cx, sidebar)),
                        );
                    });
                });
                region(ui, user, "user_band", |ui| {
                    user_band(ui, |ui| sidebar::show_user_band(ui, &mut cx, sidebar));
                });
            });

        // Room header band (spans the central width, People panel included).
        egui::TopBottomPanel::top("room_header")
            .resizable(false)
            .exact_height(theme::BAND_H)
            .show_separator_line(false)
            .frame(Frame::NONE.fill(theme::PANEL_BASE))
            .show(ctx, |ui| {
                let r = ui.max_rect();
                region(ui, r, "room_header", |ui| {
                    widgets::band(ui, theme::PANEL_BASE, BandEdge::Bottom, theme::S4, |ui| room::show_header(ui, &mut cx, room))
                });
            });

        if room_panels {
            egui::TopBottomPanel::bottom("action_bar")
                .resizable(false)
                .exact_height(theme::BAND_H)
                .show_separator_line(false)
                .frame(Frame::NONE.fill(theme::PANEL_SUNKEN))
                .show(ctx, |ui| {
                    let r = ui.max_rect();
                    region(ui, r, "action_bar", |ui| {
                        widgets::band(ui, theme::PANEL_SUNKEN, BandEdge::Top, theme::S4, |ui| action_bar::show(ui, &mut cx, action_bar))
                    });
                });
            egui::SidePanel::right("people")
                .resizable(false)
                .exact_width(theme::PEOPLE_W)
                .show_separator_line(false)
                .frame(Frame::NONE.fill(theme::PANEL_SUNKEN))
                .show(ctx, |ui| {
                    let full = ui.max_rect();
                    ui.painter().vline(full.left() + 0.5, full.y_range(), Stroke::new(1.0_f32, theme::DIVIDER));
                    let inner = Rect::from_min_max(pos2(full.left() + 1.0, full.top()), full.max);
                    region(ui, inner, "people", |ui| {
                        egui::ScrollArea::vertical().id_salt("people_scroll").auto_shrink([false, false]).show(ui, |ui| {
                            Frame::NONE
                                .inner_margin(Margin::symmetric(theme::S3 as i8, theme::S4 as i8))
                                .show(ui, |ui| vbox(ui, theme::S3, |ui| room::show_people(ui, &mut cx, room)));
                        });
                    });
                });
        }

        egui::CentralPanel::default().frame(Frame::NONE.fill(theme::PANEL_BASE)).show(ctx, |ui| {
            let r = ui.max_rect();
            region(ui, r, "room_body", |ui| {
                egui::ScrollArea::vertical().id_salt("room_body_scroll").auto_shrink([false, false]).show(ui, |ui| {
                    Frame::NONE
                        .inner_margin(Margin::same(theme::S4 as i8))
                        .show(ui, |ui| vbox(ui, theme::S4, |ui| room::show_body(ui, &mut cx, room)));
                });
            });
        });

        // Modals over the panels.
        match cx.nav.modal.clone() {
            Some(Modal::StreamSettings { capture, room }) => stream_settings::show(ctx, &mut cx, stream_settings, capture, room),
            Some(m) => modals::show(ctx, &mut cx, modals, &m),
            None => {}
        }
        // Pop-out windows: re-declared every frame they should exist.
        popout::show(ctx, &mut cx, popout);

        paint_toasts(ctx, &cx.shell.toasts);
        actions
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.now = ctx.input(|i| i.time);
        self.poll(ctx);

        // Escape closes the popover before anything else sees it (a modal
        // would otherwise close on the same press).
        if self.nav.popover.is_some() && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            self.nav.popover = None;
        }

        let actions = self.draw(ctx);
        for action in actions {
            self.apply(ctx, action);
        }

        // Keep polling the connect future / event channels with no input;
        // coarser while unfocused. `None` (backend can't tell) = focused.
        let focused = ctx.input(|i| i.viewport().focused).unwrap_or(true);
        if matches!(self.state.status, Status::Connecting) || !self.state.rooms.is_empty() || !self.shell.toasts.is_empty() {
            ctx.request_repaint_after(if focused { IDLE_REPAINT_INTERVAL } else { UNFOCUSED_REPAINT_INTERVAL });
        }
    }

    /// Clean exit: leave every room (`Leave` + close, so peers drop us at
    /// once) and stop every capture, blocking up to 1.5 s. Also the path a
    /// SIGTERM/SIGINT/SIGHUP takes (see `App::new`).
    fn on_exit(&mut self) {
        self.state.shutdown_blocking(std::time::Duration::from_millis(1500));
    }
}

/// The shell regions laid out in the latest pass, by name ("brand",
/// "sidebar_body", "user_band", "room_header", "action_bar", "people",
/// "room_body"). For tests and layout QC.
#[cfg(test)]
pub fn shell_regions(ctx: &egui::Context) -> Vec<(&'static str, Rect)> {
    ctx.data(|d| d.get_temp::<(u64, Vec<(&'static str, Rect)>)>(Id::new("ui_shell_regions"))).map(|(_, v)| v).unwrap_or_default()
}

/// Run `add` in a child `Ui` that is exactly `rect` (max rect + clip).
fn region<R>(ui: &mut Ui, rect: Rect, salt: &'static str, add: impl FnOnce(&mut Ui) -> R) -> R {
    let pass = ui.ctx().cumulative_pass_nr();
    ui.ctx().data_mut(|d| {
        let log = d.get_temp_mut_or_default::<(u64, Vec<(&'static str, Rect)>)>(Id::new("ui_shell_regions"));
        if log.0 != pass {
            *log = (pass, Vec::new());
        }
        log.1.push((salt, rect));
    });
    let mut child = ui.new_child(UiBuilder::new().id_salt(salt).max_rect(rect).layout(Layout::top_down(Align::Min)));
    child.set_clip_rect(rect.intersect(ui.clip_rect()));
    widgets::debug_vbox(&child, rect);
    add(&mut child)
}

/// A fixed-width top-down vbox with `gap` between items.
fn vbox<R>(ui: &mut Ui, gap: f32, add: impl FnOnce(&mut Ui) -> R) -> R {
    let w = ui.available_width();
    ui.set_width(w);
    ui.spacing_mut().item_spacing.y = gap;
    add(ui)
}

/// The user band (`.p-user`): `ELEV_14`, `WELL_BORDER` top line, padding 12.
/// Same 56 px and same y as the action band, so the two top lines are one.
fn user_band(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    widgets::band(ui, theme::ELEV_14, BandEdge::None, theme::S3, |ui| {
        let r = ui.max_rect().expand2(vec2(theme::S3, 0.0));
        ui.painter().hline(r.x_range(), r.top() + 0.5, Stroke::new(1.0_f32, theme::WELL_BORDER));
        add(ui)
    });
}

/// Toasts (mockup `.toasts`): bottom centre, 72 px above the window bottom,
/// stacked upwards, pill-shaped, `PANEL_RAISED` + border, icon in `SUCCESS` (`DANGER` for the "error" icon).
fn paint_toasts(ctx: &egui::Context, toasts: &[Toast]) {
    if toasts.is_empty() {
        return;
    }
    let screen = ctx.content_rect();
    let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Tooltip, Id::new("ui_toasts")));
    let f = font(Weight::Regular, theme::FS_SMALL);
    let mut y = screen.bottom() - 72.0;
    for t in toasts.iter().rev() {
        let tw = ctx.fonts_mut(|fonts| fonts.layout_no_wrap(t.text.clone(), f.clone(), theme::TEXT).size().x);
        let w = 14.0 + 16.0 + theme::S2 + tw + 14.0;
        let rect = Rect::from_min_size(pos2(screen.center().x - w / 2.0, y - theme::CTL_H), vec2(w, theme::CTL_H));
        painter.rect(rect, CornerRadius::same(16), theme::PANEL_RAISED, Stroke::new(1.0_f32, theme::BORDER), egui::StrokeKind::Inside);
        let ir = Rect::from_min_size(pos2(rect.left() + 14.0, rect.top()), vec2(16.0, rect.height()));
        let glyph = theme::icon(t.icon);
        let icon_col = if t.icon == "error" { theme::DANGER } else { theme::SUCCESS };
        painter.text(ir.center(), egui::Align2::CENTER_CENTER, glyph, theme::icon_font(16.0), icon_col);
        painter.text(pos2(ir.right() + theme::S2, rect.center().y), egui::Align2::LEFT_CENTER, &t.text, f.clone(), theme::TEXT);
        y -= theme::CTL_H + theme::S1;
    }
}
