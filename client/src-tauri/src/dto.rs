//! Serializable IPC payloads. `session.rs`'s own types are deliberately not
//! `Serialize` (it doesn't depend on serde beyond what protocol.rs needs) and
//! is out of scope to modify for this step, so every command/event boundary
//! goes through a small DTO defined here instead.

use serde::Serialize;

use simple_screensharing_client::capture::destination::{DestinationState, VideoParams};
use simple_screensharing_client::protocol::{Member, StreamInfo};
use simple_screensharing_client::rooms::BandwidthMode;
use simple_screensharing_client::session::{self, AppState, RoomHandle};

/// One row of the sidebar's ROOMS list: the left join of `RoomStore` (saved
/// rooms) and `AppState::rooms` (live sessions) described in
/// `superdoc/ui/APP-SHELL.md`.
#[derive(Serialize, Clone)]
pub struct SavedRoomRow {
    pub name: String,
    /// The full `server|room-id|key` join string. Needed by the frontend to
    /// reconnect/rename a saved row — **never render this**, it contains the
    /// room key (see `APP-SHELL.md`'s `Why:` note on `room_label`).
    pub join_string: String,
    pub state: RoomRowState,
    /// `Some` only when `state == Joined` — the handle every other command
    /// keys off.
    pub room_handle: Option<u32>,
    pub active_stream_count: u32,
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RoomRowState {
    Joined,
    Reconnecting,
    Disconnected,
}

/// Everything about one joined room the frontend needs beyond the sidebar row
/// (roster, announced streams) — emitted on `presence-updated` and fetchable
/// on demand.
#[derive(Serialize, Clone)]
pub struct RoomDetail {
    pub room_handle: u32,
    pub label: String,
    pub members: Vec<Member>,
    pub streams: Vec<StreamInfo>,
    pub watched: Vec<StreamKey>,
    pub listened: Vec<StreamKey>,
}

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StreamKey {
    pub owner: u64,
    pub stream_id: u8,
}

/// One outgoing destination, cross-room (mirrors the sidebar's STREAMS
/// section and the Stream Control tab's "Active shares" list — both are
/// deliberately cross-room, see `superdoc/features/stream-control.md`).
#[derive(Serialize, Clone)]
pub struct DestinationRow {
    pub capture_id: u32,
    pub destination_id: u32,
    pub room_handle: u32,
    pub kind: &'static str,
    pub label: String,
    pub state: &'static str,
    pub pending: Option<&'static str>,
    pub width: u32,
    pub height: u32,
    pub framerate: u32,
    pub bitrate_kbps: u32,
}

#[derive(Serialize, Clone)]
pub struct SettingsDto {
    pub language: String,
    pub presentation_mode: String,
    pub launch_presentation_mode: String,
    pub bandwidth_mode: String,
    pub absolute_total_kbps: u32,
    pub destination_floor_kbps: u32,
    pub bitrate_kbps: u32,
    pub framerate: u32,
}

#[derive(Serialize, Clone)]
pub struct AudioSourceDto {
    pub id: u32,
    pub name: String,
}

pub fn destination_state_label(state: &DestinationState) -> &'static str {
    state.label()
}

pub fn pending_label(role: session::PendingRole) -> &'static str {
    match role {
        session::PendingRole::FanOut => "fan_out",
        session::PendingRole::TransferTarget(_) => "transfer_target",
        session::PendingRole::TransferSource(_) => "transfer_source",
    }
}

pub fn bandwidth_mode_str(mode: BandwidthMode) -> &'static str {
    match mode {
        BandwidthMode::Absolute => "absolute",
        BandwidthMode::PerStream => "per_stream",
    }
}

pub fn parse_bandwidth_mode(s: &str) -> Result<BandwidthMode, String> {
    match s {
        "absolute" => Ok(BandwidthMode::Absolute),
        "per_stream" => Ok(BandwidthMode::PerStream),
        other => Err(format!("unknown bandwidth mode: {other}")),
    }
}

/// Stable, snake_case identifier for a `PresentationMode` — not `label()`
/// (that's a human-readable string for the egui dropdown), so the frontend
/// can match on it and supply its own translated label.
pub fn presentation_mode_key(mode: simple_screensharing_client::rooms::PresentationMode) -> &'static str {
    use simple_screensharing_client::rooms::PresentationMode;
    match mode {
        PresentationMode::AutoVsync => "auto_vsync",
        PresentationMode::Immediate => "immediate",
        PresentationMode::Mailbox => "mailbox",
    }
}

