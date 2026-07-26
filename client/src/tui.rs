//! Terminal (ratatui) UI mode — an **additive** alternative front-end to the
//! default eframe/egui window, selected with the `--tui` flag.
//!
//! # Why this exists
//!
//! This is a **development/testing convenience, not a product feature**. The
//! real UI is still `ui.rs`; nothing here replaces or changes it. The problem
//! it solves: an AI coding agent (and, secondarily, anyone on a headless box
//! or over SSH) cannot drive or *observe* a real GUI window. A terminal UI can
//! be launched inside a tmux pane, driven with `tmux send-keys`, and read back
//! with `tmux capture-pane` — which makes the whole join → share → watch loop
//! checkable without a display, in seconds, instead of needing a human to look
//! at a window and report back.
//!
//! Because of that goal, this module covers only the **golden path**: join
//! (typed/pasted join string, or a saved lounge from `rooms.rs`), presence
//! roster, start/stop one screen share, watch one subscribed video stream, and
//! leave/quit. Deliberately *not* covered: audio share/listen, per-stream
//! volume, preview thumbnails, pop-out windows, saving new lounges, i18n. Each
//! of those exists in `ui.rs` and none of them is needed to answer "is
//! sharing/watching actually working right now?".
//!
//! Everything below the UI is reused verbatim — `net.rs`'s
//! `ScreenshareClient` + `ClientEvent` channel, `capture.rs`'s
//! `start_sharing`, `decode.rs`'s `StreamDecoder`, `rooms.rs`'s `RoomStore`.
//! There is no TUI-specific networking, decoding or crypto anywhere in here;
//! if there ever is, that's a bug.
//!
//! # Image backend: Unicode half-blocks, forced
//!
//! Video is rendered with [`ratatui_image`], pinned to
//! [`ProtocolType::Halfblocks`] via `Picker::halfblocks()` — never
//! `Picker::from_query_stdio()`, which auto-detects Sixel/Kitty/iTerm2.
//! Reasons, in order of importance:
//!
//! 1. **It works through tmux and SSH with zero terminal support.** Half-blocks
//!    are just the ordinary characters `▀`/`▄` with a foreground and background
//!    colour — i.e. plain styled text in the ratatui `Buffer`. Sixel/Kitty/
//!    iTerm2 are out-of-band escape sequences that need specific terminal
//!    emulators and don't pass through a tmux pane reliably. Since "runs in a
//!    tmux pane" is the entire point of this mode, a backend that might not
//!    survive tmux is disqualified.
//! 2. **The rendered frame is readable by an agent.** `tmux capture-pane`
//!    returns the pane's *text*; a half-block frame comes back as real
//!    characters (and, with `-e`, their colours), so a future session can
//!    inspect what the video panel actually showed. A graphics-protocol image
//!    is invisible to `capture-pane` — it would defeat the purpose.
//! 3. **No system dependencies.** `ratatui-image` is pulled in with
//!    `default-features = false` precisely to *avoid* its default `chafa-dyn`
//!    feature, which links system libchafa; the built-in pure-Rust half-block
//!    encoder needs nothing installed.
//!
//! The tradeoff, accepted knowingly: half-blocks are low resolution (one
//! character cell = 2 pixels vertically, 1 horizontally) and colour-quantised
//! by the terminal. That is fine for the actual job — confirming a stream is
//! live, moving, and roughly the right thing — and useless for judging image
//! quality. Judge quality in the egui GUI.
//!
//! # Threading
//!
//! The render/input loop runs **on the main thread**, with `main.rs`'s
//! existing tokio runtime merely *entered* (`Runtime::enter`) rather than
//! `block_on`-ed. That is the same arrangement `ui.rs` lives in today, and it
//! is reused for the same reason: crossterm's `poll`/`read` and ratatui's
//! draw are blocking calls, so they must not sit on a tokio worker, while
//! every async operation (connect, subscribe, start/stop capture) is
//! `tokio::spawn`ed and its result collected next tick from a
//! `oneshot`/`mpsc` via `try_recv`. Concretely: this module is
//! `ui.rs`'s async-glue pattern with the eframe `update()` callback swapped
//! for an explicit loop.
//!
//! A dedicated OS thread (the shape `simple-screensharing-server/src/tui.rs`
//! uses) would also work but buys nothing here: the server's TUI thread exists
//! because its TUI *must* coexist with a QUIC listener that owns the process,
//! whereas in `--tui` mode this loop simply *is* the foreground of the
//! process, exactly where eframe's event loop would otherwise be.
//!
//! # Logging
//!
//! `tracing` output cannot go to stdout/stderr here — it would scribble over
//! the alternate screen. [`init_logging`] redirects it to a file (same
//! reasoning as the relay server's `logs/` directory) whose path is shown in
//! the TUI footer so it can be tailed alongside the pane.

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect, Size};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};
use ratatui::Frame;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::Protocol;
use ratatui_image::{FilterType, Image, Resize};
use tokio::sync::{mpsc, oneshot};

use crate::capture::{self, CaptureHandle};
use crate::decode::StreamDecoder;
use crate::net::{ClientEvent, PresenceSnapshotState, ScreenshareClient};
use crate::protocol::{ConnId, Member, PresenceEvent, StreamId, StreamInfo, StreamKind};
use crate::rooms::RoomStore;

/// How long each loop iteration waits for a key before redrawing. Also the
/// worst-case latency for reacting to a `ClientEvent` or a finished async
/// operation, so it's kept well under human perception rather than tuned for
/// idle CPU.
const TICK: Duration = Duration::from_millis(50);

/// Minimum gap between two half-block re-encodes of the watched stream.
/// Re-encoding is a full image downscale per frame, and this mode exists to
/// *sanity-check* a stream, not to play it back smoothly — ~10fps in the
/// terminal is plenty to see that motion is happening, at a fraction of the
/// CPU a full-rate re-encode would burn.
///
/// Measured live at ~8fps in a release build on a 1920x1080 share filling a
/// ~168x55-cell pane, i.e. this interval really is what's limiting. Run
/// `--release`: an unoptimized `image` downscale of a full-HD frame costs
/// roughly two *seconds*, so a debug build's video pane crawls at well under
/// 1fps. That's a build-profile artifact, not a bug in this module.
const VIDEO_REFRESH: Duration = Duration::from_millis(100);

