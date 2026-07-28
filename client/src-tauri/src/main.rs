//! Tauri desktop shell entry point.
//!
//! Wires `simple_screensharing_client::session::AppState` (see `../src/
//! session.rs`) into Tauri as managed state, registers the real command
//! surface (`commands.rs`), and spawns the one background driver task
//! (`driver.rs`) that replaces egui's per-frame poll — see that module's doc
//! comment for the full "who drives `AppState`'s polling" design note.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod api_server;
mod commands;
mod dto;
mod driver;
mod state;
mod video_server;

use tauri::Manager;

use state::Managed;

fn main() {
    tracing_subscriber::fmt::init();
    // `Managed::new()` builds `session::AppState::default()`, which eagerly
    // binds the client's local QUIC/UDP socket (`net::build_client_endpoint`).
    // That bind requires an entered Tokio reactor (quinn's `runtime-tokio`
    // feature looks it up at bind time) — but a plain `.manage(Managed::new())`
    // here runs synchronously during `Builder::default()`, before Tauri has
    // entered any runtime context (same class of problem `driver.rs` hit with
    // a raw `tokio::spawn` in `.setup()`; see that module's doc comment).
    // Left as-is, every `join_room` call failed with "no async runtime found"
    // because the endpoint had already failed to bind at construction and
    // `start_connect` just replayed that stored error.
    //
    // `tauri::async_runtime::block_on` lazily creates (or reuses) Tauri's one
    // shared Tokio runtime and *enters* it for the duration of the future,
    // which is exactly the reactor context `build_client_endpoint` needs —
    // and it's available even this early, before `.setup()`/`.run()`, unlike
    // a raw `tokio::Runtime::enter()` guard we'd have to build and hold
    // ourselves. Constructing `Managed` inside it fixes the bind without
    // touching `session.rs` or making the endpoint lazily-initialized.
    let managed = tauri::async_runtime::block_on(async { Managed::new() });
    tauri::Builder::default()
        .manage(managed)
        .setup(|app| {
            // Queue every saved room with `auto_connect` set (the
            // room-detail panel's "Auto-Connect" checkbox — see
            // `session::AppState::set_room_auto_connect`'s doc comment) to
            // be joined at startup. Only queues here — `driver::spawn`'s
            // tick loop (started right below) is what actually drains the
            // queue one room at a time via `poll_auto_connect_queue`, since
            // `pending_connect` only has room for one in-flight connect.
            {
                let managed = app.state::<Managed>();
                let mut state = managed.state.lock().unwrap();
                let auto_connect_rooms: Vec<String> =
                    state.room_store.rooms.iter().filter(|r| r.auto_connect).map(|r| r.join_string.clone()).collect();
                state.auto_connect_queue.extend(auto_connect_rooms);
            }

            driver::spawn(app.handle().clone());
            // The Electron-migration REST+SSE transport — see
            // `api_server.rs`'s module doc. Runs alongside the still-live
            // Tauri `invoke_handler` command surface below (kept in place for
            // revertibility — every `#[tauri::command]` fn it registers is
            // exactly what `api_server.rs` also dispatches into, so both
            // transports call the same code and share the same `Managed`
            // state). `driver.rs`'s events, however, now only go out over
            // this SSE transport: `app_handle.emit()` was replaced with
            // `EventBus::publish()`, so a Tauri-side event listener no longer
            // receives them (see `driver.rs`'s edit for this task).
            api_server::spawn(app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_saved_rooms,
            commands::join_room,
            commands::create_room,
            commands::leave_needs_confirm,
            commands::leave_room,
            commands::force_leave_room,
            commands::rename_room,
            commands::set_room_auto_connect,
            commands::select_active_room,
            commands::room_detail,
            commands::get_settings,
            commands::get_start_stream_defaults,
            commands::set_start_stream_defaults,
            commands::set_language,
            commands::set_presentation_mode,
            commands::set_max_jitter_buffer_ms,
            commands::start_video_share,
            commands::start_audio_share,
            commands::set_audio_share_muted,
            commands::stop_share,
            commands::list_audio_sources,
            commands::select_audio_source,
            commands::set_bitrate,
            commands::set_framerate,
            commands::set_tune,
            commands::set_quality_level,
            commands::set_rate_control,
            commands::list_active_streams,
            commands::start_destination,
            commands::transfer_destination,
            commands::edit_destination,
            commands::stop_destination,
            commands::retry_destination,
            commands::set_bandwidth_mode,
            commands::set_bandwidth_total,
            commands::set_bandwidth_floor,
            commands::toggle_watch,
            commands::toggle_listen,
            commands::set_volume,
            commands::stream_video_url,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
