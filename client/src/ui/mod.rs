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
//! │ brand band 56 ............. sidebar::show_brand                    ││ room::show_header (title, status pill, meta; band, pad 16)│
//! │ ScrollArea (pad 0/12/12, vbox gap 8) .. sidebar::show_body         │├──────────────────────────────────────────────────────────┤
//! │                                                                    ││ CentralPanel: room::show_stage = the black stage          │
//! │                                                                    ││   streams · people strip · control bar (ui/stage/)        │
//! │                                                                    ││ or ScrollArea .. room::show_body (every other state)      │
//! │ user band 56 (ELEV_14, WELL_BORDER top) .. sidebar::show_user_band ││                                                          │
//! └────────────────────────────────────────────────────────────────────┘└──────────────────────────────────────────────────────────┘
//! ```
//!
//! The People panel and the bottom action band are gone (task I3, 2026-11-04): the stage's people strip and
//! control bar replace them. The sidebar hides with the bar's button ([`Nav::sidebar_hidden`]); in stage
//! fullscreen ([`Nav::stage_fullscreen`]) neither sidebar nor header is drawn.
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
//! room::show_stage(ui: &mut Ui, cx: &mut Cx, st: &mut RoomState)
//! room::show_body(ui: &mut Ui, cx: &mut Cx)
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
//! watches start and stop. Paint a tile with
//! `cx.videos.paint(key, ui, rect, Fit::Contain)` (`Fit::Cover` crops; paint
//! overlays on top afterwards). Never `request_repaint()` for
//! video: the view repaints at the stream's frame rate on its own.

use std::collections::HashMap;

use egui::{Align, Color32, CornerRadius, Frame, Id, Layout, Margin, Rect, Sense, Stroke, Ui, UiBuilder, pos2, vec2};
use simple_screensharing_client::audio::{AudioChoice, AudioLevelTapHandle};
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

mod apply;
pub mod audio_settings;
pub mod drive_dump;
pub mod menu;
pub mod modals;
pub mod popout;
pub mod room;
pub mod share;
pub mod sidebar;
pub mod stage;
pub mod stream_settings;
#[cfg(test)]
mod tests;
mod video;
pub mod webcam_settings;

pub use stage::{StageLayout, StageSettings, StageView};
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
/// How long an error toast (icon "error") stays up, seconds. `Why` longer:
/// a failed connect or share start is the only word the user gets about it
/// (stage-6 QC), and its text names a room and a reason worth reading.
const ERROR_TOAST_SECS: f64 = 6.0;

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
    /// The audio settings menu of the control bar's audio button (task I7): source, Boost and the
    /// exclusion list, for the audio share in `room`. Rendered by [`audio_settings::show`].
    AudioSettings { room: RoomHandle },
    /// The webcam menu of the control bar's webcam button (task I8): camera, mode, codec and bitrate, for the
    /// webcam in `room`. Rendered by [`webcam_settings::show`].
    WebcamSettings { room: RoomHandle },
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
    /// "Choose your name": opened by [`App::prompt_for_name`] while the user
    /// never saved a display name. Save pushes [`UiAction::SetDisplayName`];
    /// Later / Escape / × just close it.
    ChooseName,
}

/// Which popover is open. Drawn by the screen that owns its anchor button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PopoverKind {
    /// User band "who" click: "You're sharing" list + Stop all sharing. (sidebar.rs)
    Outgoing,
    /// A control-bar dropdown: Share, Audio, Layout or Room settings. (stage/bar.rs, drawn by stage/menus.rs)
    Bar(stage::BarMenu),
    /// A stream's menu (its `⋯` button or a right click on the tile). (stage/menus.rs)
    Tile(stage::ItemId),
    /// Stream settings "Also send to…". (stream_settings.rs)
    AlsoSendTo { capture: CaptureSourceId, room: RoomHandle },
    /// Stream settings "Move to…". (stream_settings.rs)
    MoveTo { capture: CaptureSourceId, room: RoomHandle },
    /// The audio settings menu's "Add" under the exclusion list: detected sources and a text field.
    /// (audio_settings.rs)
    AudioExclude,
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
    /// Double-click fullscreen: the one watched stream filling the whole
    /// window, with every other region hidden (`draw` shows only it).
    pub fullscreen: Option<FullscreenStream>,
    /// The sidebar is hidden (the control bar's button).
    pub sidebar_hidden: bool,
    /// The stage fills the whole window (the control bar's fullscreen button): no sidebar, no header, the
    /// strip and bar lie over the stage and auto-hide. `Some(was_fullscreen)`: whether the window was
    /// already fullscreen, which leaving restores.
    pub stage_fullscreen: Option<bool>,
}

/// The stream shown fullscreen in the main window ([`Nav::fullscreen`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FullscreenStream {
    pub key: StreamKey,
    /// Whether the window was already fullscreen when the stream took over;
    /// leaving restores that (not unconditionally windowed).
    pub was_fullscreen: bool,
}

// ---------------------------------------------------------------------------
// Cross-screen actions
// ---------------------------------------------------------------------------

