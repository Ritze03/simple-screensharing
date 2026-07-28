//! Real video delivery for the View tab: a raw H.264 (Annex-B) bitstream
//! relay over a never-ending HTTP response body, meant to be decoded
//! client-side with WebCodecs (`VideoDecoder`).
//!
//! # History: replaces an MJPEG-over-HTTP decode/re-encode round trip
//!
//! This module used to decode each stream with GStreamer (`decode.rs`'s
//! `StreamDecoder`) and re-encode every frame as JPEG, served as
//! `multipart/x-mixed-replace`. That worked, but paid for a full decode +
//! re-encode on every single frame, was capped at `POLL_INTERVAL`'s 60fps
//! poll rate (in practice usually closer to the JPEG encoder's own
//! throughput), and threw away the source encoder's actual GOP structure —
//! every "frame" leaving this process was an independent JPEG regardless of
//! whether the underlying H.264 access unit was a keyframe or a delta frame.
//! `net.rs`'s `ClientEvent::MediaFrame(DecryptedMediaFrame)` already hands
//! `session.rs` a clean, access-unit-aligned Annex-B H.264 access unit per
//! frame (confirmed via `capture.rs`'s encoder caps:
//! `video/x-h264,stream-format=byte-stream,alignment=au` — no
//! GStreamer-specific/opaque wrapping, no `h264parse` in the pipeline) — so
//! this module now relays that byte-for-byte to the browser instead, which
//! decodes it directly with `VideoDecoder` (Annex-B framing is inferred by
//! Chromium when `VideoDecoder.configure()`'s `description` is omitted, so
//! no bitstream (Annex-B -> AVCC) conversion is needed here). This deletes
//! the decode+re-encode round trip entirely and removes the MJPEG poll-rate
//! ceiling — frames are forwarded as fast as they arrive.
//!
//! `Why` this module still owns its own `TcpListener`/thread-per-connection
//! machinery instead of any HTTP framework, and still needs the send-buffer
//! clamp: unchanged from the MJPEG design — see [`clamp_send_buffer`]'s doc
//! comment, detail 2 below. `Why` no WebSocket / `tungstenite`: the browser
//! side never sends anything back over this channel (subscribe/unsubscribe
//! stays on the existing REST commands in `api_server.rs`), so a plain
//! `Content-Type: application/octet-stream` response body with simple
//! length-prefixed binary frames is strictly simpler and sufficient.
//!
//! # Wire format
//!
//! The HTTP response body (`Content-Type: application/octet-stream`, never
//! terminates while the watch is active) is a sequence of chunks, each:
//!
//! ```text
//! offset  size  field
//! 0       4     body_len   u32 big-endian — byte length of everything below
//!                          (i.e. `10 + payload.len()`), NOT counting this
//!                          field itself.
//! 4       1     frame_type u8 — 0 = CodecInfo, 1 = VideoFrame (see below).
//! 5       1     flags      u8 — bit 0 (`0x01`) = keyframe. Only meaningful
//!                          for `frame_type == VideoFrame`; always 0 for
//!                          CodecInfo. Bits 1-7 reserved, always 0.
//! 6       8     pts_us     u64 big-endian — the sender's capture-time PTS
//!                          in microseconds, exactly `DecryptedMediaFrame::
//!                          pts` (same field `decode.rs`'s pacing logic
//!                          already keys off of). Always 0 for CodecInfo.
//! 14      *     payload    `body_len - 10` bytes. For VideoFrame: one raw
//!                          Annex-B H.264 access unit, byte-for-byte
//!                          identical to `DecryptedMediaFrame::payload`
//!                          (start codes included, nothing stripped or
//!                          re-wrapped). For CodecInfo: a UTF-8 WebCodecs
//!                          codec string, e.g. `avc1.640028` (no NUL
//!                          terminator — length is exact via `body_len`).
//! ```
//!
//! So each chunk is `4 + 10 + payload.len()` bytes total on the wire: a
//! 4-byte length prefix followed by a fixed 10-byte header and the payload.
//! Chunks repeat back-to-back for the life of the connection.
//!
//! **Every connection's very first chunk is always exactly one CodecInfo
//! chunk**, sent before any VideoFrame chunk, so a fresh WebCodecs
//! `VideoDecoder` on the browser side always has a codec string to
//! `configure()` with before it needs to decode anything. Every chunk after
//! that first one is a VideoFrame chunk. See [`parse_avc_codec_string`] for
//! how the codec string is derived, and the "Codec-string discovery"
//! section below for why it's delivered this way (in-band, not a REST
//! field).
//!
//! VideoFrame chunks carry raw access units exactly as received — no
//! keyframe-gating happens in this module (a watcher connecting mid-stream
//! gets whatever the producer next publishes, keyframe or not; the
//! `keyframe` flag is purely informational for the consumer). Whether a
//! freshly-subscribing watcher's *sender* actually re-sends a keyframe
//! promptly was suspected to be a separate, pre-existing bug this module
//! doesn't own — a follow-up investigation confirmed it does **not**
//! reproduce: `force_keyframe()`'s existing mechanism in `capture.rs`
//! reliably delivers a real keyframe to a late subscriber within 20-70ms,
//! verified across 12 real end-to-end runs spanning both encoder backends
//! (`x264enc` software, `vah264enc` hardware), synthetic and real
//! `xdg-desktop-portal` capture, and small and large keyframes — locked in
//! as permanent regression coverage by
//! `late_subscriber_after_unwatched_period_gets_a_real_keyframe` and its
//! `_real_portal` variant in `src/end_to_end_test.rs`. This relay just
//! forwards whatever `session.rs` hands it and correctly marks whatever NAL
//! types arrive.
//!
//! # Codec-string discovery: in-band, not a REST field
//!
//! Neither `x264enc` nor `vah264enc` in `capture.rs` sets an explicit
//! `profile=` property, so the AVC profile/level isn't statically knowable —
//! it has to be read from the stream itself. This module parses it from the
//! first SPS NAL (`nal_unit_type == 7`) seen in any keyframe access unit
//! (see [`parse_avc_codec_string`]) and caches it on the route so a
//! second/third viewer opening the same stream gets it immediately rather
//! than waiting for another keyframe. Rather than adding a second round trip
//! (e.g. extending `stream_video_url`'s DTO with a codec field, which would
//! need the frontend to poll or race it against the relay actually having
//! seen a keyframe), the codec string is delivered as the CodecInfo chunk
//! described above — always the first thing any connection to a route
//! receives. This keeps `commands.rs`/`stream_video_url`'s existing shape
//! (URL discovery only) untouched and keeps the whole contract inside one
//! place a frontend consumer has to read: this module's own wire format.
//!
//! `Why this is re-derived on every keyframe, not just the first:` profile
//! does *not* stay constant for a route's whole lifetime — `capture.rs`'s
//! `BranchSwap` (triggered by the live `tune`/`quality_level`/`rate_control`
//! encoder-tuning controls) rebuilds the encoder element from scratch, and
//! `x264enc` picks its AVC profile/level automatically from `speed-preset`
//! when no explicit `profile=` is set (confirmed empirically:
//! `speed-preset=ultrafast` emits `avc1.42c01f` — Baseline, level 3.1 —
//! while `speed-preset=veryslow` on the same resolution emits
//! `avc1.640032` — High, level 5.0 — from the exact same `x264enc` pipeline
//! fragment `capture.rs` builds). A route that cached only the *first*
//! keyframe's codec string would keep handing a stale, mismatched string to
//! every connection made after a `BranchSwap` changed it — `VideoDecoder.
//! configure()` succeeds (the string still parses fine), but every
//! `decode()` call against the new bitstream then fails, since the browser
//! configured a decoder for the old profile/level. That was a real,
//! reproduced bug: a watcher connecting after the sharer toggled any of
//! those three controls got stuck at 0fps with a `Stream error` decode
//! failure. `produce()` below now re-derives the codec string from *every*
//! keyframe's SPS (not just the first) and [`Mailbox::set_codec`] only
//! bumps the route's codec generation when the derived string actually
//! changed; [`handle_connection`]'s relay loop watches that generation
//! alongside the frame generation and pushes a fresh CodecInfo chunk to
//! *every already-connected* watcher the moment it changes, not only to new
//! connections — `tile-video.js`'s `VideoDecoder.configure()` is already
//! safe to call again on a live decoder (it resets it; the next chunk must
//! be a keyframe, which is exactly what a `BranchSwap` forces).
//!
//! # The two details carried over from the MJPEG design, unchanged
//!
//! 1. **Single-slot "latest frame" mailbox, never a queue** ([`Mailbox`]).
//!    The producer thread that receives access units from `session.rs`
//!    always *overwrites* the one slot and bumps a generation counter; it
//!    never pushes onto anything unbounded. A connection thread blocks on
//!    the mailbox's `Condvar` until the generation changes, then reads the
//!    current frame — so a slow or backgrounded reader can never accumulate
//!    a backlog of stale frames to "catch up" through; it always gets
//!    whatever is newest at the moment it looks.
//! 2. **Clamp the serving socket's send buffer** ([`clamp_send_buffer`]).
//!    A single-slot mailbox on its own is not sufficient: the OS kernel's
//!    own auto-tuned loopback TCP send buffer will happily buffer several MB
//!    between this process's `write()` calls and the webview actually
//!    reading them. A backgrounded/unfocused viewer webview that stops
//!    pumping its network stack for a few seconds would then resume by
//!    draining that kernel backlog frame by frame — exactly the
//!    "fast-forward catch-up" bug this project has already hit and fixed
//!    elsewhere (frame pacing, event ingestion — see `architecture/
//!    overview.md`). Clamping the send buffer to about one frame's worth
//!    forces `write()` itself to block/backpressure once the kernel already
//!    holds one unread frame. `Why a raw `std::net::TcpListener` and not a
//!    higher-level HTTP framework:` frameworks like `tiny_http` own the
//!    accepted socket internally and don't expose it for `setsockopt`-level
//!    tuning, which is exactly the mechanism by which this bug would
//!    reappear if one were used here.
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use simple_screensharing_client::net::DecryptedMediaFrame;