type ConnectResult =
    Result<(ScreenshareClient, PresenceSnapshotState, mpsc::UnboundedReceiver<ClientEvent>), String>;

/// Options parsed from the command line by `main.rs`. `join`/`name`
/// pre-fill the join screen's two fields: pasting a 90-character join string
/// through `tmux send-keys` works but is exactly the kind of thing that
/// silently half-arrives, so an agent scripting this mode can hand them over
/// as arguments instead.
pub struct TuiOptions {
    pub join_string: Option<String>,
    pub display_name: Option<String>,
    /// Where `tracing` output was redirected — displayed in the footer.
    pub log_path: PathBuf,
}

/// The default log destination for `--tui` mode. Not under `dirs::config_dir()`
/// (that's for things the user owns, like saved lounges) and not the working
/// directory (a dev tool shouldn't drop files in the repo).
pub fn default_log_path() -> PathBuf {
    std::env::temp_dir().join("simple-screensharing-tui.log")
}

/// Point `tracing` at a file instead of stdout/stderr, which the TUI owns.
///
/// Called by `main.rs` *instead of* `tracing_subscriber::fmt::init()` when
/// `--tui` is passed; the default GUI path is untouched. A fresh `File` handle
/// per event (via the `Fn() -> W` `MakeWriter` impl) keeps this to one line
/// with no extra dependency — this is a low-volume debug log, not a hot path.
pub fn init_logging(path: &Path) -> Result<()> {
    let file = File::create(path)
        .with_context(|| format!("failed to open TUI log file {}", path.display()))?;
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(move || file.try_clone().expect("failed to clone TUI log file handle"))
        .init();
    Ok(())
}

// ---------------------------------------------------------------------
// State
// ---------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum JoinFocus {
    Name,
    Link,
    Saved,
}

/// One subscribed video stream being watched. Mirrors `ui.rs`'s `WatchEntry`,
/// with the egui texture replaced by a half-block [`Protocol`] (the
/// terminal-side equivalent of an uploaded texture: expensive to build, cheap
/// to render, rebuilt only when there's a new frame or the pane resized).
struct Watch {
    decoder: StreamDecoder,
    protocol: Option<Protocol>,
    /// Cell size the cached `protocol` was encoded for; a change means the
    /// pane was resized and the protocol has to be rebuilt even without a new
    /// frame.
    encoded_for: Size,
    last_encode: Instant,
    frames_seen: u64,
}

/// The single outbound screen share this mode supports, as a small state
/// machine — `capture::start_sharing` and `CaptureHandle::stop` are both
/// async, so each transition is spawned and collected via a `oneshot`
/// (identical to `ui.rs`'s `ShareStatus`).
enum Share {
    Starting(oneshot::Receiver<Result<CaptureHandle, String>>),
    Active(CaptureHandle),
    Stopping(oneshot::Receiver<Result<(), String>>),
}

struct Room {
    client: ScreenshareClient,
    event_rx: mpsc::UnboundedReceiver<ClientEvent>,
    members: Vec<Member>,
    streams: Vec<StreamInfo>,
    /// Cursor into the *video* streams shown in the Streams pane.
    cursor: usize,
    watches: HashMap<(ConnId, StreamId), Watch>,
    /// Which watch the video pane renders. `None` = nothing being watched.
    selected: Option<(ConnId, StreamId)>,
    share: Option<Share>,
    share_stream_id: StreamId,
}

impl Room {
    /// Video streams only — the roster pane already lists who's present, and
    /// audio isn't playable in this mode (see the module docs), so an audio
    /// row would just be an unselectable line.
    fn video_streams(&self) -> Vec<&StreamInfo> {
        self.streams.iter().filter(|s| s.kind == StreamKind::Video).collect()
    }

    fn selected_stream(&self) -> Option<(ConnId, StreamId)> {
        self.video_streams().get(self.cursor).map(|s| (s.owner, s.stream_id))
    }

    fn owner_name(&self, owner: ConnId) -> String {
        self.members
            .iter()
            .find(|m| m.conn_id == owner)
            .map(|m| m.display_name.clone())
            .unwrap_or_else(|| format!("{owner:016x}"))
    }
}

struct App {
    // --- join screen ---
    display_name: String,
    join_string: String,
    focus: JoinFocus,
    saved_cursor: usize,
    room_store: RoomStore,
    connecting: Option<oneshot::Receiver<ConnectResult>>,
    /// The one shared QUIC endpoint every `connect()` from this front-end
    /// rides (see `net.rs`'s "One endpoint, N connections"). `Err` carries
    /// the bind failure so the join screen can report it instead of this
    /// mode panicking at startup over a socket it can't open. This mode is
    /// single-room by design, but it holds the endpoint the same way `ui.rs`
    /// does so there is exactly one place in the process that binds a socket
    /// per front-end.
    endpoint: Result<quinn::Endpoint, String>,

    // --- connected ---
    room: Option<Room>,

    // --- chrome ---
    status: String,
    error: Option<String>,
    log_path: PathBuf,
    picker: Picker,
    /// Cell rect the video pane occupied in the last draw. Recorded during
    /// draw and consumed by the *next* tick's re-encode, because the pane's
    /// size isn't known until the layout is computed — so a freshly started
    /// watch shows "waiting for video" for one frame, then renders.
    video_area: Rect,
    quit: bool,
}

impl App {
    fn new(opts: TuiOptions) -> Self {
        let (room_store, store_error) = match RoomStore::load() {
            Ok(store) => (store, None),
            Err(e) => (RoomStore::default(), Some(format!("saved lounges: {e:#}"))),
        };
        Self {
            display_name: opts.display_name.unwrap_or_else(default_display_name),
            join_string: opts.join_string.unwrap_or_default(),
            focus: JoinFocus::Link,
            saved_cursor: 0,
            room_store,
            connecting: None,
            endpoint: crate::net::build_client_endpoint().map_err(|e| format!("{e:#}")),
            room: None,
            status: "Not connected".to_string(),
            error: store_error,
            log_path: opts.log_path,
            // Forced half-blocks — see the module docs. `Picker::halfblocks()`
            // is also the only constructor that touches neither stdin nor
            // stdout, which matters because the alternate screen is already up
            // and a capability query would fight the input loop for stdin.
            picker: Picker::halfblocks(),
            video_area: Rect::ZERO,
            quit: false,
        }
    }
}

