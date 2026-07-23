//! egui UI: room join screen, presence roster, screen sharing, and stream
//! watching.
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

use std::collections::HashMap;

use tokio::sync::{mpsc, oneshot};

use crate::audio::{self, AudioCaptureHandle, AudioSource};
use crate::capture::{self, CaptureHandle};
use crate::decode::StreamDecoder;
use crate::net::{ClientEvent, PresenceSnapshotState, ScreenshareClient};
use crate::protocol::{ConnId, Member, PresenceEvent, StreamId, StreamInfo, StreamKind};
use crate::rooms::RoomStore;

type ConnectResult = Result<(ScreenshareClient, PresenceSnapshotState, mpsc::UnboundedReceiver<ClientEvent>), String>;

/// A reasonable default encode bitrate, preselected on the bitrate slider.
const DEFAULT_BITRATE_KBPS: u32 = 8000;
/// Slider bounds, matching the project's 40 Mbit/s ceiling.
const MIN_BITRATE_KBPS: u32 = 1000;
const MAX_BITRATE_KBPS: u32 = 40000;

/// Either kind of running outbound share this client can hold onto.
enum ShareHandle {
    Video(CaptureHandle),
    Audio(AudioCaptureHandle),
}

/// Per-share state machine. Starting/stopping are async
/// (`capture::start_sharing`/`audio::start_audio_sharing`, and each handle's
/// `.stop()`), so each transition is spawned and its result delivered back
/// via a oneshot, mirroring `Status`/`connect_rx` above.
enum ShareStatus {
    Starting(oneshot::Receiver<Result<ShareHandle, String>>),
    Active(ShareHandle),
    Stopping(oneshot::Receiver<Result<(), String>>),
}

/// One outbound share this connection owns (video or audio), keyed by its
/// `stream_id` in `Connected::active_shares`. Multiple can be active at
/// once — e.g. two monitors, or a video share plus an audio share.
struct ActiveShare {
    kind: StreamKind,
    /// Human-readable label for this share's row: "Screen share" for video,
    /// the source's name for audio.
    label: String,
    status: ShareStatus,
}

/// One stream currently being watched: its decoder, plus the texture handle
/// (created lazily on the first decoded frame, then updated in place).
struct WatchEntry {
    decoder: StreamDecoder,
    texture: Option<egui::TextureHandle>,
}

enum Status {
    Idle,
    Connecting,
    Error(String),
}

/// State once `connect()` has succeeded.
struct Connected {
    client: ScreenshareClient,
    event_rx: mpsc::UnboundedReceiver<ClientEvent>,
    members: Vec<Member>,
    streams: Vec<StreamInfo>,
    save_name: String,
    /// Set once the user has saved this room, so we don't offer to re-save.
    saved: bool,

    /// This connection's own outbound shares (video and/or audio), keyed by
    /// stream_id. Several can be active concurrently.
    active_shares: HashMap<StreamId, ActiveShare>,
    share_error: Option<String>,
    /// Single per-connection counter for allocating our own `stream_id`s,
    /// shared across video and audio shares (protocol nonce-domain
    /// separation is per stream_id, not per media kind).
    next_stream_id: StreamId,
    /// Bitrate (kbps) to use for the *next* video share started.
    bitrate_kbps: u32,

    /// Audio sources discovered by the last `list_audio_sources()` call.
    audio_sources: Vec<AudioSource>,
    audio_sources_error: Option<String>,
    /// Set once we've kicked off the first automatic listing, so we don't
    /// re-trigger it every frame.
    audio_sources_requested: bool,
    /// In-flight `list_audio_sources()` call, if any.
    audio_sources_rx: Option<oneshot::Receiver<Result<Vec<AudioSource>, String>>>,
    selected_audio_source: Option<AudioSource>,

    /// Streams we're currently subscribed to and decoding, keyed by
    /// (owner conn_id, stream_id).
    watches: HashMap<(ConnId, StreamId), WatchEntry>,
    watch_error: Option<String>,
    /// Which watched stream the central video panel currently displays.
    selected_watch: Option<(ConnId, StreamId)>,
}

pub struct App {
    join_string: String,
    display_name: String,
    status: Status,
    connect_rx: Option<oneshot::Receiver<ConnectResult>>,
    connected: Option<Connected>,
    room_store: RoomStore,
    room_store_error: Option<String>,
}

