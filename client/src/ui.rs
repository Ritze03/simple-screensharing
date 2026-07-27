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
//! # Session state lives in `session.rs`
//!
//! Every logic-carrying type this module renders (`RoomHandle`,
//! `CaptureSourceId`, `DestinationId`, `Destination`, `LocalCapture`,
//! `WatchEntry`, `AudioWatch`, `PopoutState`, `Status`, `RoomSession`,
//! `PendingConnect`, `PendingRole`, `DestEdit`, `BandwidthController`, and the
//! whole [`session::AppState`] data model — rooms, captures, destinations,
//! fan-out/transfer, and Absolute-Maximum bandwidth allocation) lives in
//! `session.rs` — see that module's docs for the full "Session data model"
//! writeup, including why captures (not rooms) own destinations, and why
//! fan-out/transfer are make-before-break.
//!
//! [`App`] below is a thin egui-facing wrapper around [`session::AppState`]
//! plus the handful of fields that are genuinely just view state and never
//! touched by anything in `session.rs`: [`NavTab`] selection, which rooms are
//! expanded, the "+ Add Room" card's own fields, the leave-confirm dialog,
//! and whether the theme has been installed on this `egui::Context` yet.
//!
//! Business-logic methods on `AppState`/`RoomSession` are called as
//! `self.state.method(...)` / `room.method(...)` from the rendering code
//! below. A few (`start_connect`, `disconnect_room`, `request_leave_room`,
//! `poll_connect_result`) get a thin same-named wrapper here instead of a
//! bare pass-through, because their `AppState` counterpart used to also touch
//! a UI-only field (the "+ Add Room" card's state, `expanded_rooms`,
//! `leave_confirm`) — see [`session::ConnectOutcome`] for how a connect
//! result gets back to this layer without that UI-only state needing to live
//! on `AppState`.
//!
//! The three rendering-only methods that still need `egui::Context`/textures/
//! viewports (`RoomSession::update_watch_textures`, `RoomSession::pop_out`,
//! `RoomSession::show_popouts`) get a second, egui-dependent `impl RoomSession`
//! block below, alongside `App::update_self_preview_textures` — everything
//! else on those types is in `session.rs`.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::capture::{self, DestKey};
use crate::protocol::{ConnId, StreamId, StreamInfo, StreamKind};
use crate::rooms::{BandwidthMode, PresentationMode};
use crate::session::{
    self, room_label, AppState, CaptureSourceId, ConnectOutcome, Destination, DestinationId, LocalCapture, PendingRole,
    PopoutState, RoomHandle, RoomSession, ShareHandle, ShareStatus, Status, MAX_BITRATE_KBPS,
};
use crate::theme;

/// Slider bounds, matching the project's 40 Mbit/s ceiling. The ceiling
/// (`session::MAX_BITRATE_KBPS`) is imported from `session.rs` since
/// `AppState::allocation_input` needs the same number as the per-destination
/// bandwidth ceiling; the floor is purely a UI slider bound.
const MIN_BITRATE_KBPS: u32 = 1000;

/// Idle-fallback repaint cadence while connecting/connected but nothing
/// needs a faster one (see `eframe::App::update`'s use of this constant).
const IDLE_REPAINT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);
/// Repaint cadence used instead of `IDLE_REPAINT_INTERVAL` — and instead of
/// a watched stream's full-rate `ctx.request_repaint()` — whenever the
/// window has lost OS focus, regardless of the selected `PresentationMode`.
/// See `superdoc/features/presentation-modes.md`'s "Why" note on this.
const UNFOCUSED_REPAINT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// Common refresh rates offered on the framerate dropdown, up to the
/// project's 240Hz desktop-client ceiling (`capture::MAX_FRAMERATE`) — more
/// natural as discrete choices than a continuous slider, since these are the
/// values that actually correspond to real monitor refresh rates.
const FRAMERATE_OPTIONS: [u32; 7] = [30, 60, 90, 120, 144, 165, 240];

/// Per-destination resolution choices offered in Stream Control, as a
/// downscale of whatever the capture source is actually producing.
///
/// `Why` fractions of the source rather than absolute presets like "1080p":
/// the source's own size is whatever monitor the user picked, and
/// `capture::destination::clamp_to_source` never upscales — so an absolute
/// preset would silently become a no-op on a smaller monitor and would have
/// to invent an aspect ratio on an unusual one. A fraction is always
/// meaningful, always preserves aspect, and always downscales.
const RESOLUTION_SCALES: [(u32, &str); 4] = [(100, "100%"), (75, "75%"), (50, "50%"), (33, "33%")];

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
/// multi-destination fan-out makes STREAMS' length a function of how many
/// rooms each capture feeds, so an uncapped section could take over the
/// whole rail. ROOMS is the sidebar's primary navigation and keeps the
/// greedy middle; STREAMS gets a bounded footer and its own scroll.
const STREAMS_MAX_HEIGHT: f32 = 176.0;
/// Period of the reconnecting dot's opacity pulse, in seconds.
const RECONNECT_PULSE_PERIOD: f64 = 1.2;

// Icon glyphs. Deliberately drawn from Latin-1 / Geometric Shapes / the
// emoji-icon-font subset egui's own widgets rely on (`🗙` is what
// `egui::Window`'s close button uses), because this project has not installed
// the Geist Mono Nerd Font icon fallback its styling guide calls for — see
// STYLING-GUIDE.md's "Fonts" status note. Anything from a richer icon set
// would render as tofu today.
//
// `Why` the carets are U+23F5/U+23F7 (`⏵`/`⏷`, Miscellaneous Technical) rather
// than the Geometric Shapes triangles they obviously "should" be: being in
// Geometric Shapes is *not* sufficient — egui's bundled fonts only cover the
// subset its own widgets draw from. Verified by rendering each candidate in the
// real GUI and screenshotting: U+25B6 `▶` and U+25A0 `■` render, but
// U+25B8 `▸`, U+25BE `▾`, U+25BC, U+25BD, U+25BF, U+25B3 and U+25B4 all come out
// as tofu boxes. U+23F5/U+23F7 render, are a matched pair at caret size, and
// stay visually distinct from `ICON_WATCH`.
const ICON_WATCH: &str = "\u{25b6}"; // ▶
const ICON_STOP: &str = "\u{25a0}"; // ■
const ICON_CARET_COLLAPSED: &str = "\u{23f5}"; // ⏵
const ICON_CARET_EXPANDED: &str = "\u{23f7}"; // ⏷
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

/// The egui-facing app shell: a thin wrapper around [`session::AppState`]
/// (every room/capture/destination, fan-out/transfer, and bandwidth
/// allocation — see that module's docs) plus the handful of fields that are
/// genuinely just view state and never touched by anything in `session.rs`.
pub struct App {
    state: AppState,

    /// Which nav pill is selected — i.e. what `ui_content` renders. Real,
    /// frame-persistent state, deliberately *not* derived from whether
    /// anything is joined/watching: a view selection the app silently
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
    /// `request_leave_room`.
    leave_confirm: Option<RoomHandle>,
    /// Whether `theme::install` has run for this context yet. Once per
    /// process, not once per frame: it rewrites the whole `Visuals` struct.
    theme_installed: bool,
}

impl Default for App {
    fn default() -> Self {
        Self {
            state: AppState::default(),
            nav_tab: NavTab::default(),
            add_room_open: false,
            add_room_join_string: String::new(),
            add_room_error: None,
            expanded_rooms: HashSet::new(),
            leave_confirm: None,
            theme_installed: false,
        }
    }
}

