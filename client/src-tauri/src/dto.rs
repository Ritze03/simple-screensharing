//! Serializable IPC payloads. `session.rs`'s own types are deliberately not
//! `Serialize` (it doesn't depend on serde beyond what protocol.rs needs) and
//! is out of scope to modify for this step, so every command/event boundary
//! goes through a small DTO defined here instead.

use serde::Serialize;

use simple_screensharing_client::capture::destination::{DestinationState, VideoParams};
use simple_screensharing_client::capture::{QualityLevel, RateControlMode, TuneMode};
use simple_screensharing_client::protocol::{ConnId, Member, StreamInfo};
use simple_screensharing_client::rooms::BandwidthMode;
use simple_screensharing_client::session::{self, AppState, RoomHandle};

/// `u64` (`ConnId`/`owner`) as a fixed-width lowercase-hex JSON string —
/// `serde_json::Number` is an IEEE-754 `f64` under the hood, which only
/// represents integers exactly up to `2^53`. A real `ConnId` is drawn from
/// the *full* `u64` range (see `rooms.rs`'s connection-id generation) and
/// routinely exceeds that, so sending one as a bare JSON number silently
/// *rounds* it — this was live and reproduced during this feature's own
/// integration testing: `toggle_watch`'s `owner` round-tripped through
/// `window.__TAURI__.core.invoke` came back a different `u64` than the one
/// `MediaFrame`s actually arrived tagged with, so `spawn_media_ingestion_task`
/// could never find the watch entry to feed — decode silently never started,
/// with no error anywhere (see `video_server.rs`'s module doc for how this
/// surfaced: a producer thread that polled forever and never saw a frame).
/// This exact class of bug (u64 fields losing precision over the JSON
/// boundary) was already flagged as a known limitation — see
/// `frontend/scripts/stream-control.js`'s `MOCK_MEMBERS_BY_ROOM` comment,
/// which called it "not something fixable here" — but was never actually
/// fixed for the real (non-mock) path until this feature needed it to work.
/// The existing UI convention already displays every `conn_id` as 16-hex-char
/// lowercase text (`stream-control.js`'s member-row rendering), so this just
/// makes that the wire representation too instead of a display-only
/// afterthought computed from an already-corrupted number.
mod hex_u64 {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &u64, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format!("{v:016x}"))
    }

    // `#[serde(with = "hex_u64")]` below is only ever used on `Serialize`-only
    // DTOs (server -> frontend), so this half is currently unexercised —
    // kept for symmetry/completeness of the module (and because `serde`'s
    // `with` attribute convention expects the pair) rather than deleted;
    // `dto::parse_owner` is the actual deserialize-direction helper used by
    // commands that take `owner` as a bare string argument.
    #[allow(dead_code)]
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
        let s = String::deserialize(d)?;
        u64::from_str_radix(s.trim_start_matches("0x"), 16).map_err(serde::de::Error::custom)
    }
}

/// Parse a `hex_u64`-style string (as produced above, or by
/// `stream-control.js`'s pre-existing `toString(16).padStart(16, "0")`
/// display convention) back into a real `ConnId` — the command-parameter
/// equivalent of `hex_u64::deserialize` for commands that take `owner`
/// as a bare argument rather than a struct field.
pub fn parse_owner(owner: &str) -> Result<ConnId, String> {
    u64::from_str_radix(owner.trim_start_matches("0x"), 16).map_err(|e| format!("invalid owner {owner:?}: {e}"))
}

/// `protocol::Member`, JSON-safe — see `hex_u64`'s doc comment for why
/// `conn_id` can't just be `Member`'s own `u64` field re-sent as-is.
#[derive(Serialize, Clone)]
pub struct MemberDto {
    #[serde(with = "hex_u64")]
    pub conn_id: ConnId,
    pub display_name: String,
    pub stream_count: u8,
}

impl From<&Member> for MemberDto {
    fn from(m: &Member) -> Self {
        Self { conn_id: m.conn_id, display_name: m.display_name.clone(), stream_count: m.stream_count }
    }
}