/// How long the producer thread's blocking `recv` waits before re-checking
/// `stop` — bounds shutdown latency without busy-polling. Not a frame-rate
/// ceiling: unlike the old MJPEG poll loop, frames are forwarded the instant
/// they arrive via `Receiver::recv_timeout`, not sampled at a fixed cadence.
const RECV_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Fixed size in bytes of every chunk's header (`frame_type` + `flags` +
/// `pts_us`), not counting the 4-byte length prefix — see module doc "Wire
/// format".
const CHUNK_HEADER_LEN: usize = 10;

const FRAME_TYPE_CODEC_INFO: u8 = 0;
const FRAME_TYPE_VIDEO: u8 = 1;
const FLAG_KEYFRAME: u8 = 0b0000_0001;

/// About one 1080p H.264 keyframe's worth of headroom — see this module's
/// doc comment for why this needs to be small, not merely "not unbounded".
/// Delta frames are far smaller, so this comfortably covers the worst case
/// without meaningfully increasing worst-case latency.
const CLAMPED_SEND_BUFFER_BYTES: usize = 256 * 1024;

/// One watched stream, identified by the same triple the frontend already
/// keys tiles by.
type StreamKey = (u32, u64, u8);

fn token(key: StreamKey) -> String {
    let (room_handle, owner, stream_id) = key;
    format!("{room_handle:08x}-{owner:016x}-{stream_id:02x}")
}

/// One published access unit, as handed to every connection currently
/// tailing this route's [`Mailbox`].
#[derive(Clone)]
struct RawFrame {
    pts_us: u64,
    keyframe: bool,
    /// The raw Annex-B access unit, byte-for-byte as received — see module
    /// doc "Wire format".
    payload: Arc<[u8]>,
}

/// The single-slot "latest frame" mailbox — see module doc, detail 1.
/// `generation` only ever increases; a reader that has seen generation `g`
/// blocks until the stored generation differs from `g`, then takes whatever
/// is current (never queued, never replayed).
///
/// Also carries this route's codec string (see module doc, "Codec-string
/// discovery") — a separate slot on the same `Mutex`/`Condvar` rather than a
/// second mailbox, since every connection needs to read the *current* value
/// up front regardless of which frame generation is current when it
/// connects, and (unlike the frame slot) already-connected readers also need
/// to notice a later change — see `codec_generation`'s doc comment.
struct Mailbox {
    inner: Mutex<MailboxInner>,
    cvar: Condvar,
}

struct MailboxInner {
    generation: u64,
    frame: Option<RawFrame>,
    codec: Option<Arc<str>>,
    /// Bumped every time `codec` actually *changes* value (never on a
    /// same-value `set_codec` call — see its doc comment). A connection
    /// remembers the generation it last sent a CodecInfo chunk for and
    /// re-checks this alongside the frame generation on every wakeup, so a
    /// mid-connection `BranchSwap` that changes the encoder's profile/level
    /// (see module doc) gets a fresh CodecInfo chunk pushed to it instead of
    /// silently decoding against a stale one.
    codec_generation: u64,
    closed: bool,
}