impl Default for App {
    fn default() -> Self {
        let (room_store, room_store_error) = match RoomStore::load() {
            Ok(store) => (store, None),
            Err(e) => (RoomStore::default(), Some(e.to_string())),
        };
        Self {
            join_string: String::new(),
            display_name: whoami_fallback(),
            status: Status::Idle,
            connect_rx: None,
            connected: None,
            room_store,
            room_store_error,
        }
    }
}

/// A small local fallback so the display-name field isn't empty by default.
/// Not a real identity system — just a nicer starting point than "".
fn whoami_fallback() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "friend".to_string())
}

impl Connected {
    /// "Share Screen" clicked: allocate a fresh stream id and spawn
    /// `capture::start_sharing` onto the ambient tokio runtime. Can be
    /// called again while other shares (video or audio) are already active —
    /// each call prompts the OS portal picker again, so a second click lets
    /// the user pick a different monitor/window.
    fn start_video_share(&mut self) {
        let stream_id = self.next_stream_id;
        self.next_stream_id = self.next_stream_id.wrapping_add(1);

        let client = self.client.clone();
        let bitrate_kbps = self.bitrate_kbps;
        let (tx, rx) = oneshot::channel();
        self.active_shares.insert(
            stream_id,
            ActiveShare {
                kind: StreamKind::Video,
                label: "Screen share".to_string(),
                status: ShareStatus::Starting(rx),
            },
        );
        self.share_error = None;

        tokio::spawn(async move {
            let result = capture::start_sharing(&client, stream_id, bitrate_kbps)
                .await
                .map(ShareHandle::Video)
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
        });
    }

    /// "Share Audio" clicked: allocate a fresh stream id and spawn
    /// `audio::start_audio_sharing` for the currently selected source.
    /// No-op if no source is selected (the button is disabled in that case).
    fn start_audio_share(&mut self) {
        let Some(source) = self.selected_audio_source.clone() else {
            return;
        };
        let stream_id = self.next_stream_id;
        self.next_stream_id = self.next_stream_id.wrapping_add(1);

        let client = self.client.clone();
        let (tx, rx) = oneshot::channel();
        self.active_shares.insert(
            stream_id,
            ActiveShare {
                kind: StreamKind::Audio,
                label: source.name.clone(),
                status: ShareStatus::Starting(rx),
            },
        );
        self.share_error = None;

        tokio::spawn(async move {
            let result = audio::start_audio_sharing(&client, stream_id, &source)
                .await
                .map(ShareHandle::Audio)
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
        });
    }

    /// "Stop"/"Stop Sharing Audio" clicked for `stream_id`: hand the
    /// underlying handle off to a spawned task that tears it down
    /// (`.stop()` is async on both `CaptureHandle` and `AudioCaptureHandle`).
    fn stop_share(&mut self, stream_id: StreamId) {
        let Some(share) = self.active_shares.get_mut(&stream_id) else {
            return;
        };
        if !matches!(share.status, ShareStatus::Active(_)) {
            return;
        }
        let (tx, rx) = oneshot::channel();
        let ShareStatus::Active(handle) = std::mem::replace(&mut share.status, ShareStatus::Stopping(rx)) else {
            unreachable!("just matched Active above");
        };

        tokio::spawn(async move {
            let result = match handle {
                ShareHandle::Video(h) => h.stop().await,
                ShareHandle::Audio(h) => h.stop().await,
            }
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
        });
    }

    /// Poll every in-flight share start/stop, if any. Call once per frame.
    fn poll_share_states(&mut self) {
        let mut to_remove = Vec::new();
        for (stream_id, share) in self.active_shares.iter_mut() {
            match &mut share.status {
                ShareStatus::Starting(rx) => match rx.try_recv() {
                    Ok(Ok(handle)) => share.status = ShareStatus::Active(handle),
                    Ok(Err(e)) => {
                        self.share_error = Some(e);
                        to_remove.push(*stream_id);
                    }
                    Err(oneshot::error::TryRecvError::Empty) => {}
                    Err(oneshot::error::TryRecvError::Closed) => {
                        self.share_error = Some("share-start task ended unexpectedly".to_string());
                        to_remove.push(*stream_id);
                    }
                },
                ShareStatus::Stopping(rx) => match rx.try_recv() {
                    Ok(Ok(())) => to_remove.push(*stream_id),
                    Ok(Err(e)) => {
                        self.share_error = Some(e);
                        to_remove.push(*stream_id);
                    }
                    Err(oneshot::error::TryRecvError::Empty) => {}
                    Err(oneshot::error::TryRecvError::Closed) => {
                        self.share_error = Some("share-stop task ended unexpectedly".to_string());
                        to_remove.push(*stream_id);
                    }
                },
                ShareStatus::Active(_) => {}
            }
        }
        for stream_id in to_remove {
            self.active_shares.remove(&stream_id);
        }
    }