/// Encoder settings for the *next* capture (what a staged stream goes live
/// with): `AppState`'s process-wide `framerate`/`bitrate_kbps`/`tune`/
/// `quality_level`/`gop_millis`/`codec` fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NextStream {
    pub framerate: u32,
    pub bitrate_kbps: u32,
    pub tune: simple_screensharing_client::capture::TuneMode,
    pub quality_level: simple_screensharing_client::capture::QualityLevel,
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
    /// The own tile of a video share was painted with the window focused
    /// (every pass it is): keeps the live preview running ([`LIVE_PREVIEW_GRACE`]).
    LivePreviewSeen(simple_screensharing_client::session::CaptureSourceId),
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
    /// Double-click on a tile: the window goes fullscreen showing only this
    /// stream ([`Nav::fullscreen`]).
    EnterStreamFullscreen(StreamKey),
    /// Double-click / Escape in stream fullscreen: back to the previous layout
    /// and window state.
    ExitStreamFullscreen,
    /// Return a popped-out stream to its tile ("Bring back", window closed).
    BringBack(StreamKey),
    /// Toggle a standalone listen on an audio-only sender.
    ToggleListen(StreamKey),
    /// Viewer volume 0-500 % + mute for a stream (video or audio stream id).
    SetVolume { stream: StreamKey, volume_percent: u32, muted: bool },

    // --- sharing ----------------------------------------------------------
    /// "Share screen": stage a capture for `room` (opens the OS picker).
    /// The stream-settings modal opens by itself once the picker resolves.
    StageShare(RoomHandle),
    /// Go live with a staged capture in `room` (saves the settings as
    /// defaults, closes its settings modal).
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

    // --- webcam (task I8) ---------------------------------------------------
    /// Start the webcam in `room` (or move it there from another room). Its own share, no screen needed.
    StartWebcam(RoomHandle),
    /// Stop the webcam.
    StopWebcam,
    /// The webcam menu's choices: saved, and a running webcam follows them (a new camera or mode restarts it).
    SetWebcam(simple_screensharing_client::rooms::WebcamSettings),
    /// Re-list the cameras (async; result in `cx.state.webcams`).
    RefreshWebcams,

    // --- shared audio -----------------------------------------------------
    /// Share (or switch to) `choice` in `room`: desktop audio or one app. Audio is a share of its own,
    /// independent of any screen share (task I7); switching replaces the running one and keeps it muted
    /// if it was (#118).
    ShareAudio { room: RoomHandle, choice: AudioChoice },
    StopSharingAudio { room: RoomHandle },
    SetSharedAudioMuted { room: RoomHandle, muted: bool },
    /// Audio settings > Exclude: the new list (blanks and duplicates are dropped; saved, applied to the
    /// source lists and to a running desktop share at its next poll).
    SetAudioExclusions(Vec<String>),
    /// Re-list audio sources (async; result in `cx.state.audio_sources`).
    RefreshAudioSources,
    /// Start level meters for these audio source ids (a picker opened).
    /// Read them with [`Cx::source_level`]. Replaces any running taps.
    StartLevelTaps(Vec<u32>),
    /// Stop every source meter (the picker closed).
    StopLevelTaps,

    // --- settings ---------------------------------------------------------
    SetDisplayName(String),
    /// Settings > Profile: my emoji avatar ("" = none, show the initial); announced to every room.
    SetEmoji(String),
    SetLanguage(String),
    SetPresentationMode(PresentationMode),
    SetJitterBuffer(u32),
    SetCatchUpSpeed(u32),
    /// Settings > Diagnostics: the "Send diagnostics to the relay" checkbox (task #110).
    SetSendDiagnostics(bool),
    /// Settings > Audio boost: the sender-side multiplier (slider; persisted, pushed to running
    /// audio shares). Task #114.
    SetAudioGain(f64),
    /// The Boost slider mid-drag (#135): applied to the running shares, not saved; the release sends `SetAudioGain`.
    PreviewAudioGain(f64),
    /// The control bar's Level loudness button: viewer-side loudness levelling on/off (task #119).
    SetLevelLoudness(bool),
    /// Settings > Sounds > Volume released (or the modal closed): persist the notification-sound volume (task I9).
    SetSoundVolume(u32),
    /// The Volume slider mid-drag: applied live, not saved; the release sends `SetSoundVolume`.
    PreviewSoundVolume(u32),
    /// Settings > Sounds > Test: play one notification sound now (a no-op at volume 0).
    TestSound(simple_screensharing_client::sounds::Sound),
    /// Settings > Diagnostics: copy the in-memory log (`logbuf`) to the clipboard + toast.
    CopyLogs,
    /// Settings > Diagnostics: write the log to a file in the Downloads folder, toast and
    /// remember the path in `Shell::saved_logs`.
    SaveLogs,

    // --- stage --------------------------------------------------------------
    /// The Layout menu: show `view`'s streams as Split, Crop or Floating (per view, see [`StageSettings`]).
    SetStageLayout { view: StageView, layout: StageLayout },
    /// "Hide my own streams": your own share gets no tile on any stage.
    SetHideOwnStreams(bool),
    /// "Always show names": watched streams show their name tag without hovering.
    SetAlwaysShowNames(bool),
    /// Layout menu "Show stats": numbers on every watched stream's name tag.
    SetShowStats(bool),
    /// The Layout menu's "Webcam in picture" for `view`: a webcam sits inside its owner's screen share.
    SetStagePip { view: StageView, on: bool },
    /// The bar's hide / show sidebar button.
    ToggleSidebar,
    /// The bar's fullscreen button in the main window ([`Nav::stage_fullscreen`]).
    EnterStageFullscreen,
    ExitStageFullscreen,

    // --- misc -------------------------------------------------------------
}