impl Mailbox {
    fn new() -> Self {
        Self {
            inner: Mutex::new(MailboxInner { generation: 0, frame: None, codec: None, codec_generation: 0, closed: false }),
            cvar: Condvar::new(),
        }
    }

    /// Overwrite the slot with a new frame. Called only by this stream's
    /// producer thread.
    fn publish(&self, frame: RawFrame) {
        let mut inner = self.inner.lock().unwrap();
        inner.frame = Some(frame);
        inner.generation = inner.generation.wrapping_add(1);
        self.cvar.notify_all();
    }

    /// Record this route's current codec string. A no-op — no generation
    /// bump, no wakeup — if it's unchanged from the cached value (the common
    /// case: most keyframes reconfirm the same profile/level the route
    /// already has cached). Only actually updates, bumps
    /// `codec_generation`, and wakes waiters when the derived string
    /// *differs* from what's cached — i.e. a `BranchSwap` changed the
    /// encoder's profile/level (see module doc).
    fn set_codec(&self, codec: String) {
        let mut inner = self.inner.lock().unwrap();
        if inner.codec.as_deref() != Some(codec.as_str()) {
            inner.codec = Some(Arc::from(codec));
            inner.codec_generation = inner.codec_generation.wrapping_add(1);
            self.cvar.notify_all();
        }
    }

    /// Wake every blocked reader with nothing to give them (route torn down).
    fn close(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.closed = true;
        self.cvar.notify_all();
    }

    /// Block until the codec string is known (or the mailbox closes).
    /// Returns it along with the codec generation it was read at, so the
    /// caller can tell a later `set_codec` change apart from the one it
    /// already has.
    fn wait_codec(&self) -> Option<(Arc<str>, u64)> {
        let mut inner = self.inner.lock().unwrap();
        loop {
            if let Some(codec) = &inner.codec {
                return Some((codec.clone(), inner.codec_generation));
            }
            if inner.closed {
                return None;
            }
            inner = self.cvar.wait(inner).unwrap();
        }
    }

    /// Block until either a frame newer than `*last_frame_gen` or a codec
    /// string newer than `*last_codec_gen` exists (or the mailbox closes),
    /// then return whichever is ready and advance the corresponding
    /// counter. Checks the codec first so a change is always delivered
    /// before any frame encoded under it — matches `produce()`'s own
    /// ordering (`set_codec` before `publish` for the same access unit).
    /// Never returns something the caller has already seen, and never
    /// buffers more than one frame.
    fn wait_update(&self, last_frame_gen: &mut u64, last_codec_gen: &mut u64) -> Option<MailboxUpdate> {
        let mut inner = self.inner.lock().unwrap();
        loop {
            if inner.codec_generation != *last_codec_gen {
                *last_codec_gen = inner.codec_generation;
                if let Some(codec) = &inner.codec {
                    return Some(MailboxUpdate::Codec(codec.clone()));
                }
            }
            if inner.generation != *last_frame_gen && inner.frame.is_some() {
                *last_frame_gen = inner.generation;
                return Some(MailboxUpdate::Frame(inner.frame.clone().unwrap()));
            }
            if inner.closed {
                return None;
            }
            inner = self.cvar.wait(inner).unwrap();
        }
    }
}

/// What [`Mailbox::wait_update`] hands a connection thread to relay.
enum MailboxUpdate {
    Codec(Arc<str>),
    Frame(RawFrame),
}

struct Route {
    mailbox: Arc<Mailbox>,
    stop: Arc<AtomicBool>,
    producer: JoinHandle<()>,
}

/// Owns the one local HTTP listener this process serves every watched
/// stream's H.264 relay through (routed by path, one path per stream —
/// simpler than one port per stream, and just as capable of serving several
/// simultaneous tiles with no starvation between them, since each gets its
/// own `Mailbox` and each connection its own thread). Lives in `Managed`,
/// started once at construction, torn down implicitly when the process
/// exits.
pub struct VideoServer {
    port: u16,
    routes: Arc<Mutex<HashMap<StreamKey, Route>>>,
}

impl VideoServer {
    pub fn start() -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let routes: Arc<Mutex<HashMap<StreamKey, Route>>> = Arc::new(Mutex::new(HashMap::new()));

        let accept_routes = routes.clone();
        std::thread::Builder::new()
            .name("video-server-accept".into())
            .spawn(move || accept_loop(listener, accept_routes))
            .expect("spawn video-server accept thread");

        Ok(Self { port, routes })
    }

    /// Start (or restart) serving `key`'s raw access units, read from `rx`
    /// (fed by `session.rs`'s media-ingestion task via `WatchEntry::
    /// raw_tx` — see that field's doc comment). Returns the URL `view.js`
    /// should open to receive this module's wire format (see module doc). If
    /// a route already exists for this key (shouldn't normally happen —
    /// `toggle_watch` only calls this on the transition into watching — but
    /// a stale route would otherwise leak its producer thread), it's torn
    /// down first.
    pub fn start_route(&self, key: StreamKey, rx: Receiver<DecryptedMediaFrame>) -> String {
        self.stop_route(key);

        let mailbox = Arc::new(Mailbox::new());
        let stop = Arc::new(AtomicBool::new(false));

        let producer_mailbox = mailbox.clone();
        let producer_stop = stop.clone();
        let producer = std::thread::Builder::new()
            .name(format!("video-server-relay-{}", token(key)))
            .spawn(move || produce(rx, producer_mailbox, producer_stop))
            .expect("spawn video-server producer thread");

        self.routes.lock().unwrap().insert(key, Route { mailbox, stop, producer });

        format!("http://127.0.0.1:{}/stream/{}", self.port, token(key))
    }

    /// Tear down `key`'s route, if any: signal its producer to stop, wake
    /// every blocked connection thread via `Mailbox::close` (so they notice
    /// the route is gone and close their sockets instead of blocking
    /// forever), and join the producer thread so no thread is left running
    /// past this call.
    pub fn stop_route(&self, key: StreamKey) {
        let Some(route) = self.routes.lock().unwrap().remove(&key) else { return };
        route.stop.store(true, Ordering::Relaxed);
        route.mailbox.close();
        let _ = route.producer.join();
    }

    /// Tear down every route belonging to `room_handle` — used when a whole
    /// room disconnects out from under its watches (control-plane close),
    /// which never goes through `toggle_watch`'s explicit stop path.
    pub fn stop_room(&self, room_handle: u32) {
        let keys: Vec<StreamKey> = self.routes.lock().unwrap().keys().filter(|(r, _, _)| *r == room_handle).copied().collect();
        for key in keys {
            self.stop_route(key);
        }
    }

    /// `None` if `key` isn't currently being served (not watched, or the
    /// watch was just stopped) — lets `view.js` rebuild its tile list from
    /// scratch (e.g. on tab reload) without needing to have been the caller
    /// that started the watch.
    pub fn url_for(&self, key: StreamKey) -> Option<String> {
        self.routes.lock().unwrap().contains_key(&key).then(|| format!("http://127.0.0.1:{}/stream/{}", self.port, token(key)))
    }
}