    /// Kick off (or re-kick-off) an `audio::list_audio_sources()` call on
    /// the ambient tokio runtime. That function shells out to `pw-dump`
    /// synchronously, so it's run via `spawn_blocking` rather than directly
    /// in the async task, matching the non-blocking intent of the rest of
    /// this module's spawn pattern.
    fn refresh_audio_sources(&mut self) {
        let (tx, rx) = oneshot::channel();
        self.audio_sources_rx = Some(rx);
        self.audio_sources_error = None;

        tokio::spawn(async move {
            let result = match tokio::task::spawn_blocking(audio::list_audio_sources).await {
                Ok(Ok(sources)) => Ok(sources),
                Ok(Err(e)) => Err(format!("{e:#}")),
                Err(e) => Err(format!("audio source listing task panicked: {e}")),
            };
            let _ = tx.send(result);
        });
    }

    /// Poll the in-flight `list_audio_sources()` call, if any. Call once per
    /// frame.
    fn poll_audio_sources(&mut self) {
        let Some(rx) = &mut self.audio_sources_rx else { return };
        match rx.try_recv() {
            Ok(Ok(sources)) => {
                self.audio_sources = sources;
                self.audio_sources_rx = None;
            }
            Ok(Err(e)) => {
                self.audio_sources_error = Some(e);
                self.audio_sources_rx = None;
            }
            Err(oneshot::error::TryRecvError::Empty) => {}
            Err(oneshot::error::TryRecvError::Closed) => {
                self.audio_sources_error = Some("audio source listing task ended unexpectedly".to_string());
                self.audio_sources_rx = None;
            }
        }
    }

    /// "Watch"/"Stop Watching" clicked for `(owner, stream_id)`. Subscribing
    /// and unsubscribing are async (control-plane round trip) so they're
    /// fired onto the runtime and not awaited here; the decoder itself is
    /// created/dropped synchronously since that's cheap (spawns/joins a
    /// local decode thread, no network wait).
    fn toggle_watch(&mut self, owner: ConnId, stream_id: StreamId) {
        let key = (owner, stream_id);
        if self.watches.remove(&key).is_some() {
            if self.selected_watch == Some(key) {
                self.selected_watch = self.watches.keys().next().copied();
            }
            let client = self.client.clone();
            tokio::spawn(async move {
                if let Err(e) = client.unsubscribe(owner, stream_id).await {
                    tracing::warn!("unsubscribe({owner:016x}, {stream_id}) failed: {e:#}");
                }
            });
        } else {
            match StreamDecoder::new(stream_id) {
                Ok(decoder) => {
                    self.watches.insert(key, WatchEntry { decoder, texture: None });
                    self.selected_watch = Some(key);
                    self.watch_error = None;

                    let client = self.client.clone();
                    tokio::spawn(async move {
                        if let Err(e) = client.subscribe(owner, stream_id).await {
                            tracing::warn!("subscribe({owner:016x}, {stream_id}) failed: {e:#}");
                        }
                    });
                }
                Err(e) => {
                    self.watch_error = Some(format!("{e:#}"));
                }
            }
        }
    }

    /// Upload any freshly decoded frames to their stream's texture. Call
    /// once per repaint; cheap no-op when nothing new has decoded.
    fn update_watch_textures(&mut self, ctx: &egui::Context) {
        for ((owner, stream_id), entry) in self.watches.iter_mut() {
            if let Some(image) = entry.decoder.latest_frame() {
                match &mut entry.texture {
                    Some(tex) => tex.set(image, egui::TextureOptions::LINEAR),
                    None => {
                        let name = format!("stream-{owner:016x}-{stream_id}");
                        entry.texture = Some(ctx.load_texture(name, image, egui::TextureOptions::LINEAR));
                    }
                }
            }
        }
    }
}

impl App {
    fn start_connect(&mut self) {
        let join_string = self.join_string.clone();
        let display_name = self.display_name.clone();
        let (tx, rx) = oneshot::channel();
        self.connect_rx = Some(rx);
        self.status = Status::Connecting;

        tokio::spawn(async move {
            let result = ScreenshareClient::connect(&join_string, &display_name)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
        });
    }

