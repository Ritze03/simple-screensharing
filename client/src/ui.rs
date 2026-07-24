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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, oneshot};

use crate::audio::{self, AudioCaptureHandle, AudioSource};
use crate::capture::{self, CaptureHandle};
use crate::decode::{self, AudioDecoder, PreviewCache, StreamDecoder};
use crate::net::{ClientEvent, PresenceSnapshotState, ScreenshareClient};
use crate::protocol::{ConnId, Member, PresenceEvent, StreamId, StreamInfo, StreamKind};
use crate::rooms::RoomStore;

type ConnectResult = Result<(ScreenshareClient, PresenceSnapshotState, mpsc::UnboundedReceiver<ClientEvent>), String>;

/// A reasonable default encode bitrate, preselected on the bitrate slider.
const DEFAULT_BITRATE_KBPS: u32 = 8000;
/// Slider bounds, matching the project's 40 Mbit/s ceiling.
const MIN_BITRATE_KBPS: u32 = 1000;
const MAX_BITRATE_KBPS: u32 = 40000;

/// Default capture framerate, preselected on the framerate dropdown —
/// mirrors `capture::DEFAULT_FRAMERATE` (kept as a local constant, like
/// `DEFAULT_BITRATE_KBPS` above, rather than importing it, for consistency
/// with the existing bitrate constants' pattern in this module). Deliberately
/// conservative: see `capture::DEFAULT_FRAMERATE`'s doc comment for why 30
/// (matching this pipeline's framerate before it became configurable) beats
/// defaulting straight to the 240Hz ceiling.
const DEFAULT_FRAMERATE: u32 = 30;
/// Common refresh rates offered on the framerate dropdown, up to the
/// project's 240Hz desktop-client ceiling (`capture::MAX_FRAMERATE`) — more
/// natural as discrete choices than a continuous slider, since these are the
/// values that actually correspond to real monitor refresh rates.
const FRAMERATE_OPTIONS: [u32; 7] = [30, 60, 90, 120, 144, 165, 240];

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
///
/// The decoder is behind an `Arc<Mutex<_>>` rather than owned outright
/// because, once popped out (see `popout`), it needs to be polled from
/// inside a `show_viewport_deferred` closure — those closures are `Fn`
/// (callable repeatedly, possibly from the egui backend's own bookkeeping)
/// rather than `FnMut`, so any state they mutate has to live behind shared
/// interior mutability rather than being captured by unique reference. The
/// same `Arc` is cloned into the popout closure while this struct keeps its
/// own handle, so `drain_events`' `feed()` calls keep working unchanged
/// regardless of popped-out state.
struct WatchEntry {
    decoder: Arc<Mutex<StreamDecoder>>,
    texture: Option<egui::TextureHandle>,
    /// `Some` while this stream is displayed in its own native window
    /// instead of the central panel.
    popout: Option<PopoutState>,
    /// The stream_id of the same sender's audio stream, if one was active
    /// when this video watch started (see `Connected::toggle_watch`'s
    /// auto-subscribe-audio behavior) and got auto-subscribed alongside it.
    /// Looked up in `Connected::audio_watches` to render the volume slider
    /// next to the video panel and to tear the audio watch down together
    /// with this one. `None` if the sender had no audio stream announced at
    /// watch time.
    audio_stream_id: Option<StreamId>,
}

/// One audio stream currently being decoded/played, whether auto-subscribed
/// alongside a video `WatchEntry` (see `WatchEntry::audio_stream_id`) or
/// started standalone via a "Listen" click on an audio-only stream (no
/// video from the same sender). Keyed the same way as `watches`, in
/// `Connected::audio_watches`.
struct AudioWatch {
    decoder: AudioDecoder,
    /// 0-200, the value bound to this stream's volume slider. Applied to
    /// `decoder` (as `volume_percent / 100.0`) whenever the slider changes.
    volume_percent: u32,
}

/// State for a `WatchEntry` currently rendered in its own
/// `show_viewport_deferred` window rather than the central panel.
struct PopoutState {
    /// The popout window's own texture, decoupled from `WatchEntry::texture`
    /// so the inline central panel and the popout window can each hold the
    /// most recent frame decoded at their own repaint cadence without
    /// stepping on each other. Wrapped for the same `Fn`-closure reason as
    /// `WatchEntry::decoder`.
    texture: Arc<Mutex<Option<egui::TextureHandle>>>,
    /// Set from inside the popout's `viewport_ui_cb` when it observes
    /// `ViewportInfo::close_requested()` (OS close button, Alt+F4, etc.).
    /// Polled once per main-thread frame in `show_popouts` so the entry can
    /// be un-popped-out (returned to the central panel) instead of just
    /// leaking into a window that no longer repaints.
    close_requested: Arc<AtomicBool>,
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
    /// Capture framerate cap (fps) to use for the *next* video share
    /// started — see `capture::build_pipeline`'s `videorate` cap and
    /// `key-int-max` scaling.
    framerate: u32,

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

    /// Audio streams we're currently subscribed to and playing, keyed by
    /// (owner conn_id, audio stream_id) — the audio stream's own id, not the
    /// paired video's. Populated both by watching a video stream whose
    /// sender also has an active audio stream (auto-subscribe, see
    /// `toggle_watch`) and by clicking "Listen" on an audio-only stream (see
    /// `toggle_listen`).
    audio_watches: HashMap<(ConnId, StreamId), AudioWatch>,