// ---------------------------------------------------------------------------
// Shell state shared by screens (read-only to them)
// ---------------------------------------------------------------------------

/// A capture staged for one room (picker open, or picked but not live).
#[derive(Debug, Clone)]
pub struct StagedShare {
    pub capture: CaptureSourceId,
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
    /// The stage's layout choices (set by the Layout menu through `UiAction`s).
    pub stage: StageSettings,
    /// The "Choose your name" modal was already offered this run (Later /
    /// Escape / × keeps it away until the next start).
    pub name_prompt_shown: bool,
    /// Staged captures by room. `Why` here and not in the lib: a staged
    /// capture has no destination, so nothing in `AppState` says which room
    /// it is for.
    pub staged: indexmap::IndexMap<RoomHandle, StagedShare>,
    pub toasts: Vec<Toast>,
    /// Rooms whose "Live in <room>" toast was already shown for the current
    /// on-air spell ([`newly_on_air`]).
    announced_live: Vec<RoomHandle>,
    /// Result of the last [`UiAction::CreateRoom`]; the create/join modal
    /// shows the minted link from here.
    pub create_result: Option<Result<JoinLink, String>>,
    /// Last failure of a [`UiAction::JoinRoom`].
    pub join_error: Option<String>,
    /// Where the last [`UiAction::SaveLogs`] wrote the log (shown in Settings).
    pub saved_logs: Option<String>,
    /// Why the last Connect of a room (saved or auto-connect) failed, by
    /// join string; shown under "Not connected" in that room's view until
    /// the next attempt starts or it joins. See [`Shell::connect_error`].
    pub connect_errors: Vec<(RoomKey, String)>,
    /// The join string a `JoinRoom` is waiting on (select it on success).
    pub awaiting_join: Option<RoomKey>,
    /// Running source meters: audio source id → level tap id.
    pub level_taps: HashMap<u32, u32>,
    /// Level taps being built (async), tagged with a generation so a stale
    /// build after `StopLevelTaps` is discarded.
    pending_taps: Vec<PendingTap>,
    tap_generation: u64,
    /// The sharer's own live preview per capture (task #78): when its tile
    /// was last painted and the texture of the newest frame. See
    /// [`App::poll_live_previews`].
    pub live_previews: HashMap<simple_screensharing_client::session::CaptureSourceId, LiveTile>,
}

/// One capture's own-tile live preview state ([`Shell::live_previews`]).
#[derive(Default)]
pub struct LiveTile {
    /// `ctx.input(|i| i.time)` of the last pass that painted the tile with
    /// the window focused ([`UiAction::LivePreviewSeen`]).
    pub seen: f64,
    /// The newest frame; `None` until the first one arrives, and dropped when
    /// the preview stops so a frozen picture never passes for live.
    pub tex: Option<egui::TextureHandle>,
}

/// How long after the own tile was last painted the live preview keeps
/// running. A hidden tile (other room, other view) stops it within this.
const LIVE_PREVIEW_GRACE: f64 = 0.6;
/// Repaint interval while a live preview runs (~30 fps, the branch's rate).
const LIVE_PREVIEW_REPAINT: std::time::Duration = std::time::Duration::from_millis(33);

impl Shell {
    /// The last connect failure of `key`'s room ([`Shell::connect_errors`]).
    pub fn connect_error(&self, key: &str) -> Option<&str> {
        self.connect_errors.iter().find(|(k, _)| same_room(k, key)).map(|(_, e)| e.as_str())
    }

    /// Forget `key`'s connect failure (a new attempt started, or it joined).
    pub fn clear_connect_error(&mut self, key: &str) {
        self.connect_errors.retain(|(k, _)| !same_room(k, key));
    }
}

/// The window icon (the docs/ favicon rasterised once to 256 px,
/// `assets/icon.png`). `Why:` on Windows (and X11) eframe's default "e" showed
/// in the title bar and taskbar (user report 2026-10-15); Wayland takes the
/// icon from `app_id` + the .desktop file instead, so this is harmless there.
pub fn app_icon() -> Option<std::sync::Arc<egui::IconData>> {
    // Cached: the pop-out builder asks every frame.
    static ICON: std::sync::OnceLock<Option<std::sync::Arc<egui::IconData>>> = std::sync::OnceLock::new();
    ICON.get_or_init(|| eframe::icon_data::from_png_bytes(include_bytes!("../../assets/icon.png")).ok().map(Into::into)).clone()
}

