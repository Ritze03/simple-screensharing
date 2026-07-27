//! The Tauri replacement for egui's "call `poll_*`/`drain_events` every
//! repaint" loop.
//!
//! # The driving-mechanism design problem
//!
//! `session::AppState` was built to be advanced by an always-repainting
//! caller: `poll_capture_states`, `poll_connect_result`,
//! `poll_pending_destinations`, `poll_destination_edits` and `drain_events`
//! all exist because the *old* egui frontend called every one of them once
//! per frame (up to ~240 Hz while focused, throttled while unfocused — see
//! `architecture/overview.md`'s "fast-forward catch-up" `Why:` note).
//! Tauri's webview has no equivalent frame callback a Rust command can hook.
//!
//! Two different mechanisms are used here, matching what's actually
//! latency-sensitive:
//!
//! - **Genuinely async completions already have a signal** —
//!   `spawn_media_ingestion_task` (spawned once per room by
//!   `AppState::insert_room`, unchanged) already reacts to a `MediaFrame`
//!   the instant it arrives, independent of any poll. That mechanism is left
//!   completely alone; this module doesn't touch it.
//! - **Everything else picked up by a poll** (capture start/stop settling,
//!   connect attempts resolving, destination edits/fan-out/transfer
//!   completing, and the room event queues `drain_events` empties) is driven
//!   by **one** lightweight `tokio::time::interval` loop, spawned once at
//!   startup, that locks the single `Mutex<AppState>`, runs every poll in
//!   sequence, diffs a few cheap snapshots against what it saw last tick, and
//!   emits a Tauri event for whatever changed.
//!
//! `Why one shared interval rather than one per concern:` every poll here is
//! a cheap, non-blocking `try_recv`/map-scan over in-memory state — nothing
//! like the frame-rate-critical video path, which is deliberately **not**
//! driven by this tick loop at all: `video_server.rs` runs its own
//! per-stream producer thread polling at up to 60fps, independent of this
//! loop's much coarser cadence (see that module's doc comment for the
//! MJPEG-over-HTTP mechanism itself; as of 2026-07-27 it's implemented and
//! wired to `toggle_watch`, not a future/out-of-scope item — an earlier
//! version of this comment claimed it was "a separate, already-prototyped"
//! mechanism before it existed anywhere; see `video_server.rs`'s doc comment
//! and `superdoc/ui/FRONTEND-SHELL.md`'s 2026-07-27 entries for that
//! history). A single few-hundred-ms tick is already far below any
//! human-perceptible delay for "a stream finished starting" or "a member
//! joined", so one timer buying one lock acquisition per tick is simpler
//! than N timers each fighting over the same mutex for no responsiveness
//! gain. See `superdoc/architecture/overview.md`'s command/event table for
//! the same note.
use std::collections::HashMap;
use std::time::Duration;

use tauri::{AppHandle, Emitter, Manager};

use simple_screensharing_client::session::{self, ConnectOutcome};

use crate::dto;
use crate::state::Managed;

/// How often the driver loop wakes up. Generous relative to human perception
/// (see module docs) — this is not a video-frame path.
const TICK_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Default)]
struct Snapshot {
    /// `RoomHandle -> "member_count:stream_count"` signature, cheap enough to
    /// recompute every tick without allocating a full clone to diff against.
    rooms: HashMap<u32, String>,
    /// `DestinationId -> state label`.
    destinations: HashMap<u32, &'static str>,
}

pub fn spawn(app_handle: AppHandle) {
    // `tokio::spawn` requires already running inside an entered Tokio
    // runtime context; `Builder::setup` runs synchronously on the main
    // thread before Tauri's event loop starts, with no such context
    // entered, so a raw `tokio::spawn` here panics
    // ("there is no reactor running") the instant the app launches.
    // `tauri::async_runtime::spawn` is the runtime-agnostic entry point
    // Tauri documents for exactly this: it dispatches onto the one
    // Tokio runtime Tauri itself owns and manages, regardless of what
    // context it's called from.
    tauri::async_runtime::spawn(async move {
        let mut interval = tokio::time::interval(TICK_INTERVAL);
        let mut last = Snapshot::default();
        loop {
            interval.tick().await;
            tick(&app_handle, &mut last);
        }
    });
}