/// Mirrors `ui.rs`'s `whoami_fallback` — a nicer default than an empty field,
/// not an identity system.
fn default_display_name() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "anonymous".to_string())
}

// ---------------------------------------------------------------------
// Entry point / main loop
// ---------------------------------------------------------------------

/// Set by [`watch_for_termination_signals`] when the process is told to exit
/// by a signal from outside; polled once per [`run_loop`] iteration.
static TERMINATE: AtomicBool = AtomicBool::new(false);

/// Turn an externally delivered SIGINT/SIGTERM/SIGHUP into an ordinary quit
/// instead of letting the default disposition kill the process outright.
///
/// The keyboard Ctrl-C is *not* what this is for. crossterm's raw mode clears
/// `isig`, so a Ctrl-C **keypress** never becomes a signal at all — it arrives
/// as a plain key event and is handled in [`App::on_key`]. This covers the
/// other direction: a signal delivered from outside the terminal (`kill`/
/// `pkill` from another shell, a supervisor stopping the process, a shell
/// killing the process group). Under the default disposition that terminates
/// the process *between* `ratatui::init()` and `ratatui::restore()`, which
/// leaves the invoking terminal in raw mode on the alternate screen — no echo,
/// no working Ctrl-C, and the TUI's last frame frozen on screen, so the user
/// has to blind-type `reset`. That was reproduced live before this existed:
/// `kill -INT` on a running `--tui` exited 130 with `stty -a` still reporting
/// `-echo -isig`.
///
/// Setting a flag rather than restoring the terminal from inside the handler
/// is deliberate: it routes the signal through the loop's *normal* exit, so
/// [`App::shutdown`]'s share-stop/Leave teardown still runs and
/// `ratatui::restore()` still happens exactly once, on the main thread that
/// owns the terminal. Worst-case latency is one [`TICK`].
fn watch_for_termination_signals() {
    tokio::spawn(async {
        // Despite the name, on Unix this is SIGINT (on Windows, Ctrl-C/Ctrl-Break).
        if tokio::signal::ctrl_c().await.is_ok() {
            TERMINATE.store(true, Ordering::Relaxed);
        }
    });

    #[cfg(unix)]
    tokio::spawn(async {
        use tokio::signal::unix::{signal, SignalKind};
        // SIGTERM is what a bare `kill <pid>` sends — the likeliest way
        // someone reaches for a TUI that looks stuck. SIGHUP arrives when the
        // controlling terminal goes away (e.g. the tmux pane is killed); the
        // terminal is gone in that case, but running the same graceful exit
        // still gets the share stopped and the relay told.
        let (Ok(mut term), Ok(mut hup)) = (signal(SignalKind::terminate()), signal(SignalKind::hangup()))
        else {
            tracing::warn!("could not install SIGTERM/SIGHUP handlers; falling back to default disposition");
            return;
        };
        tokio::select! {
            _ = term.recv() => {}
            _ = hup.recv() => {}
        }
        TERMINATE.store(true, Ordering::Relaxed);
    });
}

/// Run the terminal UI on the calling (main) thread until the user quits.
///
/// Must be called with `main.rs`'s tokio runtime *entered* — this function
/// never `block_on`s, it only `tokio::spawn`s, exactly like `ui.rs`.
pub fn run(opts: TuiOptions) -> Result<()> {
    // Before `ratatui::init()` puts the terminal into raw mode, so there is no
    // window in which a signal could kill the process with the terminal
    // already switched over but no handler yet installed.
    watch_for_termination_signals();
    // `init()` also installs a panic hook that restores the terminal before
    // unwinding, which is why there is no hand-rolled one here.
    let mut terminal = ratatui::init();
    let result = run_loop(&mut terminal, App::new(opts));
    ratatui::restore();
    result
}

fn run_loop(terminal: &mut ratatui::DefaultTerminal, mut app: App) -> Result<()> {
    while !app.quit && !TERMINATE.load(Ordering::Relaxed) {
        app.poll_connect();
        app.drain_events();
        app.poll_share();
        app.refresh_video();

        terminal.draw(|frame| app.draw(frame))?;

        if event::poll(TICK)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => app.on_key(key.code, key.modifiers),
                // Any other event (resize, focus, mouse) just falls through to
                // the next draw; a resize is picked up by `refresh_video`,
                // which re-encodes whenever the pane's cell size changed.
                _ => {}
            }
        }
    }

    app.shutdown();
    Ok(())
}

// ---------------------------------------------------------------------
// Async glue (spawn + try_recv, per the module docs)
// ---------------------------------------------------------------------

impl App {
    fn start_connect(&mut self, join_string: String) {
        if join_string.trim().is_empty() {
            self.error = Some("No join link to connect with".to_string());
            return;
        }
        let endpoint = match &self.endpoint {
            Ok(endpoint) => endpoint.clone(),
            Err(e) => {
                self.error = Some(format!("No local QUIC endpoint: {e}"));
                return;
            }
        };
        self.join_string = join_string.trim().to_string();
        let join_string = self.join_string.clone();
        let display_name = self.display_name.clone();
        let (tx, rx) = oneshot::channel();
        self.connecting = Some(rx);
        self.error = None;
        self.status = "Connecting...".to_string();

        tokio::spawn(async move {
            let result = ScreenshareClient::connect(&endpoint, &join_string, &display_name)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
        });
    }

    fn poll_connect(&mut self) {
        let Some(rx) = &mut self.connecting else { return };
        match rx.try_recv() {
            Ok(Ok((client, snapshot, event_rx))) => {
                self.connecting = None;
                self.status = format!("Connected as {} ({:016x})", self.display_name, client.conn_id());
                self.room = Some(Room {
                    client,
                    event_rx,
                    members: snapshot.members,
                    streams: snapshot.streams,
                    cursor: 0,
                    watches: HashMap::new(),
                    selected: None,
                    share: None,
                    share_stream_id: 0,
                });
            }
            Ok(Err(e)) => {
                self.connecting = None;
                self.status = "Not connected".to_string();
                self.error = Some(e);
            }
            Err(oneshot::error::TryRecvError::Empty) => {}
            Err(oneshot::error::TryRecvError::Closed) => {
                self.connecting = None;
                self.status = "Not connected".to_string();
                self.error = Some("connect task ended without a result".to_string());
            }
        }
    }