/// Producer: forward every access unit `rx` delivers straight to the
/// mailbox, and derive this route's codec string from every keyframe that
/// contains an SPS (not just the first — see module doc for why: a
/// `BranchSwap` can change the encoder's profile/level mid-route).
/// `Mailbox::set_codec` is idempotent for an unchanged value, so this costs
/// nothing extra beyond a cheap SPS scan on the (infrequent) keyframes.
/// Exits as soon as `stop` is observed or `rx` disconnects (the
/// `WatchEntry`/`RoomSession` that owned the sending half was torn down),
/// whichever comes first.
fn produce(rx: Receiver<DecryptedMediaFrame>, mailbox: Arc<Mailbox>, stop: Arc<AtomicBool>) {
    tracing::debug!("video_server: producer thread started");
    let mut published_any = false;
    while !stop.load(Ordering::Relaxed) {
        let frame = match rx.recv_timeout(RECV_POLL_INTERVAL) {
            Ok(frame) => frame,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };

        if frame.keyframe {
            if let Some(codec) = parse_avc_codec_string(&frame.payload) {
                tracing::debug!("video_server: derived codec string {codec:?}");
                mailbox.set_codec(codec);
            }
        }

        if !published_any {
            tracing::debug!("video_server: first access unit published, {} bytes, keyframe={}", frame.payload.len(), frame.keyframe);
            published_any = true;
        }
        mailbox.publish(RawFrame { pts_us: frame.pts, keyframe: frame.keyframe, payload: Arc::from(frame.payload) });
    }
    tracing::debug!("video_server: producer thread stopping");
}

/// Scan an Annex-B access unit for the first SPS NAL (`nal_unit_type == 7`)
/// and derive a WebCodecs `avc1.PPCCLL` codec string from its first three
/// RBSP bytes: `profile_idc`, `constraint_flag_set` (the constraint/reserved
/// flags byte), and `level_idc` (ITU-T H.264 §7.3.2.1.1 — these are always
/// the first three bytes of the SPS RBSP, right after the one-byte NAL
/// header). No emulation-prevention de-escaping is needed to read them: the
/// `0x03` escape byte only ever appears after two *preceding* zero bytes
/// within the RBSP, and there aren't two prior RBSP bytes yet by the time
/// the third one is read.
///
/// Tolerates both 3-byte (`00 00 01`) and 4-byte (`00 00 00 01`) Annex-B
/// start codes. Returns `None` if no SPS NAL is found (e.g. a keyframe whose
/// SPS/PPS didn't happen to be re-sent inline with this particular access
/// unit) — the caller just tries again on the next keyframe.
fn parse_avc_codec_string(access_unit: &[u8]) -> Option<String> {
    let mut i = 0usize;
    while i + 2 < access_unit.len() {
        let marker_len = if access_unit[i] == 0 && access_unit[i + 1] == 0 && access_unit[i + 2] == 1 {
            3
        } else if i + 3 < access_unit.len() && access_unit[i] == 0 && access_unit[i + 1] == 0 && access_unit[i + 2] == 0 && access_unit[i + 3] == 1 {
            4
        } else {
            i += 1;
            continue;
        };

        let nal_start = i + marker_len;
        let Some(&nal_header) = access_unit.get(nal_start) else { break };
        let nal_unit_type = nal_header & 0x1f;
        if nal_unit_type == 7 {
            if let Some(rbsp) = access_unit.get(nal_start + 1..nal_start + 4) {
                let [profile_idc, constraint_flags, level_idc] = [rbsp[0], rbsp[1], rbsp[2]];
                return Some(format!("avc1.{profile_idc:02x}{constraint_flags:02x}{level_idc:02x}"));
            }
        }
        i = nal_start;
    }
    None
}

fn accept_loop(listener: TcpListener, routes: Arc<Mutex<HashMap<StreamKey, Route>>>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let routes = routes.clone();
        std::thread::spawn(move || handle_connection(stream, &routes));
    }
}

fn clamp_send_buffer(stream: &TcpStream) {
    // See module doc, detail 2. Converting through `socket2::Socket` and
    // back doesn't touch the underlying fd — just gives access to
    // `setsockopt(SO_SNDBUF, ...)`, which `std::net::TcpStream` doesn't
    // expose directly.
    let sock = socket2::Socket::from(stream.try_clone().expect("clone accepted TcpStream"));
    if let Err(e) = sock.set_send_buffer_size(CLAMPED_SEND_BUFFER_BYTES) {
        tracing::warn!("video_server: set_send_buffer_size failed: {e:#}");
    }
    // `sock` (a second owner of the same fd) is dropped here without
    // closing it: `socket2::Socket`'s `Drop` calls `close(2)` on its own fd,
    // but the OS reference-counts the underlying file description — the
    // original `stream` still holds its own `dup`'d reference from
    // `try_clone`, so the connection stays open for `stream` to keep using.
}

