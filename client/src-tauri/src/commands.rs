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
use tauri::{Manager, State};

use simple_screensharing_client::session::{
    AppState, CaptureSourceId, DestinationId, PendingRole, RoomHandle,
};

use crate::dto::{self, AudioSourceDto, DestinationRow, RoomDetail, SavedRoomRow, SettingsDto};
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

pub fn join_room_impl(state: &mut AppState, join_string: String, display_name: String) -> Result<(), String> {
    if !display_name.trim().is_empty() {
        state.display_name = display_name;
    }
    state.start_connect(join_string, true)
}

#[tauri::command]
pub fn join_room(managed: State<'_, Managed>, join_string: String, display_name: String) -> Result<(), String> {
    join_room_impl(&mut managed.state.lock().unwrap(), join_string, display_name)
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
#[tauri::command]
pub fn leave_room(managed: State<'_, Managed>, room_handle: u32) -> bool {
    managed.state.lock().unwrap().request_leave_room(RoomHandle(room_handle))
}

/// Leave after the user confirmed the dialog `leave_needs_confirm` said was
/// needed — unconditional teardown.
#[tauri::command]
pub fn force_leave_room(managed: State<'_, Managed>, room_handle: u32) {
    managed.state.lock().unwrap().disconnect_room(RoomHandle(room_handle));
}

#[tauri::command]
pub fn rename_room(managed: State<'_, Managed>, join_string: String, new_name: String) -> Result<(), String> {
    let mut state = managed.state.lock().unwrap();
    state.rename_saved_room(&join_string, new_name);
    match &state.room_store_error {
        Some(e) => Err(e.clone()),
        None => Ok(()),
    }
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
    let mut state = managed.state.lock().unwrap();
    rust_i18n::set_locale(&language);
    state.language = language.clone();
    simple_screensharing_client::session::save_language(&language);
}

#[tauri::command]
pub fn set_presentation_mode(managed: State<'_, Managed>, mode: String) -> Result<(), String> {
    let mode = dto::parse_presentation_mode(&mode)?;
    let mut state = managed.state.lock().unwrap();
    state.presentation_mode = mode;
    simple_screensharing_client::session::save_presentation_mode(mode);
    Ok(())
}

// ---------------------------------------------------------------------
// Stream Control
// ---------------------------------------------------------------------

#[tauri::command]
pub fn start_video_share(managed: State<'_, Managed>, room_handle: u32) {
    managed.state.lock().unwrap().start_video_share(RoomHandle(room_handle));
}

#[tauri::command]
pub fn start_audio_share(managed: State<'_, Managed>, room_handle: u32) {
    managed.state.lock().unwrap().start_audio_share(RoomHandle(room_handle));
}

#[tauri::command]
pub fn stop_share(managed: State<'_, Managed>, capture_id: u32) {
    managed.state.lock().unwrap().stop_share(CaptureSourceId(capture_id));
}

/// Lists currently-producing-audio applications. Deliberately bypasses
/// `AppState::refresh_audio_sources`/`poll_audio_sources` (the egui
/// frontend's fire-then-poll-next-frame pattern) since a Tauri command can
/// just `.await` the blocking listing directly and hand the result straight
/// back — no poll needed for a call that already has a natural async return
/// point.
#[tauri::command]
pub async fn list_audio_sources() -> Result<Vec<AudioSourceDto>, String> {
    tokio::task::spawn_blocking(simple_screensharing_client::audio::list_audio_sources)
        .await
        .map_err(|e| format!("audio source listing task panicked: {e}"))?
        .map(|sources| sources.into_iter().map(|s| AudioSourceDto { id: s.id, name: s.name }).collect())
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub fn select_audio_source(managed: State<'_, Managed>, id: u32, name: String) {
    managed.state.lock().unwrap().selected_audio_source =
        Some(simple_screensharing_client::audio::AudioSource { id, name });
}

#[tauri::command]
pub fn set_bitrate(managed: State<'_, Managed>, kbps: u32) {
    managed.state.lock().unwrap().bitrate_kbps = kbps;
}

#[tauri::command]
pub fn set_framerate(managed: State<'_, Managed>, fps: u32) {
    managed.state.lock().unwrap().framerate = fps;
}

#[tauri::command]
pub fn list_active_streams(managed: State<'_, Managed>) -> Vec<DestinationRow> {
    dto::destination_rows(&managed.state.lock().unwrap())
}

/// "Also send to…" picked on a destination row: fan out an already-running
/// capture into one more room (see `superdoc/features/stream-control.md`).
#[tauri::command]
pub fn start_destination(managed: State<'_, Managed>, capture_id: u32, room_handle: u32) {
    managed.state.lock().unwrap().add_destination(CaptureSourceId(capture_id), RoomHandle(room_handle), PendingRole::FanOut);
}

/// "Transfer to…" picked on a destination row: make-before-break move to
/// another room.
#[tauri::command]
pub fn transfer_destination(managed: State<'_, Managed>, destination_id: u32, target_room: u32) {
    managed.state.lock().unwrap().transfer_destination(DestinationId(destination_id), RoomHandle(target_room));
}

/// "Apply" clicked on a destination row's settings edit.
#[tauri::command]
pub fn edit_destination(
    managed: State<'_, Managed>,
    destination_id: u32,
    scale_percent: u32,
    framerate: u32,
    bitrate_kbps: u32,
) {
    let mut state = managed.state.lock().unwrap();
    let destination_id = DestinationId(destination_id);
    state.with_destination(destination_id, |dest| {
        dest.edit.scale_percent = scale_percent;
        dest.edit.framerate = framerate;
        dest.edit.bitrate_kbps = bitrate_kbps;
    });
    state.apply_destination_edit(destination_id);
}

#[tauri::command]
pub fn stop_destination(managed: State<'_, Managed>, destination_id: u32) {
    managed.state.lock().unwrap().stop_destination(DestinationId(destination_id));
}

#[tauri::command]
pub fn set_bandwidth_mode(managed: State<'_, Managed>, mode: String) -> Result<(), String> {
    let mode = dto::parse_bandwidth_mode(&mode)?;
    managed.state.lock().unwrap().set_bandwidth_mode(mode);
    Ok(())
}

#[tauri::command]
pub fn set_bandwidth_total(managed: State<'_, Managed>, total_kbps: u32) {
    let mut state = managed.state.lock().unwrap();
    state.absolute_total_kbps = total_kbps;
    simple_screensharing_client::session::save_bandwidth_settings(
        state.bandwidth_mode,
        state.absolute_total_kbps,
        state.destination_floor_kbps,
    );
}

#[tauri::command]
pub fn set_bandwidth_floor(managed: State<'_, Managed>, floor_kbps: u32) {
    let mut state = managed.state.lock().unwrap();
    state.destination_floor_kbps = floor_kbps;
    simple_screensharing_client::session::save_bandwidth_settings(
        state.bandwidth_mode,
        state.absolute_total_kbps,
        state.destination_floor_kbps,
    );
}

// ---------------------------------------------------------------------
// View
// ---------------------------------------------------------------------

#[tauri::command]
pub fn toggle_watch(managed: State<'_, Managed>, room_handle: u32, owner: u64, stream_id: u8) {
    if let Some(room) = managed.state.lock().unwrap().rooms.get_mut(&RoomHandle(room_handle)) {
        room.toggle_watch(owner, stream_id);
    }
}

#[tauri::command]
pub fn toggle_listen(managed: State<'_, Managed>, room_handle: u32, owner: u64, stream_id: u8) {
    if let Some(room) = managed.state.lock().unwrap().rooms.get_mut(&RoomHandle(room_handle)) {
        room.toggle_listen(owner, stream_id);
    }
}

#[tauri::command]
pub fn set_volume(managed: State<'_, Managed>, room_handle: u32, owner: u64, stream_id: u8, volume_percent: u32) {
    if let Some(room) = managed.state.lock().unwrap().rooms.get_mut(&RoomHandle(room_handle))
        && let Some(watch) = room.audio_watches.lock().unwrap().get_mut(&(owner, stream_id))
    {
        watch.volume_percent = volume_percent;
        watch.decoder.set_volume(volume_percent as f64 / 100.0);
    }
}

/// Opens a real second OS window showing just one stream — the Tauri
/// replacement for the old egui `show_viewport_deferred` pop-out (see the
/// task brief). Points at the same `frontend/index.html` shell with query
/// parameters identifying the stream; a later frontend stage reads those to
/// render just that one tile instead of the full shell. This command only
/// proves the window-opening mechanism — the popout page's real content is
/// the frontend worker's job.
#[tauri::command]
pub fn pop_out_stream(app: tauri::AppHandle, room_handle: u32, owner: u64, stream_id: u8) -> Result<(), String> {
    let label = format!("popout-{room_handle}-{owner:016x}-{stream_id}");
    let url = format!("index.html?popout=1&room={room_handle}&owner={owner:016x}&stream={stream_id}");
    if let Some(existing) = app.get_webview_window(&label) {
        let _ = existing.set_focus();
        return Ok(());
    }
    tauri::WebviewWindowBuilder::new(&app, label, tauri::WebviewUrl::App(url.into()))
        .title("Simple Screensharing — Stream")
        .inner_size(640.0, 400.0)
        .build()
        .map(|_| ())
        .map_err(|e| format!("{e:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use simple_screensharing_client::rooms::RoomStore;

    fn isolated_config_home() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        // SAFETY: tests in this module run sequentially with respect to this
        // var (no other test in this crate reads/writes `XDG_CONFIG_HOME`),
        // and `dirs::config_dir()` is only consulted lazily, at the point
        // `RoomStore`/`AppSettings` load/save — never cached at process
        // startup — so setting it before use is sufficient.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", dir.path());
        }
        dir
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
        let result = join_room_impl(&mut state, "not-a-valid-join-string".to_string(), String::new());
        assert!(result.is_ok(), "start_connect itself only fails on a dead local endpoint: {result:?}");
        assert!(state.pending_connect.is_some());
    }

    #[test]
    fn leave_needs_confirm_is_false_with_no_live_room() {
        let state = AppState::default();
        assert!(!state.leave_needs_confirm(RoomHandle(0)));
    }
}