    /// Drain everything `net.rs` has delivered since the last tick. Same event
    /// set, same handling shape as `ui.rs::drain_events` — minus the audio,
    /// preview-thumbnail and texture-upload arms this mode doesn't have.
    fn drain_events(&mut self) {
        let Some(room) = &mut self.room else { return };
        let mut closed: Option<String> = None;

        while let Ok(event) = room.event_rx.try_recv() {
            match event {
                ClientEvent::PresenceDelta(PresenceEvent::MemberJoined(member)) => {
                    room.members.retain(|m| m.conn_id != member.conn_id);
                    room.members.push(member);
                }
                ClientEvent::PresenceDelta(PresenceEvent::MemberLeft { conn_id }) => {
                    room.members.retain(|m| m.conn_id != conn_id);
                }
                ClientEvent::PresenceDelta(PresenceEvent::StreamStarted(info)) => {
                    if let Some(m) = room.members.iter_mut().find(|m| m.conn_id == info.owner) {
                        m.stream_count = m.stream_count.saturating_add(1);
                    }
                    room.streams.retain(|s| !(s.owner == info.owner && s.stream_id == info.stream_id));
                    room.streams.push(info);
                }
                ClientEvent::PresenceDelta(PresenceEvent::StreamStopped { conn_id, stream_id }) => {
                    if let Some(m) = room.members.iter_mut().find(|m| m.conn_id == conn_id) {
                        m.stream_count = m.stream_count.saturating_sub(1);
                    }
                    room.streams.retain(|s| !(s.owner == conn_id && s.stream_id == stream_id));
                    // Drop any watch of a stream that no longer exists, so the
                    // video pane doesn't sit on a frozen last frame forever.
                    room.watches.remove(&(conn_id, stream_id));
                    if room.selected == Some((conn_id, stream_id)) {
                        room.selected = room.watches.keys().next().copied();
                    }
                }
                // Only meaningful for a stream we're sending; forward to the
                // live `CaptureHandle` exactly as `ui.rs` does, so the
                // encoder's idle-pause / forced-keyframe reactions behave the
                // same in this mode.
                ClientEvent::SubscriberCount { stream_id, count } => {
                    if let Some(Share::Active(handle)) = &room.share {
                        if stream_id == room.share_stream_id {
                            handle.notify_subscriber_count(count);
                        }
                    }
                }
                ClientEvent::KeyframeRequest { stream_id } => {
                    if let Some(Share::Active(handle)) = &room.share {
                        if stream_id == room.share_stream_id {
                            handle.notify_keyframe_request();
                        }
                    }
                }
                ClientEvent::MediaFrame(frame) => {
                    if let Some(watch) = room.watches.get_mut(&(frame.conn_id, frame.stream_id)) {
                        watch.decoder.feed(&frame);
                    }
                }
                // Previews are decoded and shown as roster thumbnails in
                // `ui.rs`; there's no useful place for a 30-second-old still
                // in a golden-path terminal view, so they're dropped here.
                ClientEvent::Preview(_) => {}
                ClientEvent::ControlClosed(reason) => {
                    closed = Some(reason);
                    break;
                }
            }
        }

        if let Some(reason) = closed {
            self.disconnect(false);
            self.error = Some(format!("Disconnected: {reason}"));
        }
    }

    fn toggle_share(&mut self) {
        let Some(room) = &mut self.room else { return };
        match room.share.take() {
            None => {
                let stream_id = room.share_stream_id;
                let client = room.client.clone();
                let (tx, rx) = oneshot::channel();
                room.share = Some(Share::Starting(rx));
                self.error = None;
                tokio::spawn(async move {
                    let result = capture::start_sharing(
                        &client,
                        stream_id,
                        capture::DEFAULT_BITRATE_KBPS,
                        capture::DEFAULT_FRAMERATE,
                    )
                    .await
                    .map_err(|e| format!("{e:#}"));
                    let _ = tx.send(result);
                });
            }
            Some(Share::Active(handle)) => {
                let (tx, rx) = oneshot::channel();
                room.share = Some(Share::Stopping(rx));
                tokio::spawn(async move {
                    let _ = tx.send(handle.stop().await.map_err(|e| format!("{e:#}")));
                });
            }
            // Mid-transition: put it back and ignore the keypress rather than
            // racing a start against a stop.
            other => room.share = other,
        }
    }

    fn poll_share(&mut self) {
        let Some(room) = &mut self.room else { return };
        let Some(share) = &mut room.share else { return };
        match share {
            Share::Starting(rx) => match rx.try_recv() {
                Ok(Ok(handle)) => room.share = Some(Share::Active(handle)),
                Ok(Err(e)) => {
                    room.share = None;
                    self.error = Some(format!("Share failed: {e}"));
                }
                Err(oneshot::error::TryRecvError::Empty) => {}
                Err(oneshot::error::TryRecvError::Closed) => {
                    room.share = None;
                    self.error = Some("share start task ended without a result".to_string());
                }
            },
            Share::Stopping(rx) => match rx.try_recv() {
                Ok(Ok(())) => {
                    room.share = None;
                    // A fresh stream_id for the next share: the relay treats
                    // (conn_id, stream_id) as the stream identity, and reusing
                    // an id right after StopStream invites the relay and other
                    // clients to conflate the old and new stream.
                    room.share_stream_id = room.share_stream_id.wrapping_add(1);
                }
                Ok(Err(e)) => {
                    room.share = None;
                    room.share_stream_id = room.share_stream_id.wrapping_add(1);
                    self.error = Some(format!("Stop share failed: {e}"));
                }
                Err(oneshot::error::TryRecvError::Empty) => {}
                Err(oneshot::error::TryRecvError::Closed) => {
                    room.share = None;
                    self.error = Some("share stop task ended without a result".to_string());
                }
            },
            Share::Active(_) => {}
        }
    }

