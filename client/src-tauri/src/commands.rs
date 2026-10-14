//! Tauri command surface: thin wrappers over `session::AppState`.
//!
//! Every command follows the same shape: lock `Managed::state`, call straight
//! into an `AppState` method (or a small helper below when a command needs to
//! translate frontend-friendly primitives — a room's `u32` handle, a
//! `"absolute"`/`"per_stream"` string — into the real types), and return a
//! DTO from `dto.rs`. The `_impl` functions below take `&mut AppState`
//! directly (rather than the `tauri::State` the `#[tauri::command]` fns
//! receive) so tests can call them against a real `AppState` without needing
//! a running `tauri::App` — see the `tests` module at the bottom.
use tauri::State;

use simple_screensharing_client::session::{
    AppState, CaptureSourceId, DestEdit, DestinationId, PendingRole, RoomHandle,
};

use crate::dto::{self, AudioSourceDto, DestinationRow, RoomDetail, SavedRoomRow, SettingsDto, StartStreamDefaultsDto};
use crate::state::Managed;

// ---------------------------------------------------------------------
// Room / connection
// ---------------------------------------------------------------------

pub fn list_saved_rooms_impl(state: &AppState) -> Vec<SavedRoomRow> {
    dto::saved_room_rows(state)
}

#[tauri::command]
pub fn list_saved_rooms(managed: State<'_, Managed>) -> Vec<SavedRoomRow> {
    list_saved_rooms_impl(&managed.state.lock().unwrap())
}

/// See `session::AppState::join_room` (room-name pre-seed + connect).
pub fn join_room_impl(state: &mut AppState, join_string: String, display_name: String, room_name: String) -> Result<(), String> {
    state.join_room(join_string, display_name, room_name)
}

// `Why:` `join_room` is a plain (non-`async`) `#[tauri::command]`. Tauri's
// command macro only enters its managed Tokio runtime for `async fn`
// commands (it wraps their execution in `tauri::async_runtime::spawn` before
// polling them); a `fn` command is instead called straight from whatever
// thread delivered the IPC message — on Linux that's the webkit2gtk
// `register_uri_scheme` callback, a C FFI boundary a panic cannot unwind
// across (fatal process abort, not a recoverable error). `join_room_impl`
// eventually calls `session::AppState::start_connect`, which does a raw
// `tokio::spawn` — this is the exact same "no reactor running" class
// `main.rs`'s `Managed::new()` fix and `driver.rs`'s own `spawn()` doc
// comment already document, just a third call site. `session.rs` stays
// untouched (it's shared with the egui frontend, which enters its own Tokio
// runtime directly and has no `tauri` dependency at all) — instead,
// `tauri::async_runtime::block_on` enters Tauri's one shared runtime for the
// duration of this synchronous call, same as `main.rs`'s fix. `block_on`
// here does not block on any real I/O: `start_connect` only needs the
// reactor to *issue* `tokio::spawn`, not to await the spawned task's
// completion (that's `poll_connect_result`'s job, run by `driver.rs`'s
// already-runtime-entered tick loop), so this returns effectively
// immediately. The same defect, reached the same way, exists in every other
// mutating command below that (transitively) spawns — each carries a short
// pointer back to this note rather than repeating it.
#[tauri::command]
pub fn join_room(managed: State<'_, Managed>, join_string: String, display_name: String, room_name: String) -> Result<(), String> {
    tauri::async_runtime::block_on(async { join_room_impl(&mut managed.state.lock().unwrap(), join_string, display_name, room_name) })
}

pub fn cancel_connect_impl(state: &mut AppState) {
    state.cancel_connect();
}

/// Cancel the in-flight connect attempt — the right-click menu's "Disconnect"
/// while a room row shows "Reconnecting…" (either the 15s timeout hasn't
/// fired yet, or the user doesn't want to wait for it). Unlike `join_room` et
/// al, this needs no `block_on`: `AppState::cancel_connect` only calls
/// `AbortHandle::abort()` (a plain flag flip, not a `tokio::spawn`) and never
/// touches the reactor itself — see `join_room`'s `Why:` note above for the
/// class of bug that guard exists for; this command doesn't belong to it.
#[tauri::command]
pub fn cancel_connect(managed: State<'_, Managed>) {
    cancel_connect_impl(&mut managed.state.lock().unwrap());
}

/// "Create a room": mint a fresh room-id + key, save it under `room_name`, and
/// join it — see `session::AppState::create_room` /
/// `session::AppState::create_and_join_room` for the full rationale (no
/// create verb on the wire, key generated in Rust with the CSPRNG, name
/// pre-seeded before connecting, why `display_name` is not a parameter).
pub fn create_room_impl(state: &mut AppState, server: String, room_name: String) -> Result<(), String> {
    state.create_and_join_room(&server, &room_name)
}

// See `join_room`'s `Why:` note above — `create_room_impl` transitively
// calls `join_room_impl`, which spawns.
#[tauri::command]
pub fn create_room(managed: State<'_, Managed>, server: String, room_name: String) -> Result<(), String> {
    tauri::async_runtime::block_on(async { create_room_impl(&mut managed.state.lock().unwrap(), server, room_name) })
}

/// Whether `leave_room` should be preceded by a confirm dialog — exposed as
/// its own query so the frontend can decide whether to show one *before*
/// calling `leave_room` (see the app-shell doc's "Leaving a room" section).
#[tauri::command]
pub fn leave_needs_confirm(managed: State<'_, Managed>, room_handle: u32) -> bool {
    managed.state.lock().unwrap().leave_needs_confirm(RoomHandle(room_handle))
}

/// "Leave" clicked without anything to lose: tears the room down immediately
/// and returns whether it actually did (`false` means the frontend should
/// have called `leave_needs_confirm` first and shown its own dialog instead).
// See `join_room`'s `Why:` note above — `request_leave_room` transitively
// calls `disconnect_room`, which can spawn (`drop_capture_fire_and_forget`/
// `stop_destination`), so this needs the same `block_on` treatment.
#[tauri::command]
pub fn leave_room(managed: State<'_, Managed>, room_handle: u32) -> bool {
    tauri::async_runtime::block_on(async { managed.state.lock().unwrap().request_leave_room(RoomHandle(room_handle)) })
}

/// Leave after the user confirmed the dialog `leave_needs_confirm` said was
/// needed — unconditional teardown.
// See `join_room`'s `Why:` note above — `disconnect_room` can spawn.
#[tauri::command]
pub fn force_leave_room(managed: State<'_, Managed>, room_handle: u32) {
    tauri::async_runtime::block_on(async { managed.state.lock().unwrap().disconnect_room(RoomHandle(room_handle)) });
}

#[tauri::command]
pub fn rename_room(managed: State<'_, Managed>, join_string: String, new_name: String) -> Result<(), String> {
    let mut state = managed.state.lock().unwrap();
    state.rename_saved_room(&join_string, new_name);
    state.room_store_result()
}

/// Set/clear the room-detail panel's "Auto-Connect" checkbox for a saved
/// room — see `session::AppState::set_room_auto_connect`'s doc comment for
/// what the flag means and where it's consumed (`main.rs`'s `.setup()`).
#[tauri::command]
pub fn set_room_auto_connect(managed: State<'_, Managed>, join_string: String, auto_connect: bool) -> Result<(), String> {
    let mut state = managed.state.lock().unwrap();
    state.set_room_auto_connect(&join_string, auto_connect);
    state.room_store_result()
}

/// Forget a saved room — see `session::AppState::forget_room`. Keyed by
/// `join_string` rather than a room handle since "forget" only applies to a
/// saved, disconnected row, which has no handle at all.
pub fn forget_room_impl(state: &mut AppState, join_string: String) -> Result<(), String> {
    state.forget_room(&join_string)
}

#[tauri::command]
pub fn forget_room(managed: State<'_, Managed>, join_string: String) -> Result<(), String> {
    forget_room_impl(&mut managed.state.lock().unwrap(), join_string)
}

#[tauri::command]
pub fn select_active_room(managed: State<'_, Managed>, room_handle: u32) {
    let mut state = managed.state.lock().unwrap();
    let handle = RoomHandle(room_handle);
    if state.rooms.contains_key(&handle) {
        state.active_room = Some(handle);
    }
}