/// `protocol::StreamInfo`, JSON-safe — see `hex_u64`'s doc comment.
#[derive(Serialize, Clone)]
pub struct StreamInfoDto {
    #[serde(with = "hex_u64")]
    pub owner: ConnId,
    pub stream_id: u8,
    pub kind: simple_screensharing_client::protocol::StreamKind,
    pub label: String,
    pub width: u16,
    pub height: u16,
}

impl From<&StreamInfo> for StreamInfoDto {
    fn from(s: &StreamInfo) -> Self {
        Self { owner: s.owner, stream_id: s.stream_id, kind: s.kind, label: s.label.clone(), width: s.width, height: s.height }
    }
}

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
    /// Count of *other* members currently streaming into this room (distinct
    /// owners, mirrors `RoomSession::active_stream_count`/egui's
    /// `active_user_count`) — **not** `room.destinations.len()`, which is
    /// only this client's own outgoing streams and stays 0 whenever nobody
    /// local is sharing even though someone else in the room is.
    pub active_stream_count: u32,
    /// Mirrors `rooms::SavedRoom::auto_connect` — whether this room is
    /// rejoined automatically at app startup. Always `false` for the "live
    /// but not (yet) saved" rows `saved_room_rows` synthesizes below, since
    /// those have no `SavedRoom` entry to carry the flag.
    pub auto_connect: bool,
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
    pub members: Vec<MemberDto>,
    pub streams: Vec<StreamInfoDto>,
    pub watched: Vec<StreamKey>,
    pub listened: Vec<StreamKey>,
}

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StreamKey {
    #[serde(with = "hex_u64")]
    pub owner: ConnId,
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
    /// Audio rows only: the `capture_id` of the screen share this audio was
    /// attached to, so the frontend can render it as a row *inside* that
    /// share's card instead of as a card of its own. See
    /// `session::LocalCapture::audio_for_capture` for why the pairing is
    /// explicit rather than inferred from the room.
    pub paired_capture_id: Option<u32>,
    /// Audio rows only: whether the share is currently muted (the Mute/Unmute
    /// toggle's state). Always `false` for video.
    pub muted: bool,
}

/// A capture that's running (real screen grab, real GStreamer pipeline, real
/// negotiated resolution) but has **zero destinations** — the "Share
/// Screen"/"Share another screen" staged card, before "Go live" is clicked
/// (see `superdoc/features/stream-control.md`'s 2026-07-28 note and
/// `session::AppState::stage_video_share`/`go_live`).
///
/// `framerate`/`bitrate_kbps`/`tune`/`quality_level`/`rate_control` are the
/// *process-wide* settings the staged card's Preset/Advanced-tuning/
/// Framerate/Bitrate controls read and write (`set_framerate`/`set_bitrate`/
/// `set_tune`/`set_quality_level`/`set_rate_control` — same commands
/// `start_video_share` already used these for), not per-capture state: a
/// staged capture has no destination yet to own its own params, so there is
/// nothing more specific to show here than what "Go live" will actually use.
#[derive(Serialize, Clone)]
pub struct StagedCaptureRow {
    pub capture_id: u32,
    /// `"starting"` while waiting on the portal/pipeline, `"ready"` once the
    /// capture is up and "Go live" can be pressed, `"stopping"` while
    /// "Cancel" (`stop_share`) is tearing it down.
    pub state: &'static str,
    /// Real negotiated size, `0x0` until the pipeline reports caps (see
    /// `capture::CaptureHandle::source_caps`) — usually already valid by the
    /// time this row is first fetched.
    pub width: u32,
    pub height: u32,
    pub framerate: u32,
    pub bitrate_kbps: u32,
    pub tune: TuneMode,
    pub quality_level: QualityLevel,
    pub rate_control: RateControlMode,
}

pub fn staged_capture_rows(state: &AppState) -> Vec<StagedCaptureRow> {
    let mut rows = Vec::new();
    for (capture_id, capture) in &state.captures {
        if capture.kind != simple_screensharing_client::protocol::StreamKind::Video || !capture.destinations.is_empty() {
            continue;
        }
        let (width, height, row_state) = match &capture.status {
            session::ShareStatus::Starting(_) => (0, 0, "starting"),
            session::ShareStatus::Stopping(_) => (0, 0, "stopping"),
            session::ShareStatus::Active(session::ShareHandle::Video(handle)) => {
                let caps = handle.source_caps();
                (caps.width, caps.height, "ready")
            }
            session::ShareStatus::Active(session::ShareHandle::Audio(_)) => continue,
        };
        rows.push(StagedCaptureRow {
            capture_id: capture_id.0,
            state: row_state,
            width,
            height,
            framerate: state.framerate,
            bitrate_kbps: state.bitrate_kbps,
            tune: state.tune,
            quality_level: state.quality_level,
            rate_control: state.rate_control,
        });
    }
    rows
}