    /// Subscribe to / unsubscribe from the highlighted stream, creating or
    /// dropping its `StreamDecoder`. Same split as `ui.rs::toggle_watch`: the
    /// control-plane round trip is spawned, the decoder is built/dropped
    /// synchronously (cheap — it just starts/stops a local GStreamer thread).
    fn toggle_watch(&mut self) {
        let Some(room) = &mut self.room else { return };
        let Some((owner, stream_id)) = room.selected_stream() else {
            self.error = Some("No stream selected".to_string());
            return;
        };
        let key = (owner, stream_id);

        if room.watches.remove(&key).is_some() {
            if room.selected == Some(key) {
                room.selected = room.watches.keys().next().copied();
            }
            let client = room.client.clone();
            tokio::spawn(async move {
                if let Err(e) = client.unsubscribe(owner, stream_id).await {
                    tracing::warn!("unsubscribe({owner:016x}, {stream_id}) failed: {e:#}");
                }
            });
            return;
        }

        match StreamDecoder::new(stream_id) {
            Ok(decoder) => {
                room.watches.insert(
                    key,
                    Watch {
                        decoder,
                        protocol: None,
                        encoded_for: Size::new(0, 0),
                        last_encode: Instant::now() - VIDEO_REFRESH,
                        frames_seen: 0,
                    },
                );
                room.selected = Some(key);
                self.error = None;
                let client = room.client.clone();
                tokio::spawn(async move {
                    if let Err(e) = client.subscribe(owner, stream_id).await {
                        tracing::warn!("subscribe({owner:016x}, {stream_id}) failed: {e:#}");
                    }
                });
            }
            Err(e) => self.error = Some(format!("Decoder failed: {e:#}")),
        }
    }

    /// Pull the newest decoded frame for the *displayed* watch and re-encode
    /// it as half-blocks, at most once per [`VIDEO_REFRESH`].
    ///
    /// Only the displayed watch is polled: a non-displayed stream's decoder
    /// keeps running (so the subscription and pacing stay honest) but its
    /// frames are never turned into terminal cells nobody would see.
    fn refresh_video(&mut self) {
        let area = self.video_area;
        let picker = &self.picker;
        let Some(room) = &mut self.room else { return };
        let Some(key) = room.selected else { return };
        let Some(watch) = room.watches.get_mut(&key) else { return };

        if area.width < 2 || area.height < 2 {
            return;
        }
        let target = Size::new(area.width, area.height);
        let due = watch.last_encode.elapsed() >= VIDEO_REFRESH;
        let resized = watch.encoded_for != target;
        if !due && !resized {
            return;
        }

        let Some(image) = watch.decoder.latest_frame() else { return };
        watch.frames_seen += 1;
        watch.last_encode = Instant::now();

        // `Resize::Scale` rather than `Fit`: `Fit` leaves an image that is
        // already smaller than the pane at its natural size, which for a
        // half-block "pixel grid" means a postage stamp in the corner of a
        // large pane. Scale always fills the pane (preserving aspect ratio),
        // which is what someone eyeballing a stream wants.
        match picker.new_protocol(color_image_to_dynamic(&image), target, Resize::Scale(Some(FilterType::Triangle))) {
            Ok(protocol) => {
                watch.protocol = Some(protocol);
                watch.encoded_for = target;
            }
            Err(e) => tracing::warn!("half-block encode failed: {e}"),
        }
    }

    /// Leave the room and return to the join screen. `announce`: whether to
    /// send a `Leave` (skipped when the control connection is already gone).
    fn disconnect(&mut self, announce: bool) {
        let Some(mut room) = self.room.take() else { return };
        // Stop an in-flight share first so the portal session is closed and
        // the relay hears StopStream, rather than relying on `CaptureHandle`'s
        // `Drop` backstop. Fire-and-forget, mirroring
        // `ui.rs::stop_all_shares_fire_and_forget`.
        match room.share.take() {
            Some(Share::Active(handle)) => {
                tokio::spawn(async move {
                    if let Err(e) = handle.stop().await {
                        tracing::warn!("stopping share during disconnect failed: {e:#}");
                    }
                });
            }
            // A share that is still *starting* can't be stopped yet, and just
            // dropping the receiver here is not harmless: the spawned
            // `start_sharing` runs to completion regardless, and its
            // `CaptureHandle` then lands in a `send` nobody is listening to
            // and is dropped bare. `Drop` only stops the GStreamer pipeline —
            // closing the portal session is an async D-Bus call it explicitly
            // cannot make (see `CaptureHandle`'s `Drop` doc comment) — so on
            // the `l`-to-leave path, where the process keeps running, the OS
            // screen-recording indicator stays lit and the relay never hears
            // StopStream. Await the start and then stop it properly instead.
            // A `RecvError` (start task cancelled by runtime shutdown on the
            // quit path) needs no handling: there is no handle to stop.
            Some(Share::Starting(rx)) => {
                tokio::spawn(async move {
                    if let Ok(Ok(handle)) = rx.await
                        && let Err(e) = handle.stop().await
                    {
                        tracing::warn!("stopping a just-started share during disconnect failed: {e:#}");
                    }
                });
            }
            // Already stopping, or never sharing: the in-flight `stop()` owns
            // the handle and finishes on its own.
            Some(Share::Stopping(_)) | None => {}
        }
        if announce {
            let client = room.client.clone();
            tokio::spawn(async move {
                if let Err(e) = client.leave().await {
                    tracing::warn!("leave() failed: {e:#}");
                }
            });
        }
        self.status = "Not connected".to_string();
        self.error = None;
    }