#[tauri::command]
pub fn room_detail(managed: State<'_, Managed>, room_handle: u32) -> Option<RoomDetail> {
    dto::room_detail(&managed.state.lock().unwrap(), RoomHandle(room_handle))
}

// ---------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------

#[tauri::command]
pub fn get_settings(managed: State<'_, Managed>) -> SettingsDto {
    dto::settings_dto(&managed.state.lock().unwrap())
}

#[tauri::command]
pub fn set_language(managed: State<'_, Managed>, language: String) {
    managed.state.lock().unwrap().set_language(&language);
}

/// Persist the global display name (Settings tab) — see
/// `session::AppState::set_display_name`.
#[tauri::command]
pub fn set_display_name(managed: State<'_, Managed>, display_name: String) {
    managed.state.lock().unwrap().set_display_name(&display_name);
}

#[tauri::command]
pub fn set_presentation_mode(managed: State<'_, Managed>, mode: String) -> Result<(), String> {
    let mode = dto::parse_presentation_mode(&mode)?;
    managed.state.lock().unwrap().set_presentation_mode(mode);
    Ok(())
}

/// Clamped in the lib (defense in depth) — see
/// `session::AppState::set_max_jitter_buffer_ms`.
#[tauri::command]
pub fn set_max_jitter_buffer_ms(managed: State<'_, Managed>, ms: u32) {
    managed.state.lock().unwrap().set_max_jitter_buffer_ms(ms);
}

/// Clamped in the lib (defense in depth) — see
/// `session::AppState::set_catch_up_speed_percent`.
#[tauri::command]
pub fn set_catch_up_speed_percent(managed: State<'_, Managed>, percent: u32) {
    managed.state.lock().unwrap().set_catch_up_speed_percent(percent);
}

/// The "START A STREAM" form's persisted last-used values — read once when
/// the Stream Control tab loads to pre-fill `state.startForm`
/// (`stream-control.js`) instead of the hardcoded defaults it used to reset
/// to on every launch. See `session::AppState::start_stream_defaults` (a
/// settings-file read).
#[tauri::command]
pub fn get_start_stream_defaults() -> StartStreamDefaultsDto {
    let d = AppState::start_stream_defaults();
    StartStreamDefaultsDto {
        framerate: d.framerate,
        bitrate_kbps: d.bitrate_kbps,
        tune: d.tune,
        quality_level: d.quality_level,
        rate_control: d.rate_control,
        codec: d.codec,
    }
}

/// Persist the "START A STREAM" form's current values, called whenever the
/// user changes a field in that form (mirroring `set_bandwidth_total`/`set_
/// bandwidth_floor`'s save-on-every-change pattern) — see `session::
/// save_start_stream_defaults`.
#[tauri::command]
pub fn set_start_stream_defaults(
    framerate: u32,
    bitrate_kbps: u32,
    tune: String,
    quality_level: String,
    rate_control: String,
    codec: String,
) -> Result<(), String> {
    let tune = dto::parse_tune_mode(&tune)?;
    let quality_level = dto::parse_quality_level(&quality_level)?;
    let rate_control = dto::parse_rate_control_mode(&rate_control)?;
    let codec = dto::parse_video_codec(&codec)?;
    AppState::set_start_stream_defaults(simple_screensharing_client::rooms::StartStreamDefaults {
        framerate,
        bitrate_kbps,
        tune,
        quality_level,
        rate_control,
        codec,
    });
    Ok(())
}

// ---------------------------------------------------------------------
// Stream Control
// ---------------------------------------------------------------------

// See `join_room`'s `Why:` note above — `start_video_share` spawns.
#[tauri::command]
pub fn start_video_share(managed: State<'_, Managed>, room_handle: u32) {
    tauri::async_runtime::block_on(async { managed.state.lock().unwrap().start_video_share(RoomHandle(room_handle)) });
}

/// "Share Screen"/"Share another screen" clicked: start the staged half of
/// the "Go live" flow (real capture, zero destinations — see
/// `session::AppState::stage_video_share`'s doc comment). Returns the new
/// capture's id, which the frontend threads through `go_live`/`stop_share`
/// for that card's "Go live"/"Cancel" buttons and uses to find its row in
/// `list_staged_captures`.
// See `join_room`'s `Why:` note above — `stage_video_share` spawns.
#[tauri::command]
pub fn stage_video_share(managed: State<'_, Managed>) -> u32 {
    tauri::async_runtime::block_on(async { managed.state.lock().unwrap().stage_video_share() }).0
}

/// "Go live" clicked on a staged card: attach its first destination into
/// `room_handle`, using whatever framerate/bitrate/tune/quality-level/
/// rate-control the staged card currently has set (via `set_framerate`/
/// `set_bitrate`/`set_tune`/`set_quality_level`/`set_rate_control` — same
/// commands `start_video_share` already relies on for this, see
/// `AppState::go_live`'s doc comment).
// See `join_room`'s `Why:` note above — `go_live` spawns
// (`CaptureHandle::spawn_add_destination`).
#[tauri::command]
pub fn go_live(managed: State<'_, Managed>, capture_id: u32, room_handle: u32) {
    tauri::async_runtime::block_on(async {
        managed.state.lock().unwrap().go_live(CaptureSourceId(capture_id), RoomHandle(room_handle))
    });
}

/// Staged captures (real capture running, zero destinations — see
/// `dto::StagedCaptureRow`) for the "Share Screen"/"Share another screen"
/// staged card(s) currently open.
#[tauri::command]
pub fn list_staged_captures(managed: State<'_, Managed>) -> Vec<dto::StagedCaptureRow> {
    dto::staged_capture_rows(&managed.state.lock().unwrap())
}

/// See `join_room`'s `Why:` note above — `start_audio_share` spawns.
///
/// `forCaptureId` is the screen share this audio is being attached to (the
/// "Add audio" affordance lives on a share's own card now, not in a
/// standalone SHARE AUDIO card), so the resulting audio destination can be
/// rendered as a row inside that card. Mandatory, not `Option`: there is no
/// standalone-audio mode any more (see `session::AppState::start_audio_
/// share`'s doc comment for the "no audio-only streams, ever" invariant this
/// enforces), and returns an error rather than silently no-opping if
/// `for_capture_id` names a capture that isn't live yet — the frontend
/// should keep this button disabled/hidden until then (see `stream-
/// control.js`), but this is the backstop that makes it a hard rule instead
/// of a UI convention.
#[tauri::command]
pub fn start_audio_share(managed: State<'_, Managed>, room_handle: u32, for_capture_id: u32) -> Result<(), String> {
    tauri::async_runtime::block_on(async {
        managed.state.lock().unwrap().start_audio_share(RoomHandle(room_handle), CaptureSourceId(for_capture_id))
    })
}

/// Mute/unmute a running audio share in place — the Mute/Unmute toggle on
/// the screen share card's audio row. Returns the resulting muted state so
/// the button can relabel itself without a follow-up query.
#[tauri::command]
pub fn set_audio_share_muted(managed: State<'_, Managed>, capture_id: u32, muted: bool) -> bool {
    managed.state.lock().unwrap().set_audio_share_muted(CaptureSourceId(capture_id), muted)
}

// See `join_room`'s `Why:` note above — `stop_share` spawns.
#[tauri::command]
pub fn stop_share(managed: State<'_, Managed>, capture_id: u32) {
    tauri::async_runtime::block_on(async { managed.state.lock().unwrap().stop_share(CaptureSourceId(capture_id)) });
}

/// Lists currently-producing-audio applications. Deliberately bypasses
/// `AppState::refresh_audio_sources`/`poll_audio_sources` (the egui
/// frontend's fire-then-poll-next-frame pattern) since a Tauri command can
/// just `.await` the blocking listing directly and hand the result straight
/// back — no poll needed for a call that already has a natural async return
/// point.
#[tauri::command]
pub async fn list_audio_sources() -> Result<Vec<AudioSourceDto>, String> {
    simple_screensharing_client::session::list_audio_sources_async()
        .await
        .map(|sources| sources.into_iter().map(|s| AudioSourceDto { id: s.id, name: s.name }).collect())
}