#[derive(Serialize, Clone)]
pub struct SettingsDto {
    /// The user's global display name (`rooms::AppSettings::display_name`,
    /// set via `commands::set_display_name`) — used everywhere a name is
    /// shown, including the caller's own MEMBERS row in `shell.js`'s
    /// `renderRoomPanel` (previously hardcoded to "You").
    pub display_name: String,
    pub language: String,
    pub presentation_mode: String,
    pub launch_presentation_mode: String,
    pub bandwidth_mode: String,
    pub absolute_total_kbps: u32,
    pub destination_floor_kbps: u32,
    pub max_jitter_buffer_ms: u32,
    pub catch_up_speed_percent: u32,
    pub bitrate_kbps: u32,
    pub framerate: u32,
}

/// The "START A STREAM" form's persisted last-used values (`rooms::
/// AppSettings::start_stream_defaults`), read by `get_start_stream_defaults`
/// and pre-filled into `stream-control.js`'s `state.startForm` at load —
/// see `commands::get_start_stream_defaults`/`set_start_stream_defaults`.
///
/// `tune`/`quality_level`/`rate_control` serialize as the same snake_case
/// strings `edit_destination`'s `tune`/`quality_level`/`rate_control`
/// arguments already use (`TuneMode`/`QualityLevel`/`RateControlMode` now
/// derive `Serialize` with `#[serde(rename_all = "snake_case")]` — see
/// `capture.rs`), so the frontend's Preset/Advanced-tuning controls read this
/// DTO with the exact same value set they already send back on `set_start_
/// stream_defaults`/`edit_destination`, no separate string mapping needed.
#[derive(Serialize, Clone)]
pub struct StartStreamDefaultsDto {
    pub framerate: u32,
    pub bitrate_kbps: u32,
    pub tune: TuneMode,
    pub quality_level: QualityLevel,
    pub rate_control: RateControlMode,
}

#[derive(Serialize, Clone)]
pub struct AudioSourceDto {
    pub id: u32,
    pub name: String,
}

pub fn destination_state_label(state: &DestinationState) -> &'static str {
    state.label()
}

/// `stream-stats-updated` event payload — live throughput diagnostics,
/// emitted unconditionally every driver tick (see `driver.rs`'s module
/// docs). Unlike `presence-updated`/`destination-state-changed`, this is
/// never diff-suppressed: FPS numbers change every tick, so suppressing
/// no-op ticks would defeat the point of a live readout.
#[derive(Serialize, Clone)]
pub struct StreamStatsUpdated {
    /// One entry per active capture destination (outgoing).
    pub destinations: Vec<DestinationStatsDto>,
    /// One entry per active watched stream (incoming).
    pub watched: Vec<WatchedStatsDto>,
}