    /// Latest decoded preview still per active stream, regardless of
    /// whether it's being watched — previews arrive for every active
    /// stream (spec design), so the "Streams" list row for any of them can
    /// show a thumbnail once one has arrived.
    preview_cache: PreviewCache,
    /// Lazily created/updated texture per stream for the "Streams" list
    /// thumbnail, keyed the same way as `preview_cache`/`watches`. Mirrors
    /// `WatchEntry::texture`'s create-once/`set`-in-place pattern.
    preview_textures: HashMap<(ConnId, StreamId), egui::TextureHandle>,
}

pub struct App {
    join_string: String,
    display_name: String,
    status: Status,
    connect_rx: Option<oneshot::Receiver<ConnectResult>>,
    connected: Option<Connected>,
    room_store: RoomStore,
    room_store_error: Option<String>,
    /// Current UI language ("en" or "de"), mirrored into `rust_i18n`'s
    /// global locale on startup and on every toggle. Kept here too (rather
    /// than re-reading `rust_i18n::locale()` each frame) so the language
    /// `ComboBox` has a plain `&mut String` to bind to.
    language: String,
}

impl Default for App {
    fn default() -> Self {
        let (room_store, room_store_error) = match RoomStore::load() {
            Ok(store) => (store, None),
            Err(e) => (RoomStore::default(), Some(e.to_string())),
        };
        let language = crate::rooms::AppSettings::load().language;
        rust_i18n::set_locale(&language);
        Self {
            join_string: String::new(),
            display_name: whoami_fallback(),
            status: Status::Idle,
            connect_rx: None,
            connected: None,
            room_store,
            room_store_error,
            language,
        }
    }
}

/// Persist the current language choice, logging (not panicking) on failure —
/// losing the language preference across restarts is a minor inconvenience,
/// not worth surfacing as a UI error.
fn save_language(language: &str) {
    let settings = crate::rooms::AppSettings { language: language.to_string() };
    if let Err(e) = settings.save() {
        tracing::warn!("failed to save language preference: {e:#}");
    }
}