#[tauri::command]
pub fn select_audio_source(managed: State<'_, Managed>, id: u32, name: String) {
    managed.state.lock().unwrap().selected_audio_source =
        Some(simple_screensharing_client::audio::AudioSource { id, name, serial: None });
}

/// Starts a non-announcing per-candidate level tap for the "Add
/// audio"/"Switch" dropdown's meters. Returns the tap id the frontend
/// polls/stops with; `Err` (e.g. the source vanished, or has no
/// `object.serial` yet) just means that one dropdown entry's meter stays
/// unlit.
///
/// `Why the tap is built before `managed.state` is locked:` see
/// `session::build_audio_level_tap`'s doc comment — building the GStreamer
/// pipeline under the `AppState` mutex let one slow candidate block every
/// other command in the app.
#[tauri::command]
pub async fn start_audio_level_tap(managed: State<'_, Managed>, source_id: u32) -> Result<u32, String> {
    let handle = simple_screensharing_client::session::build_audio_level_tap(source_id).await?;
    Ok(managed.state.lock().unwrap().insert_audio_level_tap(handle))
}

/// Polls a live level tap's newest reading (0.0-1.0), or `None` once it's
/// been stopped/never existed — the dropdown's ~100ms poll loop, matching
/// the backend's own `LEVEL_INTERVAL_NS` cadence (see `audio.rs`).
#[tauri::command]
pub fn poll_audio_level_tap(managed: State<'_, Managed>, tap_id: u32) -> Option<f32> {
    managed.state.lock().unwrap().audio_level_tap_level(tap_id)
}

// See `join_room`'s `Why:` note above — `stop_audio_level_tap` spawns
// (`AudioLevelTapHandle::stop` is handed off to a `tokio::spawn`ed task the
// same way `stop_share` hands off `AudioCaptureHandle::stop`), so this needs
// the same `block_on` wrapper to have a runtime to spawn onto. Missing this
// wrapper panicked the whole request thread the first time this was wired
// up through `api_server.rs`'s plain-thread HTTP dispatch (confirmed live:
// "socket hang up" on the Electron side, `tokio::spawn` called with no
// runtime entered on the Rust side) — fixed here rather than reintroduced.
#[tauri::command]
pub fn stop_audio_level_tap(managed: State<'_, Managed>, tap_id: u32) {
    tauri::async_runtime::block_on(async { managed.state.lock().unwrap().stop_audio_level_tap(tap_id) });
}

#[tauri::command]
pub fn set_bitrate(managed: State<'_, Managed>, kbps: u32) {
    managed.state.lock().unwrap().bitrate_kbps = kbps;
}

#[tauri::command]
pub fn set_framerate(managed: State<'_, Managed>, fps: u32) {
    managed.state.lock().unwrap().framerate = fps;
}

/// Process-wide encoder tuning knobs for the *next* video capture started —
/// same "applied before `start_video_share`, not a per-call argument" shape
/// as `set_bitrate`/`set_framerate` above, set by the "START A STREAM"
/// form's Preset/Advanced-tuning controls (`stream-control.js`) right before
/// it invokes `start_video_share`. See `AppState::tune`'s doc comment.
#[tauri::command]
pub fn set_tune(managed: State<'_, Managed>, tune: String) -> Result<(), String> {
    let tune = dto::parse_tune_mode(&tune)?;
    managed.state.lock().unwrap().tune = tune;
    Ok(())
}

#[tauri::command]
pub fn set_quality_level(managed: State<'_, Managed>, quality_level: String) -> Result<(), String> {
    let quality_level = dto::parse_quality_level(&quality_level)?;
    managed.state.lock().unwrap().quality_level = quality_level;
    Ok(())
}

#[tauri::command]
pub fn set_rate_control(managed: State<'_, Managed>, rate_control: String) -> Result<(), String> {
    let rate_control = dto::parse_rate_control_mode(&rate_control)?;
    managed.state.lock().unwrap().rate_control = rate_control;
    Ok(())
}

/// GOP/keyframe interval (ms), process-wide for the *next* video capture
/// started — same shape as `set_tune`/`set_quality_level`/`set_rate_control`
/// above, driven by the Preset picker (`components.js`'s `ENCODER_PRESETS`;
/// the Gaming preset is the only one that sets this away from
/// `capture::DEFAULT_GOP_MILLIS`). Deliberately **not** persisted to
/// `settings.toml` like `set_start_stream_defaults`'s fields — a plain
/// process-wide session default that resets to the protocol-spec 2s figure
/// on every launch, since a shorter GOP is a preset-specific tradeoff, not a
/// standalone setting worth remembering across restarts on its own.
#[tauri::command]
pub fn set_gop_millis(managed: State<'_, Managed>, gop_millis: u32) {
    managed.state.lock().unwrap().gop_millis = gop_millis;
}

/// Process-wide codec (H.264/H.265/AV1) for the *next* video capture started
/// — same shape as `set_tune`/`set_quality_level`/`set_rate_control` above.
/// The frontend should only ever send a value from `supported_codecs`'s
/// current result for this machine's backend (see that command's doc
/// comment) — this command does not itself re-validate against the backend,
/// since [`simple_screensharing_client::capture::EncoderBackend::actual_codec`]
/// already makes an unsupported combination degrade safely (to H.264)
/// rather than fail, and re-validating here would just duplicate that
/// gating in a second place.
#[tauri::command]
pub fn set_codec(managed: State<'_, Managed>, codec: String) -> Result<(), String> {
    let codec = dto::parse_video_codec(&codec)?;
    managed.state.lock().unwrap().codec = codec;
    Ok(())
}

/// Which codecs (`"h264"`/`"h265"`/`"av1"`) are actually usable on this
/// machine's current encoder backend — the mechanism Stage 2's frontend
/// codec selector should call to grey out/disable unavailable options. See
/// `capture::supported_codecs`'s doc comment for why this is a static
/// function of the backend rather than a live per-codec hardware probe.
#[tauri::command]
pub fn supported_codecs() -> Vec<&'static str> {
    // No `managed` parameter (and no lock) on purpose: `AppState::supported_codecs`
    // doesn't read any session state, and this command is also called while a
    // slow command holds the lock.
    AppState::supported_codecs().iter().map(|c| dto::video_codec_str(*c)).collect()
}

#[tauri::command]
pub fn list_active_streams(managed: State<'_, Managed>) -> Vec<DestinationRow> {
    dto::destination_rows(&managed.state.lock().unwrap())
}

/// "Also send to…" picked on a destination row: fan out an already-running
/// capture into one more room (see `superdoc/features/stream-control.md`).
// See `join_room`'s `Why:` note above — `add_destination` spawns
// (`CaptureHandle::spawn_add_destination`).
#[tauri::command]
pub fn start_destination(managed: State<'_, Managed>, capture_id: u32, room_handle: u32) {
    tauri::async_runtime::block_on(async {
        managed.state.lock().unwrap().add_destination(CaptureSourceId(capture_id), RoomHandle(room_handle), PendingRole::FanOut)
    });
}

/// "Transfer to…" picked on a destination row: make-before-break move to
/// another room.
// See `join_room`'s `Why:` note above — `transfer_destination` calls
// `add_destination`, which spawns.
#[tauri::command]
pub fn transfer_destination(managed: State<'_, Managed>, destination_id: u32, target_room: u32) {
    tauri::async_runtime::block_on(async {
        managed.state.lock().unwrap().transfer_destination(DestinationId(destination_id), RoomHandle(target_room))
    });
}

/// "Apply" clicked on a destination row's settings edit.
///
/// `tune`/`quality_level`/`rate_control` are the three testing-oriented
/// encoder tuning knobs (`capture::TuneMode`/`QualityLevel`/
/// `RateControlMode`) — snake_case strings parsed via
/// `dto::parse_tune_mode`/`parse_quality_level`/`parse_rate_control_mode`,
/// the same convention `set_bandwidth_mode` uses. Any change to one of the
/// three (like a resolution/framerate/backend change) is applied as a
/// `ChangeTier::BranchSwap` by `AppState::apply_destination_edit` →
/// `capture::destination::classify_change` — never a live property write.
#[tauri::command]
pub fn edit_destination(
    managed: State<'_, Managed>,
    destination_id: u32,
    scale_percent: u32,
    framerate: u32,
    bitrate_kbps: u32,
    tune: String,
    quality_level: String,
    rate_control: String,
    gop_millis: u32,
    codec: String,
) -> Result<(), String> {
    let tune = dto::parse_tune_mode(&tune)?;
    let quality_level = dto::parse_quality_level(&quality_level)?;
    let rate_control = dto::parse_rate_control_mode(&rate_control)?;
    let codec = dto::parse_video_codec(&codec)?;

    managed.state.lock().unwrap().edit_destination(
        DestinationId(destination_id),
        DestEdit { scale_percent, framerate, bitrate_kbps, tune, quality_level, rate_control, gop_millis, codec },
    );
    Ok(())
}

