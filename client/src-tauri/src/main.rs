//! Tauri desktop shell entry point.
//!
//! Wires `simple_screensharing_client::session::AppState` (see `../src/
//! session.rs`) into Tauri as managed state, registers the real command
//! surface (`commands.rs`), and spawns the one background driver task
//! (`driver.rs`) that replaces egui's per-frame poll — see that module's doc
//! comment for the full "who drives `AppState`'s polling" design note.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod dto;
mod driver;
mod state;

use state::Managed;

fn main() {
    tauri::Builder::default()
        .manage(Managed::new())
        .setup(|app| {
            driver::spawn(app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_saved_rooms,
            commands::join_room,
            commands::leave_needs_confirm,
            commands::leave_room,
            commands::force_leave_room,
            commands::rename_room,
            commands::select_active_room,
            commands::room_detail,
            commands::get_settings,
            commands::set_language,
            commands::set_presentation_mode,
            commands::start_video_share,
            commands::start_audio_share,
            commands::stop_share,
            commands::list_audio_sources,
            commands::select_audio_source,
            commands::set_bitrate,
            commands::set_framerate,
            commands::list_active_streams,
            commands::start_destination,
            commands::transfer_destination,
            commands::edit_destination,
            commands::stop_destination,
            commands::set_bandwidth_mode,
            commands::set_bandwidth_total,
            commands::set_bandwidth_floor,
            commands::toggle_watch,
            commands::toggle_listen,
            commands::set_volume,
            commands::pop_out_stream,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