pub fn parse_presentation_mode(s: &str) -> Result<simple_screensharing_client::rooms::PresentationMode, String> {
    use simple_screensharing_client::rooms::PresentationMode;
    match s {
        "auto_vsync" => Ok(PresentationMode::AutoVsync),
        "immediate" => Ok(PresentationMode::Immediate),
        "mailbox" => Ok(PresentationMode::Mailbox),
        other => Err(format!("unknown presentation mode: {other}")),
    }
}

/// Build the sidebar's ROOMS rows: every saved room, plus any live room that
/// isn't (yet) saved — mirrors `room_store ⟕ rooms` from `APP-SHELL.md`.
pub fn saved_room_rows(state: &AppState) -> Vec<SavedRoomRow> {
    let mut rows: Vec<SavedRoomRow> = Vec::new();
    for saved in &state.room_store.rooms {
        let live = state.rooms.iter().find(|(_, room)| room.join_string == saved.join_string);
        let row = match live {
            Some((handle, room)) => SavedRoomRow {
                name: saved.name.clone(),
                join_string: saved.join_string.clone(),
                state: RoomRowState::Joined,
                room_handle: Some(handle.0),
                active_stream_count: room.destinations.len() as u32,
            },
            None if state.is_reconnecting(&saved.join_string) => SavedRoomRow {
                name: saved.name.clone(),
                join_string: saved.join_string.clone(),
                state: RoomRowState::Reconnecting,
                room_handle: None,
                active_stream_count: 0,
            },
            None => SavedRoomRow {
                name: saved.name.clone(),
                join_string: saved.join_string.clone(),
                state: RoomRowState::Disconnected,
                room_handle: None,
                active_stream_count: 0,
            },
        };
        rows.push(row);
    }
    // Live rooms with no saved entry yet (shouldn't normally happen — a
    // successful join always upserts one, see `AppState::poll_connect_result`
    // — but keeps this list honest if the store failed to save).
    for (handle, room) in &state.rooms {
        if state.room_store.name_for(&room.join_string).is_none() {
            rows.push(SavedRoomRow {
                name: session::room_label(&state.room_store, &room.join_string),
                join_string: room.join_string.clone(),
                state: RoomRowState::Joined,
                room_handle: Some(handle.0),
                active_stream_count: room.destinations.len() as u32,
            });
        }
    }
    rows
}

pub fn room_detail(state: &AppState, handle: RoomHandle) -> Option<RoomDetail> {
    let room = state.rooms.get(&handle)?;
    let watched = room.watches.lock().unwrap().keys().map(|(owner, sid)| StreamKey { owner: *owner, stream_id: *sid }).collect();
    let listened =
        room.audio_watches.lock().unwrap().keys().map(|(owner, sid)| StreamKey { owner: *owner, stream_id: *sid }).collect();
    Some(RoomDetail {
        room_handle: handle.0,
        label: session::room_label(&state.room_store, &room.join_string),
        members: room.members.clone(),
        streams: room.streams.clone(),
        watched,
        listened,
    })
}

pub fn destination_rows(state: &AppState) -> Vec<DestinationRow> {
    let mut rows = Vec::new();
    for (capture_id, capture) in &state.captures {
        for (destination_id, dest) in &capture.destinations {
            let params: VideoParams = dest.live_params();
            rows.push(DestinationRow {
                capture_id: capture_id.0,
                destination_id: destination_id.0,
                room_handle: dest.room.0,
                kind: if capture.kind == simple_screensharing_client::protocol::StreamKind::Audio { "audio" } else { "video" },
                label: capture.label.clone(),
                state: destination_state_label(&dest.state()),
                pending: dest.pending.map(pending_label),
                width: params.width,
                height: params.height,
                framerate: params.framerate,
                bitrate_kbps: params.bitrate_kbps,
            });
        }
    }
    rows
}

pub fn settings_dto(state: &AppState) -> SettingsDto {
    SettingsDto {
        language: state.language.clone(),
        presentation_mode: presentation_mode_key(state.presentation_mode).to_string(),
        launch_presentation_mode: presentation_mode_key(state.launch_presentation_mode).to_string(),
        bandwidth_mode: bandwidth_mode_str(state.bandwidth_mode).to_string(),
        absolute_total_kbps: state.absolute_total_kbps,
        destination_floor_kbps: state.destination_floor_kbps,
        bitrate_kbps: state.bitrate_kbps,
        framerate: state.framerate,
    }
}