/// One outgoing destination's live throughput: `captured_fps` is
/// `capture::CaptureHandle::encoder_fps` (encoded frames reaching the
/// appsink, regardless of send outcome); `sent_fps` is
/// `capture::CaptureHandle::sent_fps` (only frames that actually made it
/// onto the wire) — the two can diverge on a send error.
#[derive(Serialize, Clone)]
pub struct DestinationStatsDto {
    pub destination_id: u32,
    pub captured_fps: u32,
    pub sent_fps: u32,
    /// Audio destinations only: the live capture level, 0.0 (silence) to 1.0
    /// (full scale), straight off the capture pipeline's `level` element (see
    /// `audio::AudioLevel`). `None` for video, which has no such thing.
    /// Drives the VU meter on the screen share card's audio row.
    pub audio_level: Option<f32>,
    /// This destination's actual live encoder bitrate — `Destination::
    /// live_params().bitrate_kbps`, the same value `capture::branch::
    /// DestBranch::set_bitrate_kbps` just wrote (or the initial configured
    /// one, before any write). `None` for audio, which has no video encoder
    /// bitrate to report.
    ///
    /// `Why this needs its own field, carried on an already-unconditional-
    /// every-tick payload, rather than a new event:` in Absolute-Maximum
    /// mode, `AppState::apply_bandwidth_allocation` writes a destination's
    /// new bitrate straight into its live encoder (a tier-1 property write —
    /// see `capture::CaptureHandle::set_destination_bitrate`'s doc comment)
    /// without ever changing `DestinationState`, so `driver.rs`'s tick loop
    /// never emits `destination-state-changed` for it and `stream-
    /// control.js`'s `mockDestinations` cache — which is what the
    /// destination card's "Bitrate" readout renders from — never learns the
    /// allocator changed anything. This was the reported "the bitrate
    /// doesn't update when the Absolute Maximum total is changed" bug: the
    /// real encoder bitrate *did* change (proved for real against GStreamer
    /// by `capture::branch::tests::
    /// setting_one_branchs_bitrate_writes_through_to_that_encoder_only`, and
    /// the decision layer that decides to write it by `ui.rs`'s
    /// `bandwidth_wiring_tests`), only the frontend's cached display of it
    /// was stale — the exact same "a live change with no accompanying event"
    /// shape as `driver.rs`'s `destination-state-changed` "removed" pass
    /// (see that pass's own doc comment) and `applyDestinationEdit`'s
    /// Per-Stream-mode optimistic mirror (`stream-control.js`), which is why
    /// Per-Stream mode never showed this symptom: a user-driven edit has a
    /// call site to optimistically patch, but an allocator-driven change
    /// happening entirely inside the backend does not. Piggybacking on
    /// `stream-stats-updated` (already unconditional every ~250ms, already
    /// keyed by `destination_id` on the frontend) is cheaper and simpler
    /// than adding a new diffed event for a field that already changes at
    /// roughly the same cadence as the FPS counters it travels alongside.
    pub bitrate_kbps: Option<u32>,
}

/// One watched stream's live throughput: `received_fps` is
/// `decode::StreamDecoder::current_fps` (new frames actually yielded to the
/// UI after pacing/staleness drops, not just how often the decoder is
/// polled). Identified the same flat `room_handle`/`owner`/`stream_id` way
/// `commands::toggle_watch` takes a watch, rather than nesting a `StreamKey`
/// struct — matching that existing IPC convention rather than introducing a
/// second one.
#[derive(Serialize, Clone)]
pub struct WatchedStatsDto {
    pub room_handle: u32,
    #[serde(with = "hex_u64")]
    pub owner: ConnId,
    pub stream_id: u8,
    pub received_fps: f64,
    /// Real received throughput, not the sender's configured encoder
    /// bitrate — see `decode::ByteRateCounter`'s doc comment for why a
    /// viewer measures this off actually-arrived bytes rather than reading
    /// it from the sender (a viewer has no live channel back to the
    /// sender's own `DestinationStatsDto::bitrate_kbps`).
    pub received_bitrate_kbps: f64,
}

/// A tile's current volume/mute state, as `commands::get_volume` reports it
/// back to whichever window (View tab or a pop-out) asks — the backend
/// (`RoomSession::audio_watches` / `audio_volume_by_owner`) is the single
/// source of truth both windows read from, rather than each window's own
/// JS `tileVolume`/`tileMuted` state (which is per-renderer-process and
/// can't see the other window's changes — see this DTO's 2026-07-29 doc
/// entry in `superdoc/features/video-pipeline.md` for the bug this fixes).
#[derive(Serialize, Clone)]
pub struct VolumeStateDto {
    /// 0-200, the slider's *desired* value — unlike the value historically
    /// pushed to the decoder, this is never zeroed just because `muted` is
    /// true, so unmuting can restore it.
    pub volume_percent: u32,
    pub muted: bool,
}