    fn poll_connect_result(&mut self) {
        let Some(rx) = &mut self.connect_rx else { return };
        match rx.try_recv() {
            Ok(Ok((client, snapshot, event_rx))) => {
                self.status = Status::Idle;
                self.connect_rx = None;
                self.connected = Some(Connected {
                    client,
                    event_rx,
                    members: snapshot.members,
                    streams: snapshot.streams,
                    save_name: String::new(),
                    saved: false,
                    active_shares: HashMap::new(),
                    share_error: None,
                    next_stream_id: 0,
                    bitrate_kbps: DEFAULT_BITRATE_KBPS,
                    audio_sources: Vec::new(),
                    audio_sources_error: None,
                    audio_sources_requested: false,
                    audio_sources_rx: None,
                    selected_audio_source: None,
                    watches: HashMap::new(),
                    watch_error: None,
                    selected_watch: None,
                });
            }
            Ok(Err(e)) => {
                self.status = Status::Error(e);
                self.connect_rx = None;
            }
            Err(oneshot::error::TryRecvError::Empty) => {}
            Err(oneshot::error::TryRecvError::Closed) => {
                self.status = Status::Error("connection task ended unexpectedly".to_string());
                self.connect_rx = None;
            }
        }
    }

    fn drain_events(&mut self) {
        let Some(connected) = &mut self.connected else { return };
        while let Ok(event) = connected.event_rx.try_recv() {
            match event {
                ClientEvent::PresenceDelta(PresenceEvent::MemberJoined(member)) => {
                    connected.members.retain(|m| m.conn_id != member.conn_id);
                    connected.members.push(member);
                }
                ClientEvent::PresenceDelta(PresenceEvent::MemberLeft { conn_id }) => {
                    connected.members.retain(|m| m.conn_id != conn_id);
                }
                ClientEvent::PresenceDelta(PresenceEvent::StreamStarted(info)) => {
                    if let Some(m) = connected.members.iter_mut().find(|m| m.conn_id == info.owner) {
                        m.stream_count = m.stream_count.saturating_add(1);
                    }
                    connected.streams.retain(|s| !(s.owner == info.owner && s.stream_id == info.stream_id));
                    connected.streams.push(info);
                }
                ClientEvent::PresenceDelta(PresenceEvent::StreamStopped { conn_id, stream_id }) => {
                    if let Some(m) = connected.members.iter_mut().find(|m| m.conn_id == conn_id) {
                        m.stream_count = m.stream_count.saturating_sub(1);
                    }
                    connected.streams.retain(|s| !(s.owner == conn_id && s.stream_id == stream_id));
                }
                ClientEvent::SubscriberCount { .. } | ClientEvent::KeyframeRequest { .. } => {
                    // Relevant once this client can be a sender (capture.rs, later).
                }
                ClientEvent::MediaFrame(frame) => {
                    if let Some(entry) = connected.watches.get_mut(&(frame.conn_id, frame.stream_id)) {
                        entry.decoder.feed(&frame);
                    }
                }
                ClientEvent::Preview(_) => {
                    // No preview UI in this pass; only the full decode path is wired up.
                }
                ClientEvent::ControlClosed(reason) => {
                    self.status = Status::Error(format!("disconnected: {reason}"));
                    self.connected = None;
                    return;
                }
            }
        }
    }