fn tick(app_handle: &AppHandle, last: &mut Snapshot) {
    let managed = app_handle.state::<Managed>();
    // `Why:` recover the inner value on a poisoned lock instead of
    // propagating the poison with `.unwrap()`. This loop runs forever on its
    // own background task (see `spawn` above) — if some *other* lock holder
    // ever panics while holding `managed.state` (any of the `#[tauri::command]`
    // handlers in `commands.rs`, all of which also lock it), the mutex is left
    // poisoned but `AppState`'s data is still perfectly readable; there is no
    // invariant here that a caller-side panic could have left half-applied.
    // Propagating the poison would just kill this polling loop too — every
    // room's presence/event draining, capture-state polling, etc. — turning
    // one unrelated panic into two failures instead of one.
    let mut state = managed.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

    // 1. Connect attempts.
    match state.poll_connect_result() {
        ConnectOutcome::None => {}
        ConnectOutcome::Joined(handle) => {
            if let Some(detail) = dto::room_detail(&state, handle) {
                let _ = app_handle.emit("room-connected", &detail);
            }
        }
        ConnectOutcome::Failed { error, from_add_card } => {
            let _ = app_handle.emit("room-connect-failed", serde_json::json!({ "error": error, "fromAddCard": from_add_card }));
        }
    }

    // 2. Capture/destination lifecycle.
    state.poll_capture_states();
    state.poll_pending_destinations();
    state.poll_destination_edits();
    state.poll_audio_sources();

    // 3. Room event queues (roster, announced streams, control-closed).
    let rooms_before: Vec<u32> = state.rooms.keys().map(|h| h.0).collect();
    state.drain_events(&managed.egui_ctx);
    // `drain_events` uploads preview thumbnails onto `managed.egui_ctx` via
    // `egui::Context::load_texture`/`TextureHandle::set`, which only ever
    // frees a texture's *metadata* immediately (`TextureManager::free`) —
    // the actual `TexturesDelta` queue (`set`/`free` entries, `set` carrying
    // the full decoded image bytes) is only ever drained by
    // `TextureManager::take_delta`, which egui's real render integrations
    // call once per repaint. Nothing ever repaints this standalone,
    // never-shown context, so without this call that queue grows without
    // bound for the life of the process — one leaked full-size preview
    // image per texture ever allocated or updated, never reclaimed even
    // after the underlying stream/room goes away. Draining and discarding
    // it here every tick keeps that queue's size bounded by what changed
    // within one 250ms window (normally zero, since previews refresh only
    // every ~30s) instead of the whole session's history.
    let _ = managed.egui_ctx.tex_manager().write().take_delta();
    let rooms_after: Vec<u32> = state.rooms.keys().map(|h| h.0).collect();
    for handle in rooms_before {
        if !rooms_after.contains(&handle) {
            let reason = match &state.status {
                session::Status::Error(e) => e.clone(),
                _ => "disconnected".to_string(),
            };
            // A room can disappear without ever going through
            // `commands::toggle_watch`'s explicit stop path (control-plane
            // close while still watching something) — without this, that
            // stream's video-server producer thread and connection-serving
            // route would leak for the rest of the process's life.
            managed.video_server.stop_room(handle);
            let _ = app_handle.emit("room-disconnected", serde_json::json!({ "roomHandle": handle, "reason": reason }));
        }
    }

    // 4. Bandwidth allocation (Absolute-Maximum mode's live wiring; a cheap
    // no-op in Per-Stream mode, see `AppState::apply_bandwidth_allocation`).
    state.apply_bandwidth_allocation();
    if let Some(alloc) = state.bandwidth.allocation()
        && alloc.overshoot_kbps > 0
    {
        let _ = app_handle.emit("bandwidth-overshoot", serde_json::json!({ "overshootKbps": alloc.overshoot_kbps }));
    }

    // 5. Diff presence + destination-state snapshots, emit only on change.
    let mut rooms_snapshot = HashMap::new();
    for (handle, room) in &state.rooms {
        let sig = format!("{}:{}", room.members.len(), room.streams.len());
        rooms_snapshot.insert(handle.0, sig.clone());
        if last.rooms.get(&handle.0) != Some(&sig)
            && let Some(detail) = dto::room_detail(&state, *handle)
        {
            let _ = app_handle.emit("presence-updated", &detail);
        }
    }
    last.rooms = rooms_snapshot;

    let mut dest_snapshot = HashMap::new();
    for capture in state.captures.values() {
        for (destination_id, dest) in &capture.destinations {
            let label = dto::destination_state_label(&dest.state());
            dest_snapshot.insert(destination_id.0, label);
            if last.destinations.get(&destination_id.0) != Some(&label) {
                let _ = app_handle.emit(
                    "destination-state-changed",
                    serde_json::json!({ "destinationId": destination_id.0, "roomHandle": dest.room.0, "state": label }),
                );
            }
        }
    }
    // A destination that was in `last.destinations` but isn't in this tick's
    // `dest_snapshot` was just removed from `AppState` entirely (Stop
    // clicked, and — for a capture's last destination — its whole
    // `LocalCapture` reaped by `poll_capture_states`). `Why this needs its
    // own pass, not just the loop above:` the loop above can only ever
    // notice a value *changing* for a key that still exists post-tick; a key
    // vanishing produces no iteration at all, so it was silently invisible
    // here. That silence was a real, reproduced bug (see
    // `superdoc/ui/FRONTEND-SHELL.md`'s 2026-07-27 "Stop never clears" dated
    // entry): `AppState` itself reaps a stopped destination within about one
    // tick (confirmed directly against a real relay + real portal capture,
    // never more than ~300ms), but the frontend only ever re-fetches
    // `list_active_streams` in response to this exact event (or
    // `presence-updated`, which the relay deliberately never sends back to
    // the very client whose own stream just stopped — see `relay.rs`'s
    // `StopStream` handler, `broadcast(.., Some(conn_id))` excludes the
    // sender) — so without this, the Stream Control tab's destination card
    // for *your own* stopped stream never learns it's gone, and stays stuck
    // showing stale `Live` state until some unrelated event happens to
    // trigger a refresh. The emitted state label of `"removed"` isn't a real
    // `DestinationState` variant; the frontend's listener (`shell.js`'s
    // `wireEvents`) ignores the event payload entirely and just re-fetches,
    // so any value here only needs to make the event fire, not be otherwise
    // meaningful. `roomHandle: 0` for the same reason — the destination's
    // real room is gone from `AppState` by this point, and nothing reads it.
    for (&destination_id, _) in last.destinations.iter().filter(|(id, _)| !dest_snapshot.contains_key(id)) {
        let _ = app_handle.emit(
            "destination-state-changed",
            serde_json::json!({ "destinationId": destination_id, "roomHandle": 0, "state": "removed" }),
        );
    }
    last.destinations = dest_snapshot;
}