/// Rooms in `on_air` that were not already announced; `announced` becomes
/// exactly `on_air` (a room that left air is announced again next time).
/// `Why:` the "Live in <room>" toast used to fire when Go live was pressed, so
/// a share that then failed to start (task #73: no `capssetter` in the
/// Windows package) showed "Live in Test" above the error.
fn newly_on_air(announced: &mut Vec<RoomHandle>, on_air: &[RoomHandle]) -> Vec<RoomHandle> {
    let fresh = on_air.iter().copied().filter(|h| !announced.contains(h)).collect();
    *announced = on_air.to_vec();
    fresh
}

/// The first non-empty line of an error, at most 90 characters: what a toast
/// or a status line has room for.
pub fn short_error(e: &str) -> String {
    let line = e.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    if line.chars().count() > 90 { line.chars().take(89).chain(['…']).collect() } else { line.to_string() }
}

// ---------------------------------------------------------------------------
// Screen context + selectors
// ---------------------------------------------------------------------------

/// A room's connection state as the UI shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnState {
    Joined,
    /// A first-time connect to this room is in flight (no reconnect entry).
    /// `Why` apart from [`Self::Reconnecting`]: "reconnecting" means a
    /// suspended room (its connection dropped; `meta/TERMINOLOGY.md`), and the
    /// first Connect of a saved room read as "Reconnecting" (stage-6 QC).
    Connecting,
    /// A suspended room: its connection dropped and the lib is retrying
    /// (`AppState::reconnect_info` has an entry).
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
    /// The room is connecting or reconnecting: nothing to share into.
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

    /// The emoji avatar of the connection `conn` in `room` ("" for none or an unknown member). Your own entry
    /// comes from your setting (`RoomSession::emoji_of`), so a change shows at once. `Why` not by name: a person is a
    /// connection, and the first member of a name may be someone else, even you (the roster's order is the relay's).
    pub fn emoji_of(&self, room: RoomHandle, conn: ConnId) -> String {
        self.session(room).map(|s| s.emoji_of(conn)).unwrap_or_default()
    }

    /// Display name of `owner` in `room` ("?" if unknown).
    pub fn member_name(&self, room: RoomHandle, owner: ConnId) -> String {
        self.session(room)
            .and_then(|s| s.members.iter().find(|m| m.conn_id == owner))
            .map(|m| m.display_name.clone())
            .unwrap_or_else(|| "?".to_string())
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
        if matches!(room.conn, ConnState::Connecting | ConnState::Reconnecting) {
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
        for c in self.state.captures.values().filter(|c| c.kind == StreamKind::Video && !c.is_webcam()) {
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
        for c in self.state.captures.values().filter(|c| c.kind == StreamKind::Video && !c.is_webcam()) {
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

    /// Our screen captures that have at least one destination (a webcam is not one: [`Self::webcam_share`]).
    pub fn live_captures(&self) -> Vec<(CaptureSourceId, &'a LocalCapture)> {
        self.state
            .captures
            .iter()
            .filter(|(_, c)| c.kind == StreamKind::Video && !c.is_webcam() && !c.destinations.is_empty())
            .map(|(id, c)| (*id, c))
            .collect()
    }

    /// Our webcam share in `room`, if there is one (task I8): its capture, destination and the capture. A
    /// stopping one is gone already ([`AppState::webcam_share`]).
    pub fn webcam_share(&self, room: RoomHandle) -> Option<(CaptureSourceId, DestinationId, &'a LocalCapture)> {
        let (id, r) = self.state.webcam_share()?;
        let c = self.state.captures.get(&id)?;
        let dest = c.destinations.iter().find(|(_, d)| d.room == room).map(|(d, _)| *d);
        (r == room).then_some(dest).flatten().map(|d| (id, d, c))
    }

    /// A capture by id.
    pub fn capture(&self, id: CaptureSourceId) -> Option<&'a LocalCapture> {
        self.state.captures.get(&id)
    }

    /// Our own audio share in `room` (desktop or one app), if any: independent of the screen share
    /// (task I7). A stopping one loses to a running one ([`AppState::audio_share_in`]).
    pub fn audio_share(&self, room: RoomHandle) -> Option<(CaptureSourceId, &'a LocalCapture)> {
        self.state.audio_share_in(room).and_then(|id| self.state.captures.get(&id).map(|c| (id, c)))
    }

    /// What our audio share in `room` captures, if there is one.
    pub fn audio_choice(&self, room: RoomHandle) -> Option<AudioChoice> {
        self.state.audio_choice_in(room)
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
        None if state.reconnect_info(js).is_some() => ConnState::Reconnecting,
        None if state.is_reconnecting(js) => ConnState::Connecting,
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
            conn: conn_of(&p.join_string, None),
            auto_connect: false,
        });
    }
    out
}