    fn shutdown(&mut self) {
        self.disconnect(true);
        // Give the spawned Leave/stop a moment to actually reach the relay
        // before `main` drops the runtime out from under them. Bounded and
        // short: this is the same best-effort teardown `ui.rs::on_exit` does,
        // and a lost Leave only costs the relay a timeout.
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// `egui::ColorImage` (what `decode.rs` produces) → `image::DynamicImage`
/// (what `ratatui-image` consumes). Video frames are fully opaque, so the
/// premultiplied-alpha channels of `Color32` are already straight RGB and the
/// alpha channel can simply be dropped.
fn color_image_to_dynamic(image: &egui::ColorImage) -> image::DynamicImage {
    let [width, height] = image.size;
    let mut rgb = Vec::with_capacity(width * height * 3);
    for pixel in &image.pixels {
        rgb.push(pixel.r());
        rgb.push(pixel.g());
        rgb.push(pixel.b());
    }
    let buffer = image::RgbImage::from_raw(width as u32, height as u32, rgb)
        .expect("ColorImage pixel count always matches its declared size");
    image::DynamicImage::ImageRgb8(buffer)
}

// ---------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------

impl App {
    fn on_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        if modifiers.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('c')) {
            self.quit = true;
            return;
        }
        if self.room.is_some() {
            self.on_key_room(code);
        } else {
            self.on_key_join(code);
        }
    }