/// Parse just enough of the request line to get the path — this is a
/// single-purpose local frame server, not a general HTTP implementation.
fn read_request_path(stream: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    // Read until we've seen the blank line ending the headers, bounded so a
    // misbehaving/garbage client can't make this loop forever.
    while buf.len() < 8192 {
        if stream.read(&mut byte).ok()? == 0 {
            return None;
        }
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let request_line = text.lines().next()?;
    let mut parts = request_line.split_whitespace();
    parts.next()?; // method
    let path = parts.next()?;
    Some(path.trim_start_matches("/stream/").to_string())
}

/// Write one wire-format chunk (length prefix + header + payload) — see
/// module doc "Wire format". The 4-byte length prefix and 10-byte header are
/// combined into a single `write_all` (one syscall) followed by a second
/// `write_all` for the payload, rather than one `write_all` over a freshly
/// concatenated buffer, to avoid copying every access unit's payload just to
/// prepend 14 bytes in front of it.
fn write_chunk(stream: &mut TcpStream, frame_type: u8, flags: u8, pts_us: u64, payload: &[u8]) -> std::io::Result<()> {
    let mut header = [0u8; 4 + CHUNK_HEADER_LEN];
    let body_len = (CHUNK_HEADER_LEN + payload.len()) as u32;
    header[0..4].copy_from_slice(&body_len.to_be_bytes());
    header[4] = frame_type;
    header[5] = flags;
    header[6..14].copy_from_slice(&pts_us.to_be_bytes());
    stream.write_all(&header)?;
    stream.write_all(payload)
}

fn handle_connection(mut stream: TcpStream, routes: &Arc<Mutex<HashMap<StreamKey, Route>>>) {
    clamp_send_buffer(&stream);
    let _ = stream.set_nodelay(true);

    let Some(requested_token) = read_request_path(&mut stream) else { return };

    let mailbox = {
        let routes = routes.lock().unwrap();
        routes.iter().find(|(key, _)| token(**key) == requested_token).map(|(_, route)| route.mailbox.clone())
    };
    let Some(mailbox) = mailbox else {
        tracing::debug!(
            "video_server: 404 for token {requested_token:?}, known routes: {:?}",
            routes.lock().unwrap().keys().map(|k| token(*k)).collect::<Vec<_>>()
        );
        let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n");
        return;
    };

    let headers =
        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nCache-Control: no-cache, no-store\r\nConnection: close\r\n\r\n";
    if stream.write_all(headers.as_bytes()).is_err() {
        return;
    }

    // Every connection's first chunk is always CodecInfo — see module doc
    // "Wire format" and "Codec-string discovery". Blocks until the route's
    // producer has derived it from a keyframe's SPS (or the route closes
    // first, e.g. the watch was stopped before any keyframe ever arrived).
    let Some((codec, mut last_codec_seen)) = mailbox.wait_codec() else { return };
    if write_chunk(&mut stream, FRAME_TYPE_CODEC_INFO, 0, 0, codec.as_bytes()).is_err() {
        return;
    }

    // Deliberately starts at generation 0 with nothing yet seen: the first
    // `wait_update` call returns whatever the producer has already
    // published (if generation has already moved past 0 by the time we get
    // here) or blocks until it does — either way, a viewer that opens a tile
    // before the first frame is ready simply waits for it instead of
    // erroring. `last_codec_seen` starts at the generation the initial
    // CodecInfo chunk above was already sent for, so this loop only reacts
    // to a *later* change (e.g. a mid-connection `BranchSwap` — see module
    // doc), never re-sending the one already written above.
    // Whether this connection has relayed a keyframe yet — gates every
    // VideoFrame chunk below. `wait_update`'s very first call for a brand
    // new connection returns whatever the mailbox's single slot currently
    // holds, which is the route's *latest* published frame, not necessarily
    // a keyframe: a route that's been flowing for a while (the ordinary
    // steady state once the first watcher has been connected for more than
    // a GOP or two) has a delta frame cached the vast majority of the time.
    // A connection that started watching via `toggle_watch` gets a fresh
    // keyframe forced for it by `capture.rs`'s `force_keyframe()` on
    // subscribe (see module doc), but a SECOND connection to an
    // already-watched stream's route (e.g. the standalone viewer window
    // opening a stream the main window's View tab is already watching —
    // `stream_video_url` resolves the existing route without a new
    // `toggle_watch` call) gets no such forced keyframe. Without this gate,
    // that connection's first VideoFrame chunk could be a delta frame,
    // which a freshly-configured `VideoDecoder` cannot decode (see
    // `tile-video.js`'s `startTileVideoInner`) — surfacing as the decoder's
    // `error` callback and the "Stream error" placeholder, reproduced and
    // confirmed by
    // `tests::a_late_connection_to_an_already_flowing_route_gets_a_keyframe_first`.
    // Every delta frame published before this connection's first keyframe
    // is simply skipped (not buffered/replayed — consistent with the
    // single-slot mailbox's never-queue design); `capture.rs`'s periodic
    // `GOP_SECONDS`-interval keyframe (2s, independent of any subscribe
    // event) bounds how long that wait can take even with no forced
    // keyframe. A mid-connection `BranchSwap` (a later `Codec` update) resets
    // this the same way a fresh connection starts, since `tile-video.js`
    // resets its decoder on every CodecInfo chunk and needs a keyframe again
    // right after.
    let mut has_relayed_keyframe = false;

    let mut last_seen = 0u64;
    loop {
        match mailbox.wait_update(&mut last_seen, &mut last_codec_seen) {
            Some(MailboxUpdate::Codec(codec)) => {
                has_relayed_keyframe = false;
                if write_chunk(&mut stream, FRAME_TYPE_CODEC_INFO, 0, 0, codec.as_bytes()).is_err() {
                    break;
                }
            }
            Some(MailboxUpdate::Frame(frame)) => {
                if !has_relayed_keyframe && !frame.keyframe {
                    continue;
                }
                has_relayed_keyframe = true;
                let flags = if frame.keyframe { FLAG_KEYFRAME } else { 0 };
                if write_chunk(&mut stream, FRAME_TYPE_VIDEO, flags, frame.pts_us, &frame.payload).is_err() {
                    break;
                }
            }
            None => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic Annex-B access unit for a 1920x1080-ish High Profile
    /// (`profile_idc = 0x64`), no extra constraints (`0x00`), Level 4.0
    /// (`level_idc = 0x28`) stream: one SPS NAL (type 7) with a handful of
    /// filler RBSP bytes after the three bytes this module actually reads,
    /// followed by one IDR slice NAL (type 5) standing in for the rest of
    /// the keyframe.
    fn synthetic_keyframe_au() -> Vec<u8> {
        let mut au = Vec::new();
        au.extend_from_slice(&[0, 0, 0, 1]); // 4-byte start code
        au.push(0x67); // NAL header: nal_ref_idc=3, type=7 (SPS)
        au.extend_from_slice(&[0x64, 0x00, 0x28]); // profile/constraints/level
        au.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]); // rest of SPS RBSP (not read)
        au.extend_from_slice(&[0, 0, 1]); // 3-byte start code
        au.push(0x65); // NAL header: type=5 (IDR slice)
        au.extend_from_slice(&[0x11, 0x22, 0x33]); // stand-in slice bytes
        au
    }

    fn synthetic_delta_au() -> Vec<u8> {
        let mut au = Vec::new();
        au.extend_from_slice(&[0, 0, 1]);
        au.push(0x41); // NAL header: type=1 (non-IDR slice)
        au.extend_from_slice(&[0x99, 0x88]);
        au
    }

    #[test]
    fn parse_avc_codec_string_reads_sps_profile_constraints_level() {
        let au = synthetic_keyframe_au();
        assert_eq!(parse_avc_codec_string(&au).as_deref(), Some("avc1.640028"));
    }

    #[test]
    fn parse_avc_codec_string_none_without_sps() {
        assert_eq!(parse_avc_codec_string(&synthetic_delta_au()), None);
    }

    /// End-to-end over a real loopback socket: starts a route, connects a
    /// plain `TcpStream` to the returned URL (standing in for the browser's
    /// `fetch()`), then feeds a synthetic keyframe and delta frame through
    /// the same `std::sync::mpsc` channel `commands::toggle_watch` wires up
    /// in the real app — one at a time, only sending the next frame after
    /// reading the previous one off the wire, since the single-slot mailbox
    /// (by design — see module doc, detail 1) would otherwise let a second
    /// frame published before the first is read overwrite it. Parses the
    /// wire format byte-for-byte: first chunk must be CodecInfo with the
    /// derived codec string, second chunk must be the keyframe VideoFrame
    /// with the keyframe flag set and the exact payload bytes, third chunk
    /// must be the delta VideoFrame with the flag clear.
    #[test]
    fn relay_streams_codec_info_then_frames_in_wire_format() {
        let server = VideoServer::start().expect("start VideoServer");
        let key = (1u32, 2u64, 3u8);
        let (tx, rx) = std::sync::mpsc::channel();
        let url = server.start_route(key, rx);

        // Strip "http://127.0.0.1:{port}" and connect directly rather than
        // parsing the URL with a crate this module has no other need for.
        let addr = url.trim_start_matches("http://").split('/').next().unwrap();
        let mut stream = TcpStream::connect(addr).expect("connect to relay");
        let path = format!("/{}", url.splitn(4, '/').nth(3).unwrap());
        stream.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes()).unwrap();

        // Read and discard the HTTP response headers.
        let mut header_buf = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            stream.read_exact(&mut byte).unwrap();
            header_buf.push(byte[0]);
            if header_buf.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let headers = String::from_utf8_lossy(&header_buf);
        assert!(headers.starts_with("HTTP/1.1 200 OK"), "headers: {headers:?}");
        assert!(headers.contains("Content-Type: application/octet-stream"), "headers: {headers:?}");

        let read_chunk = |stream: &mut TcpStream| -> (u8, u8, u64, Vec<u8>) {
            let mut prefix = [0u8; 4];
            stream.read_exact(&mut prefix).unwrap();
            let body_len = u32::from_be_bytes(prefix) as usize;
            let mut body = vec![0u8; body_len];
            stream.read_exact(&mut body).unwrap();
            let frame_type = body[0];
            let flags = body[1];
            let pts_us = u64::from_be_bytes(body[2..10].try_into().unwrap());
            let payload = body[10..].to_vec();
            (frame_type, flags, pts_us, payload)
        };

        let keyframe_payload = synthetic_keyframe_au();
        tx.send(DecryptedMediaFrame {
            conn_id: 2,
            stream_id: 3,
            pts: 1_000,
            keyframe: true,
            stalled: false,
            payload: keyframe_payload.clone(),
        })
        .unwrap();

        let (frame_type, _flags, _pts, payload) = read_chunk(&mut stream);
        assert_eq!(frame_type, FRAME_TYPE_CODEC_INFO);
        assert_eq!(String::from_utf8(payload).unwrap(), "avc1.640028");

        let (frame_type, flags, pts_us, payload) = read_chunk(&mut stream);
        assert_eq!(frame_type, FRAME_TYPE_VIDEO);
        assert_eq!(flags & FLAG_KEYFRAME, FLAG_KEYFRAME);
        assert_eq!(pts_us, 1_000);
        assert_eq!(payload, keyframe_payload);

        let delta_payload = synthetic_delta_au();
        tx.send(DecryptedMediaFrame {
            conn_id: 2,
            stream_id: 3,
            pts: 1_033,
            keyframe: false,
            stalled: false,
            payload: delta_payload.clone(),
        })
        .unwrap();

        let (frame_type, flags, pts_us, payload) = read_chunk(&mut stream);
        assert_eq!(frame_type, FRAME_TYPE_VIDEO);
        assert_eq!(flags & FLAG_KEYFRAME, 0);
        assert_eq!(pts_us, 1_033);
        assert_eq!(payload, delta_payload);

        server.stop_route(key);
    }

    /// A second synthetic keyframe with a *different* SPS profile/level from
    /// [`synthetic_keyframe_au`] — Baseline (`profile_idc = 0x42`), no extra
    /// constraints, Level 3.1 (`level_idc = 0x1f`) — standing in for the
    /// real profile/level jump this project measured `capture.rs`'s
    /// `x264enc` actually producing across a `BranchSwap` between
    /// `speed-preset=veryslow` and `speed-preset=ultrafast` at the same
    /// resolution (`avc1.640032` vs. `avc1.42c01f`, see module doc).
    fn synthetic_keyframe_au_different_profile() -> Vec<u8> {
        let mut au = Vec::new();
        au.extend_from_slice(&[0, 0, 0, 1]);
        au.push(0x67);
        au.extend_from_slice(&[0x42, 0xc0, 0x1f]);
        au.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);
        au.extend_from_slice(&[0, 0, 1]);
        au.push(0x65);
        au.extend_from_slice(&[0x11, 0x22, 0x33]);
        au
    }

    /// Regression test for the reproduced bug: a watcher connects, gets the
    /// route's first CodecInfo chunk and a keyframe under it (exactly like
    /// [`relay_streams_codec_info_then_frames_in_wire_format`]), then the
    /// simulated `BranchSwap` sends a keyframe with a *different* SPS
    /// profile/level — standing in for `capture.rs` rebuilding the encoder
    /// with a different `tune`/`quality_level`/`rate_control` after the
    /// watcher already connected. Before the fix, `Mailbox::set_codec` was a
    /// no-op after the first call and the connection's relay loop never
    /// re-checked the codec at all, so the already-connected watcher would
    /// silently keep decoding new-profile frames against the old cached
    /// codec string — exactly the "stuck at 0fps / decode failure" bug this
    /// asserts against by requiring a *second* CodecInfo chunk, with the new
    /// profile, arrive before the new keyframe.
    #[test]
    fn relay_pushes_a_fresh_codec_info_chunk_when_a_branch_swap_changes_profile() {
        let server = VideoServer::start().expect("start VideoServer");
        let key = (1u32, 2u64, 3u8);
        let (tx, rx) = std::sync::mpsc::channel();
        let url = server.start_route(key, rx);

        let addr = url.trim_start_matches("http://").split('/').next().unwrap();
        let mut stream = TcpStream::connect(addr).expect("connect to relay");
        let path = format!("/{}", url.splitn(4, '/').nth(3).unwrap());
        stream.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes()).unwrap();

        let mut header_buf = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            stream.read_exact(&mut byte).unwrap();
            header_buf.push(byte[0]);
            if header_buf.ends_with(b"\r\n\r\n") {
                break;
            }
        }

        let read_chunk = |stream: &mut TcpStream| -> (u8, u8, u64, Vec<u8>) {
            let mut prefix = [0u8; 4];
            stream.read_exact(&mut prefix).unwrap();
            let body_len = u32::from_be_bytes(prefix) as usize;
            let mut body = vec![0u8; body_len];
            stream.read_exact(&mut body).unwrap();
            let frame_type = body[0];
            let flags = body[1];
            let pts_us = u64::from_be_bytes(body[2..10].try_into().unwrap());
            let payload = body[10..].to_vec();
            (frame_type, flags, pts_us, payload)
        };

        // First keyframe, "pre-swap" profile — connection gets CodecInfo
        // then the keyframe, same as the existing happy-path test.
        tx.send(DecryptedMediaFrame {
            conn_id: 2,
            stream_id: 3,
            pts: 1_000,
            keyframe: true,
            stalled: false,
            payload: synthetic_keyframe_au(),
        })
        .unwrap();
        let (frame_type, _, _, payload) = read_chunk(&mut stream);
        assert_eq!(frame_type, FRAME_TYPE_CODEC_INFO);
        assert_eq!(String::from_utf8(payload).unwrap(), "avc1.640028");
        let (frame_type, _, _, _) = read_chunk(&mut stream);
        assert_eq!(frame_type, FRAME_TYPE_VIDEO);

        // Simulated `BranchSwap`: a keyframe with a different SPS arrives on
        // the same still-open connection. The relay must push a fresh
        // CodecInfo chunk (with the new profile) before the new keyframe,
        // not just keep relaying frames under the stale cached string.
        tx.send(DecryptedMediaFrame {
            conn_id: 2,
            stream_id: 3,
            pts: 2_000,
            keyframe: true,
            stalled: false,
            payload: synthetic_keyframe_au_different_profile(),
        })
        .unwrap();
        let (frame_type, _, _, payload) = read_chunk(&mut stream);
        assert_eq!(frame_type, FRAME_TYPE_CODEC_INFO, "expected a second CodecInfo chunk after the profile changed");
        assert_eq!(String::from_utf8(payload).unwrap(), "avc1.42c01f");
        let (frame_type, flags, pts_us, payload) = read_chunk(&mut stream);
        assert_eq!(frame_type, FRAME_TYPE_VIDEO);
        assert_eq!(flags & FLAG_KEYFRAME, FLAG_KEYFRAME);
        assert_eq!(pts_us, 2_000);
        assert_eq!(payload, synthetic_keyframe_au_different_profile());

        server.stop_route(key);
    }

    /// Reproduces the actual root cause of the "viewer window shows Stream
    /// error while the main window's tile keeps working" bug report: a late
    /// connection to a route that's ALREADY flowing (main window has been
    /// watching for a while, so the mailbox's *currently cached* frame is a
    /// delta frame, not a keyframe — the ordinary steady state) must get a
    /// KEYFRAME as its first VideoFrame chunk, since a fresh `VideoDecoder`
    /// cannot decode a delta frame with nothing to reference. Before the
    /// fix, `wait_update`'s first call for a new connection thread returns
    /// whatever the mailbox currently has cached — keyframe or not — which
    /// is exactly the bug: the viewer window's second connection to an
    /// already-watched stream gets no fresh subscribe (no `toggle_watch`
    /// call — see `stream_video_url`'s doc comment), so `capture.rs`'s
    /// `force_keyframe()` is never triggered for it, and it silently
    /// receives a delta frame as its first-ever decode input.
    #[test]
    fn a_late_connection_to_an_already_flowing_route_gets_a_keyframe_first() {
        let server = VideoServer::start().expect("start VideoServer");
        let key = (1u32, 2u64, 3u8);
        let (tx, rx) = std::sync::mpsc::channel();
        let url = server.start_route(key, rx);

        // Simulate the route having been flowing for a while already: a
        // keyframe, then several delta frames, published *before* the late
        // connection ever attaches — exactly what a route the main window
        // has been watching for a bit looks like from the mailbox's point of
        // view. No connection is reading yet, so each `publish()` simply
        // overwrites the single slot (by design).
        tx.send(DecryptedMediaFrame { conn_id: 2, stream_id: 3, pts: 1_000, keyframe: true, stalled: false, payload: synthetic_keyframe_au() })
            .unwrap();
        std::thread::sleep(Duration::from_millis(50));
        for i in 1..=3u64 {
            tx.send(DecryptedMediaFrame {
                conn_id: 2,
                stream_id: 3,
                pts: 1_000 + i * 33,
                keyframe: false,
                stalled: false,
                payload: synthetic_delta_au(),
            })
            .unwrap();
            std::thread::sleep(Duration::from_millis(20));
        }
        // Give the producer thread time to actually publish the last delta
        // frame into the mailbox before the late connection attaches.
        std::thread::sleep(Duration::from_millis(50));

        // NOW the late connection attaches — standing in for the viewer
        // window's fetch() to an already-watched stream's route.
        let addr = url.trim_start_matches("http://").split('/').next().unwrap();
        let mut stream = TcpStream::connect(addr).expect("connect to relay");
        // Bounds this test to a clear failure instead of hanging forever if
        // a future regression reintroduces a wait-forever bug (e.g. the
        // keyframe gate never getting satisfied).
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let path = format!("/{}", url.splitn(4, '/').nth(3).unwrap());
        stream.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes()).unwrap();
        let mut header_buf = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            stream.read_exact(&mut byte).unwrap();
            header_buf.push(byte[0]);
            if header_buf.ends_with(b"\r\n\r\n") {
                break;
            }
        }

        let read_chunk = |stream: &mut TcpStream| -> (u8, u8, u64, Vec<u8>) {
            let mut prefix = [0u8; 4];
            stream.read_exact(&mut prefix).unwrap();
            let body_len = u32::from_be_bytes(prefix) as usize;
            let mut body = vec![0u8; body_len];
            stream.read_exact(&mut body).unwrap();
            (body[0], body[1], u64::from_be_bytes(body[2..10].try_into().unwrap()), body[10..].to_vec())
        };

        let (frame_type, _, _, _) = read_chunk(&mut stream);
        assert_eq!(frame_type, FRAME_TYPE_CODEC_INFO, "first chunk must always be CodecInfo");

        // More delta frames arrive AFTER the late connection has attached
        // too — standing in for `capture.rs`'s ordinary encoder output
        // continuing while this connection has not yet seen a keyframe.
        // These must be silently skipped, not relayed, since relaying one
        // as this connection's first VideoFrame chunk is exactly the bug.
        for i in 4..=5u64 {
            tx.send(DecryptedMediaFrame {
                conn_id: 2,
                stream_id: 3,
                pts: 1_000 + i * 33,
                keyframe: false,
                stalled: false,
                payload: synthetic_delta_au(),
            })
            .unwrap();
        }
        // Then a real keyframe — standing in for `capture.rs`'s periodic
        // `GOP_SECONDS`-interval keyframe (2s, independent of any subscribe
        // event), which is what actually unblocks a late connection like
        // this one in the real app.
        let keyframe_payload = synthetic_keyframe_au();
        tx.send(DecryptedMediaFrame { conn_id: 2, stream_id: 3, pts: 1_200, keyframe: true, stalled: false, payload: keyframe_payload.clone() })
            .unwrap();

        let (frame_type, flags, _, payload) = read_chunk(&mut stream);
        assert_eq!(frame_type, FRAME_TYPE_VIDEO);
        assert_eq!(
            flags & FLAG_KEYFRAME,
            FLAG_KEYFRAME,
            "a late connection's first VideoFrame chunk must be a keyframe — a fresh VideoDecoder cannot decode a delta frame first"
        );
        assert_eq!(payload, keyframe_payload, "the delta frames published before/after connecting must have been skipped, not relayed");

        server.stop_route(key);
    }

    /// Diagnostic for the "viewer window shows Stream error while the main
    /// window's tile keeps working" bug report: opens TWO independent
    /// `TcpStream` connections to the SAME route (standing in for the main
    /// window's tile and the viewer window's tile both watching the same
    /// stream) and asserts BOTH receive the CodecInfo chunk and every
    /// subsequently published frame, not just whichever connected first.
    #[test]
    fn two_concurrent_connections_to_the_same_route_both_see_every_frame() {
        let server = VideoServer::start().expect("start VideoServer");
        let key = (1u32, 2u64, 3u8);
        let (tx, rx) = std::sync::mpsc::channel();
        let url = server.start_route(key, rx);

        let connect = |url: &str| -> TcpStream {
            let addr = url.trim_start_matches("http://").split('/').next().unwrap();
            let mut stream = TcpStream::connect(addr).expect("connect to relay");
            let path = format!("/{}", url.splitn(4, '/').nth(3).unwrap());
            stream.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes()).unwrap();
            let mut header_buf = Vec::new();
            let mut byte = [0u8; 1];
            loop {
                stream.read_exact(&mut byte).unwrap();
                header_buf.push(byte[0]);
                if header_buf.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            stream
        };

        // Connection A ("main window's tile") connects first.
        let mut conn_a = connect(&url);
        // Connection B ("viewer window's tile") connects to the SAME route
        // second — this is the exact structural situation the bug report
        // describes.
        let mut conn_b = connect(&url);

        let read_chunk = |stream: &mut TcpStream| -> (u8, u8, u64, Vec<u8>) {
            let mut prefix = [0u8; 4];
            stream.read_exact(&mut prefix).unwrap();
            let body_len = u32::from_be_bytes(prefix) as usize;
            let mut body = vec![0u8; body_len];
            stream.read_exact(&mut body).unwrap();
            (body[0], body[1], u64::from_be_bytes(body[2..10].try_into().unwrap()), body[10..].to_vec())
        };

        let keyframe_payload = synthetic_keyframe_au();
        tx.send(DecryptedMediaFrame { conn_id: 2, stream_id: 3, pts: 1_000, keyframe: true, stalled: false, payload: keyframe_payload.clone() })
            .unwrap();

        // Both connections must independently get CodecInfo then the
        // keyframe — a shared single-slot mailbox with per-connection
        // generation counters should serve N readers just fine; this proves
        // it actually does.
        for (label, conn) in [("A", &mut conn_a), ("B", &mut conn_b)] {
            let (frame_type, _, _, payload) = read_chunk(conn);
            assert_eq!(frame_type, FRAME_TYPE_CODEC_INFO, "connection {label} should get CodecInfo first");
            assert_eq!(String::from_utf8(payload).unwrap(), "avc1.640028", "connection {label} codec string");
            let (frame_type, flags, pts_us, payload) = read_chunk(conn);
            assert_eq!(frame_type, FRAME_TYPE_VIDEO, "connection {label} should get the keyframe next");
            assert_eq!(flags & FLAG_KEYFRAME, FLAG_KEYFRAME);
            assert_eq!(pts_us, 1_000);
            assert_eq!(payload, keyframe_payload, "connection {label} keyframe payload");
        }

        // A second, later frame must ALSO reach both connections.
        let delta_payload = synthetic_delta_au();
        tx.send(DecryptedMediaFrame { conn_id: 2, stream_id: 3, pts: 1_033, keyframe: false, stalled: false, payload: delta_payload.clone() })
            .unwrap();
        for (label, conn) in [("A", &mut conn_a), ("B", &mut conn_b)] {
            let (frame_type, flags, pts_us, payload) = read_chunk(conn);
            assert_eq!(frame_type, FRAME_TYPE_VIDEO, "connection {label} should get the delta frame");
            assert_eq!(flags & FLAG_KEYFRAME, 0);
            assert_eq!(pts_us, 1_033);
            assert_eq!(payload, delta_payload, "connection {label} delta payload");
        }

        server.stop_route(key);
    }
}
