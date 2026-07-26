//! Tauri desktop shell entry point.
//!
//! This is deliberately a minimal scaffold: it proves the IPC bridge works
//! (`ping` command below, invoked from the placeholder `frontend/index.html`)
//! and that the backend crate (`simple_screensharing_client`, the existing
//! `net`/`capture`/`decode`/`rooms`/`bandwidth`/`protocol`/`fragment`/`audio`
//! logic) links in unmodified as a path dependency. The real IPC surface and
//! UI wiring are separate follow-up work.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

/// Trivial round-trip command proving the Tauri IPC bridge is wired up end
/// to end: `frontend/index.html` calls this via
/// `window.__TAURI__.core.invoke('ping')`.
#[tauri::command]
fn ping() -> &'static str {
    "pong"
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![ping])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