/// A small local fallback so the display-name field isn't empty by default.
/// Not a real identity system — just a nicer starting point than "".
fn whoami_fallback() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| t!("common.default_display_name").to_string())
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
        let framerate = self.framerate;
        let (tx, rx) = oneshot::channel();
        self.active_shares.insert(
            stream_id,
            ActiveShare {
                kind: StreamKind::Video,
                label: t!("share.video_label").to_string(),
                status: ShareStatus::Starting(rx),
            },
        );
        self.share_error = None;

        tokio::spawn(async move {
            let result = capture::start_sharing(&client, stream_id, bitrate_kbps, framerate)
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

    /// Abrupt teardown (relay/control connection lost, or the app quitting
    /// while sharing): fire off `.stop()` for every currently `Active` share
    /// so the portal session gets closed and the relay gets a stop-stream
    /// notification, instead of just letting the handles drop synchronously
    /// when `Connected`/`App` goes away right after this call.
    ///
    /// Mirrors `stop_share`'s spawn pattern exactly, but for every active
    /// share at once and without waiting to see the result — the caller is
    /// about to discard this `Connected` (or the whole app is exiting)
    /// regardless of whether the stop finishes, so there is nothing
    /// meaningful left to poll it into. `Starting`/`Stopping` shares are
    /// left alone: a `Starting` share has no handle yet to stop, and a
    /// `Stopping` one is already being torn down.
    fn stop_all_shares_fire_and_forget(&mut self) {
        for share in self.active_shares.values_mut() {
            if !matches!(share.status, ShareStatus::Active(_)) {
                continue;
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
                        self.share_error = Some(t!("errors.share_start_task_ended").to_string());
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
                        self.share_error = Some(t!("errors.share_stop_task_ended").to_string());
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
                self.audio_sources_error = Some(t!("errors.audio_sources_task_ended").to_string());
                self.audio_sources_rx = None;
            }
        }
    }

    /// The `stream_id` of `owner`'s currently-announced audio stream, if
    /// any — used by `toggle_watch` to auto-subscribe audio alongside video
    /// from the same sender. `announced-streams` state (`self.streams`) is
    /// exactly the `StreamInfo`/`ANNOUNCE_STREAM` state described in this
    /// task's instructions, already tracked here from
    /// `PresenceSnapshot`/`PresenceDelta`.
    fn sibling_audio_stream(&self, owner: ConnId) -> Option<StreamId> {
        self.streams.iter().find(|s| s.owner == owner && s.kind == StreamKind::Audio).map(|s| s.stream_id)
    }

    /// Whether `owner` currently has an announced video stream — used to
    /// decide whether an audio stream gets its own "Listen" row in the
    /// Streams list (only audio-only senders do; a sender with video has
    /// their audio auto-subscribed via `toggle_watch` instead).
    fn owner_has_video(&self, owner: ConnId) -> bool {
        self.streams.iter().any(|s| s.owner == owner && s.kind == StreamKind::Video)
    }

    /// Start decoding/playing `(owner, stream_id)`'s audio if not already
    /// doing so, and subscribe to it. Shared by `toggle_watch`'s
    /// auto-subscribe-audio-alongside-video path and `toggle_listen`'s
    /// standalone audio-only path — both just need "make sure this audio
    /// stream is playing", so a click on "Listen" for a stream already
    /// auto-subscribed via its video watch (or vice versa) is a safe no-op
    /// rather than a second decoder.
    fn start_audio_watch(&mut self, owner: ConnId, stream_id: StreamId) {
        let key = (owner, stream_id);
        if self.audio_watches.contains_key(&key) {
            return;
        }
        match AudioDecoder::new(stream_id) {
            Ok(decoder) => {
                self.audio_watches.insert(key, AudioWatch { decoder, volume_percent: 100 });
                self.watch_error = None;

                let client = self.client.clone();
                tokio::spawn(async move {
                    if let Err(e) = client.subscribe(owner, stream_id).await {
                        tracing::warn!("subscribe (audio) ({owner:016x}, {stream_id}) failed: {e:#}");
                    }
                });
            }
            Err(e) => {
                self.watch_error = Some(format!("{e:#}"));
            }
        }
    }

    /// Tear down `(owner, stream_id)`'s audio watch, if one is active, and
    /// unsubscribe. Dropping the removed `AudioWatch` drops its
    /// `AudioDecoder`, which synchronously shuts down its pipeline thread
    /// (see `AudioDecoder`'s `Drop` impl).
    fn teardown_audio_watch(&mut self, owner: ConnId, stream_id: StreamId) {
        if self.audio_watches.remove(&(owner, stream_id)).is_some() {
            let client = self.client.clone();
            tokio::spawn(async move {
                if let Err(e) = client.unsubscribe(owner, stream_id).await {
                    tracing::warn!("unsubscribe (audio) ({owner:016x}, {stream_id}) failed: {e:#}");
                }
            });
        }
    }

    /// "Watch"/"Stop Watching" clicked for `(owner, stream_id)`. Subscribing
    /// and unsubscribing are async (control-plane round trip) so they're
    /// fired onto the runtime and not awaited here; the decoder itself is
    /// created/dropped synchronously since that's cheap (spawns/joins a
    /// local decode thread, no network wait).
    ///
    /// If `owner` also has an active audio stream, it's auto-subscribed and
    /// played alongside the video (see `start_audio_watch`) — matching the
    /// natural expectation that "watching someone's screen" includes
    /// hearing their audio, without a separate manual step — and torn down
    /// together with the video watch when watching stops.
    fn toggle_watch(&mut self, owner: ConnId, stream_id: StreamId) {
        let key = (owner, stream_id);
        if let Some(entry) = self.watches.remove(&key) {
            if self.selected_watch == Some(key) {
                self.selected_watch = self.watches.keys().next().copied();
            }
            let client = self.client.clone();
            tokio::spawn(async move {
                if let Err(e) = client.unsubscribe(owner, stream_id).await {
                    tracing::warn!("unsubscribe({owner:016x}, {stream_id}) failed: {e:#}");
                }
            });
            if let Some(audio_stream_id) = entry.audio_stream_id {
                self.teardown_audio_watch(owner, audio_stream_id);
            }
        } else {
            match StreamDecoder::new(stream_id) {
                Ok(decoder) => {
                    let audio_stream_id = self.sibling_audio_stream(owner);
                    self.watches.insert(
                        key,
                        WatchEntry {
                            decoder: Arc::new(Mutex::new(decoder)),
                            texture: None,
                            popout: None,
                            audio_stream_id,
                        },
                    );
                    self.selected_watch = Some(key);
                    self.watch_error = None;

                    let client = self.client.clone();
                    tokio::spawn(async move {
                        if let Err(e) = client.subscribe(owner, stream_id).await {
                            tracing::warn!("subscribe({owner:016x}, {stream_id}) failed: {e:#}");
                        }
                    });

                    if let Some(audio_stream_id) = audio_stream_id {
                        self.start_audio_watch(owner, audio_stream_id);
                    }
                }
                Err(e) => {
                    self.watch_error = Some(format!("{e:#}"));
                }
            }
        }
    }

    /// "Listen"/"Stop Listening" clicked for an audio-only stream (a sender
    /// with no video announced — see `owner_has_video`). Just toggles that
    /// stream's `AudioWatch` via the same start/teardown helpers
    /// `toggle_watch` uses for its auto-subscribed audio.
    fn toggle_listen(&mut self, owner: ConnId, stream_id: StreamId) {
        if self.audio_watches.contains_key(&(owner, stream_id)) {
            self.teardown_audio_watch(owner, stream_id);
        } else {
            self.start_audio_watch(owner, stream_id);
        }
    }

    /// Upload any freshly decoded frames to their stream's texture. Call
    /// once per repaint; cheap no-op when nothing new has decoded.
    ///
    /// Skips popped-out entries: once a stream is popped out, its own
    /// viewport closure (`show_popouts`) is the sole `latest_frame()`
    /// poller for that stream, so the main-thread pass here doesn't steal
    /// frames out from under it.
    fn update_watch_textures(&mut self, ctx: &egui::Context) {
        for ((owner, stream_id), entry) in self.watches.iter_mut() {
            if entry.popout.is_some() {
                continue;
            }
            let image = entry.decoder.lock().unwrap().latest_frame();
            if let Some(image) = image {
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

    /// "Pop out" clicked for `(owner, stream_id)`: mark that `WatchEntry` as
    /// popped-out so the central panel stops rendering it and `show_popouts`
    /// starts giving it its own native window instead.
    fn pop_out(&mut self, owner: ConnId, stream_id: StreamId) {
        let key = (owner, stream_id);
        if self.selected_watch == Some(key) {
            // Free the central panel to show something else immediately —
            // otherwise it'd sit on "(popped out)" with nothing to fall
            // back to for a lone watched stream.
            self.selected_watch = self.watches.keys().find(|k| **k != key).copied();
        }
        if let Some(entry) = self.watches.get_mut(&key) {
            entry.popout = Some(PopoutState {
                texture: Arc::new(Mutex::new(None)),
                close_requested: Arc::new(AtomicBool::new(false)),
            });
        }
    }

    /// For every currently popped-out `WatchEntry`, (re)declare its deferred
    /// viewport — `show_viewport_deferred` must be called every frame a
    /// viewport should keep existing, per egui's viewport API. Call once per
    /// repaint, alongside `update_watch_textures`.
    fn show_popouts(&mut self, ctx: &egui::Context) {
        for ((owner, stream_id), entry) in self.watches.iter() {
            let Some(popout) = &entry.popout else { continue };
            let owner = *owner;
            let stream_id = *stream_id;
            let viewport_id = egui::ViewportId::from_hash_of((owner, stream_id));
            let decoder = entry.decoder.clone();
            let texture = popout.texture.clone();
            let close_requested = popout.close_requested.clone();
            let title = t!("popout.title", stream_id = stream_id, owner = format!("{owner:016x}")).to_string();

            ctx.show_viewport_deferred(
                viewport_id,
                egui::ViewportBuilder::default().with_decorations(false).with_title(title),
                move |ctx, _class| {
                    // Poll this stream's decoder on the popout's own
                    // repaint cadence rather than the root viewport's —
                    // `show_viewport_deferred`'s closure is driven
                    // independently once the viewport is a real native
                    // window, which is exactly what lets a high-refresh-rate
                    // popped-out stream repaint faster than a slower main
                    // window.
                    let mut got_new_frame = false;
                    if let Some(image) = decoder.lock().unwrap().latest_frame() {
                        let mut tex_guard = texture.lock().unwrap();
                        match &mut *tex_guard {
                            Some(tex) => tex.set(image, egui::TextureOptions::LINEAR),
                            None => {
                                let name = format!("popout-{owner:016x}-{stream_id}");
                                *tex_guard = Some(ctx.load_texture(name, image, egui::TextureOptions::LINEAR));
                            }
                        }
                        got_new_frame = true;
                    }

                    egui::CentralPanel::default().frame(egui::Frame::NONE).show(ctx, |ui| {
                        let tex_guard = texture.lock().unwrap();
                        match &*tex_guard {
                            Some(tex) => {
                                let available = ui.available_size();
                                let tex_size = tex.size_vec2();
                                let scale = if tex_size.x > 0.0 && tex_size.y > 0.0 {
                                    (available.x / tex_size.x).min(available.y / tex_size.y)
                                } else {
                                    1.0
                                };
                                let display_size = tex_size * scale.max(0.01);
                                ui.centered_and_justified(|ui| ui.image((tex.id(), display_size)));
                            }
                            None => {
                                ui.centered_and_justified(|ui| ui.label(t!("video.waiting_frame").to_string()));
                            }
                        }
                    });

                    if ctx.input(|i| i.key_pressed(egui::Key::F11)) {
                        let is_fullscreen = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!is_fullscreen));
                    }

                    if ctx.input(|i| i.viewport().close_requested()) {
                        close_requested.store(true, Ordering::Relaxed);
                    }

                    // Only this viewport, not the whole app, needs to
                    // repaint again for the frame we just drew.
                    if got_new_frame {
                        ctx.request_repaint();
                    }
                },
            );
        }

        // Re-embed any popouts whose native window the user closed, instead
        // of leaving them marked popped-out with nothing left repainting
        // them.
        let mut to_unpop: Vec<(ConnId, StreamId)> = Vec::new();
        for (key, entry) in self.watches.iter() {
            if let Some(popout) = &entry.popout {
                if popout.close_requested.load(Ordering::Relaxed) {
                    to_unpop.push(*key);
                }
            }
        }
        for key in to_unpop {
            if let Some(entry) = self.watches.get_mut(&key) {
                if let Some(popout) = entry.popout.take() {
                    // Hand the popout's last frame over as the inline
                    // texture so returning to the central panel doesn't
                    // flash blank until the next decoded frame.
                    if let Some(tex) = popout.texture.lock().unwrap().take() {
                        entry.texture = Some(tex);
                    }
                }
                if self.selected_watch.is_none() {
                    self.selected_watch = Some(key);
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
                    framerate: DEFAULT_FRAMERATE,
                    audio_sources: Vec::new(),
                    audio_sources_error: None,
                    audio_sources_requested: false,
                    audio_sources_rx: None,
                    selected_audio_source: None,
                    watches: HashMap::new(),
                    watch_error: None,
                    selected_watch: None,
                    audio_watches: HashMap::new(),
                    preview_cache: PreviewCache::new(),
                    preview_textures: HashMap::new(),
                });
            }
            Ok(Err(e)) => {
                self.status = Status::Error(e);
                self.connect_rx = None;
            }
            Err(oneshot::error::TryRecvError::Empty) => {}
            Err(oneshot::error::TryRecvError::Closed) => {
                self.status = Status::Error(t!("errors.connect_task_ended").to_string());
                self.connect_rx = None;
            }
        }
    }

    fn drain_events(&mut self, ctx: &egui::Context) {
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
                    connected.preview_cache.remove(conn_id, stream_id);
                    connected.preview_textures.remove(&(conn_id, stream_id));
                }
                // Only meaningful for a stream we're actively *sending*
                // (relay spec §9: both are sent only to the connection that
                // owns `stream_id`) — forward to that stream's live
                // `CaptureHandle` so `capture.rs`'s pump loop can
                // pause/resume the encoder and force keyframes. A miss (no
                // `active_shares` entry, not `Active` yet, or an audio
                // share) is a normal, silent no-op: audio has no encoder to
                // pause, and a `Starting`/`Stopping`/absent share has
                // nothing listening yet.
                ClientEvent::SubscriberCount { stream_id, count } => {
                    if let Some(ActiveShare { status: ShareStatus::Active(ShareHandle::Video(handle)), .. }) =
                        connected.active_shares.get(&stream_id)
                    {
                        handle.notify_subscriber_count(count);
                    }
                }
                ClientEvent::KeyframeRequest { stream_id } => {
                    if let Some(ActiveShare { status: ShareStatus::Active(ShareHandle::Video(handle)), .. }) =
                        connected.active_shares.get(&stream_id)
                    {
                        handle.notify_keyframe_request();
                    }
                }
                ClientEvent::MediaFrame(frame) => {
                    let key = (frame.conn_id, frame.stream_id);
                    if let Some(entry) = connected.watches.get_mut(&key) {
                        entry.decoder.lock().unwrap().feed(&frame);
                    }
                    // A given (conn_id, stream_id) only ever matches one of
                    // `watches` (video) or `audio_watches` (audio) — each
                    // owner allocates a distinct stream_id per share
                    // regardless of kind — so both are checked
                    // unconditionally rather than needing to know the
                    // frame's `StreamKind` up front.
                    if let Some(audio_watch) = connected.audio_watches.get_mut(&key) {
                        audio_watch.decoder.feed(&frame);
                    }
                }
                ClientEvent::Preview(preview) => match decode::decode_preview(&preview) {
                    Ok(image) => {
                        let key = (preview.conn_id, preview.stream_id);
                        // Upload/update the roster thumbnail texture right
                        // away, mirroring `WatchEntry`'s
                        // create-once-then-`set`-in-place pattern (see
                        // `update_watch_textures`) rather than reuploading
                        // an unchanged image every repaint.
                        match connected.preview_textures.get_mut(&key) {
                            Some(tex) => tex.set(image.clone(), egui::TextureOptions::LINEAR),
                            None => {
                                let name = format!("preview-{:016x}-{}", preview.conn_id, preview.stream_id);
                                let tex = ctx.load_texture(name, image.clone(), egui::TextureOptions::LINEAR);
                                connected.preview_textures.insert(key, tex);
                            }
                        }
                        connected.preview_cache.update(preview.conn_id, preview.stream_id, image);
                    }
                    Err(e) => {
                        tracing::warn!(
                            "failed to decode preview for stream {} on conn {:016x}: {e:#}",
                            preview.stream_id,
                            preview.conn_id
                        );
                    }
                },
                ClientEvent::ControlClosed(reason) => {
                    self.status = Status::Error(t!("errors.disconnected", reason = reason).to_string());
                    if let Some(connected) = &mut self.connected {
                        connected.stop_all_shares_fire_and_forget();
                    }
                    self.connected = None;
                    return;
                }
            }
        }
    }

    /// Small "English"/"Deutsch" language switcher. Language names are
    /// deliberately left untranslated (a language selector conventionally
    /// shows each option in its own language, e.g. "Deutsch" rather than
    /// "German" while in English mode) so a user can always find their way
    /// back regardless of the current locale.
    fn ui_language_toggle(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(t!("settings.language_label").to_string());
            let mut changed = false;
            if ui.selectable_label(self.language == "en", "English").clicked() && self.language != "en" {
                self.language = "en".to_string();
                changed = true;
            }
            if ui.selectable_label(self.language == "de", "Deutsch").clicked() && self.language != "de" {
                self.language = "de".to_string();
                changed = true;
            }
            if changed {
                rust_i18n::set_locale(&self.language);
                save_language(&self.language);
            }
        });
    }

    fn ui_join_screen(&mut self, ui: &mut egui::Ui) {
        self.ui_language_toggle(ui);
        ui.add_space(4.0);

        ui.heading(t!("app.title").to_string());
        ui.add_space(8.0);

        ui.label(t!("join.display_name_label").to_string());
        ui.text_edit_singleline(&mut self.display_name);
        ui.add_space(4.0);

        ui.label(t!("join.link_label").to_string());
        ui.text_edit_singleline(&mut self.join_string);
        ui.add_space(8.0);

        let connecting = matches!(self.status, Status::Connecting);
        ui.add_enabled_ui(!connecting && !self.join_string.trim().is_empty(), |ui| {
            if ui.button(t!("join.button").to_string()).clicked() {
                self.start_connect();
            }
        });

        match &self.status {
            Status::Idle => {}
            Status::Connecting => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(t!("join.connecting").to_string());
                });
            }
            Status::Error(e) => {
                ui.colored_label(egui::Color32::RED, t!("join.error", error = e).to_string());
            }
        }

        ui.add_space(12.0);
        ui.separator();
        ui.label(t!("join.saved_lounges").to_string());
        if let Some(err) = &self.room_store_error {
            ui.colored_label(egui::Color32::RED, t!("join.load_error", error = err).to_string());
        }
        if self.room_store.rooms.is_empty() {
            ui.label(t!("join.no_saved_rooms").to_string());
        } else {
            let mut to_join: Option<String> = None;
            let mut to_remove: Option<String> = None;
            for room in &self.room_store.rooms {
                ui.horizontal(|ui| {
                    ui.label(&room.name);
                    if ui.small_button(t!("join.button").to_string()).clicked() {
                        to_join = Some(room.join_string.clone());
                    }
                    if ui.small_button(t!("rooms.remove_button").to_string()).clicked() {
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

        ui.heading(t!("app.title").to_string());
        ui.label(t!(
            "connected.status",
            name = self.display_name,
            conn_id = format!("{:016x}", connected.client.conn_id())
        ).to_string());
        ui.add_space(8.0);

        ui.horizontal(|ui| {
            ui.label(t!("rooms.save_label").to_string());
            ui.text_edit_singleline(&mut connected.save_name);
            let can_save = !connected.save_name.trim().is_empty();
            if ui.add_enabled(can_save, egui::Button::new(t!("rooms.save_button").to_string())).clicked() {
                self.room_store.upsert(connected.save_name.clone(), self.join_string.clone());
                if let Err(e) = self.room_store.save() {
                    self.room_store_error = Some(e.to_string());
                } else {
                    connected.saved = true;
                }
            }
        });
        if connected.saved {
            ui.colored_label(egui::Color32::GREEN, t!("rooms.saved_confirmation").to_string());
        }

        ui.add_space(12.0);
        ui.separator();
        ui.label(t!("members.header", count = connected.members.len()).to_string());
        egui::ScrollArea::vertical().max_height(120.0).show(ui, |ui| {
            // Show all members, not just streamers (spec §8/§4.2: a lurker
            // with stream_count == 0 still appears in the roster).
            for member in &connected.members {
                ui.horizontal(|ui| {
                    if member.stream_count > 0 {
                        ui.colored_label(egui::Color32::from_rgb(80, 200, 120), "\u{25cf}");
                        let key = if member.stream_count == 1 { "members.stream_singular" } else { "members.stream_plural" };
                        ui.label(t!(key, name = member.display_name, count = member.stream_count).to_string());
                    } else {
                        ui.colored_label(egui::Color32::GRAY, "\u{25cb}");
                        ui.label(&member.display_name);
                    }
                });
            }
        });

        ui.add_space(12.0);
        ui.separator();
        ui.label(t!("share.video_header").to_string());
        ui.horizontal(|ui| {
            ui.label(t!("share.bitrate_label").to_string());
            ui.add(egui::Slider::new(&mut connected.bitrate_kbps, MIN_BITRATE_KBPS..=MAX_BITRATE_KBPS));
        });
        ui.horizontal(|ui| {
            ui.label(t!("share.framerate_label").to_string());
            egui::ComboBox::from_id_salt("framerate_combo")
                .selected_text(format!("{} fps", connected.framerate))
                .show_ui(ui, |ui| {
                    for &fps in &FRAMERATE_OPTIONS {
                        ui.selectable_value(&mut connected.framerate, fps, format!("{fps} fps"));
                    }
                });
        });
        ui.horizontal(|ui| {
            // Always enabled: clicking again while a video share is already
            // active starts *another* one (a second stream_id), re-prompting
            // the OS portal picker so the user can pick a different
            // monitor/window.
            if ui.button(t!("share.screen_button").to_string()).clicked() {
                connected.start_video_share();
            }
            let starting_video = connected
                .active_shares
                .values()
                .any(|s| s.kind == StreamKind::Video && matches!(s.status, ShareStatus::Starting(_)));
            if starting_video {
                ui.spinner();
                ui.label(t!("share.waiting_permission").to_string());
            }
        });

        ui.add_space(12.0);
        ui.separator();
        ui.label(t!("share.audio_header").to_string());
        ui.horizontal(|ui| {
            if !connected.audio_sources_requested {
                connected.audio_sources_requested = true;
                connected.refresh_audio_sources();
            }
            if ui.button(t!("share.refresh_button").to_string()).clicked() {
                connected.refresh_audio_sources();
            }
            if connected.audio_sources_rx.is_some() {
                ui.spinner();
            }
        });
        ui.horizontal(|ui| {
            let placeholder = t!("share.select_source_placeholder").to_string();
            let selected_text = connected
                .selected_audio_source
                .as_ref()
                .map(|s| s.name.as_str())
                .unwrap_or(placeholder.as_str());
            egui::ComboBox::from_label(t!("share.source_label").to_string())
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
            if ui.add_enabled(can_share_audio, egui::Button::new(t!("share.audio_button").to_string())).clicked() {
                connected.start_audio_share();
            }
        });
        if connected.audio_sources.is_empty() && connected.audio_sources_rx.is_none() {
            ui.label(t!("share.no_audio_sources").to_string());
        }
        if let Some(err) = &connected.audio_sources_error {
            ui.colored_label(egui::Color32::RED, t!("share.audio_sources_error", error = err).to_string());
        }

        ui.add_space(12.0);
        ui.separator();
        ui.label(t!("share.active_header").to_string());
        if connected.active_shares.is_empty() {
            ui.label(t!("share.none").to_string());
        } else {
            let mut stream_ids: Vec<StreamId> = connected.active_shares.keys().copied().collect();
            stream_ids.sort_unstable();
            let mut to_stop = Vec::new();
            for stream_id in stream_ids {
                let share = &connected.active_shares[&stream_id];
                let kind_label = match share.kind {
                    StreamKind::Video => t!("share.kind_video").to_string(),
                    StreamKind::Audio => t!("share.kind_audio").to_string(),
                };
                let (state_label, stoppable) = match &share.status {
                    ShareStatus::Starting(_) => (t!("share.state_starting").to_string(), false),
                    ShareStatus::Active(_) => (t!("share.state_active").to_string(), true),
                    ShareStatus::Stopping(_) => (t!("share.state_stopping").to_string(), false),
                };
                ui.horizontal(|ui| {
                    ui.label(
                        t!(
                            "share.active_line",
                            stream_id = stream_id,
                            kind = kind_label,
                            label = share.label.clone(),
                            state = state_label
                        )
                        .to_string(),
                    );
                    let stop_label = if share.kind == StreamKind::Audio {
                        t!("share.stop_audio_button").to_string()
                    } else {
                        t!("share.stop_button").to_string()
                    };
                    if ui.add_enabled(stoppable, egui::Button::new(stop_label)).clicked() {
                        to_stop.push(stream_id);
                    }
                    // Sharer-side live encode rate: the encoder branch's
                    // actual output framerate (see `CaptureHandle::encoder_fps`),
                    // confirming the configured framerate is really being
                    // hit rather than just selected in the UI. Video shares
                    // only — audio has no comparable per-frame rate.
                    if let ShareStatus::Active(ShareHandle::Video(handle)) = &share.status {
                        ui.label(t!("share.fps_label", fps = handle.encoder_fps()).to_string());
                    }
                });
            }
            for stream_id in to_stop {
                connected.stop_share(stream_id);
            }
        }
        if let Some(err) = &connected.share_error {
            ui.colored_label(egui::Color32::RED, t!("share.error", error = err).to_string());
        }

        ui.add_space(12.0);
        ui.separator();
        ui.label(t!("streams.header").to_string());
        if let Some(err) = &connected.watch_error {
            ui.colored_label(egui::Color32::RED, t!("streams.watch_error", error = err).to_string());
        }
        if connected.streams.is_empty() {
            ui.label(t!("streams.none_active").to_string());
        } else {
            let mut to_toggle: Option<(ConnId, StreamId)> = None;
            let mut to_pop_out: Option<(ConnId, StreamId)> = None;
            let mut to_toggle_listen: Option<(ConnId, StreamId)> = None;
            let unknown_owner = t!("streams.unknown_owner").to_string();
            for info in &connected.streams {
                let key = (info.owner, info.stream_id);
                let owner_name = connected
                    .members
                    .iter()
                    .find(|m| m.conn_id == info.owner)
                    .map(|m| m.display_name.as_str())
                    .unwrap_or(unknown_owner.as_str());

                if info.kind == StreamKind::Audio {
                    // A sender with video gets their audio auto-subscribed
                    // via the video row's Watch button (see
                    // `Connected::toggle_watch`) — only an audio-only sender
                    // (no video announced) gets its own "Listen" row here.
                    if connected.owner_has_video(info.owner) {
                        continue;
                    }
                    let listening = connected.audio_watches.contains_key(&key);
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label(
                                t!(
                                    "streams.audio_line",
                                    owner = owner_name,
                                    label = info.label.clone()
                                )
                                .to_string(),
                            );
                            ui.horizontal(|ui| {
                                let button_label = if listening {
                                    t!("streams.stop_listening_button").to_string()
                                } else {
                                    t!("streams.listen_button").to_string()
                                };
                                if ui.small_button(button_label).clicked() {
                                    to_toggle_listen = Some(key);
                                }
                                if listening {
                                    ui.colored_label(
                                        egui::Color32::from_rgb(80, 200, 120),
                                        t!("streams.listening").to_string(),
                                    );
                                }
                            });
                            if let Some(audio_watch) = connected.audio_watches.get_mut(&key) {
                                ui.horizontal(|ui| {
                                    ui.label(t!("audio.volume_label").to_string());
                                    let slider = egui::Slider::new(&mut audio_watch.volume_percent, 0..=200).suffix("%");
                                    if ui.add(slider).changed() {
                                        audio_watch.decoder.set_volume(audio_watch.volume_percent as f64 / 100.0);
                                    }
                                });
                            }
                        });
                    });
                    continue;
                }

                let watching = connected.watches.contains_key(&key);
                ui.horizontal(|ui| {
                    // Thumbnail (or a placeholder box until the first
                    // preview for this stream arrives) — previews arrive
                    // for every active stream regardless of whether we're
                    // watching it, so this shows up independent of the
                    // Watch/Stop Watching toggle below.
                    const THUMB_WIDTH: f32 = 120.0;
                    const THUMB_HEIGHT: f32 = THUMB_WIDTH * 9.0 / 16.0;
                    match connected.preview_textures.get(&key) {
                        Some(tex) => {
                            let tex_size = tex.size_vec2();
                            let scale = if tex_size.x > 0.0 { THUMB_WIDTH / tex_size.x } else { 1.0 };
                            ui.image((tex.id(), tex_size * scale));
                        }
                        None => {
                            let (rect, _) =
                                ui.allocate_exact_size(egui::vec2(THUMB_WIDTH, THUMB_HEIGHT), egui::Sense::hover());
                            ui.painter().rect_filled(rect, 4.0, egui::Color32::from_gray(60));
                        }
                    }

                    let popped_out = connected.watches.get(&key).is_some_and(|e| e.popout.is_some());

                    ui.vertical(|ui| {
                        ui.label(
                            t!(
                                "streams.line",
                                owner = owner_name,
                                label = info.label.clone(),
                                width = info.width,
                                height = info.height
                            )
                            .to_string(),
                        );
                        ui.horizontal(|ui| {
                            let button_label = if watching {
                                t!("streams.stop_watching_button").to_string()
                            } else {
                                t!("streams.watch_button").to_string()
                            };
                            if ui.small_button(button_label).clicked() {
                                to_toggle = Some(key);
                            }
                            if watching {
                                // Pop out into its own native window (or,
                                // once popped out, this just reports state —
                                // closing that window is how it comes back).
                                ui.add_enabled_ui(!popped_out, |ui| {
                                    if ui.small_button(t!("streams.pop_out_button").to_string()).clicked() {
                                        to_pop_out = Some(key);
                                    }
                                });
                            }
                            if popped_out {
                                ui.colored_label(egui::Color32::LIGHT_BLUE, t!("streams.popped_out").to_string());
                            } else if connected.selected_watch == Some(key) {
                                ui.colored_label(egui::Color32::from_rgb(80, 200, 120), t!("streams.viewing").to_string());
                            }
                        });
                    });
                });
            }
            if let Some((owner, stream_id)) = to_toggle {
                connected.toggle_watch(owner, stream_id);
            }
            if let Some((owner, stream_id)) = to_pop_out {
                connected.pop_out(owner, stream_id);
            }
            if let Some((owner, stream_id)) = to_toggle_listen {
                connected.toggle_listen(owner, stream_id);
            }
        }

        ui.add_space(12.0);
        ui.separator();
        ui.label(t!("video.header").to_string());

        // Viewer-side live FPS: how often the currently displayed watch's
        // decoder actually yields a *new* frame (see
        // `StreamDecoder::current_fps`), not just the UI's own repaint rate —
        // the most directly useful FPS readout, since it tells you whether
        // playback of what's on screen right now is actually smooth. Reads
        // through the same `Arc<Mutex<StreamDecoder>>` `update_watch_textures`/
        // the popout closure poll `latest_frame()` through, so this reflects
        // real decode activity regardless of whether the stream is currently
        // shown inline or popped out into its own window.
        if let Some(key) = connected.selected_watch {
            if let Some(entry) = connected.watches.get(&key) {
                let fps = entry.decoder.lock().unwrap().current_fps();
                ui.label(t!("video.fps_label", fps = format!("{fps:.1}")).to_string());
            }
        }

        // If the currently displayed video watch also auto-subscribed the
        // same sender's audio (see `Connected::toggle_watch`), show its
        // volume slider here, right above the video panel — "somewhere
        // sensible in the watched-stream UI", per this task's brief.
        let linked_audio_key: Option<(ConnId, StreamId)> = connected
            .selected_watch
            .and_then(|key| connected.watches.get(&key))
            .and_then(|entry| entry.audio_stream_id)
            .map(|audio_stream_id| (connected.selected_watch.expect("checked above").0, audio_stream_id));
        if let Some(audio_key) = linked_audio_key {
            if let Some(audio_watch) = connected.audio_watches.get_mut(&audio_key) {
                ui.horizontal(|ui| {
                    ui.label(t!("audio.volume_label").to_string());
                    let slider = egui::Slider::new(&mut audio_watch.volume_percent, 0..=200).suffix("%");
                    if ui.add(slider).changed() {
                        audio_watch.decoder.set_volume(audio_watch.volume_percent as f64 / 100.0);
                    }
                });
            }
        }

        match connected.selected_watch.and_then(|key| connected.watches.get(&key)) {
            Some(entry) if entry.popout.is_some() => {
                ui.label(t!("video.popped_out_note").to_string());
            }
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
                    ui.label(t!("video.waiting_frame").to_string());
                }
            },
            None => {
                ui.label(t!("video.not_watching").to_string());
            }
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_connect_result();
        self.drain_events(ctx);
        if let Some(connected) = &mut self.connected {
            connected.poll_share_states();
            connected.poll_audio_sources();
            connected.update_watch_textures(ctx);
            connected.show_popouts(ctx);
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

    /// Called once on shutdown (window closed, app quit) — covers the same
    /// "abrupt teardown while sharing" gap as `ClientEvent::ControlClosed`,
    /// just triggered by the user closing the app instead of the connection
    /// dying underneath them.
    ///
    /// `eframe::App::on_exit` is a synchronous callback with no way to block
    /// on (or even poll) async work before eframe tears the window down, so
    /// this can't `.await` `.stop()` to completion the way the voluntary
    /// `stop_share` path does. Best effort: spawn `.stop()` for every active
    /// share onto the ambient tokio runtime (mirroring `ControlClosed`'s
    /// `stop_all_shares_fire_and_forget`) so graceful teardown (portal
    /// session close, relay stop-stream notification) at least gets
    /// *attempted*, rather than relying solely on the handles' `Drop` impls
    /// once this process starts exiting.
    fn on_exit(&mut self) {
        if let Some(connected) = &mut self.connected {
            connected.stop_all_shares_fire_and_forget();
        }
    }
}