/// [`Cx::share_state`] for a joined room.
pub fn share_state(state: &AppState, shell: &Shell, room: RoomHandle) -> ShareState {
    for (id, c) in &state.captures {
        // A webcam is its own share with its own button (task I8): never the Share screen button's state.
        if c.kind != StreamKind::Video || c.is_webcam() {
            continue;
        }
        if let Some((dest_id, _)) = c.destinations.iter().find(|(_, d)| d.room == room) {
            return ShareState::Live { capture: *id, destination: *dest_id };
        }
    }
    // A capture being stopped is not a staged share any more (#98): its
    // destinations were released at the click, so the room is free at once.
    if let Some(staged) = shell.staged.get(&room)
        && let Some(c) = state.captures.get(&staged.capture)
        && !matches!(c.status, ShareStatus::Stopping(_))
    {
        return match c.status {
            ShareStatus::Starting(_) => ShareState::Picking { capture: staged.capture },
            _ => ShareState::Staged { capture: staged.capture },
        };
    }
    ShareState::Idle
}



// ---------------------------------------------------------------------------
// Strings
// ---------------------------------------------------------------------------

/// Snap distance of the volume slider's 100 % detent, percent. The slider is
/// 110 px wide for 0-500 % (~5.2 %/px), so +-10 % is a ~4 px wide target.
const VOLUME_DETENT_SNAP: f64 = 10.0;

/// One mouse-wheel step of the viewer volume: 5 % up to 200 %, 10 % above it, always
/// landing on a multiple of the step and clamped to 0-500 %. The step is the one of
/// the segment being *entered*: 195 -> 200 -> 210 up, 210 -> 200 -> 195 down.
/// Passes through the 100 % detent like any other value (95 -> 100 -> 105).
pub fn wheel_step(percent: u32, up: bool) -> u32 {
    let max = simple_screensharing_client::session::MAX_VOLUME_PERCENT;
    if up {
        let step = if percent >= 200 { 10 } else { 5 };
        ((percent / step + 1) * step).min(max)
    } else {
        let step = if percent > 200 { 10 } else { 5 };
        (percent.saturating_sub(1) / step) * step
    }
}

/// The viewer volume slider shared by the tile pill and the pop-out window:
/// 0-500 % in steps of 5, with a tick and a snap at 100 % (the default), and the
/// mouse wheel over it steps the value ([`wheel_step`]; the wheel does not also
/// scroll the panel underneath). Task #113, user: "make sure to make the volume
/// slider scrollable (5% steps, above 200% it should be 10%."
pub fn volume_slider(ui: &mut Ui, percent: &mut u32) -> egui::Response {
    let at = f64::from(simple_screensharing_client::session::DEFAULT_VOLUME_PERCENT);
    let max = simple_screensharing_client::session::MAX_VOLUME_PERCENT;
    let mut resp = widgets::slider_detent(ui, percent, 0..=max, 5.0, Some((at, VOLUME_DETENT_SNAP)));
    let notches = widgets::wheel_notches(ui, &resp);
    let before = *percent;
    for _ in 0..notches.unsigned_abs() {
        *percent = wheel_step(*percent, notches > 0);
    }
    if *percent != before {
        resp.mark_changed();
    }
    resp
}

/// The one place a person's avatar is drawn (task I3): the name's gradient and its initial, in `rect`, with
/// the green presence dot when `presence`. The strip, the hover card, tiles, menus and the user band all call
/// this. `emoji` ("" = none; look it up with [`Cx::emoji_of`]) replaces the initial and sets the
/// background from its own colour; otherwise the colours are a pure function of the display name
/// (`theme::avatar_colors`, `APP-SHELL.md` "Avatar colour").
pub fn paint_person(ui: &Ui, rect: Rect, name: &str, emoji: &str, presence: bool) {
    let initial: String = name.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_default();
    widgets::paint_avatar_emoji(ui, rect, emoji, &initial, theme::avatar_colors(name), presence);
}

/// The "Level loudness" toggle that sits right after every viewer volume slider (the tile
/// pill, the pop-out / fullscreen pill): a 25 px round button painting `rect`. On = accent
/// ring and tinted disc, light-blue `graphic_eq`; off = a faint icon. Returns true when it
/// was clicked; the caller sends [`UiAction::SetLevelLoudness`]`(!on)`. It is the one global
/// `level_loudness` setting, the same switch as the control bar button, not per-stream state.
/// Task #126, user: "next to the volume slider, have a little button that indicates the
/// dynamic volume setting".
pub fn level_toggle(ui: &Ui, rect: Rect, id: Id, on: bool) -> bool {
    let resp = ui.interact(rect, id, Sense::click());
    let p = ui.painter();
    let c = rect.center();
    let col = if on {
        p.circle(c, 12.0, theme::ACCENT_CHIP_BG, Stroke::new(1.0_f32, theme::ACCENT_CHIP_BORDER));
        theme::ACCENT_LIGHTER
    } else {
        if resp.hovered() {
            p.circle_filled(c, 12.5, Color32::from_white_alpha(20));
        }
        theme::TEXT_FAINT
    };
    widgets::paint_icon(ui, rect, "graphic_eq", 16.0, col);
    let tip = if on {
        tr("ui.room.level_on", "Level loudness: on")
    } else {
        tr("ui.room.level_off", "Level loudness: off")
    };
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tip).clicked()
}

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