    fn ui_join_screen(&mut self, ui: &mut egui::Ui) {
        ui.heading("Simple Screensharing");
        ui.add_space(8.0);

        ui.label("Display name");
        ui.text_edit_singleline(&mut self.display_name);
        ui.add_space(4.0);

        ui.label("Join link (server|room-id|key)");
        ui.text_edit_singleline(&mut self.join_string);
        ui.add_space(8.0);

        let connecting = matches!(self.status, Status::Connecting);
        ui.add_enabled_ui(!connecting && !self.join_string.trim().is_empty(), |ui| {
            if ui.button("Join").clicked() {
                self.start_connect();
            }
        });

        match &self.status {
            Status::Idle => {}
            Status::Connecting => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Connecting…");
                });
            }
            Status::Error(e) => {
                ui.colored_label(egui::Color32::RED, format!("Error: {e}"));
            }
        }

        ui.add_space(12.0);
        ui.separator();
        ui.label("Saved lounges");
        if let Some(err) = &self.room_store_error {
            ui.colored_label(egui::Color32::RED, format!("Could not load saved rooms: {err}"));
        }
        if self.room_store.rooms.is_empty() {
            ui.label("(none yet)");
        } else {
            let mut to_join: Option<String> = None;
            let mut to_remove: Option<String> = None;
            for room in &self.room_store.rooms {
                ui.horizontal(|ui| {
                    ui.label(&room.name);
                    if ui.small_button("Join").clicked() {
                        to_join = Some(room.join_string.clone());
                    }
                    if ui.small_button("Remove").clicked() {
                        to_remove = Some(room.name.clone());
                    }
                });
            }
            if let Some(join_string) = to_join {
                self.join_string = join_string;
                self.start_connect();
            }
            if let Some(name) = to_remove {
                self.room_store.remove(&name);
                if let Err(e) = self.room_store.save() {
                    self.room_store_error = Some(e.to_string());
                }
            }
        }
    }

    fn ui_connected_screen(&mut self, ui: &mut egui::Ui) {
        let Some(connected) = &mut self.connected else { return };

        ui.heading("Simple Screensharing");
        ui.label(format!(
            "Connected as {} (conn_id {:016x})",
            self.display_name,
            connected.client.conn_id()
        ));
        ui.add_space(8.0);

        ui.horizontal(|ui| {
            ui.label("Save this room as:");
            ui.text_edit_singleline(&mut connected.save_name);
            let can_save = !connected.save_name.trim().is_empty();
            if ui.add_enabled(can_save, egui::Button::new("Save")).clicked() {
                self.room_store.upsert(connected.save_name.clone(), self.join_string.clone());
                if let Err(e) = self.room_store.save() {
                    self.room_store_error = Some(e.to_string());
                } else {
                    connected.saved = true;
                }
            }
        });
        if connected.saved {
            ui.colored_label(egui::Color32::GREEN, "Saved.");
        }

        ui.add_space(12.0);
        ui.separator();
        ui.label(format!("Members ({})", connected.members.len()));
        egui::ScrollArea::vertical().max_height(120.0).show(ui, |ui| {
            // Show all members, not just streamers (spec §8/§4.2: a lurker
            // with stream_count == 0 still appears in the roster).
            for member in &connected.members {
                ui.horizontal(|ui| {
                    if member.stream_count > 0 {
                        ui.colored_label(egui::Color32::from_rgb(80, 200, 120), "\u{25cf}");
                        ui.label(format!(
                            "{} ({} stream{})",
                            member.display_name,
                            member.stream_count,
                            if member.stream_count == 1 { "" } else { "s" }
                        ));
                    } else {
                        ui.colored_label(egui::Color32::GRAY, "\u{25cb}");
                        ui.label(&member.display_name);
                    }
                });
            }
        });

        ui.add_space(12.0);
        ui.separator();
        ui.label("Share video");
        ui.horizontal(|ui| {
            ui.label("Bitrate (kbps):");
            ui.add(egui::Slider::new(&mut connected.bitrate_kbps, MIN_BITRATE_KBPS..=MAX_BITRATE_KBPS));
        });
        ui.horizontal(|ui| {
            // Always enabled: clicking again while a video share is already
            // active starts *another* one (a second stream_id), re-prompting
            // the OS portal picker so the user can pick a different
            // monitor/window.
            if ui.button("Share Screen").clicked() {
                connected.start_video_share();
            }
            let starting_video = connected
                .active_shares
                .values()
                .any(|s| s.kind == StreamKind::Video && matches!(s.status, ShareStatus::Starting(_)));
            if starting_video {
                ui.spinner();
                ui.label("Waiting for screen-share permission…");
            }
        });

        ui.add_space(12.0);
        ui.separator();
        ui.label("Share audio");
        ui.horizontal(|ui| {
            if !connected.audio_sources_requested {
                connected.audio_sources_requested = true;
                connected.refresh_audio_sources();
            }
            if ui.button("Refresh").clicked() {
                connected.refresh_audio_sources();
            }
            if connected.audio_sources_rx.is_some() {
                ui.spinner();
            }
        });
        ui.horizontal(|ui| {
            let selected_text = connected
                .selected_audio_source
                .as_ref()
                .map(|s| s.name.as_str())
                .unwrap_or("(select a source)");
            egui::ComboBox::from_label("Source")
                .selected_text(selected_text)
                .show_ui(ui, |ui| {
                    for source in &connected.audio_sources {
                        let selected = connected.selected_audio_source.as_ref() == Some(source);
                        if ui.selectable_label(selected, &source.name).clicked() {
                            connected.selected_audio_source = Some(source.clone());
                        }
                    }
                });
            let can_share_audio = connected.selected_audio_source.is_some();
            if ui.add_enabled(can_share_audio, egui::Button::new("Share Audio")).clicked() {
                connected.start_audio_share();
            }
        });
        if connected.audio_sources.is_empty() && connected.audio_sources_rx.is_none() {
            ui.label("(no audio sources found — click Refresh once something is playing)");
        }
        if let Some(err) = &connected.audio_sources_error {
            ui.colored_label(egui::Color32::RED, format!("Audio source list error: {err}"));
        }

        ui.add_space(12.0);
        ui.separator();
        ui.label("Active shares");
        if connected.active_shares.is_empty() {
            ui.label("(none)");
        } else {
            let mut stream_ids: Vec<StreamId> = connected.active_shares.keys().copied().collect();
            stream_ids.sort_unstable();
            let mut to_stop = Vec::new();
            for stream_id in stream_ids {
                let share = &connected.active_shares[&stream_id];
                let kind_label = match share.kind {
                    StreamKind::Video => "Video",
                    StreamKind::Audio => "Audio",
                };
                let (state_label, stoppable) = match &share.status {
                    ShareStatus::Starting(_) => ("Starting…", false),
                    ShareStatus::Active(_) => ("Active", true),
                    ShareStatus::Stopping(_) => ("Stopping…", false),
                };
                ui.horizontal(|ui| {
                    ui.label(format!("[{stream_id}] {kind_label}: {} — {state_label}", share.label));
                    let stop_label = if share.kind == StreamKind::Audio { "Stop Sharing Audio" } else { "Stop Sharing" };
                    if ui.add_enabled(stoppable, egui::Button::new(stop_label)).clicked() {
                        to_stop.push(stream_id);
                    }
                });
            }
            for stream_id in to_stop {
                connected.stop_share(stream_id);
            }
        }
        if let Some(err) = &connected.share_error {
            ui.colored_label(egui::Color32::RED, format!("Share error: {err}"));
        }

        ui.add_space(12.0);
        ui.separator();
        ui.label("Streams");
        if let Some(err) = &connected.watch_error {
            ui.colored_label(egui::Color32::RED, format!("Watch error: {err}"));
        }
        if connected.streams.is_empty() {
            ui.label("(none active)");
        } else {
            let mut to_toggle: Option<(ConnId, StreamId)> = None;
            for info in &connected.streams {
                if info.kind != StreamKind::Video {
                    // decode.rs only handles video in this pass.
                    continue;
                }
                let key = (info.owner, info.stream_id);
                let watching = connected.watches.contains_key(&key);
                let owner_name = connected
                    .members
                    .iter()
                    .find(|m| m.conn_id == info.owner)
                    .map(|m| m.display_name.as_str())
                    .unwrap_or("unknown");
                ui.horizontal(|ui| {
                    ui.label(format!("{owner_name} — {} ({}x{})", info.label, info.width, info.height));
                    let button_label = if watching { "Stop Watching" } else { "Watch" };
                    if ui.small_button(button_label).clicked() {
                        to_toggle = Some(key);
                    }
                    if connected.selected_watch == Some(key) {
                        ui.colored_label(egui::Color32::from_rgb(80, 200, 120), "(viewing)");
                    }
                });
            }
            if let Some((owner, stream_id)) = to_toggle {
                connected.toggle_watch(owner, stream_id);
            }
        }

        ui.add_space(12.0);
        ui.separator();
        ui.label("Video");
        match connected.selected_watch.and_then(|key| connected.watches.get(&key)) {
            Some(entry) => match &entry.texture {
                Some(tex) => {
                    let available = ui.available_size();
                    let tex_size = tex.size_vec2();
                    let scale = if tex_size.x > 0.0 && tex_size.y > 0.0 {
                        (available.x / tex_size.x).min(available.y.max(60.0) / tex_size.y).min(1.0)
                    } else {
                        1.0
                    };
                    let display_size = tex_size * scale.max(0.01);
                    ui.image((tex.id(), display_size));
                }
                None => {
                    ui.label("Waiting for first decoded frame…");
                }
            },
            None => {
                ui.label("(not watching any stream)");
            }
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_connect_result();
        self.drain_events();
        if let Some(connected) = &mut self.connected {
            connected.poll_share_states();
            connected.poll_audio_sources();
            connected.update_watch_textures(ctx);
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            if self.connected.is_some() {
                self.ui_connected_screen(ui);
            } else {
                self.ui_join_screen(ui);
            }
        });

        // Keep polling the connect future / event channel even though egui
        // has no input to react to yet.
        if matches!(self.status, Status::Connecting) || self.connected.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
    }
}
