//! Tauri-managed application state, plus the background driver that replaces
//! egui's per-frame `update()` poll (see this module's doc comment on
//! [`spawn_driver`]).
//!
//! `Managed` bundles the real `session::AppState` (behind a plain
//! `std::sync::Mutex`, since every `AppState` method used from a command is
//! synchronous — it spawns its own async work and returns immediately, same
//! as the egui frontend always did) with a standalone `egui::Context`. That
//! context exists for exactly one reason: `session::AppState::drain_events`
//! takes `&egui::Context` to upload decoded preview thumbnails as egui
//! textures, and `session.rs` is out of scope to modify for this port step
//! (see module docs below). `egui::Context::default()` needs no window, no
//! renderer and no event loop — it's just a data structure — so constructing
//! one here and never displaying it is a harmless way to keep calling the
//! exact same `drain_events` the egui frontend calls, textures and all; they
//! simply go nowhere. The frontend gets the same preview bytes a different
//! way: `RoomSession::preview_cache` is read directly by
//! [`crate::commands::list_active_streams`].
use std::sync::Mutex;

use simple_screensharing_client::session::AppState;

pub struct Managed {
    pub state: Mutex<AppState>,
    /// See the module doc comment above for why this exists at all.
    pub egui_ctx: egui::Context,
}

impl Managed {
    pub fn new() -> Self {
        Self { state: Mutex::new(AppState::default()), egui_ctx: egui::Context::default() }
    }
}

impl Default for Managed {
    fn default() -> Self {
        Self::new()
    }
}