// See `join_room`'s `Why:` note above — `stop_destination` spawns.
#[tauri::command]
pub fn stop_destination(managed: State<'_, Managed>, destination_id: u32) {
    tauri::async_runtime::block_on(async { managed.state.lock().unwrap().stop_destination(DestinationId(destination_id)) });
}

/// "Retry" clicked on a Failed destination card. A dedicated command rather
/// than the front-end doing `stop_destination` + `start_destination` back to
/// back (which is what this used to be): see `AppState::retry_destination`'s
/// `Why:` note — that pairing both lost the destination's tuned encoder
/// settings and, for a capture's last destination, could tear down the whole
/// capture instead of just the one failed branch.
// See `join_room`'s `Why:` note above — `retry_destination` spawns.
#[tauri::command]
pub fn retry_destination(managed: State<'_, Managed>, destination_id: u32) {
    tauri::async_runtime::block_on(async { managed.state.lock().unwrap().retry_destination(DestinationId(destination_id)) });
}

#[tauri::command]
pub fn set_bandwidth_mode(managed: State<'_, Managed>, mode: String) -> Result<(), String> {
    let mode = dto::parse_bandwidth_mode(&mode)?;
    managed.state.lock().unwrap().set_bandwidth_mode(mode);
    Ok(())
}

#[tauri::command]
pub fn set_bandwidth_total(managed: State<'_, Managed>, total_kbps: u32) {
    managed.state.lock().unwrap().set_bandwidth_total(total_kbps);
}

#[tauri::command]
pub fn set_bandwidth_floor(managed: State<'_, Managed>, floor_kbps: u32) {
    managed.state.lock().unwrap().set_bandwidth_floor(floor_kbps);
}

// ---------------------------------------------------------------------
// View
// ---------------------------------------------------------------------

// See `join_room`'s `Why:` note above — `RoomSession::toggle_watch` spawns
// (subscribe/unsubscribe control messages).
//
// Also starts/stops this stream's real video-frame delivery
// (`video_server.rs`) in lockstep with the watch itself: `RoomSession::
// toggle_watch` inserts (or removes) a `WatchEntry` synchronously before
// returning, so right after it runs, `room.watches` having an entry for this
// key *is* "we just started watching" and not having one *is* "we just
// stopped" — no separate state to keep in sync. Returns the relay URL
// `view.js` should open (see `video_server.rs`'s module doc for the wire
// format) when a watch just started, or `None` when it just stopped (or the
// room doesn't exist).
//
// `owner` is a hex string, not `u64` — see `dto::hex_u64`'s doc comment for
// why a bare `u64` command parameter is exactly the bug that made the first
// end-to-end test of this command produce a decoder that never saw a frame.
#[tauri::command]
pub fn toggle_watch(managed: State<'_, Managed>, room_handle: u32, owner: String, stream_id: u8) -> Result<Option<String>, String> {
    let owner = dto::parse_owner(&owner)?;
    Ok(tauri::async_runtime::block_on(async {
        // If this call is starting a new watch, wire up a plain
        // `std::sync::mpsc` channel and hand `WatchEntry::raw_tx` its
        // sending half *before* releasing the state lock, so the very next
        // `MediaFrame` the media-ingestion task sees for this stream is
        // already forwarded — see `WatchEntry::raw_tx`'s doc comment for why
        // this is a plain `mpsc::Sender` and not the `tokio::sync::mpsc`
        // used elsewhere.
        let raw_rx = {
            let mut state = managed.state.lock().unwrap();
            let room = state.rooms.get_mut(&RoomHandle(room_handle))?;
            room.toggle_watch(owner, stream_id);
            room.watches.lock().unwrap().get_mut(&(owner, stream_id)).map(|entry| {
                let (tx, rx) = std::sync::mpsc::channel();
                entry.raw_tx = Some(tx);
                rx
            })
        };
        let key = (room_handle, owner, stream_id);
        match raw_rx {
            Some(rx) => Some(managed.video_server.start_route(key, rx)),
            None => {
                managed.video_server.stop_route(key);
                None
            }
        }
    }))
}

/// Look up the relay URL for a stream that's already being watched (or
/// `None` if it isn't) — lets `view.js` rebuild its whole tile list (e.g. on
/// tab reload) without needing to have been the caller that started the
/// watch; see `toggle_watch`'s doc comment for the immediate-feedback path.
#[tauri::command]
pub fn stream_video_url(managed: State<'_, Managed>, room_handle: u32, owner: String, stream_id: u8) -> Result<Option<String>, String> {
    let owner = dto::parse_owner(&owner)?;
    Ok(managed.video_server.url_for((room_handle, owner, stream_id)))
}

// See `join_room`'s `Why:` note above — `RoomSession::toggle_listen` spawns.
#[tauri::command]
pub fn toggle_listen(managed: State<'_, Managed>, room_handle: u32, owner: String, stream_id: u8) -> Result<(), String> {
    let owner = dto::parse_owner(&owner)?;
    tauri::async_runtime::block_on(async {
        if let Some(room) = managed.state.lock().unwrap().rooms.get_mut(&RoomHandle(room_handle)) {
            room.toggle_listen(owner, stream_id);
        }
    });
    Ok(())
}

/// Set playback volume (0-200%) and mute state for `owner`'s audio.
/// `stream_id` may be the audio stream's own id or the video stream's id the
/// frontend is showing for that sender — resolved in the lib, see
/// `session::RoomSession::set_volume`.
#[tauri::command]
pub fn set_volume(managed: State<'_, Managed>, room_handle: u32, owner: String, stream_id: u8, volume_percent: u32, muted: bool) -> Result<(), String> {
    let owner = dto::parse_owner(&owner)?;
    managed.state.lock().unwrap().set_volume(RoomHandle(room_handle), owner, stream_id, volume_percent, muted);
    Ok(())
}

/// Read back `owner`'s current volume/mute state — the counterpart query to
/// `set_volume` (see `session::RoomSession::volume`). Two windows (View tab
/// and any pop-out) read it for a tile they haven't seen before; see
/// `frontend/scripts/view.js`'s `ensureVolumeSeeded` and `viewer.js`'s
/// matching copy.
#[tauri::command]
pub fn get_volume(managed: State<'_, Managed>, room_handle: u32, owner: String, stream_id: u8) -> Result<dto::VolumeStateDto, String> {
    let owner = dto::parse_owner(&owner)?;
    let v = managed.state.lock().unwrap().volume(RoomHandle(room_handle), owner, stream_id);
    Ok(dto::VolumeStateDto { volume_percent: v.volume_percent, muted: v.muted })
}

// `pop_out_stream` (a native Tauri `WebviewWindowBuilder` second window
// pointed at the full `index.html` app shell) was removed 2026-07-28: the
// window-in-its-own-OS-window feature it half-built was consolidated onto
// Electron's `BrowserWindow`-based pop-out mechanism instead
// (`electron/main.js`'s `openPopoutWindow` / `frontend/scripts/view.js`'s
// `popOutWatch`, both loading the lean `frontend/viewer.html` rather than
// the full shell). See `superdoc/features/viewer-window.md`'s 2026-07-28
// entry for the full rationale — in short, this command was a leftover
// from before the Tauri->Electron shell migration
// (`superdoc/ui/FRONTEND-SHELL.md`) that nothing in the frontend actually
// finished wiring up (`index.html`'s `?popout=1` query param was never
// read by any JS), while Electron's window management already had a real,
// working, per-stream multi-window mechanism (originally built for
// fullscreen/borderless) that this consolidation extended instead.