impl App {
    /// Upload any freshly captured self-preview frames (see
    /// `capture::CaptureHandle::self_preview_frame` and
    /// `superdoc/features/self-preview.md`) to each running video capture's
    /// own texture. Call once per repaint alongside
    /// `RoomSession::update_watch_textures`; cheap no-op when nothing new has
    /// arrived.
    ///
    /// Stays here rather than on `AppState` even though it only touches
    /// `self.state.captures`: it needs `egui::Context` to create/update
    /// textures, and `session.rs`'s business-logic methods deliberately don't
    /// grow new egui dependencies — see that module's docs.
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
        for (capture_id, capture) in self.state.captures.iter_mut() {
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
}

/// Room-scoped *rendering* operations. The room-scoped business logic
/// (`toggle_watch`, `toggle_listen`, `start_audio_watch`,
/// `teardown_audio_watch`, `sibling_audio_stream`, `owner_has_video`) lives in
/// `session.rs`'s `impl RoomSession` block — this is a second, egui-dependent
/// `impl RoomSession` block for the parts that genuinely need
/// `egui::Context`/textures/viewports.
impl RoomSession {
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

/// One Stream Control action, recorded while the tab is rendering (and
/// holding `&self` borrows of `captures`/`rooms`) and applied by
/// `apply_stream_action` afterwards — the same deferral `RoomAction` uses for
/// the sidebar, for the same borrow-checker reason.
enum StreamAction {
    StartCapture(RoomHandle),
    StartAudio(RoomHandle),
    RefreshAudio,
    StopDestination(DestinationId),
    /// Move this destination to another room, make-before-break.
    Transfer(DestinationId, RoomHandle),
    /// Send this *capture* into one more room as an additional destination.
    FanOut(CaptureSourceId, RoomHandle),
    /// Push this destination's edited settings through `classify_change`.
    ApplyEdit(DestinationId),
    EditScale(DestinationId, u32),
    EditFramerate(DestinationId, u32),
    EditBitrate(DestinationId, u32),
    RenameRoom(String, String, RoomHandle),
}

/// How many *other* people are streaming into this room right now — the
/// "N Active Users" in a joined room's collapsed row.
///
/// Thin wrapper: the actual logic lives on `RoomSession::active_stream_count`
/// so the Tauri shell's `dto::saved_room_rows` can share it (see that
/// method's docs).
fn active_user_count(room: &RoomSession) -> usize {
    room.active_stream_count()
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

// `room_label`/`room_id_prefix` live in `session.rs` (used by
// `AppState::insert_room`/`poll_connect_result`, not just rendering) —
// imported above as `session::room_label`.

impl App {
    /// Kick off a connect to `join_string`. `from_add_card` routes a failure
    /// back into the inline "+ Add Room" card instead of the ROOMS list's own
    /// error line (see `AppState::start_connect`).
    fn start_connect(&mut self, join_string: String, from_add_card: bool) {
        self.add_room_error = None;
        if let Err(e) = self.state.start_connect(join_string, from_add_card)
            && from_add_card
        {
            self.add_room_error = Some(e);
        }
    }

    /// Whether `join_string` is the room a connect is currently in flight for
    /// — what puts a saved row into the amber "reconnecting" state.
    fn is_reconnecting(&self, join_string: &str) -> bool {
        self.state.is_reconnecting(join_string)
    }

    // `insert_room` has no wrapper here — `poll_connect_result` and tests both
    // call `self.state.insert_room` / `app.state.insert_room` directly (see
    // `session::AppState::insert_room`).

    /// Tear one room down. See `AppState::disconnect_room`; this wrapper also
    /// clears the two UI-only fields (`expanded_rooms`, `leave_confirm`) that
    /// used to be reset inline there.
    fn disconnect_room(&mut self, handle: RoomHandle) {
        self.state.disconnect_room(handle);
        // Purely view state, but ids never rewind, so a stale entry would
        // linger for the process's lifetime rather than ever being reused.
        self.expanded_rooms.remove(&handle);
        if self.leave_confirm == Some(handle) {
            self.leave_confirm = None;
        }
    }

    /// "Leave" clicked on a joined room row: tear it down now, or raise the
    /// confirm dialog first if there's active work in it (see
    /// `AppState::request_leave_room`).
    fn request_leave_room(&mut self, handle: RoomHandle) {
        if !self.state.request_leave_room(handle) {
            self.leave_confirm = Some(handle);
        }
    }

    /// Poll the in-flight connect attempt and react to whatever just resolved
    /// (see `AppState::poll_connect_result`): a success closes the "+ Add
    /// Room" card, a failure from that card reports inline in it.
    fn poll_connect_result(&mut self) {
        match self.state.poll_connect_result() {
            ConnectOutcome::None => {}
            ConnectOutcome::Joined(_) => {
                self.add_room_open = false;
                self.add_room_join_string.clear();
                self.add_room_error = None;
            }
            ConnectOutcome::Failed { error, from_add_card } => {
                if from_add_card {
                    self.add_room_error = Some(error);
                }
            }
        }
    }

    /// Drain every joined room's event channel. See `AppState::drain_events`.
    fn drain_events(&mut self, ctx: &egui::Context) {
        self.state.drain_events(ctx);
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
                if ui.selectable_label(self.state.language == "en", "English").clicked() && self.state.language != "en" {
                    self.state.language = "en".to_string();
                    changed = true;
                }
                if ui.selectable_label(self.state.language == "de", "Deutsch").clicked() && self.state.language != "de" {
                    self.state.language = "de".to_string();
                    changed = true;
                }
                if changed {
                    rust_i18n::set_locale(&self.state.language);
                    session::save_language(&self.state.language);
                }
            });

            theme::setting_row(ui, t!("settings.presentation_mode_label").as_ref(), |ui| {
                let mut changed = false;
                egui::ComboBox::from_id_salt("presentation_mode_combo")
                    .selected_text(self.state.presentation_mode.label())
                    .show_ui(ui, |ui| {
                        for mode in PresentationMode::ALL {
                            if ui.selectable_value(&mut self.state.presentation_mode, mode, mode.label()).changed() {
                                changed = true;
                            }
                        }
                    });
                if changed {
                    session::save_presentation_mode(self.state.presentation_mode);
                }
                // Runtime present-mode swap isn't available with this pinned
                // eframe/egui-wgpu (see superdoc/features/presentation-modes.md):
                // the surface is only ever configured once, at window
                // creation. Surface this as an inline hint rather than
                // silently doing nothing until the next launch.
                if self.state.presentation_mode != self.state.launch_presentation_mode {
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

        if let Some(err) = &self.state.room_store_error {
            ui.colored_label(theme::DANGER, t!("join.load_error", error = err).to_string());
        }
        // A connect failure that didn't come from the Add Room card (i.e. a
        // `[Reconnect]`, or a dropped connection) has no card to live in, so
        // it reports here, above the list it concerns.
        if let Status::Error(e) = &self.state.status
            && !self.add_room_open
        {
            ui.colored_label(theme::DANGER, t!("join.error", error = e).to_string());
        }

        let mut actions: Vec<RoomAction> = Vec::new();
        egui::ScrollArea::vertical()
            .id_salt("rooms_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for saved in &self.state.room_store.rooms {
                    let handle = self
                        .state
                        .rooms
                        .iter()
                        .find(|(_, room)| room.join_string == saved.join_string)
                        .map(|(handle, _)| *handle);
                    self.ui_room_row(ui, &saved.name, &saved.join_string, handle, &mut actions);
                }
                for (handle, room) in &self.state.rooms {
                    if self.state.room_store.name_for(&room.join_string).is_some() {
                        continue;
                    }
                    let name = room_label(&self.state.room_store, &room.join_string);
                    self.ui_room_row(ui, &name, &room.join_string, Some(*handle), &mut actions);
                }
                if self.state.room_store.rooms.is_empty() && self.state.rooms.is_empty() {
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
        let selected = handle.is_some() && self.state.active_room == handle;
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
                                let room = &self.state.rooms[&h];
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
        let Some(room) = self.state.rooms.get(&handle) else { return };
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
                ui.add(egui::TextEdit::singleline(&mut self.state.display_name).desired_width(ui.available_width()));
                ui.label(egui::RichText::new(t!("join.link_label").to_string()).small().color(theme::TEXT_DIM));
                ui.add(egui::TextEdit::singleline(&mut self.add_room_join_string).desired_width(ui.available_width()));

                let connecting = matches!(self.state.status, Status::Connecting);
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

        if self.state.captures.is_empty() {
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
                for (capture_id, capture) in &self.state.captures {
                    for dest in capture.destinations.values() {
                        let room_name = match self.state.rooms.get(&dest.room) {
                            Some(room) => room_label(&self.state.room_store, &room.join_string),
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
            self.state.stop_share(capture_id);
        }
    }

    /// Apply one deferred sidebar action, now that the render borrows are
    /// gone. See `ui_room_row`.
    fn apply_room_action(&mut self, action: RoomAction) {
        match action {
            RoomAction::Select(handle) => self.state.active_room = Some(handle),
            RoomAction::ToggleExpand(handle) => {
                if !self.expanded_rooms.remove(&handle) {
                    self.expanded_rooms.insert(handle);
                }
            }
            RoomAction::Leave(handle) => self.request_leave_room(handle),
            RoomAction::Reconnect(join_string) => self.start_connect(join_string, false),
            RoomAction::Forget(name) => {
                self.state.room_store.remove(&name);
                if let Err(e) = self.state.room_store.save() {
                    self.state.room_store_error = Some(e.to_string());
                }
            }
            RoomAction::Watch(handle, owner, stream_id) => {
                if let Some(room) = self.state.rooms.get_mut(&handle) {
                    room.toggle_watch(owner, stream_id);
                    // Watching from a room's row is also a statement about
                    // which room you're looking at — otherwise the View tab
                    // would keep showing a different room's stream and the
                    // click would look like it did nothing.
                    self.state.active_room = Some(handle);
                    self.nav_tab = NavTab::View;
                }
            }
            RoomAction::Listen(handle, owner, stream_id) => {
                if let Some(room) = self.state.rooms.get_mut(&handle) {
                    room.toggle_listen(owner, stream_id);
                }
            }
        }
    }

    /// Confirm dialog for leaving a room with active work in it. See
    /// `leave_needs_confirm` for when this is raised at all.
    fn ui_leave_confirm(&mut self, ctx: &egui::Context) {
        let Some(handle) = self.leave_confirm else { return };
        let Some(room) = self.state.rooms.get(&handle) else {
            self.leave_confirm = None;
            return;
        };
        let name = room_label(&self.state.room_store, &room.join_string);
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

    /// One deferred Stream Control action, recorded while the tab's borrows
    /// are live and applied afterwards (same pattern as `RoomAction` for the
    /// sidebar).
    fn ui_stream_control_tab(&mut self, ui: &mut egui::Ui) {
        if self.state.rooms.is_empty() {
            Self::ui_empty_state(ui, t!("stream_control.empty").as_ref());
            return;
        }
        // Default the "start a stream" room picker to whichever room is
        // selected, so the common case is one click.
        if self.state.new_stream_room.is_none_or(|handle| !self.state.rooms.contains_key(&handle)) {
            self.state.new_stream_room = self.state.active_room.or_else(|| self.state.rooms.keys().next().copied());
        }

        let mut actions: Vec<StreamAction> = Vec::new();

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.set_max_width(theme::PANEL_MAX_WIDTH);
            ui.spacing_mut().item_spacing.y = 0.0;

            self.ui_bandwidth_card(ui);
            self.ui_start_stream_card(ui, &mut actions);
            self.ui_destinations_card(ui, &mut actions);
            self.ui_room_card(ui, &mut actions);
        });

        for action in actions {
            self.apply_stream_action(action);
        }
    }

    /// The bandwidth card: the mode toggle, each mode's own settings, the
    /// fan-out multiplication note, and the overshoot warning.
    fn ui_bandwidth_card(&mut self, ui: &mut egui::Ui) {
        let mut mode = self.state.bandwidth_mode;
        let mut totals_changed = false;

        theme::card(ui, t!("bandwidth.header").as_ref(), |ui| {
            // Radio-style pair of selectable buttons: `theme.rs` has no
            // radio-group helper, and a two-option exclusive choice is exactly
            // what `selectable_label` already expresses (the same shape the
            // Settings tab's language switcher uses), so this doesn't warrant
            // a new theme component.
            theme::setting_row(ui, t!("bandwidth.mode_label").as_ref(), |ui| {
                if ui
                    .selectable_label(mode == BandwidthMode::Absolute, t!("bandwidth.mode_absolute").to_string())
                    .on_hover_text(t!("bandwidth.mode_absolute_hint").to_string())
                    .clicked()
                {
                    mode = BandwidthMode::Absolute;
                }
                if ui
                    .selectable_label(mode == BandwidthMode::PerStream, t!("bandwidth.mode_per_stream").to_string())
                    .on_hover_text(t!("bandwidth.mode_per_stream_hint").to_string())
                    .clicked()
                {
                    mode = BandwidthMode::PerStream;
                }
            });

            match self.state.bandwidth_mode {
                BandwidthMode::Absolute => {
                    totals_changed |= theme::slider_row(
                        ui,
                        t!("bandwidth.total_label").as_ref(),
                        &mut self.state.absolute_total_kbps,
                        MIN_BITRATE_KBPS..=MAX_BITRATE_KBPS,
                        50.0,
                        " kbps",
                    );
                    totals_changed |= theme::slider_row(
                        ui,
                        t!("bandwidth.floor_label").as_ref(),
                        &mut self.state.destination_floor_kbps,
                        100..=MIN_BITRATE_KBPS * 4,
                        10.0,
                        " kbps",
                    );
                    ui.label(
                        egui::RichText::new(t!("bandwidth.absolute_explainer").to_string())
                            .small()
                            .color(theme::TEXT_FAINT),
                    );
                }
                BandwidthMode::PerStream => {
                    ui.label(
                        egui::RichText::new(t!("bandwidth.per_stream_explainer").to_string())
                            .small()
                            .color(theme::TEXT_FAINT),
                    );
                }
            }

            // Fan-out's bandwidth consequence, stated in the same place the
            // budget is set. Deliberately phrased as a multiplication rather
            // than a total: the point is that a second room is not free.
            let live = self.live_destination_count();
            if live > 1 {
                ui.label(
                    egui::RichText::new(t!("bandwidth.fanout_note", rooms = live, factor = live).to_string())
                        .small()
                        .color(theme::WARNING),
                );
            }

            // Overshoot: the allocator was asked for less than the guaranteed
            // floors add up to. Never silently exceeded and never starved
            // below floor (plan doc, round-1 Q8) — so the only honest thing
            // left is to say so.
            if let Some(allocation) = self.state.bandwidth.allocation()
                && allocation.overshoot_kbps > 0
            {
                ui.add_space(theme::SPACING * 0.5);
                theme::warning_banner(
                    ui,
                    &t!(
                        "bandwidth.overshoot_warning",
                        over = allocation.overshoot_kbps,
                        total = self.state.absolute_total_kbps,
                        assigned = allocation.assigned_total_kbps
                    ),
                );
            }
        });

        if totals_changed {
            session::save_bandwidth_settings(
                self.state.bandwidth_mode,
                self.state.absolute_total_kbps,
                self.state.destination_floor_kbps,
            );
        }
        if mode != self.state.bandwidth_mode {
            self.state.set_bandwidth_mode(mode);
        }
    }

    /// How many destinations are actually encoding right now — the fan-out
    /// bandwidth multiplier.
    fn live_destination_count(&self) -> usize {
        self.state
            .captures
            .values()
            .filter(|c| c.kind == StreamKind::Video && matches!(c.status, ShareStatus::Active(_)))
            .map(|c| c.destinations.values().filter(|d| d.runtime.is_some()).count())
            .sum()
    }

    /// "Start a stream": pick a joined room and the settings the new capture
    /// starts with.
    fn ui_start_stream_card(&mut self, ui: &mut egui::Ui, actions: &mut Vec<StreamAction>) {
        theme::card(ui, t!("stream_control.start_header").as_ref(), |ui| {
            theme::setting_row(ui, t!("stream_control.room_label").as_ref(), |ui| {
                let selected = self
                    .state
                    .new_stream_room
                    .and_then(|handle| self.state.rooms.get(&handle))
                    .map(|room| room_label(&self.state.room_store, &room.join_string))
                    .unwrap_or_else(|| t!("stream_control.pick_room").to_string());
                egui::ComboBox::from_id_salt("start_stream_room").selected_text(selected).show_ui(ui, |ui| {
                    for (handle, room) in &self.state.rooms {
                        let label = room_label(&self.state.room_store, &room.join_string);
                        ui.selectable_value(&mut self.state.new_stream_room, Some(*handle), label);
                    }
                });
            });
            theme::setting_row(ui, t!("share.framerate_label").as_ref(), |ui| {
                egui::ComboBox::from_id_salt("start_stream_framerate")
                    .selected_text(format!("{} fps", self.state.framerate))
                    .show_ui(ui, |ui| {
                        for &fps in &FRAMERATE_OPTIONS {
                            ui.selectable_value(&mut self.state.framerate, fps, format!("{fps} fps"));
                        }
                    });
            });
            // The starting bitrate only means anything in Per-Stream mode; in
            // Absolute mode the allocator overwrites it within a second of the
            // stream going live, so offering the slider would be a lie.
            if self.state.bandwidth_mode == BandwidthMode::PerStream {
                theme::slider_row(
                    ui,
                    t!("share.bitrate_label").as_ref(),
                    &mut self.state.bitrate_kbps,
                    MIN_BITRATE_KBPS..=MAX_BITRATE_KBPS,
                    50.0,
                    " kbps",
                );
            }
            ui.horizontal(|ui| {
                ui.add_enabled_ui(self.state.new_stream_room.is_some(), |ui| {
                    if ui.add(theme::primary_button(t!("share.screen_button").to_string())).clicked()
                        && let Some(room) = self.state.new_stream_room
                    {
                        actions.push(StreamAction::StartCapture(room));
                    }
                });
                let starting_video = self
                    .state
                    .captures
                    .values()
                    .any(|c| c.kind == StreamKind::Video && matches!(c.status, ShareStatus::Starting(_)));
                if starting_video {
                    ui.spinner();
                    ui.label(egui::RichText::new(t!("share.waiting_permission").to_string()).color(theme::TEXT_DIM));
                }
            });
            if let Some(err) = self.state.share_error.as_ref() {
                ui.colored_label(theme::DANGER, t!("share.error", error = err).to_string());
            }
        });
    }

    /// Every outgoing destination this client has, across **every** room —
    /// with its live state, its editable settings, and the transfer/fan-out
    /// actions.
    ///
    /// `Why` this list is cross-room rather than scoped to the selected room:
    /// one capture can be feeding several rooms at once, and a destination the
    /// user is still paying bandwidth for must never be hidden behind a room
    /// selection (the same reasoning as the sidebar's STREAMS section).
    fn ui_destinations_card(&mut self, ui: &mut egui::Ui, actions: &mut Vec<StreamAction>) {
        theme::card(ui, t!("share.active_header").as_ref(), |ui| {
            if self.state.captures.is_empty() {
                ui.label(egui::RichText::new(t!("share.none").to_string()).color(theme::TEXT_FAINT));
                return;
            }
            for (capture_id, capture) in &self.state.captures {
                for (destination_id, dest) in &capture.destinations {
                    self.ui_destination_row(ui, *capture_id, capture, *destination_id, dest, actions);
                    ui.add_space(theme::SPACING);
                }
            }
        });
    }

    /// One destination: what it is, how it's doing, and everything that can be
    /// done to it.
    #[allow(clippy::too_many_arguments)]
    fn ui_destination_row(
        &self,
        ui: &mut egui::Ui,
        capture_id: CaptureSourceId,
        capture: &LocalCapture,
        destination_id: DestinationId,
        dest: &Destination,
        actions: &mut Vec<StreamAction>,
    ) {
        let room_name = match self.state.rooms.get(&dest.room) {
            Some(room) => room_label(&self.state.room_store, &room.join_string),
            None => t!("streams_section.unknown_room").to_string(),
        };
        let video = capture.kind == StreamKind::Video;
        let active = matches!(capture.status, ShareStatus::Active(_));

        ui.horizontal(|ui| {
            let (dot, label) = self.destination_status(capture, dest);
            theme::status_dot(ui, dot, true);
            ui.label(
                egui::RichText::new(t!("share.destination_line", room = room_name.clone(), state = label).to_string())
                    .color(theme::TEXT),
            );
            if video && active {
                ui.label(
                    egui::RichText::new(destination_geometry(dest)).small().color(theme::TEXT_DIM),
                );
                if let ShareStatus::Active(ShareHandle::Video(handle)) = &capture.status {
                    ui.label(
                        egui::RichText::new(
                            t!("share.fps_label", fps = handle.encoder_fps(DestKey(destination_id.0))).to_string(),
                        )
                        .small()
                        .color(theme::TEXT_DIM),
                    );
                }
            } else if !video {
                ui.label(egui::RichText::new(capture.label.clone()).small().color(theme::TEXT_DIM));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_enabled_ui(active, |ui| {
                    if ui.add(theme::danger_button(t!("share.stop_button").to_string())).clicked() {
                        actions.push(StreamAction::StopDestination(destination_id));
                    }
                });
            });
        });

        // The live bitrate this destination's encoder is actually running at.
        // In Absolute mode that is the allocator's most recent write, which is
        // the number worth showing — the manual slider below is not in force.
        if video && active {
            let live_kbps = match self.state.bandwidth_mode {
                BandwidthMode::Absolute => self.state.bandwidth.applied_kbps(destination_id),
                BandwidthMode::PerStream => Some(dest.live_params().bitrate_kbps),
            };
            if let Some(kbps) = live_kbps {
                ui.label(
                    egui::RichText::new(t!("share.live_bitrate", kbps = kbps).to_string())
                        .small()
                        .color(theme::TEXT_DIM),
                );
            }
        }

        if !video || !active {
            return;
        }

        // Per-destination settings. Every edit here goes through
        // `classify_change` on apply — the row deliberately does not try to
        // predict which tier it will land in.
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(t!("stream_control.resolution_label").to_string()).small().color(theme::TEXT_DIM));
            let mut scale = dest.edit.scale_percent;
            let selected = RESOLUTION_SCALES
                .iter()
                .find(|(percent, _)| *percent == scale)
                .map(|(_, label)| *label)
                .unwrap_or("100%");
            egui::ComboBox::from_id_salt(("dest_scale", destination_id.0))
                .selected_text(selected)
                .width(72.0)
                .show_ui(ui, |ui| {
                    for (percent, label) in RESOLUTION_SCALES {
                        if ui.selectable_label(scale == percent, label).clicked() {
                            scale = percent;
                        }
                    }
                });
            if scale != dest.edit.scale_percent {
                actions.push(StreamAction::EditScale(destination_id, scale));
            }

            ui.label(egui::RichText::new(t!("share.framerate_label").to_string()).small().color(theme::TEXT_DIM));
            let mut framerate = dest.edit.framerate;
            egui::ComboBox::from_id_salt(("dest_fps", destination_id.0))
                .selected_text(format!("{framerate} fps"))
                .width(84.0)
                .show_ui(ui, |ui| {
                    for &fps in &FRAMERATE_OPTIONS {
                        if ui.selectable_label(framerate == fps, format!("{fps} fps")).clicked() {
                            framerate = fps;
                        }
                    }
                });
            if framerate != dest.edit.framerate {
                actions.push(StreamAction::EditFramerate(destination_id, framerate));
            }
        });

        if self.state.bandwidth_mode == BandwidthMode::PerStream {
            let mut bitrate = dest.edit.bitrate_kbps;
            if theme::slider_row(
                ui,
                t!("share.bitrate_label").as_ref(),
                &mut bitrate,
                MIN_BITRATE_KBPS..=MAX_BITRATE_KBPS,
                50.0,
                " kbps",
            ) {
                actions.push(StreamAction::EditBitrate(destination_id, bitrate));
            }
        }

        ui.horizontal(|ui| {
            let applying = dest.apply_rx.is_some();
            ui.add_enabled_ui(!applying, |ui| {
                if ui.add(theme::secondary_button(t!("stream_control.apply_button").to_string())).clicked() {
                    actions.push(StreamAction::ApplyEdit(destination_id));
                }
            });
            if applying {
                ui.spinner();
                ui.label(egui::RichText::new(t!("stream_control.applying").to_string()).small().color(theme::TEXT_DIM));
            }

            // Transfer and fan-out: both are "this same capture, in another
            // room", so both are a room picker. They differ only in whether
            // the current room is left behind.
            //
            // `Why both pickers exclude rooms this capture already feeds:` the
            // result would be two independent streams of one screen in one
            // room — which nobody means by either action, and which doubles
            // that room's incoming bandwidth for a duplicate picture. For
            // transfer it would additionally be self-defeating (the "old"
            // destination is in a room the capture would still be feeding).
            let others: Vec<(RoomHandle, String)> = self
                .state
                .rooms
                .iter()
                .filter(|(handle, _)| **handle != dest.room)
                .filter(|(handle, _)| {
                    !self
                        .state
                        .captures
                        .get(&capture_id)
                        .is_some_and(|c| c.destinations.values().any(|d| d.room == **handle))
                })
                .map(|(handle, room)| (*handle, room_label(&self.state.room_store, &room.join_string)))
                .collect();
            let can_move = !others.is_empty() && dest.pending.is_none();
            ui.add_enabled_ui(can_move, |ui| {
                egui::ComboBox::from_id_salt(("dest_transfer", destination_id.0))
                    .selected_text(t!("stream_control.transfer_button").to_string())
                    .show_ui(ui, |ui| {
                        for (handle, label) in &others {
                            if ui.button(label.clone()).clicked() {
                                actions.push(StreamAction::Transfer(destination_id, *handle));
                            }
                        }
                    });
                egui::ComboBox::from_id_salt(("dest_fanout", destination_id.0))
                    .selected_text(t!("stream_control.fanout_button").to_string())
                    .show_ui(ui, |ui| {
                        ui.label(
                            egui::RichText::new(t!("stream_control.fanout_hint").to_string())
                                .small()
                                .color(theme::TEXT_FAINT),
                        );
                        for (handle, label) in &others {
                            if ui.button(label.clone()).clicked() {
                                actions.push(StreamAction::FanOut(capture_id, *handle));
                            }
                        }
                    });
            });
        });

        // Sender-side self-preview: a small live view of this machine's own
        // captured video, so the person sharing can confirm what viewers
        // actually see. See `superdoc/features/self-preview.md`. Rendered once
        // per capture (on its first destination's row), since it shows the
        // *capture*, which every destination of it shares.
        if capture.destinations.keys().next() == Some(&destination_id) {
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

    /// Colour + text for a destination's status dot, folding the capture's own
    /// start/stop state, the make-before-break transfer/fan-out roles, and the
    /// live `DestinationState` into one label.
    fn destination_status(&self, capture: &LocalCapture, dest: &Destination) -> (egui::Color32, String) {
        match &capture.status {
            ShareStatus::Starting(_) => return (theme::WARNING, t!("share.state_starting").to_string()),
            ShareStatus::Stopping(_) => return (theme::TEXT_FAINT, t!("share.state_stopping").to_string()),
            ShareStatus::Active(_) => {}
        }
        if let Some(role) = dest.pending {
            let label = match role {
                PendingRole::FanOut => t!("stream_control.state_adding").to_string(),
                PendingRole::TransferTarget(from) => {
                    t!("stream_control.state_transfer_in", room = self.room_name(from)).to_string()
                }
                PendingRole::TransferSource(to) => {
                    t!("stream_control.state_transfer_out", room = self.room_name(to)).to_string()
                }
            };
            return (theme::WARNING, label);
        }
        match dest.state() {
            capture::destination::DestinationState::Starting => {
                (theme::WARNING, t!("share.state_starting").to_string())
            }
            capture::destination::DestinationState::Live => (theme::SUCCESS, t!("share.state_active").to_string()),
            capture::destination::DestinationState::Rebuilding { .. } => {
                (theme::WARNING, t!("stream_control.state_rebuilding").to_string())
            }
            capture::destination::DestinationState::Stalled { .. } => {
                (theme::WARNING, t!("stream_control.state_stalled").to_string())
            }
            capture::destination::DestinationState::Failed(e) => {
                (theme::DANGER, t!("stream_control.state_failed", error = e).to_string())
            }
        }
    }

    fn room_name(&self, handle: RoomHandle) -> String {
        match self.state.rooms.get(&handle) {
            Some(room) => room_label(&self.state.room_store, &room.join_string),
            None => t!("streams_section.unknown_room").to_string(),
        }
    }

    /// The selected room's own card: who's in it, its saved name, and audio
    /// sharing (still a per-capture concept of its own — see the plan doc's
    /// audio-attachment stage).
    fn ui_room_card(&mut self, ui: &mut egui::Ui, actions: &mut Vec<StreamAction>) {
        let Some(room_handle) = self.state.active_room else { return };
        let AppState {
            rooms,
            room_store,
            audio_sources,
            audio_sources_error,
            audio_sources_requested,
            audio_sources_rx,
            selected_audio_source,
            ..
        } = &mut self.state;
        let Some(room) = rooms.get_mut(&room_handle) else { return };

        theme::card(ui, t!("members.header", count = room.members.len()).as_ref(), |ui| {
            ui.label(
                egui::RichText::new(
                    t!("connected.status", conn_id = format!("{:016x}", room.client.conn_id())).to_string(),
                )
                .small()
                .color(theme::TEXT_DIM),
            );
            egui::ScrollArea::vertical().id_salt("members_scroll").max_height(120.0).show(ui, |ui| {
                // Show all members, not just streamers (spec §8/§4.2: a
                // lurker with stream_count == 0 still appears in the roster).
                for member in &room.members {
                    ui.horizontal(|ui| {
                        if member.stream_count > 0 {
                            theme::status_dot(ui, theme::SUCCESS, true);
                            let key =
                                if member.stream_count == 1 { "members.stream_singular" } else { "members.stream_plural" };
                            ui.label(
                                egui::RichText::new(
                                    t!(key, name = member.display_name, count = member.stream_count).to_string(),
                                )
                                .color(theme::TEXT),
                            );
                        } else {
                            theme::status_dot(ui, theme::TEXT_FAINT, false);
                            ui.label(egui::RichText::new(&member.display_name).color(theme::TEXT_DIM));
                        }
                    });
                }
            });

            // Rename this room's saved entry. A single field plus one button
            // doesn't need card ceremony of its own (STYLING-GUIDE.md: cards
            // are for *grouping* related controls).
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(t!("rooms.save_label").to_string()).color(theme::TEXT));
                ui.text_edit_singleline(&mut room.save_name);
                let new_name = room.save_name.trim().to_string();
                ui.add_enabled_ui(!new_name.is_empty(), |ui| {
                    if ui.add(theme::secondary_button(t!("rooms.save_button").to_string())).clicked() {
                        actions.push(StreamAction::RenameRoom(room.join_string.clone(), new_name.clone(), room_handle));
                    }
                });
            });
            if room.saved {
                ui.colored_label(theme::SUCCESS, t!("rooms.saved_confirmation").to_string());
            }
            let _ = room_store;
        });

        theme::card(ui, t!("share.audio_header").as_ref(), |ui| {
            ui.horizontal(|ui| {
                if !*audio_sources_requested {
                    *audio_sources_requested = true;
                    actions.push(StreamAction::RefreshAudio);
                }
                if ui.add(theme::secondary_button(t!("share.refresh_button").to_string())).clicked() {
                    actions.push(StreamAction::RefreshAudio);
                }
                if audio_sources_rx.is_some() {
                    ui.spinner();
                }
            });
            ui.horizontal(|ui| {
                let placeholder = t!("share.select_source_placeholder").to_string();
                let selected_text =
                    selected_audio_source.as_ref().map(|s| s.name.as_str()).unwrap_or(placeholder.as_str());
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
                ui.add_enabled_ui(selected_audio_source.is_some(), |ui| {
                    if ui.add(theme::primary_button(t!("share.audio_button").to_string())).clicked() {
                        actions.push(StreamAction::StartAudio(room_handle));
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
    }

    /// Apply one deferred Stream Control action, now that the render borrows
    /// are gone (same shape as `apply_room_action`).
    fn apply_stream_action(&mut self, action: StreamAction) {
        match action {
            StreamAction::StartCapture(room) => self.state.start_video_share(room),
            StreamAction::StartAudio(room) => self.state.start_audio_share(room),
            StreamAction::RefreshAudio => self.state.refresh_audio_sources(),
            StreamAction::StopDestination(destination_id) => self.state.stop_destination(destination_id),
            StreamAction::Transfer(destination_id, room) => self.state.transfer_destination(destination_id, room),
            StreamAction::FanOut(capture_id, room) => self.state.add_destination(capture_id, room, PendingRole::FanOut),
            StreamAction::ApplyEdit(destination_id) => self.state.apply_destination_edit(destination_id),
            StreamAction::EditScale(destination_id, scale) => {
                self.state.with_destination(destination_id, |dest| dest.edit.scale_percent = scale);
            }
            StreamAction::EditFramerate(destination_id, framerate) => {
                self.state.with_destination(destination_id, |dest| dest.edit.framerate = framerate);
            }
            StreamAction::EditBitrate(destination_id, bitrate) => {
                self.state.with_destination(destination_id, |dest| dest.edit.bitrate_kbps = bitrate);
            }
            StreamAction::RenameRoom(join_string, name, room_handle) => {
                self.state.rename_saved_room(&join_string, name);
                if let Some(room) = self.state.rooms.get_mut(&room_handle) {
                    room.saved = true;
                }
            }
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
        let Some(room_handle) = self.state.active_room else {
            Self::ui_empty_state(ui, t!("view.empty").as_ref());
            return;
        };
        let Some(room) = self.state.rooms.get_mut(&room_handle) else { return };

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
        self.state.poll_capture_states();
        self.state.poll_pending_destinations();
        self.state.poll_destination_edits();
        self.state.poll_audio_sources();
        self.update_self_preview_textures(ctx);
        // Absolute-Maximum mode's live wiring. Called every frame but
        // internally rate-limited to one recompute per second and hysteresis-
        // filtered per destination, so this is a cheap no-op on the vast
        // majority of frames — see `session::BandwidthController`.
        self.state.apply_bandwidth_allocation();

        // Every room's watches are polled and repainted, not just the one the
        // UI happens to be showing: a `StreamDecoder` that stops being fed
        // (or stops being drained) stalls, and a room being off-screen is a
        // view decision, not a reason to starve its decoders.
        let mut has_inline_watch = false;
        for (handle, room) in self.state.rooms.iter_mut() {
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
        if matches!(self.state.status, Status::Connecting) || !self.state.rooms.is_empty() {
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
        self.state.stop_all_captures_fire_and_forget();
    }
}

#[cfg(test)]
mod session_model_tests {
    //! Pure (no I/O, no GStreamer, no network) coverage for the session
    //! model's cross-room lookups, plus a couple of view-layer-adjacent
    //! formatting/default checks (`NavTab::default`, `remote_stream_geometry`,
    //! `destination_geometry`) that stayed with the rendering code they serve
    //! rather than moving into `session.rs`.
    //!
    //! The whole point of the rooms/captures/destinations split is that a
    //! protocol `StreamId` is only unique *within one connection*: two rooms
    //! will both hand out `stream_id` 0 for their first outgoing stream, and
    //! the old singleton `active_shares: HashMap<StreamId, _>` therefore had
    //! one entry where two were needed. These tests pin the replacement
    //! behaviour with that collision deliberately constructed.
    use super::*;
    use crate::session::{capture_feeding, find_destination, DestEdit, DEFAULT_BITRATE_KBPS, DEFAULT_FRAMERATE};
    use indexmap::IndexMap;
    use tokio::sync::oneshot;

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
            pending: None,
            edit: DestEdit { scale_percent: 100, framerate: DEFAULT_FRAMERATE, bitrate_kbps: DEFAULT_BITRATE_KBPS },
            apply_rx: None,
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
        let (a, a_dest) = capture_feeding(&captures, room_a, 0).expect("room A's stream 0 must resolve");
        let (b, b_dest) = capture_feeding(&captures, room_b, 0).expect("room B's stream 0 must resolve");
        assert!(std::ptr::eq(a, &captures[&CaptureSourceId(10)]), "room A's stream 0 resolved to the wrong capture");
        assert!(std::ptr::eq(b, &captures[&CaptureSourceId(11)]), "room B's stream 0 resolved to the wrong capture");
        assert!(!std::ptr::eq(a, b), "the two rooms' stream 0 must not resolve to the same capture");
        // The destination id comes back too, because every relay event this
        // lookup serves acts on one destination's own encode branch.
        assert_eq!(a_dest, DestinationId(100));
        assert_eq!(b_dest, DestinationId(101));

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

        let store = crate::rooms::RoomStore::default();
        let label = room_label(&store, &join_string);
        assert!(label.contains("00112233"), "expected the room-id prefix in {label:?}");
        assert!(!label.contains(key), "the room key must never reach a rendered label");
        assert!(!label.contains("relay.example"), "the label is an identifier, not a connection string");

        // A saved room wins over the fallback.
        let mut store = crate::rooms::RoomStore::default();
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
        // `AppState::default()` binds a real UDP socket for the shared
        // endpoint, so this needs a runtime entered — hence `#[tokio::test]`.
        let mut state = AppState::default();
        assert_eq!(state.alloc_capture_id(), CaptureSourceId(0));
        assert_eq!(state.alloc_capture_id(), CaptureSourceId(1));
        // Allocating a destination id must not disturb the capture counter,
        // and vice versa — they are separate spaces on purpose.
        assert_eq!(state.alloc_destination_id(), DestinationId(0));
        assert_eq!(state.alloc_capture_id(), CaptureSourceId(2));
        assert_eq!(state.alloc_destination_id(), DestinationId(1));
    }
}

#[cfg(test)]
mod bandwidth_wiring_tests {
    //! Absolute-Maximum mode's live wiring: that the allocator's output
    //! actually converges the *encoders'* bitrates, and that the hysteresis
    //! and rate limit really do suppress redundant writes rather than being
    //! decorative.
    //!
    //! `Why` a stand-in encoder map rather than real GStreamer branches: what
    //! is under test here is the decision layer — when a write happens and
    //! what value it carries. That a write, once decided, lands on the live
    //! encoder is proved separately and for real against GStreamer, in
    //! `capture::branch::tests::
    //! setting_one_branchs_bitrate_writes_through_to_that_encoder_only`.
    //! Splitting it this way keeps the timing assertions deterministic (the
    //! controller takes an injected `Instant`, so nothing sleeps).
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    use crate::bandwidth::{AllocInput, DestId, DestSpec};
    use crate::session::{needs_reapply, BandwidthController, ALLOC_RECOMPUTE_INTERVAL};

    use super::{DestinationId, MAX_BITRATE_KBPS};

    fn spec(id: u32, width: u32, height: u32, fps: u32) -> DestSpec {
        DestSpec { id: DestId(id), width, height, fps, floor_kbps: 500, ceiling_kbps: Some(MAX_BITRATE_KBPS) }
    }

    fn input(total_kbps: u32, destinations: Vec<DestSpec>) -> AllocInput {
        AllocInput { total_kbps, audio_reserved_kbps: 0, destinations }
    }

    /// A stand-in for "what each destination's encoder is currently set to",
    /// updated exactly the way `AppState::apply_bandwidth_allocation` updates
    /// the real ones.
    #[derive(Default)]
    struct FakeEncoders {
        bitrates: HashMap<DestinationId, u32>,
        writes: u32,
    }

    impl FakeEncoders {
        fn apply(&mut self, controller: &mut BandwidthController, now: Instant, input: &AllocInput) {
            let Some(tick) = controller.tick(now, input) else { return };
            for (id, kbps) in tick.writes {
                self.bitrates.insert(id, kbps);
                self.writes += 1;
                controller.note_applied(id, kbps);
            }
        }
    }

    /// Two destinations of very different weight: after one tick, both
    /// encoders carry exactly what `bandwidth::allocate` decided, and their
    /// shares reflect the `width*height*fps` split rather than an even one.
    #[test]
    fn absolute_mode_converges_two_destinations_onto_the_allocators_output() {
        let mut controller = BandwidthController::new();
        let mut encoders = FakeEncoders::default();
        let now = Instant::now();

        // 1080p60 and 720p30: weights 124,416,000 vs 27,648,000, i.e. ~4.5:1.
        let input = input(20_000, vec![spec(1, 1920, 1080, 60), spec(2, 1280, 720, 30)]);
        encoders.apply(&mut controller, now, &input);

        let expected = crate::bandwidth::allocate(&input);
        for (id, kbps) in &expected.per_destination {
            assert_eq!(
                encoders.bitrates.get(&DestinationId(id.0)),
                Some(kbps),
                "destination {id:?}'s encoder did not converge onto the allocator's value"
            );
        }
        let big = encoders.bitrates[&DestinationId(1)];
        let small = encoders.bitrates[&DestinationId(2)];
        assert!(big > small * 3, "the heavier destination should get a proportionally larger share ({big} vs {small})");
        assert_eq!(big + small, 20_000, "the whole budget should be assigned");
        assert_eq!(encoders.writes, 2);
    }

    /// The per-client aggregate really is per client: adding a third
    /// destination — in a different room, which the allocator neither knows
    /// nor cares about — shrinks the existing two.
    #[test]
    fn starting_another_stream_anywhere_shrinks_every_other_destinations_share() {
        let mut controller = BandwidthController::new();
        let mut encoders = FakeEncoders::default();
        let now = Instant::now();

        let two = input(20_000, vec![spec(1, 1920, 1080, 60), spec(2, 1280, 720, 30)]);
        encoders.apply(&mut controller, now, &two);
        let before = encoders.bitrates.clone();

        let three =
            input(20_000, vec![spec(1, 1920, 1080, 60), spec(2, 1280, 720, 30), spec(3, 1920, 1080, 30)]);
        encoders.apply(&mut controller, now + ALLOC_RECOMPUTE_INTERVAL, &three);

        for id in [DestinationId(1), DestinationId(2)] {
            assert!(
                encoders.bitrates[&id] < before[&id],
                "{id:?} kept its share ({} -> {}) after a third stream started",
                before[&id],
                encoders.bitrates[&id]
            );
        }
        let total: u32 = encoders.bitrates.values().sum();
        assert_eq!(total, 20_000);
    }

    /// The rate limit: a second tick inside `ALLOC_RECOMPUTE_INTERVAL`
    /// produces no recompute at all, even when the inputs changed enough that
    /// it otherwise would have written.
    #[test]
    fn recomputes_are_rate_limited_to_one_per_interval() {
        let mut controller = BandwidthController::new();
        let mut encoders = FakeEncoders::default();
        let now = Instant::now();

        let one = input(20_000, vec![spec(1, 1920, 1080, 60)]);
        encoders.apply(&mut controller, now, &one);
        assert_eq!(encoders.writes, 1);

        // Roster churn: a second destination appears 10ms later. Suppressed.
        let two = input(20_000, vec![spec(1, 1920, 1080, 60), spec(2, 1920, 1080, 60)]);
        for step in [10u64, 200, 900] {
            encoders.apply(&mut controller, now + Duration::from_millis(step), &two);
            assert_eq!(encoders.writes, 1, "a recompute at +{step}ms should have been rate-limited away");
        }

        // Past the interval, it recomputes and halves the first destination.
        encoders.apply(&mut controller, now + ALLOC_RECOMPUTE_INTERVAL, &two);
        assert_eq!(encoders.writes, 3, "one write per destination once the rate limit allows a recompute");
        assert_eq!(encoders.bitrates[&DestinationId(1)], 10_000);
    }

    /// The hysteresis: re-ticking with unchanged inputs recomputes (the rate
    /// limit allows it) but writes nothing, and a sub-threshold change is
    /// likewise not worth touching the encoder for.
    #[test]
    fn hysteresis_suppresses_redundant_and_tiny_reapplications() {
        let mut controller = BandwidthController::new();
        let mut encoders = FakeEncoders::default();
        let mut now = Instant::now();

        let steady = input(20_000, vec![spec(1, 1920, 1080, 60), spec(2, 1280, 720, 30)]);
        encoders.apply(&mut controller, now, &steady);
        assert_eq!(encoders.writes, 2);

        for _ in 0..5 {
            now += ALLOC_RECOMPUTE_INTERVAL;
            encoders.apply(&mut controller, now, &steady);
        }
        assert_eq!(encoders.writes, 2, "nothing changed, so nothing should have been written to any encoder");

        // A 50 kbit/s bump on a ~16 Mbit/s stream: under both the 5% and the
        // 100 kbit/s bounds, so still not worth a write.
        now += ALLOC_RECOMPUTE_INTERVAL;
        let nudged = input(20_050, vec![spec(1, 1920, 1080, 60), spec(2, 1280, 720, 30)]);
        encoders.apply(&mut controller, now, &nudged);
        assert_eq!(encoders.writes, 2, "a sub-threshold change must not reach the encoders");

        // A real change does get through.
        now += ALLOC_RECOMPUTE_INTERVAL;
        let halved = input(10_000, vec![spec(1, 1920, 1080, 60), spec(2, 1280, 720, 30)]);
        encoders.apply(&mut controller, now, &halved);
        assert_eq!(encoders.writes, 4, "halving the total budget must reach both encoders");
    }

    /// The threshold itself, at both ends of the range it has to work across.
    #[test]
    fn the_hysteresis_threshold_is_the_looser_of_five_percent_and_100_kbps() {
        // Never written before: always worth writing.
        assert!(needs_reapply(None, 1_000));
        // High rate: 5% of 8000 is 400, so the 100 kbit/s bound is what bites.
        assert!(!needs_reapply(Some(8_000), 8_090));
        assert!(needs_reapply(Some(8_000), 8_150));
        // Low rate: 5% of 400 is 20, which bites well before 100 kbit/s would.
        assert!(!needs_reapply(Some(400), 415));
        assert!(needs_reapply(Some(400), 430));
        assert!(!needs_reapply(Some(1_000), 1_000), "an identical value is never worth a write");
    }

    /// Overshoot is surfaced, not swallowed: floors that don't fit the total
    /// still get honoured and the controller keeps the allocation around for
    /// the warning banner to read.
    #[test]
    fn overshoot_is_kept_for_the_warning_banner() {
        let mut controller = BandwidthController::new();
        let input = input(800, vec![spec(1, 1920, 1080, 60), spec(2, 1280, 720, 30)]);
        let tick = controller.tick(Instant::now(), &input).expect("first tick always recomputes");

        assert_eq!(tick.allocation.overshoot_kbps, 200, "two 500 kbps floors against an 800 kbps total");
        for (_, kbps) in &tick.allocation.per_destination {
            assert_eq!(*kbps, 500, "no destination may be starved below its floor");
        }
        assert_eq!(
            controller.allocation().map(|a| a.overshoot_kbps),
            Some(200),
            "the UI reads the overshoot back off the controller"
        );
    }

    /// Leaving Absolute mode drops the allocator's bookkeeping, so the next
    /// time it is switched on every destination is written afresh rather than
    /// being compared against a stale "last applied" value from before the
    /// user's manual rates took over.
    #[test]
    fn resetting_forgets_what_the_allocator_had_applied() {
        let mut controller = BandwidthController::new();
        let mut encoders = FakeEncoders::default();
        let now = Instant::now();
        let steady = input(20_000, vec![spec(1, 1920, 1080, 60)]);

        encoders.apply(&mut controller, now, &steady);
        assert_eq!(encoders.writes, 1);
        assert_eq!(controller.applied_kbps(DestinationId(1)), Some(20_000));

        controller.reset();
        assert_eq!(controller.applied_kbps(DestinationId(1)), None);
        // Same inputs, but nothing is remembered — so it writes again.
        encoders.apply(&mut controller, now, &steady);
        assert_eq!(encoders.writes, 2);
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
    use crate::decode::StreamDecoder;
    use crate::end_to_end_test::{connect_with_retry, pick_free_udp_port, spawn_relay_server};
    use crate::net::{format_join_string, ClientEvent};
    use crate::protocol::{PresenceEvent, RoomId};
    use crate::session::WatchEntry;
    use indexmap::IndexMap;
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

        // Stand the room up through the exact same `AppState::insert_room`
        // call `poll_connect_result` uses in production, then wire up a watch
        // on the sharer's stream the same way `toggle_watch` does — this is
        // the real code under test, not a re-implementation of it.
        let mut app = App::default();
        app.state.framerate = TEST_FPS;
        let room_handle = app.state.insert_room(client_b, snapshot_b, events_b, join_string.clone());
        app.state.active_room = Some(room_handle);
        let decoder = StreamDecoder::new(TEST_STREAM_ID).expect("failed to build StreamDecoder");
        app.state.rooms[&room_handle].watches.lock().unwrap().insert(
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
                let Some(room) = app.state.rooms.get_mut(&room_handle) else { return };
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

        let observed_fps = app.state.rooms[&room_handle]
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
            app.state.rooms[&room_handle].watches.lock().unwrap().contains_key(&(owner, TEST_STREAM_ID)),
            "the watch was torn down at some point during nav-tab switching"
        );
    }

    /// Regression test for the "fast-forward catch-up" bug (see
    /// `superdoc/brainstorm/ui-redesign-multiroom-streaming.md`,
    /// "Performance/Rendering" round 2, and `session::spawn_media_ingestion_task`'s
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
        let mut app = App::default();
        app.state.framerate = TEST_FPS;
        let room_handle = app.state.insert_room(client_b, snapshot_b, events_b, join_string.clone());
        app.state.active_room = Some(room_handle);
        let decoder = StreamDecoder::new(TEST_STREAM_ID).expect("failed to build StreamDecoder");
        app.state.rooms[&room_handle].watches.lock().unwrap().insert(
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
                let Some(room) = app.state.rooms.get_mut(&room_handle) else { return };
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

        let observed_fps = app.state.rooms[&room_handle]
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

    /// Stage-2 Stream Control, end to end against a real relay and a real
    /// GStreamer capture: **fan-out** (one capture, a second independently
    /// encoded and encrypted destination in another room), **transfer**
    /// (make-before-break to a third room), **Absolute-Maximum bandwidth**
    /// (`bandwidth::allocate` applied to the live encoders), and the Stream
    /// Control tab's real layout code running over all of it.
    ///
    /// `Why` all four in one test rather than four: they share one expensive
    /// fixture (a relay child process, three real QUIC connections, a real
    /// encode pipeline), and — more importantly — the interesting failures are
    /// in how they interact. A fan-out that works but leaves the allocator
    /// with a destination it can't find, or a transfer that completes but
    /// leaves the UI rendering a row for a destination that no longer exists,
    /// would both pass a narrower test of either half.
    ///
    /// The egui pass at the end is deliberately a *headless layout* run
    /// (`egui::Context::run`, no window, no GPU): what it proves is that the
    /// tab's real widget code — with real captures, real destinations,
    /// mid-transfer states and a live allocation — lays out without panicking
    /// on an id collision, a double borrow, or a `None` it didn't expect.
    #[tokio::test]
    #[ignore = "spawns a real relay server child process and a real GStreamer capture pipeline; \
                run explicitly with `cargo test ui:: -- --ignored --nocapture`"]
    async fn fan_out_transfer_and_absolute_bandwidth_across_three_rooms() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        gstreamer::init().expect("gstreamer init");

        // Point this test's settings/rooms files at a throwaway directory
        // before anything reads or writes them.
        //
        // `Why:` the mode switch below deliberately goes through the real
        // `set_bandwidth_mode`, which *persists* — and `rooms.rs` resolves its
        // config path through `dirs::config_dir()` at call time, so without
        // this the test would rewrite the developer's own
        // `~/.config/simple-screensharing/settings.toml`. `set_var` is
        // `unsafe` in edition 2024 because another thread could be reading the
        // environment concurrently; this runs before any room, capture or
        // task exists, and no other test in this crate touches `XDG_CONFIG_HOME`.
        let config_home = std::env::temp_dir().join(format!("sss-stream-control-test-{}", std::process::id()));
        std::fs::create_dir_all(&config_home).expect("temp config dir");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &config_home) };

        let port = pick_free_udp_port().expect("failed to pick a free UDP port for the test relay");
        eprintln!("[stream-control] starting relay server on 127.0.0.1:{port}");
        let _server = spawn_relay_server(port).expect("failed to spawn relay server child process");
        let server_addr = format!("127.0.0.1:{port}");

        let mut app = App::default();
        let mut handles = Vec::new();
        let mut clients = Vec::new();
        for name in ["room-a", "room-b", "room-c"] {
            let room_id: RoomId = rand::random();
            let room_key: [u8; 32] = rand::random();
            let join_string = format_join_string(&server_addr, &room_id, &room_key);
            let (client, snapshot, events) = connect_with_retry(&join_string, name)
                .await
                .unwrap_or_else(|e| panic!("{name} failed to connect: {e:#}"));
            clients.push(client.clone());
            handles.push(app.state.insert_room(client, snapshot, events, join_string));
        }
        let (room_a, room_b, room_c) = (handles[0], handles[1], handles[2]);

        // Start a capture into room A exactly as `start_video_share` does,
        // then adopt the handle the way `poll_capture_states` does. (The real
        // `start_video_share` would go through the interactive portal, which
        // no unattended test can drive — hence the same `TestSource` seam
        // `end_to_end_test` uses.)
        let capture_id = app.state.alloc_capture_id();
        let destination_a = app.state.alloc_destination_id();
        assert_eq!(destination_a, DestinationId(0), "the test source's DestKey(0) must match the first destination id");
        let stream_id = {
            let room = app.state.rooms.get_mut(&room_a).expect("room A");
            let stream_id = room.next_stream_id;
            room.next_stream_id += 1;
            room.destinations.push(destination_a);
            stream_id
        };
        let source_fragment = "videotestsrc is-live=true pattern=ball \
             ! video/x-raw,width=320,height=240,framerate=30/1";
        let capture_handle = capture::start_sharing_from_test_source(&clients[0], stream_id, 4_000, 30, source_fragment)
            .await
            .expect("start_sharing_from_test_source failed");
        let runtime = capture_handle.destination_runtime(DestKey(destination_a.0));
        let params = runtime.as_ref().expect("the first destination must exist").lock().unwrap().params();
        app.state.captures.insert(
            capture_id,
            LocalCapture {
                kind: StreamKind::Video,
                label: "test capture".to_string(),
                status: ShareStatus::Active(ShareHandle::Video(capture_handle)),
                destinations: IndexMap::from([(
                    destination_a,
                    Destination {
                        room: room_a,
                        stream_id,
                        params,
                        runtime,
                        pending: None,
                        edit: Destination::edit_from(params),
                        apply_rx: None,
                    },
                )]),
                self_preview_texture: None,
            },
        );

        // --- Fan-out: the same capture into room B as a second stream. ---
        eprintln!("[stream-control] fanning out into room B");
        app.state.add_destination(capture_id, room_b, PendingRole::FanOut);
        wait_for(&mut app, Duration::from_secs(20), |app| app.state.pending_destinations.is_empty()).await;
        assert!(app.state.share_error.is_none(), "fan-out reported an error: {:?}", app.state.share_error);
        {
            let capture = app.state.captures.get(&capture_id).expect("capture must still exist");
            assert_eq!(capture.destinations.len(), 2, "fan-out must add a second destination to the same capture");
            let rooms: HashSet<RoomHandle> = capture.destinations.values().map(|d| d.room).collect();
            assert_eq!(rooms, HashSet::from([room_a, room_b]), "the two destinations must be in different rooms");
            for (id, dest) in &capture.destinations {
                assert!(dest.runtime.is_some(), "destination {id:?} has no live runtime after fan-out");
            }
        }
        // Both destinations must actually reach `Live` — i.e. each has its own
        // encode branch producing IDRs, which is what makes fan-out two real
        // encoded streams rather than one stream claimed twice.
        wait_for(&mut app, Duration::from_secs(20), |app| {
            app.state.captures[&capture_id]
                .destinations
                .values()
                .all(|d| d.state() == capture::destination::DestinationState::Live)
        })
        .await;

        // --- Absolute-Maximum bandwidth over both destinations. ---
        eprintln!("[stream-control] switching to Absolute Maximum");
        app.state.absolute_total_kbps = 6_000;
        app.state.destination_floor_kbps = 500;
        app.state.set_bandwidth_mode(BandwidthMode::Absolute);
        app.state.apply_bandwidth_allocation();
        let applied: Vec<u32> = app.state.captures[&capture_id]
            .destinations
            .keys()
            .map(|id| app.state.bandwidth.applied_kbps(*id).unwrap_or_else(|| panic!("{id:?} got no allocation")))
            .collect();
        assert_eq!(applied.iter().sum::<u32>(), 6_000, "the whole per-client budget should be assigned: {applied:?}");
        let allocation = app.state.bandwidth.allocation().expect("an allocation must have been recorded");
        assert_eq!(allocation.overshoot_kbps, 0, "6000 kbps comfortably covers two 500 kbps floors");

        // A second immediate call must be rate-limited away entirely.
        let before = app.state.bandwidth.applied_kbps(destination_a);
        app.state.absolute_total_kbps = 40_000;
        app.state.apply_bandwidth_allocation();
        assert_eq!(
            app.state.bandwidth.applied_kbps(destination_a),
            before,
            "a recompute inside the rate-limit window must not reach the encoders"
        );

        // --- Transfer room B's destination to room C, make-before-break. ---
        eprintln!("[stream-control] transferring room B's destination to room C");
        let destination_b = *app.state.captures[&capture_id]
            .destinations
            .iter()
            .find(|(_, d)| d.room == room_b)
            .map(|(id, _)| id)
            .expect("room B destination");
        app.state.transfer_destination(destination_b, room_c);
        // While it's in flight the *old* destination is still there and still
        // marked as the transfer's source — that is the "make" half being
        // visible rather than a silent swap.
        assert!(
            matches!(
                app.state.captures[&capture_id].destinations[&destination_b].pending,
                Some(PendingRole::TransferSource(room)) if room == room_c
            ),
            "the source destination should be marked mid-transfer"
        );
        // Render the tab mid-transfer, which is the state most likely to trip
        // the UI up (two rows for one logical stream, one of them with no
        // runtime yet).
        render_stream_control(&mut app);

        wait_for(&mut app, Duration::from_secs(20), |app| app.state.pending_destinations.is_empty()).await;
        assert!(app.state.share_error.is_none(), "transfer reported an error: {:?}", app.state.share_error);
        {
            let capture = app.state.captures.get(&capture_id).expect("capture must survive a transfer");
            let rooms: HashSet<RoomHandle> = capture.destinations.values().map(|d| d.room).collect();
            assert_eq!(
                rooms,
                HashSet::from([room_a, room_c]),
                "after the transfer the capture must feed room A and room C, and no longer room B"
            );
            assert_eq!(capture.destinations.len(), 2, "a transfer must not leave the old destination behind");
            assert!(
                capture.destinations.values().all(|d| d.pending.is_none()),
                "no destination should still be marked mid-transfer once it settled"
            );
        }
        // Room A's destination — the one nobody touched — must have kept
        // streaming across both the fan-out and the transfer.
        assert_eq!(
            app.state.captures[&capture_id].destinations[&destination_a].state(),
            capture::destination::DestinationState::Live,
            "an unrelated destination of the same capture must be untouched by fan-out/transfer"
        );

        // --- The tab's real layout code, over the settled state. ---
        render_stream_control(&mut app);
        app.state.set_bandwidth_mode(BandwidthMode::PerStream);
        render_stream_control(&mut app);

        eprintln!("[stream-control] tearing down");
        app.state.stop_all_captures_fire_and_forget();
        tokio::time::sleep(Duration::from_secs(1)).await;
        let _ = std::fs::remove_dir_all(&config_home);
    }

    /// Polls `app` (draining its rooms' events and its own pending work, the
    /// way `App::update` does) until `done` or the deadline passes.
    async fn wait_for(app: &mut App, timeout: Duration, done: impl Fn(&App) -> bool) {
        let ctx = egui::Context::default();
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            app.drain_events(&ctx);
            app.state.poll_capture_states();
            app.state.poll_pending_destinations();
            app.state.poll_destination_edits();
            if done(app) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("condition not met within {timeout:?}");
    }

    /// Lays the Stream Control tab out headlessly — no window, no GPU, just
    /// egui's real widget/layout pass over the app's real state.
    fn render_stream_control(app: &mut App) {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        for _ in 0..2 {
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| app.ui_stream_control_tab(ui));
            });
        }
    }

    /// Waits for the watcher to see the sharer's `StreamStarted` presence
    /// delta (the relay silently drops a `Subscribe` for a stream it
    /// doesn't know about yet) and then subscribes — mirrors
    /// `end_to_end_test`'s same wait, just against `RoomSession::event_rx`
    /// directly instead of a bare receiver.
    async fn room_client_b_subscribe(app: &mut App, room_handle: RoomHandle, owner: ConnId) {
        let connected = app.state.rooms.get_mut(&room_handle).expect("room must be present");
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