    /// Join screen. Note there is no `q`-to-quit here: two of the three focus
    /// targets are text fields, so a bare letter has to mean "type that
    /// letter". Esc and Ctrl-C quit.
    fn on_key_join(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => self.quit = true,
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = match self.focus {
                    JoinFocus::Name => JoinFocus::Link,
                    JoinFocus::Link => JoinFocus::Saved,
                    JoinFocus::Saved => JoinFocus::Name,
                };
            }
            KeyCode::Up if self.focus == JoinFocus::Saved => {
                self.saved_cursor = self.saved_cursor.saturating_sub(1);
            }
            KeyCode::Down if self.focus == JoinFocus::Saved => {
                let last = self.room_store.rooms.len().saturating_sub(1);
                self.saved_cursor = (self.saved_cursor + 1).min(last);
            }
            KeyCode::Enter => {
                let target = if self.focus == JoinFocus::Saved {
                    self.room_store.rooms.get(self.saved_cursor).map(|r| r.join_string.clone())
                } else {
                    Some(self.join_string.clone())
                };
                match target {
                    Some(join_string) => self.start_connect(join_string),
                    None => self.error = Some("No saved lounge selected".to_string()),
                }
            }
            KeyCode::Backspace => match self.focus {
                JoinFocus::Name => {
                    self.display_name.pop();
                }
                JoinFocus::Link => {
                    self.join_string.pop();
                }
                JoinFocus::Saved => {}
            },
            KeyCode::Char(c) => match self.focus {
                JoinFocus::Name => self.display_name.push(c),
                JoinFocus::Link => self.join_string.push(c),
                JoinFocus::Saved => {}
            },
            _ => {}
        }
    }

    fn on_key_room(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('l') => self.disconnect(true),
            KeyCode::Char('s') => self.toggle_share(),
            KeyCode::Char('w') | KeyCode::Enter => self.toggle_watch(),
            KeyCode::Up => {
                if let Some(room) = &mut self.room {
                    room.cursor = room.cursor.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                if let Some(room) = &mut self.room {
                    let last = room.video_streams().len().saturating_sub(1);
                    room.cursor = (room.cursor + 1).min(last);
                }
            }
            // Cycle which watched stream the video pane shows, for the case of
            // several concurrent watches.
            KeyCode::Tab => {
                if let Some(room) = &mut self.room {
                    let keys: Vec<_> = room.watches.keys().copied().collect();
                    if !keys.is_empty() {
                        let next = room
                            .selected
                            .and_then(|cur| keys.iter().position(|k| *k == cur))
                            .map(|i| (i + 1) % keys.len())
                            .unwrap_or(0);
                        room.selected = Some(keys[next]);
                    }
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------

const HELP_JOIN: &str =
    "Tab: switch field  \u{2191}/\u{2193}: pick saved lounge  Enter: connect  Esc: quit";
const HELP_ROOM: &str = "\u{2191}/\u{2193}: select stream  Enter/w: watch  s: share screen  \
                         Tab: cycle video  l: leave  q: quit";

impl App {
    fn draw(&mut self, frame: &mut Frame) {
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(0), Constraint::Length(2)])
            .split(frame.area());

        let header = Paragraph::new(Line::from(vec![
            Span::styled(" Simple Screensharing ", Style::default().add_modifier(Modifier::BOLD)),
            Span::styled(
                self.status.clone(),
                Style::default().fg(if self.room.is_some() { Color::Green } else { Color::Gray }),
            ),
        ]))
        .block(Block::default().borders(Borders::ALL).title(" TUI mode "));
        frame.render_widget(header, outer[0]);

        if self.room.is_some() {
            self.draw_room(frame, outer[1]);
        } else {
            self.draw_join(frame, outer[1]);
        }

        let help = if self.room.is_some() { HELP_ROOM } else { HELP_JOIN };
        let mut footer = vec![Line::from(Span::styled(help, Style::default().fg(Color::DarkGray)))];
        footer.push(match &self.error {
            Some(e) => Line::from(Span::styled(format!("! {e}"), Style::default().fg(Color::Red))),
            None => Line::from(Span::styled(
                format!("log: {}", self.log_path.display()),
                Style::default().fg(Color::DarkGray),
            )),
        });
        frame.render_widget(Paragraph::new(footer).wrap(Wrap { trim: true }), outer[2]);
    }

    fn draw_join(&mut self, frame: &mut Frame, area: Rect) {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Length(3), Constraint::Min(0)])
            .split(area);

        let field = |label: &str, value: &str, focused: bool| {
            let style = if focused {
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Paragraph::new(Span::styled(value.to_string(), style)).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {label}{} ", if focused { " (editing)" } else { "" })),
            )
        };
        frame.render_widget(
            field("Display name", &self.display_name, self.focus == JoinFocus::Name),
            rows[0],
        );
        frame.render_widget(
            field("Join link (server|room-id|key)", &self.join_string, self.focus == JoinFocus::Link),
            rows[1],
        );

        let items: Vec<ListItem> = if self.room_store.rooms.is_empty() {
            vec![ListItem::new("(no saved lounges)")]
        } else {
            self.room_store
                .rooms
                .iter()
                .enumerate()
                .map(|(i, room)| {
                    let selected = self.focus == JoinFocus::Saved && i == self.saved_cursor;
                    let style = if selected {
                        Style::default().fg(Color::Black).bg(Color::Yellow)
                    } else {
                        Style::default()
                    };
                    ListItem::new(Span::styled(format!("{} {}", if selected { ">" } else { " " }, room.name), style))
                })
                .collect()
        };
        frame.render_widget(
            List::new(items).block(Block::default().borders(Borders::ALL).title(" Saved lounges ")),
            rows[2],
        );
    }

    fn draw_room(&mut self, frame: &mut Frame, area: Rect) {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(38), Constraint::Min(0)])
            .split(area);
        let left = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(45), Constraint::Min(0), Constraint::Length(3)])
            .split(cols[0]);

        let Some(room) = &mut self.room else { return };

        // --- members ---
        let members: Vec<ListItem> = room
            .members
            .iter()
            .map(|m| {
                let (mark, color) = if m.stream_count > 0 {
                    ("\u{25cf}", Color::Green)
                } else {
                    ("\u{25cb}", Color::DarkGray)
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{mark} "), Style::default().fg(color)),
                    Span::raw(m.display_name.clone()),
                    Span::styled(
                        if m.stream_count > 0 { format!("  ({} streams)", m.stream_count) } else { String::new() },
                        Style::default().fg(Color::DarkGray),
                    ),
                ]))
            })
            .collect();
        frame.render_widget(
            List::new(members).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" Members ({}) ", room.members.len())),
            ),
            left[0],
        );

        // --- streams ---
        let video_streams = room.video_streams();
        let stream_items: Vec<ListItem> = if video_streams.is_empty() {
            vec![ListItem::new("(no video streams)")]
        } else {
            video_streams
                .iter()
                .enumerate()
                .map(|(i, info)| {
                    let key = (info.owner, info.stream_id);
                    let watching = room.watches.contains_key(&key);
                    let selected = i == room.cursor;
                    let style = if selected {
                        Style::default().fg(Color::Black).bg(Color::Yellow)
                    } else if watching {
                        Style::default().fg(Color::Green)
                    } else {
                        Style::default()
                    };
                    ListItem::new(Span::styled(
                        format!(
                            "{} {} \u{2013} {} {}x{}{}",
                            if selected { ">" } else { " " },
                            room.owner_name(info.owner),
                            info.label,
                            info.width,
                            info.height,
                            if watching { "  [watching]" } else { "" }
                        ),
                        style,
                    ))
                })
                .collect()
        };
        frame.render_widget(
            List::new(stream_items)
                .block(Block::default().borders(Borders::ALL).title(" Streams (Enter to watch) ")),
            left[1],
        );

        // --- own share ---
        let share_line = match &room.share {
            None => Span::styled("not sharing  (press s)", Style::default().fg(Color::DarkGray)),
            Some(Share::Starting(_)) => Span::styled(
                "starting \u{2013} approve the screen-capture prompt",
                Style::default().fg(Color::Yellow),
            ),
            Some(Share::Active(handle)) => Span::styled(
                format!("sharing screen (stream {}, {} fps)", room.share_stream_id, handle.encoder_fps()),
                Style::default().fg(Color::Green),
            ),
            Some(Share::Stopping(_)) => Span::styled("stopping...", Style::default().fg(Color::Yellow)),
        };
        frame.render_widget(
            Paragraph::new(Line::from(share_line))
                .block(Block::default().borders(Borders::ALL).title(" My share (s) ")),
            left[2],
        );

        // --- video ---
        let title = match room.selected {
            Some((owner, stream_id)) => {
                let fps = room
                    .watches
                    .get_mut(&(owner, stream_id))
                    .map(|w| w.decoder.current_fps())
                    .unwrap_or(0.0);
                format!(" Video \u{2013} {} stream {stream_id} ({fps:.1} fps) ", room.owner_name(owner))
            }
            None => " Video ".to_string(),
        };
        let block = Block::default().borders(Borders::ALL).title(title);
        let inner = block.inner(cols[1]);
        frame.render_widget(block, cols[1]);
        // Recorded for the *next* tick's `refresh_video` — the pane's cell
        // size only becomes known here, during layout.
        self.video_area = inner;

        let protocol = room.selected.and_then(|key| room.watches.get(&key)).and_then(|w| w.protocol.as_ref());
        match protocol {
            // `allow_clipping`: the protocol was encoded for the *previous*
            // draw's pane size, so a resize between then and now can leave it
            // a cell too big. Clipping beats rendering nothing at all.
            Some(protocol) => frame.render_widget(Image::new(protocol).allow_clipping(true), inner),
            None => {
                let msg = if room.selected.is_some() {
                    "waiting for video \u{2013} the sender's next keyframe starts playback"
                } else {
                    "not watching anything \u{2013} pick a stream on the left and press Enter"
                };
                frame.render_widget(
                    Paragraph::new(Span::styled(msg, Style::default().fg(Color::DarkGray)))
                        .wrap(Wrap { trim: true }),
                    inner,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one piece of real logic in this module that isn't a UI callback:
    /// the `decode.rs` → `ratatui-image` bridge. A wrong stride or channel
    /// order here would show up as a garbled video pane, which is exactly the
    /// signal this whole mode exists to provide — so it's worth pinning.
    #[test]
    fn color_image_converts_to_rgb_preserving_size_and_channels() {
        let pixels = vec![
            egui::Color32::from_rgb(10, 20, 30),
            egui::Color32::from_rgb(40, 50, 60),
            egui::Color32::from_rgb(70, 80, 90),
            egui::Color32::from_rgb(100, 110, 120),
            egui::Color32::from_rgb(130, 140, 150),
            egui::Color32::from_rgb(160, 170, 180),
        ];
        let image = egui::ColorImage::new([3, 2], pixels);
        let dynamic = color_image_to_dynamic(&image);
        assert_eq!(dynamic.width(), 3);
        assert_eq!(dynamic.height(), 2);
        let rgb = dynamic.to_rgb8();
        assert_eq!(rgb.get_pixel(0, 0).0, [10, 20, 30]);
        assert_eq!(rgb.get_pixel(2, 0).0, [70, 80, 90]);
        assert_eq!(rgb.get_pixel(0, 1).0, [100, 110, 120]);
        assert_eq!(rgb.get_pixel(2, 1).0, [160, 170, 180]);
    }

    /// Half-block encoding must actually produce styled terminal cells for a
    /// known image — this is the claim the module's backend choice rests on
    /// (that a frame renders as ordinary characters a `tmux capture-pane` can
    /// read back), so it's checked against a real ratatui buffer.
    #[test]
    fn halfblock_backend_renders_an_image_into_terminal_cells() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let picker = Picker::halfblocks();
        let image = egui::ColorImage::new(
            [4, 4],
            vec![egui::Color32::from_rgb(200, 30, 30); 16],
        );
        let protocol = picker
            .new_protocol(
                color_image_to_dynamic(&image),
                Size::new(8, 4),
                Resize::Scale(Some(FilterType::Triangle)),
            )
            .expect("half-block encode must succeed");

        let mut terminal = Terminal::new(TestBackend::new(8, 4)).unwrap();
        terminal
            .draw(|frame| frame.render_widget(Image::new(&protocol).allow_clipping(true), frame.area()))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let painted = buffer
            .content()
            .iter()
            .filter(|cell| cell.fg == Color::Rgb(200, 30, 30) || cell.bg == Color::Rgb(200, 30, 30))
            .count();
        assert!(painted > 0, "half-block render produced no coloured cells");
    }

    /// The whole point of this mode, proven live and headlessly: a real relay
    /// server child process, a real capture→encrypt→fragment→send pipeline, a
    /// real `StreamDecoder`, and this module's *real* `drain_events` /
    /// `refresh_video` / `draw` driving a ratatui `TestBackend` — asserting
    /// that the video pane ends up full of distinctly-coloured terminal cells.
    ///
    /// Uses `capture.rs`'s `#[cfg(test)]` `videotestsrc` seam for the same
    /// reason `end_to_end_test.rs` does: the Wayland portal's consent dialog
    /// is the one step that cannot be automated. Everything downstream of the
    /// capture source — which is all this test is about — is the production
    /// path.
    #[tokio::test]
    #[ignore = "spawns a real relay server child process and a real GStreamer capture/decode \
                pipeline; run explicitly with `cargo test tui:: -- --ignored --nocapture`"]
    async fn watched_stream_renders_as_halfblocks_end_to_end() {
        use crate::end_to_end_test::{connect_with_retry, pick_free_udp_port, spawn_relay_server};
        use crate::net::format_join_string;
        use crate::protocol::RoomId;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        const STREAM_ID: StreamId = 1;

        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        gstreamer::init().expect("gstreamer init");

        let port = pick_free_udp_port().expect("failed to pick a free UDP port");
        let _server = spawn_relay_server(port).expect("failed to spawn relay server");
        let room_id: RoomId = rand::random();
        let room_key: [u8; 32] = rand::random();
        let join_string = format_join_string(&format!("127.0.0.1:{port}"), &room_id, &room_key);

        let (client_a, _snapshot_a, events_a) =
            connect_with_retry(&join_string, "sharer").await.expect("sharer connect");
        let (client_b, snapshot_b, events_b) =
            connect_with_retry(&join_string, "tui-watcher").await.expect("watcher connect");
        // Keep the sharer's event channel alive — dropping it makes net.rs's
        // reader loops tear the connection down (see end_to_end_test.rs).
        let _keepalive = events_a;

        let capture_handle = capture::start_sharing_from_test_source(
            &client_a,
            STREAM_ID,
            4000,
            30,
            "videotestsrc is-live=true pattern=smpte \
             ! video/x-raw,width=320,height=240,framerate=30/1",
        )
        .await
        .expect("start_sharing_from_test_source");
        let owner = client_a.conn_id();

        let mut app = App::new(TuiOptions {
            join_string: None,
            display_name: Some("tui-watcher".to_string()),
            log_path: std::env::temp_dir().join("simple-screensharing-tui-test.log"),
        });
        app.room = Some(Room {
            client: client_b,
            event_rx: events_b,
            members: snapshot_b.members,
            streams: snapshot_b.streams,
            cursor: 0,
            watches: HashMap::new(),
            selected: None,
            share: None,
            share_stream_id: 0,
        });

        // Wait for the sharer's StreamStarted delta through the real
        // `drain_events` path — the relay drops a Subscribe for a stream it
        // hasn't seen announced yet.
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            app.drain_events();
            if !app.room.as_ref().unwrap().video_streams().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            !app.room.as_ref().unwrap().video_streams().is_empty(),
            "watcher never saw the sharer's video stream announced"
        );

        // Exactly what pressing Enter in the Streams pane does.
        app.toggle_watch();
        assert_eq!(
            app.room.as_ref().unwrap().selected,
            Some((owner, STREAM_ID)),
            "toggle_watch did not start a watch on the sharer's stream"
        );

        let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("test terminal");
        let mut distinct_colors = 0usize;
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            app.drain_events();
            app.refresh_video();
            terminal.draw(|frame| app.draw(frame)).expect("draw");

            let video_area = app.video_area;
            let buffer = terminal.backend().buffer();
            let mut colors = std::collections::HashSet::new();
            for y in video_area.top()..video_area.bottom() {
                for x in video_area.left()..video_area.right() {
                    let cell = &buffer[(x, y)];
                    if let Color::Rgb(r, g, b) = cell.fg {
                        colors.insert((r, g, b));
                    }
                    if let Color::Rgb(r, g, b) = cell.bg {
                        colors.insert((r, g, b));
                    }
                }
            }
            distinct_colors = colors.len();
            // SMPTE bars are seven strongly different colours; anything in
            // that ballpark means real decoded picture reached the terminal
            // buffer, not a blank pane or a single flat fill.
            if distinct_colors >= 6 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let frames_seen = app
            .room
            .as_ref()
            .unwrap()
            .watches
            .values()
            .next()
            .map(|w| w.frames_seen)
            .unwrap_or(0);
        eprintln!("[tui] distinct colours in video pane = {distinct_colors}, frames encoded = {frames_seen}");

        if let Err(e) = capture_handle.stop().await {
            tracing::warn!("[tui] capture stop reported an error (non-fatal): {e:#}");
        }

        assert!(frames_seen > 0, "no decoded frame was ever half-block encoded for the video pane");
        assert!(
            distinct_colors >= 6,
            "video pane only had {distinct_colors} distinct colours — the decoded SMPTE pattern \
             does not appear to have reached the terminal buffer"
        );
    }
}