/// A share's label as the viewer should read it, in the current language.
/// The lib stores our own capture's label (`LocalCapture::label`) once, in
/// the language active when it was created (`share.video_label`), and peers
/// announce the fixed wire default "Screen". `Why` translate at draw time:
/// after a language switch "Screen share" would otherwise stay English on the
/// share card, the action bar, the stream settings and the Outgoing popover.
/// A label that is a known default in *any* locale (or empty) is shown as the
/// current locale's text; anything else (an audio source's name, a label a
/// peer chose) as is.
pub fn share_label(raw: &str) -> String {
    let default_in_any_locale = |key: &str| rust_i18n::available_locales!().into_iter().any(|l| t!(key, locale = l).as_ref() == raw);
    if raw.is_empty() || default_in_any_locale("share.video_label") {
        tr("share.video_label", "Screen share")
    } else if raw == "Screen" || default_in_any_locale("ui.stream.screen") {
        tr("ui.stream.screen", "Screen")
    } else if simple_screensharing_client::capture::webcam::is_webcam_label(raw) {
        // The webcam's wire label (task I8), `Webcam` in every language.
        tr("ui.stream.webcam", "Webcam")
    } else if default_in_any_locale("share.desktop_audio_label") {
        // A desktop-audio share announces the English label on the wire (task I7).
        tr("share.desktop_audio_label", "Desktop audio")
    } else {
        raw.to_string()
    }
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
    let frame = menu::frame();
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
            body(ui);
            drive_dump::rect(|| "menu".into(), ui.min_rect());
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
    pub stream_settings: stream_settings::StreamSettingsState,
    pub audio_settings: audio_settings::AudioSettingsState,
    pub webcam_settings: webcam_settings::WebcamSettingsState,
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
            Some(rs) => {
                let info = rs.adapter.get_info();
                simple_screensharing_client::logbuf::fact(
                    "gpu",
                    format!("{} ({:?}, {:?}, driver {} {})", info.name, info.device_type, info.backend, info.driver, info.driver_info),
                );
                simple_screensharing_client::decode::render::VideoRenderer::register(rs)
            }
            None => tracing::error!("no wgpu render state: video tiles will stay black"),
        }
        // SIGTERM/SIGINT/SIGHUP close the window, so `on_exit` leaves every
        // room cleanly and peers drop us at once instead of on a timeout.
        simple_screensharing_client::session::install_shutdown_signal_handlers(cc.egui_ctx.clone());
        // The one place notification sounds may reach a sound card: tests build the app through `with_state`.
        simple_screensharing_client::sounds::enable_output();
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
            nav: Nav { view, ..Default::default() },
            shell: Shell { stage: StageSettings::from_saved(simple_screensharing_client::rooms::AppSettings::load().stage), ..Shell::default() },
            videos: VideoViews::default(),
            sidebar: Default::default(),
            room: Default::default(),
            stream_settings: Default::default(),
            audio_settings: Default::default(),
            webcam_settings: Default::default(),
            modals: Default::default(),
            popout: Default::default(),
            now: 0.0,
        }
    }

    /// Let the stage settings learn each joined room's saved key (and seed a new room from the file, #131).
    fn sync_stage_keys(&mut self) {
        self.shell.stage.sync_rooms(self.state.rooms.iter().map(|(h, r)| (*h, r.join_string.as_str())));
    }

    /// Per-frame session polling, revived from the old `ui.rs` `update()`:
    /// auto-connect queue, connect result, events, capture/destination/edit
    /// state, audio sources, bandwidth allocation; then UI bookkeeping
    /// (video views, staged captures, level taps, toasts).
    fn poll(&mut self, ctx: &egui::Context) {
        self.state.poll_auto_connect_queue();
        // `ConnectOutcome::Failed` doesn't name the room: note which one is in
        // flight before polling.
        let connecting = self.state.pending_connect.as_ref().map(|p| p.join_string.clone());
        match self.state.poll_connect_result() {
            ConnectOutcome::None => {}
            ConnectOutcome::Joined(handle) => self.on_joined(handle),
            ConnectOutcome::Failed { error, from_add_card } => {
                if from_add_card {
                    self.shell.join_error = Some(error);
                } else if let Some(key) = connecting {
                    // A saved room's Connect (or an auto-connect) failed: say
                    // so, instead of the row silently going back to "Not
                    // connected" (stage-6 QC).
                    self.now = ctx.input(|i| i.time);
                    let name = room_label(&self.state.room_store, &key);
                    let short = short_error(&error);
                    self.toast(tr_with("ui.toast.connect_room_failed", "Couldn't connect to %{room}: %{error}", &[("room", &name), ("error", &short)]), "error");
                    self.shell.clear_connect_error(&key);
                    self.shell.connect_errors.push((key, short));
                }
                self.shell.awaiting_join = None;
            }
        }
        self.state.drain_events(ctx);
        self.state.poll_capture_states();
        self.state.poll_capture_recovery();
        self.state.poll_pending_destinations();
        self.state.poll_destination_edits();
        self.state.poll_audio_sources();
        self.state.poll_webcams();
        // Rate-limited + hysteresis-filtered internally; cheap most frames.
        self.state.apply_bandwidth_allocation();

        // A failed share start/stop would otherwise vanish without a word
        // (nothing else in the new UI shows `share_error`).
        if let Some(e) = self.state.share_error.take() {
            self.now = ctx.input(|i| i.time);
            // One line: a toast is a single-line pill.
            self.toast(tr_with("share.error", "Share error: %{error}", &[("error", &short_error(&e))]), "error");
        }
        // "Live in <room>" when a destination actually goes on air, not when
        // Go live is pressed (the share can still fail to start).
        let on_air = {
            let mut unused = Vec::new();
            Cx::new(&self.state, &self.nav, &self.shell, &self.videos, &mut unused).on_air_rooms()
        };
        for room in newly_on_air(&mut self.shell.announced_live, &on_air) {
            self.now = ctx.input(|i| i.time);
            let name = self.room_name(room);
            self.toast(tr_with("ui.toast.live_in", "Live in %{room}", &[("room", &name.to_string())]), "cast");
        }
        self.videos.sync(&self.state);
        self.sync_stage_keys();
        self.poll_live_previews(ctx);
        self.poll_staged();
        self.poll_level_taps();
        self.prompt_for_name();
        let now = ctx.input(|i| i.time);
        self.shell.toasts.retain(|t| t.until > now);
        // A modal or view pointing at something that's gone falls back.
        if let Some(Modal::StreamSettings { capture, .. }) = &self.nav.modal
            && !self.state.captures.contains_key(capture)
        {
            self.nav.modal = None;
        }
        if let Some(Modal::LeaveConfirm { room } | Modal::AudioSettings { room } | Modal::WebcamSettings { room }) = &self.nav.modal
            && !self.state.rooms.contains_key(room)
        {
            self.nav.modal = None;
        }
    }

    /// The sharer's own live preview (task #78, user request 2026-10-15: show
    /// a preview while tabbed into the program, otherwise hide it). A video
    /// capture's preview branch is open only while the window is focused and
    /// its own tile was painted a moment ago; otherwise the valve is closed
    /// (no scale, no download, no texture upload, no repaint) and the texture
    /// is dropped. Also uploads the newest frame.
    fn poll_live_previews(&mut self, ctx: &egui::Context) {
        use simple_screensharing_client::session::{ShareHandle, ShareStatus};
        let (focused, minimized) = ctx.input(|i| (i.viewport().focused.unwrap_or(true), i.viewport().minimized.unwrap_or(false)));
        let now = ctx.input(|i| i.time);
        let live = &mut self.shell.live_previews;
        live.retain(|id, _| self.state.captures.contains_key(id));
        let mut running = false;
        for (id, cap) in &self.state.captures {
            let ShareStatus::Active(ShareHandle::Video(h)) = &cap.status else { continue };
            let tile = live.get_mut(id);
            let wanted = focused && !minimized && tile.as_ref().is_some_and(|t| now - t.seen < LIVE_PREVIEW_GRACE);
            h.set_live_preview(wanted);
            let Some(tile) = tile else { continue };
            if !wanted {
                tile.tex = None;
                continue;
            }
            running = true;
            if let Some(f) = h.live_preview_frame() {
                let img = egui::ColorImage::from_rgba_unmultiplied([f.width as usize, f.height as usize], &f.rgba);
                match &mut tile.tex {
                    Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                    None => tile.tex = Some(ctx.load_texture(format!("live-preview-{id:?}"), img, egui::TextureOptions::LINEAR)),
                }
            }
        }
        if running {
            ctx.request_repaint_after(LIVE_PREVIEW_REPAINT);
        }
    }

    /// Offer the "Choose your name" modal once per run while no display name
    /// was ever saved, as soon as no other modal is open. `Why:` user request
    /// 2026-10-15, encourage the user to set their own name instead of
    /// silently using the OS username.
    fn prompt_for_name(&mut self) {
        if self.state.display_name_set || self.shell.name_prompt_shown || self.nav.modal.is_some() {
            return;
        }
        self.shell.name_prompt_shown = true;
        self.nav.modal = Some(Modal::ChooseName);
    }

    fn on_joined(&mut self, handle: RoomHandle) {
        let Some(js) = self.state.rooms.get(&handle).map(|r| r.join_string.clone()) else { return };
        self.shell.clear_connect_error(&js);
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
        let secs = if icon == "error" { ERROR_TOAST_SECS } else { TOAST_SECS };
        self.shell.toasts.push(Toast { text, icon, until: self.now + secs });
    }

    /// Lay the window out and run every screen. Returns the actions they pushed.
    fn draw(&mut self, ctx: &egui::Context) -> Vec<UiAction> {
        let mut actions = Vec::new();
        let App { state, nav, shell, videos, sidebar, room, stream_settings, audio_settings, webcam_settings, modals, popout, .. } = self;
        let mut cx = Cx::new(state, nav, shell, videos, &mut actions);
        if let Some(fs) = cx.nav.fullscreen {
            if cx.videos.get(fs.key).is_some_and(|e| !e.popped) {
                // Only the stream: no sidebar, bands, modals or toasts.
                // `Why` the same black `Fit::Contain` paint as a pop-out
                // (`popout.rs` `viewport`), so the letterboxing is identical.
                egui::CentralPanel::default().frame(Frame::NONE.fill(Color32::BLACK)).show(ctx, |ui| {
                    let rect = ui.max_rect();
                    let pic = cx.videos.paint(fs.key, ui, rect, simple_screensharing_client::decode::render::Fit::Contain);
                    // Registered before the overlay, whose button then takes its own clicks.
                    let dbl = ui.interact(rect, Id::new("stream_fullscreen"), Sense::click()).double_clicked();
                    // The webcam in picture of this stream, if it has one (the stage's own, same place and size);
                    // after the double click above, so a press on it is the PIP's and does not leave fullscreen.
                    stage::fullscreen_pip(ui, &mut cx, &mut room.stage, fs.key, rect);
                    // `Why` the pop-out's overlay (`popout::fullscreen_overlay`): user request
                    // 2026-11-03, the same overlay as a fullscreened pop-out.
                    let first_frame = pic.is_some_and(|r| r.width() > 0.0);
                    if popout::fullscreen_overlay(ui, &mut cx, fs.key, rect, first_frame) || dbl {
                        cx.act(UiAction::ExitStreamFullscreen);
                    }
                });
                popout::show(ctx, &mut cx, popout);
                return actions;
            }
            // The stream ended or was popped out meanwhile: leave and draw
            // the normal layout this frame.
            cx.act(UiAction::ExitStreamFullscreen);
        }
        // Stage fullscreen draws the stage alone; it ends with the stage (room left, view changed).
        let stage_fs = cx.nav.stage_fullscreen.is_some() && room::shows_stage(&cx);
        if cx.nav.stage_fullscreen.is_some() && !stage_fs {
            cx.act(UiAction::ExitStageFullscreen);
        }

        // Left sidebar: brand band / scroll body / user band, split by hand
        // so each region is exactly its spec size whatever its content does.
        if !stage_fs && !cx.nav.sidebar_hidden {
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
        }

        // Room header band (title, status, meta).
        if !stage_fs {
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
        }

        egui::CentralPanel::default().frame(Frame::NONE.fill(theme::PANEL_BASE)).show(ctx, |ui| {
            let r = ui.max_rect();
            region(ui, r, "room_body", |ui| {
                if room::shows_stage(&cx) {
                    room::show_stage(ui, &mut cx, room);
                    return;
                }
                egui::ScrollArea::vertical().id_salt("room_body_scroll").auto_shrink([false, false]).show(ui, |ui| {
                    Frame::NONE
                        .inner_margin(Margin::same(theme::S4 as i8))
                        .show(ui, |ui| vbox(ui, theme::S4, |ui| room::show_body(ui, &mut cx)));
                });
            });
        });

        // Modals over the panels.
        match cx.nav.modal.clone() {
            Some(Modal::StreamSettings { capture, room }) => stream_settings::show(ctx, &mut cx, stream_settings, capture, room),
            Some(Modal::AudioSettings { room }) => audio_settings::show(ctx, &mut cx, audio_settings, room),
            Some(Modal::WebcamSettings { room }) => webcam_settings::show(ctx, &mut cx, webcam_settings, room),
            Some(m) => modals::show(ctx, &mut cx, modals, &m),
            None => {}
        }
        // Pop-out windows: re-declared every frame they should exist.
        popout::show(ctx, &mut cx, popout);
        stage::show_windows(ctx, &mut cx, &mut room.stage);

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
        let popover_was_open = self.nav.popover.is_some();
        if self.nav.popover.is_some() && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            self.nav.popover = None;
        }

        // An open dropdown takes Escape the same way: it closes alone, the modal it is in stays.
        if menu::any_select_open(ctx) && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            egui::Popup::close_all(ctx);
        }

        // Escape leaves stream fullscreen (no popover or modal is open then).
        if self.nav.fullscreen.is_some() && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.apply(ctx, UiAction::ExitStreamFullscreen);
        }

        // Escape leaves stage fullscreen (a menu or modal takes the key first: a frame that started with a menu
        // open only closes it, whatever the key events say).
        if self.nav.stage_fullscreen.is_some()
            && !popover_was_open
            && self.nav.modal.is_none()
            && self.nav.fullscreen.is_none()
            && ctx.input(|i| i.key_pressed(egui::Key::Escape))
        {
            self.apply(ctx, UiAction::ExitStageFullscreen);
        }

        drive_dump::begin();
        let actions = self.draw(ctx);
        drive_dump::line(|| format!("popover {}\nmodal {}\nstage_fs {}\nstream_fs {}", u8::from(self.nav.popover.is_some()), u8::from(self.nav.modal.is_some()), u8::from(self.nav.stage_fullscreen.is_some()), u8::from(self.nav.fullscreen.is_some())));
        drive_dump::flush(ctx);
        for action in actions {
            drive_dump::action(&action);
            self.apply(ctx, action);
        }

        // Keep polling the connect future / event channels with no input;
        // coarser while unfocused. `None` (backend can't tell) = focused.
        let focused = ctx.input(|i| i.viewport().focused).unwrap_or(true);
        if matches!(self.state.status, Status::Connecting)
            || !self.state.rooms.is_empty()
            || !self.shell.toasts.is_empty()
            || !self.state.recoveries.is_empty()
        {
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