#[cfg(test)]
mod tests {
    use super::*;
    use simple_screensharing_client::rooms::RoomStore;
    // `Manager` (for `AppHandle`/`App::state()`) is only needed by test
    // helpers below — the one production call site that needed it
    // (`pop_out_stream`) was removed 2026-07-28; see this file's note where
    // that command used to be.
    use tauri::Manager;

    /// Serializes every test that touches the process-global `XDG_CONFIG_HOME`
    /// env var. `cargo test` runs tests in parallel threads within one
    /// process, so without this lock two tests calling
    /// `isolated_config_home()` concurrently can stomp each other's
    /// `XDG_CONFIG_HOME` mid-test, leaking one test's room/settings data into
    /// another's.
    static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Bundles the lock guard together with the `TempDir` so both stay alive
    /// for the caller's binding lifetime: the `TempDir` must not be dropped
    /// (or its directory disappears out from under a still-running test),
    /// and the guard must not be released (or another thread's test can set
    /// `XDG_CONFIG_HOME` out from under this one) until the test is done.
    struct IsolatedConfigHome {
        _guard: std::sync::MutexGuard<'static, ()>,
        _dir: tempfile::TempDir,
    }

    fn isolated_config_home() -> IsolatedConfigHome {
        // Poisoning only happens if an earlier test panicked while holding
        // this lock; that earlier test's failure is already reported, so
        // recover the lock rather than cascading the panic into every test
        // that runs after it.
        let guard = TEST_ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        // SAFETY: held under `TEST_ENV_LOCK` for the guard's whole lifetime,
        // so no other test in this module can read/write `XDG_CONFIG_HOME`
        // concurrently. `dirs::config_dir()` is only consulted lazily, at the
        // point `RoomStore`/`AppSettings` load/save — never cached at
        // process startup — so setting it before use is sufficient.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", dir.path());
        }
        IsolatedConfigHome { _guard: guard, _dir: dir }
    }

    #[tokio::test]
    async fn list_saved_rooms_reflects_a_real_on_disk_room_store() {
        let _guard = isolated_config_home();
        let mut store = RoomStore::default();
        store.upsert("Movie Night".to_string(), "server|00112233|deadbeef".to_string());
        store.save().expect("save room store");

        let mut state = AppState::default();
        assert_eq!(state.room_store.rooms.len(), 1, "AppState::default() should have loaded the fixture store");

        let rows = list_saved_rooms_impl(&state);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "Movie Night");
        assert_eq!(rows[0].state, crate::dto::RoomRowState::Disconnected);
        assert_eq!(rows[0].room_handle, None);

        // join_room against an unreachable/invalid join string still
        // exercises the full command delegation path (start_connect spawning
        // onto the runtime) and resolves to a real, reportable failure rather
        // than hanging or panicking.
        let result = join_room_impl(&mut state, "not-a-valid-join-string".to_string(), String::new(), String::new());
        assert!(result.is_ok(), "start_connect itself only fails on a dead local endpoint: {result:?}");
        assert!(state.pending_connect.is_some());
    }

    /// Reproduces the reported bug end-to-end at the command layer: a saved,
    /// disconnected room's `forget_room` call must remove it from the
    /// on-disk `RoomStore` file immediately, not just from in-memory state,
    /// so a later `RoomStore::load()` (e.g. triggered by joining another
    /// room) doesn't resurrect it.
    #[test]
    fn forget_room_persists_the_removal_to_disk() {
        let _guard = isolated_config_home();
        let mut store = RoomStore::default();
        store.upsert("Movie Night".to_string(), "server|00112233|deadbeef".to_string());
        store.upsert("Gaming Night".to_string(), "server|44556677|c0ffee".to_string());
        store.save().expect("save room store");

        let mut state = AppState::default();
        assert_eq!(state.room_store.rooms.len(), 2);

        let result = forget_room_impl(&mut state, "server|00112233|deadbeef".to_string());
        assert!(result.is_ok(), "forget_room should succeed: {result:?}");

        // In-memory view no longer has it...
        assert_eq!(state.room_store.rooms.len(), 1);
        assert_eq!(state.room_store.rooms[0].name, "Gaming Night");

        // ...and, crucially, neither does a fresh load from disk — this is
        // exactly what the bug report described: the room reappearing once
        // another action (e.g. joining a different room) reloads/reconciles
        // from the on-disk file.
        let reloaded = RoomStore::load().expect("reload room store from disk");
        assert_eq!(reloaded.rooms.len(), 1, "deletion must be persisted to disk immediately");
        assert_eq!(reloaded.rooms[0].name, "Gaming Night");
    }

    #[test]
    fn leave_needs_confirm_is_false_with_no_live_room() {
        let state = AppState::default();
        assert!(!state.leave_needs_confirm(RoomHandle(0)));
    }

    /// Reproduces the real-app crash at `session.rs`'s `start_connect`: a
    /// plain (non-`async`) `#[tauri::command]` like `join_room` is invoked by
    /// Tauri directly on whatever thread delivered the IPC message — on
    /// Linux that's the webkit2gtk `register_uri_scheme` callback — with no
    /// Tokio reactor entered (only `async fn` commands get dispatched via
    /// `tauri::async_runtime::spawn`, which enters one). This calls the real
    /// `#[tauri::command] join_room` wrapper (not just `join_room_impl`,
    /// which only exercises `AppState` and would miss a bug that's actually
    /// in the wrapper), via a `tauri::test` mock app so a real `State<'_,
    /// Managed>` exists to pass it — `State`'s field is private, so it can't
    /// be built by hand. This is a plain `#[test]` (no `#[tokio::test]`)
    /// precisely to recreate that "no runtime entered" caller context.
    #[test]
    fn join_room_outside_a_tokio_runtime_does_not_panic() {
        let _guard = isolated_config_home();
        // `Managed::new()` itself needs a reactor too (see `main.rs`'s own
        // `Why:` note) — build it inside a throwaway runtime, same as
        // `main.rs` does with Tauri's real one, then drop that runtime
        // before calling `join_room` so the command wrapper is the only
        // thing left responsible for entering one.
        let rt = tokio::runtime::Runtime::new().expect("build throwaway runtime");
        let managed = rt.block_on(async { Managed::new() });
        drop(rt);

        let app = tauri::test::mock_builder()
            .manage(managed)
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .expect("build mock tauri app");

        // Pre-fix, `start_connect`'s raw `tokio::spawn` panics here with
        // "there is no reactor running, must be called from the context of
        // a Tokio 1.x runtime" — fatal in the real app because it happens
        // inside a non-unwindable FFI callback.
        let result = join_room(app.state::<Managed>(), "not-a-valid-join-string".to_string(), String::new(), String::new());
        assert!(result.is_ok(), "join_room should not fail outside a Tokio runtime context: {result:?}");
    }

    /// Calls `f` on its own thread and waits up to `timeout` for it to
    /// return. Used below to *detect* a hang: an in-process function call
    /// that genuinely never returns can't otherwise be interrupted, so this
    /// is how "this command call never resolved" gets turned into an
    /// observable result instead of blocking the whole test process forever.
    /// The spawned thread is deliberately leaked on a timeout (there's no
    /// way to cancel a blocked native thread) — that's fine for a diagnostic
    /// harness whose whole point is to leave the process alive afterward for
    /// `gdb -p <pid> -batch -ex "thread apply all bt"` to inspect.
    fn call_with_timeout<F, T>(label: &str, timeout: std::time::Duration, f: F) -> Option<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let (tx, rx) = std::sync::mpsc::channel();
        let start = std::time::Instant::now();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        match rx.recv_timeout(timeout) {
            Ok(v) => {
                eprintln!("[hang_repro] {label} returned after {:?}", start.elapsed());
                Some(v)
            }
            Err(_) => {
                eprintln!("[hang_repro] !!! HANG DETECTED: {label} did not return within {timeout:?}");
                None
            }
        }
    }

    /// Stand-in for `driver::tick` that works against `tauri::test`'s
    /// `MockRuntime` — `driver::tick`/`driver::spawn` are hard-wired to the
    /// real `AppHandle<Wry<...>>` (they call `app_handle.emit(...)`, which
    /// this diagnostic harness doesn't need), so this reimplements just the
    /// state-polling half: the same `Managed::state` lock, the same
    /// `AppState` poll methods, in the same order, on the same 250ms cadence
    /// — everything that actually matters for reproducing a lock/scheduling
    /// race, minus the Tauri event emission a mock app has no listener for
    /// anyway. See `driver.rs`'s `tick` for the real (annotated) version this
    /// mirrors.
    fn test_tick(managed: &Managed) {
        let mut state = managed.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let _ = state.poll_connect_result();
        state.poll_capture_states();
        state.poll_pending_destinations();
        state.poll_destination_edits();
        state.poll_audio_sources();
        state.drain_events(&managed.egui_ctx);
        let _ = managed.egui_ctx.tex_manager().write().take_delta();
        // See `driver::tick`'s step 3.5 — this mock harness needs the same
        // stale-watch/route pruning the real driver loop does, or a test
        // exercising a watched stream that stops would never see it.
        crate::driver::prune_stale_watches(&mut state, managed);
        state.apply_bandwidth_allocation();
    }

    /// Polls `f` every 100ms until it returns `Some` or `timeout` elapses.
    fn wait_for<T>(timeout: std::time::Duration, mut f: impl FnMut() -> Option<T>) -> Option<T> {
        let start = std::time::Instant::now();
        loop {
            if let Some(v) = f() {
                return Some(v);
            }
            if start.elapsed() > timeout {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    /// Manual diagnostic harness for the "repeated Stop→Share-Screen restart
    /// cycles back to back with no pause between attempts ... permanently
    /// wedges the backend" bug — see `superdoc/ui/FRONTEND-SHELL.md`'s
    /// 2026-07-27 dated entry for the original report.
    ///
    /// Not a normal CI test: it needs a real relay server reachable at
    /// `HANG_REPRO_RELAY` (`host:port`, default `127.0.0.1:4477`) and a real
    /// xdg-desktop-portal screencast permission granted by hand for *every*
    /// iteration (`capture.rs`'s `set_persist_mode(PersistMode::DoNot)` means
    /// there is no way around a fresh picker each time). Run with:
    ///
    /// ```text
    /// HANG_REPRO_RELAY=127.0.0.1:4477 HANG_REPRO_CYCLES=6 \
    ///   cargo test --release -p simple-screensharing-client-tauri \
    ///   -- --ignored --nocapture hang_repro_stop_share_cycles
    /// ```
    ///
    /// Every command call goes through [`call_with_timeout`] so a hang is
    /// *detected* (reported, with the process left alive) rather than
    /// silently blocking the test forever — at that point, attach with
    /// `gdb -p <pid> -batch -ex "thread apply all bt"` from another shell to
    /// see exactly what every thread is doing.
    ///
    /// This calls the exact same `#[tauri::command]` wrapper functions the
    /// real frontend's `invoke()` calls reach (not just the `_impl` helpers),
    /// each from its own freshly spawned thread — the same "arbitrary
    /// external, non-Tokio-runtime thread" context a real IPC dispatch uses
    /// for a plain `fn` command (see `join_room`'s `Why:` note above) — while
    /// `driver::spawn`'s real 250ms poll loop runs concurrently in the
    /// background, exactly as in the real app.
    #[test]
    #[ignore]
    fn hang_repro_stop_share_cycles() {
        let _guard = isolated_config_home();
        // Unlike `join_room_outside_a_tokio_runtime_does_not_panic` above,
        // this test needs a real, *working* QUIC connection all the way
        // through — building `Managed` inside a throwaway runtime and
        // dropping that runtime (that test's pattern) leaves
        // `AppState.endpoint` bound to a reactor that's already gone,
        // failing every real connection with "endpoint stopping". Use
        // `tauri::async_runtime::block_on`, exactly like `main.rs`'s real
        // startup path, so the endpoint is bound inside Tauri's one
        // persistent shared runtime instead of a throwaway one.
        let managed = tauri::async_runtime::block_on(async { Managed::new() });

        let app = tauri::test::mock_builder()
            .manage(managed)
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .expect("build mock tauri app");

        let poller_handle = app.handle().clone();
        tauri::async_runtime::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(250));
            loop {
                interval.tick().await;
                test_tick(&poller_handle.state::<Managed>());
            }
        });

        let relay = std::env::var("HANG_REPRO_RELAY").unwrap_or_else(|_| "127.0.0.1:4477".to_string());
        let cycles: u32 = std::env::var("HANG_REPRO_CYCLES").ok().and_then(|s| s.parse().ok()).unwrap_or(6);

        // A throwaway, non-cryptographic but unique-enough room id/key —
        // this is a disposable test room on a local relay, not a real
        // secret. `RoomId = [u8; 16]`, the room key is AES-256 (32 bytes).
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let mut room_id = [0u8; 16];
        let mut room_key = [0u8; 32];
        for (i, b) in room_id.iter_mut().enumerate() {
            *b = (seed >> (i % 16 * 4)) as u8 ^ (i as u8);
        }
        for (i, b) in room_key.iter_mut().enumerate() {
            *b = (seed >> (i % 16 * 4)) as u8 ^ (i as u8).wrapping_mul(31);
        }
        let join_string = simple_screensharing_client::net::format_join_string(&relay, &room_id, &room_key);

        eprintln!("[hang_repro] joining {join_string}");
        let handle = app.handle().clone();
        let jr = join_room(handle.state::<Managed>(), join_string, "HangReproSharer".to_string(), String::new());
        assert!(jr.is_ok(), "join_room returned an error: {jr:?}");

        let room_handle = wait_for(std::time::Duration::from_secs(15), || {
            let managed = app.state::<Managed>();
            let state = managed.state.lock().unwrap();
            let status_str = match &state.status {
                simple_screensharing_client::session::Status::Idle => "Idle".to_string(),
                simple_screensharing_client::session::Status::Connecting => "Connecting".to_string(),
                simple_screensharing_client::session::Status::Error(e) => format!("Error({e})"),
            };
            eprintln!(
                "[hang_repro] waiting for connect: status={status_str} pending_connect={} rooms={}",
                state.pending_connect.is_some(),
                state.rooms.len()
            );
            state.rooms.keys().next().copied()
        })
        .expect("room should connect to the real relay within 15s — is HANG_REPRO_RELAY reachable?");
        eprintln!("[hang_repro] joined room {room_handle:?}");

        // `Why this loop tolerates an individual cycle being slow or failing
        // outright, rather than treating that as the bug:` GStreamer/PipeWire
        // genuinely can take a while (or fail once) to settle a brand-new
        // capture started immediately after the previous one's teardown —
        // that's real-world churn, not by itself "the backend is
        // permanently wedged" (the actual reported bug). The property this
        // harness actually needs to prove is the one from the report: that
        // every `#[tauri::command]` call — regardless of what any individual
        // capture attempt is doing — keeps resolving quickly, cycle after
        // cycle. So every command call here still goes through
        // `call_with_timeout` and any timeout is still a hard failure; only
        // "this particular capture didn't reach Active" is treated as
        // non-fatal and retried on the next cycle.
        let mut activated = 0u32;
        for cycle in 0..cycles {
            eprintln!("[hang_repro] === cycle {cycle}/{cycles}: start_video_share ===");
            let h = app.handle().clone();
            let started = call_with_timeout("start_video_share", std::time::Duration::from_secs(5), move || {
                start_video_share(h.state::<Managed>(), room_handle.0)
            });
            assert!(started.is_some(), "start_video_share itself hung — see thread dump");

            eprintln!("[hang_repro] waiting for the capture to go Active (up to 30s) ...");
            let capture_id = wait_for(std::time::Duration::from_secs(30), || {
                let managed = app.state::<Managed>();
                let state = managed.state.lock().unwrap();
                state.captures.iter().find_map(|(id, c)| {
                    matches!(c.status, simple_screensharing_client::session::ShareStatus::Active(_)).then_some(*id)
                })
            });

            // Every cycle — active capture or not — must still leave the
            // backend responsive. This is the actual regression check: prove
            // via a real command round-trip that nothing has wedged, not just
            // that the loop kept running.
            let h = app.handle().clone();
            let sanity =
                call_with_timeout("list_saved_rooms (per-cycle responsiveness check)", std::time::Duration::from_secs(5), move || {
                    list_saved_rooms(h.state::<Managed>())
                });
            assert!(
                sanity.is_some(),
                "cycle {cycle}: the backend is wedged — list_saved_rooms did not resolve. This is the actual bug. \
                 pid {} is left running for `gdb -p <pid> -batch -ex \"thread apply all bt\"` (attach quickly).",
                std::process::id()
            );

            let Some(capture_id) = capture_id else {
                eprintln!(
                    "[hang_repro] cycle {cycle}: capture never went Active within 30s, but the backend is still \
                     responsive (list_saved_rooms OK) — real-world GStreamer/portal churn slowness, not the \
                     reported permanent-wedge bug. Moving on to the next cycle."
                );
                continue;
            };
            activated += 1;
            eprintln!("[hang_repro] capture {capture_id:?} is Active");

            eprintln!(
                "[hang_repro] === cycle {cycle}: stop_share, then IMMEDIATELY start_video_share again next \
                 iteration — no pause (this is the reported trigger) ==="
            );
            let h = app.handle().clone();
            let stopped = call_with_timeout("stop_share", std::time::Duration::from_secs(5), move || {
                stop_share(h.state::<Managed>(), capture_id.0)
            });
            assert!(stopped.is_some(), "stop_share itself hung — see thread dump");
            // Deliberately no sleep here — the next loop iteration's
            // start_video_share fires immediately, while stop_share's spawned
            // teardown task may still be in flight. That's the exact
            // "no pause between attempts" repro condition from the report.
        }

        eprintln!(
            "[hang_repro] stress cycles done ({activated}/{cycles} captures actually went Active) — final sanity \
             check: does the backend still respond at all?"
        );
        let h = app.handle().clone();
        let sanity = call_with_timeout("list_saved_rooms (post-stress sanity check)", std::time::Duration::from_secs(5), move || {
            list_saved_rooms(h.state::<Managed>())
        });
        assert!(sanity.is_some(), "backend is permanently wedged: list_saved_rooms did not resolve after the stress cycles");
        eprintln!("[hang_repro] backend still responds after {cycles} cycles — no hang this run.");
    }

    /// Same repro-harness shape as [`hang_repro_stop_share_cycles`], for the
    /// *other* reported trigger: rapidly toggling bandwidth mode many times
    /// against one live share, then Stop. See that test's doc comment for
    /// run instructions/prerequisites (same `HANG_REPRO_RELAY` env var).
    #[test]
    #[ignore]
    fn hang_repro_bandwidth_toggle_then_stop() {
        let _guard = isolated_config_home();
        let managed = tauri::async_runtime::block_on(async { Managed::new() });

        let app = tauri::test::mock_builder()
            .manage(managed)
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .expect("build mock tauri app");

        let poller_handle = app.handle().clone();
        tauri::async_runtime::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(250));
            loop {
                interval.tick().await;
                test_tick(&poller_handle.state::<Managed>());
            }
        });

        let relay = std::env::var("HANG_REPRO_RELAY").unwrap_or_else(|_| "127.0.0.1:4477".to_string());
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let mut room_id = [0u8; 16];
        let mut room_key = [0u8; 32];
        for (i, b) in room_id.iter_mut().enumerate() {
            *b = (seed >> (i % 16 * 4)) as u8 ^ (i as u8) ^ 0x55;
        }
        for (i, b) in room_key.iter_mut().enumerate() {
            *b = (seed >> (i % 16 * 4)) as u8 ^ (i as u8).wrapping_mul(31) ^ 0x55;
        }
        let join_string = simple_screensharing_client::net::format_join_string(&relay, &room_id, &room_key);

        eprintln!("[hang_repro2] joining {join_string}");
        let handle = app.handle().clone();
        let jr = join_room(handle.state::<Managed>(), join_string, "HangReproSharer2".to_string(), String::new());
        assert!(jr.is_ok(), "join_room returned an error: {jr:?}");

        let room_handle = wait_for(std::time::Duration::from_secs(15), || {
            app.state::<Managed>().state.lock().unwrap().rooms.keys().next().copied()
        })
        .expect("room should connect to the real relay within 15s — is HANG_REPRO_RELAY reachable?");
        eprintln!("[hang_repro2] joined room {room_handle:?}");

        let h = app.handle().clone();
        let started = call_with_timeout("start_video_share", std::time::Duration::from_secs(5), move || {
            start_video_share(h.state::<Managed>(), room_handle.0)
        });
        assert!(started.is_some(), "start_video_share itself hung");

        let capture_id = wait_for(std::time::Duration::from_secs(30), || {
            let managed = app.state::<Managed>();
            let state = managed.state.lock().unwrap();
            state.captures.iter().find_map(|(id, c)| {
                matches!(c.status, simple_screensharing_client::session::ShareStatus::Active(_)).then_some(*id)
            })
        })
        .expect("capture should go Active within 30s to set up this repro");
        eprintln!("[hang_repro2] capture {capture_id:?} is Active — starting rapid bandwidth-mode toggle stress (15x, no pause)");

        for i in 0..15 {
            let mode = if i % 2 == 0 { "absolute" } else { "per_stream" };
            let h = app.handle().clone();
            let mode_owned = mode.to_string();
            let result = call_with_timeout(&format!("set_bandwidth_mode({mode})"), std::time::Duration::from_secs(5), move || {
                set_bandwidth_mode(h.state::<Managed>(), mode_owned)
            });
            assert!(result.is_some(), "set_bandwidth_mode hung on toggle {i} — see thread dump");
        }

        eprintln!("[hang_repro2] toggle stress done — now Stop, immediately");
        let h = app.handle().clone();
        let stopped = call_with_timeout("stop_share", std::time::Duration::from_secs(5), move || {
            stop_share(h.state::<Managed>(), capture_id.0)
        });
        assert!(stopped.is_some(), "stop_share itself hung — see thread dump");

        eprintln!("[hang_repro2] final sanity check: does the backend still respond?");
        let h = app.handle().clone();
        let sanity = call_with_timeout("list_saved_rooms (post-stress sanity check)", std::time::Duration::from_secs(5), move || {
            list_saved_rooms(h.state::<Managed>())
        });
        assert!(sanity.is_some(), "backend is permanently wedged after bandwidth-toggle-then-Stop");
        eprintln!("[hang_repro2] backend still responds — no hang this run.");
    }

    /// DIAGNOSTIC harness for the "Stop genuinely halts the pipeline but the
    /// destination is never removed from app state" bug — see
    /// `superdoc/ui/FRONTEND-SHELL.md`'s 2026-07-27 dated entry for the
    /// original report. Unlike `hang_repro_stop_share_cycles` (which only
    /// checks that the `stop_share` *command call itself* returns quickly —
    /// it fires a `tokio::spawn` and returns immediately, so it always
    /// returns fast even when the bug is present), this test checks the
    /// thing the bug report actually complains about: that the capture (and
    /// therefore its destination) is actually *reaped* out of
    /// `AppState::captures` — i.e. `ShareStatus::Stopping` -> gone — within a
    /// bounded, human-reasonable time after `stop_share` is called, not just
    /// that the command returned.
    ///
    /// Same real-relay/real-portal prerequisites as `hang_repro_stop_share_cycles`
    /// (see that test's doc comment). Run with:
    /// ```text
    /// HANG_REPRO_RELAY=127.0.0.1:4433 \
    ///   cargo test --release -p simple-screensharing-client-tauri \
    ///   -- --ignored --nocapture stop_destination_actually_clears_from_state
    /// ```
    #[test]
    #[ignore]
    fn stop_destination_actually_clears_from_state() {
        let _guard = isolated_config_home();
        let managed = tauri::async_runtime::block_on(async { Managed::new() });

        let app = tauri::test::mock_builder()
            .manage(managed)
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .expect("build mock tauri app");

        let poller_handle = app.handle().clone();
        tauri::async_runtime::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(250));
            loop {
                interval.tick().await;
                test_tick(&poller_handle.state::<Managed>());
            }
        });

        let relay = std::env::var("HANG_REPRO_RELAY").unwrap_or_else(|_| "127.0.0.1:4477".to_string());
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let mut room_id = [0u8; 16];
        let mut room_key = [0u8; 32];
        for (i, b) in room_id.iter_mut().enumerate() {
            *b = (seed >> (i % 16 * 4)) as u8 ^ (i as u8) ^ 0xAA;
        }
        for (i, b) in room_key.iter_mut().enumerate() {
            *b = (seed >> (i % 16 * 4)) as u8 ^ (i as u8).wrapping_mul(31) ^ 0xAA;
        }
        let join_string = simple_screensharing_client::net::format_join_string(&relay, &room_id, &room_key);

        eprintln!("[stop_reap_repro] joining {join_string}");
        let handle = app.handle().clone();
        let jr = join_room(handle.state::<Managed>(), join_string, "StopReaproSharer".to_string(), String::new());
        assert!(jr.is_ok(), "join_room returned an error: {jr:?}");

        let room_handle = wait_for(std::time::Duration::from_secs(15), || {
            app.state::<Managed>().state.lock().unwrap().rooms.keys().next().copied()
        })
        .expect("room should connect to the real relay within 15s — is HANG_REPRO_RELAY reachable?");
        eprintln!("[stop_reap_repro] joined room {room_handle:?}");

        let h = app.handle().clone();
        let started = call_with_timeout("start_video_share", std::time::Duration::from_secs(5), move || {
            start_video_share(h.state::<Managed>(), room_handle.0)
        });
        assert!(started.is_some(), "start_video_share itself hung");

        let capture_id = wait_for(std::time::Duration::from_secs(30), || {
            let managed = app.state::<Managed>();
            let state = managed.state.lock().unwrap();
            state.captures.iter().find_map(|(id, c)| {
                matches!(c.status, simple_screensharing_client::session::ShareStatus::Active(_)).then_some(*id)
            })
        })
        .expect("capture should go Active within 30s to set up this repro — this is the very first Stop ever, matching the fresh-app report");
        eprintln!("[stop_reap_repro] capture {capture_id:?} is Active — clicking Stop exactly once, like the real report");

        let stop_clicked_at = std::time::Instant::now();
        let h = app.handle().clone();
        let stopped = call_with_timeout("stop_share", std::time::Duration::from_secs(5), move || {
            stop_share(h.state::<Managed>(), capture_id.0)
        });
        assert!(stopped.is_some(), "stop_share itself hung — see thread dump");

        eprintln!("[stop_reap_repro] stop_share command returned; now polling AppState.captures to see whether the capture is actually reaped");
        let reaped = wait_for(std::time::Duration::from_secs(20), || {
            let managed = app.state::<Managed>();
            let state = managed.state.lock().unwrap();
            (!state.captures.contains_key(&capture_id)).then_some(())
        });
        let elapsed = stop_clicked_at.elapsed();
        eprintln!("[stop_reap_repro] capture reaped: {} after {:?}", reaped.is_some(), elapsed);
        assert!(
            reaped.is_some(),
            "THE BUG: {elapsed:?} after stop_share was called, the capture (and its destination) is STILL present in \
             AppState.captures — the pipeline may have genuinely stopped (encoder CPU/frame freeze), but the \
             Rust-side bookkeeping never caught up. pid {} left running for a debugger to attach.",
            std::process::id()
        );
        assert!(elapsed < std::time::Duration::from_secs(3), "capture was reaped, but took {elapsed:?} — too slow to read as instant in the UI");
    }

    /// Same as [`stop_destination_actually_clears_from_state`], but repeated
    /// back-to-back with no pause (start immediately follows the previous
    /// cycle's Stop) — the exact contention condition
    /// `superdoc/ui/FRONTEND-SHELL.md`'s `start_pipeline` `Why:` note
    /// documents as the trigger for GStreamer/PipeWire state changes stalling
    /// unpredictably. Reports the reap latency of *every* cycle so an
    /// intermittent stall (rather than a permanent one) still shows up.
    #[test]
    #[ignore]
    fn stop_reap_stress_cycles() {
        let _guard = isolated_config_home();
        let managed = tauri::async_runtime::block_on(async { Managed::new() });

        let app = tauri::test::mock_builder()
            .manage(managed)
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .expect("build mock tauri app");

        let poller_handle = app.handle().clone();
        tauri::async_runtime::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(250));
            loop {
                interval.tick().await;
                test_tick(&poller_handle.state::<Managed>());
            }
        });

        let relay = std::env::var("HANG_REPRO_RELAY").unwrap_or_else(|_| "127.0.0.1:4477".to_string());
        let cycles: u32 = std::env::var("HANG_REPRO_CYCLES").ok().and_then(|s| s.parse().ok()).unwrap_or(8);
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let mut room_id = [0u8; 16];
        let mut room_key = [0u8; 32];
        for (i, b) in room_id.iter_mut().enumerate() {
            *b = (seed >> (i % 16 * 4)) as u8 ^ (i as u8) ^ 0x33;
        }
        for (i, b) in room_key.iter_mut().enumerate() {
            *b = (seed >> (i % 16 * 4)) as u8 ^ (i as u8).wrapping_mul(31) ^ 0x33;
        }
        let join_string = simple_screensharing_client::net::format_join_string(&relay, &room_id, &room_key);

        eprintln!("[stop_reap_stress] joining {join_string}");
        let handle = app.handle().clone();
        let jr = join_room(handle.state::<Managed>(), join_string, "StopReapStressSharer".to_string(), String::new());
        assert!(jr.is_ok(), "join_room returned an error: {jr:?}");

        let room_handle = wait_for(std::time::Duration::from_secs(15), || {
            app.state::<Managed>().state.lock().unwrap().rooms.keys().next().copied()
        })
        .expect("room should connect to the real relay within 15s — is HANG_REPRO_RELAY reachable?");
        eprintln!("[stop_reap_stress] joined room {room_handle:?}");

        let mut max_reap = std::time::Duration::ZERO;
        for cycle in 0..cycles {
            eprintln!("[stop_reap_stress] === cycle {cycle}/{cycles}: start_video_share ===");
            let h = app.handle().clone();
            let started = call_with_timeout("start_video_share", std::time::Duration::from_secs(5), move || {
                start_video_share(h.state::<Managed>(), room_handle.0)
            });
            assert!(started.is_some(), "start_video_share itself hung — see thread dump");

            let capture_id = wait_for(std::time::Duration::from_secs(30), || {
                let managed = app.state::<Managed>();
                let state = managed.state.lock().unwrap();
                state.captures.iter().find_map(|(id, c)| {
                    matches!(c.status, simple_screensharing_client::session::ShareStatus::Active(_)).then_some(*id)
                })
            });
            let Some(capture_id) = capture_id else {
                eprintln!("[stop_reap_stress] cycle {cycle}: capture never went Active within 30s (real-world churn) — skipping this cycle's Stop");
                continue;
            };
            eprintln!("[stop_reap_stress] cycle {cycle}: capture {capture_id:?} Active — clicking Stop");

            let stop_clicked_at = std::time::Instant::now();
            let h = app.handle().clone();
            let stopped = call_with_timeout("stop_share", std::time::Duration::from_secs(5), move || {
                stop_share(h.state::<Managed>(), capture_id.0)
            });
            assert!(stopped.is_some(), "cycle {cycle}: stop_share itself hung — see thread dump");

            let reaped = wait_for(std::time::Duration::from_secs(30), || {
                let managed = app.state::<Managed>();
                let state = managed.state.lock().unwrap();
                (!state.captures.contains_key(&capture_id)).then_some(())
            });
            let elapsed = stop_clicked_at.elapsed();
            eprintln!("[stop_reap_stress] cycle {cycle}: reaped={} after {elapsed:?}", reaped.is_some());
            assert!(
                reaped.is_some(),
                "cycle {cycle}: THE BUG — capture never reaped from AppState.captures within 30s of Stop being \
                 clicked. pid {} left running for a debugger to attach.",
                std::process::id()
            );
            max_reap = max_reap.max(elapsed);
            // Deliberately no pause — next cycle's start_video_share fires
            // immediately, exactly the reported trigger condition.
        }
        eprintln!("[stop_reap_stress] done — worst-case reap latency across {cycles} cycles: {max_reap:?}");
        assert!(max_reap < std::time::Duration::from_secs(5), "worst-case reap latency {max_reap:?} is too slow to read as \"a second or two\" in the UI");
    }
}