pub fn pending_label(role: session::PendingRole) -> &'static str {
    match role {
        session::PendingRole::FanOut => "fan_out",
        session::PendingRole::GoingLive => "going_live",
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

/// Stable, snake_case identifiers for the three testing-oriented encoder
/// tuning knobs (`capture::TuneMode`/`QualityLevel`/`RateControlMode`) —
/// same `parse_*` convention as [`parse_bandwidth_mode`], used by
/// `edit_destination`.
pub fn parse_tune_mode(s: &str) -> Result<TuneMode, String> {
    match s {
        "low_latency" => Ok(TuneMode::LowLatency),
        "best_quality" => Ok(TuneMode::BestQuality),
        other => Err(format!("unknown tune mode: {other}")),
    }
}

pub fn parse_quality_level(s: &str) -> Result<QualityLevel, String> {
    match s {
        "performance" => Ok(QualityLevel::Performance),
        "balanced" => Ok(QualityLevel::Balanced),
        "quality" => Ok(QualityLevel::Quality),
        other => Err(format!("unknown quality level: {other}")),
    }
}

pub fn parse_rate_control_mode(s: &str) -> Result<RateControlMode, String> {
    match s {
        "constant" => Ok(RateControlMode::Constant),
        "adaptive" => Ok(RateControlMode::Adaptive),
        "quality" => Ok(RateControlMode::Quality),
        other => Err(format!("unknown rate control mode: {other}")),
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
                active_stream_count: room.active_stream_count() as u32,
                auto_connect: saved.auto_connect,
            },
            None if state.is_reconnecting(&saved.join_string) => SavedRoomRow {
                name: saved.name.clone(),
                join_string: saved.join_string.clone(),
                state: RoomRowState::Reconnecting,
                room_handle: None,
                active_stream_count: 0,
                auto_connect: saved.auto_connect,
            },
            None => SavedRoomRow {
                name: saved.name.clone(),
                join_string: saved.join_string.clone(),
                state: RoomRowState::Disconnected,
                room_handle: None,
                active_stream_count: 0,
                auto_connect: saved.auto_connect,
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
                active_stream_count: room.active_stream_count() as u32,
                auto_connect: false,
            });
        }
    }
    // A first-time join with no typed room name never got upserted into
    // `room_store` before connecting (`join_room_impl` only pre-seeds a name
    // when the caller passed a non-empty one) — without this, such a connect
    // has no row at all to show "Reconnecting…" on, and therefore nothing to
    // right-click Disconnect on. Synthesize one from `pending_connect`
    // itself, same id-derived-name fallback `poll_connect_result` uses on
    // success, purely for display: this never touches `room_store`, so it
    // stays gone the instant the connect resolves either way (nothing to
    // clean up).
    if let Some(pending) = &state.pending_connect {
        if state.room_store.name_for(&pending.join_string).is_none() {
            rows.push(SavedRoomRow {
                name: session::room_label(&state.room_store, &pending.join_string),
                join_string: pending.join_string.clone(),
                state: RoomRowState::Reconnecting,
                room_handle: None,
                active_stream_count: 0,
                auto_connect: false,
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
        members: room.members.iter().map(MemberDto::from).collect(),
        streams: room.streams.iter().map(StreamInfoDto::from).collect(),
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
                state: destination_state_label(&capture.destination_state(dest)),
                pending: dest.pending.map(pending_label),
                width: params.width,
                height: params.height,
                framerate: params.framerate,
                bitrate_kbps: params.bitrate_kbps,
                paired_capture_id: capture.audio_for_capture.map(|c| c.0),
                muted: state.is_audio_share_muted(*capture_id),
            });
        }
    }
    rows
}

pub fn settings_dto(state: &AppState) -> SettingsDto {
    SettingsDto {
        display_name: state.display_name.clone(),
        language: state.language.clone(),
        presentation_mode: presentation_mode_key(state.presentation_mode).to_string(),
        launch_presentation_mode: presentation_mode_key(state.launch_presentation_mode).to_string(),
        bandwidth_mode: bandwidth_mode_str(state.bandwidth_mode).to_string(),
        absolute_total_kbps: state.absolute_total_kbps,
        destination_floor_kbps: state.destination_floor_kbps,
        max_jitter_buffer_ms: state.max_jitter_buffer_ms,
        catch_up_speed_percent: state.catch_up_speed_percent,
        bitrate_kbps: state.bitrate_kbps,
        framerate: state.framerate,
    }
}
