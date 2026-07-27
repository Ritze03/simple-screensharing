//! Real video delivery for the View tab: local MJPEG-over-HTTP.
//!
//! `frontend/scripts/view.js` renders each watched tile as a plain
//! `<img src="http://127.0.0.1:PORT/stream/{token}">`. That `<img>` gets
//! genuinely live video for free — no per-frame JS, hardware-accelerated
//! JPEG decode in the webview's own image pipeline, and every tile/popout is
//! just another `<img>` pointed at its own path — by speaking the classic
//! "IP camera" protocol: an HTTP response whose body never ends, with
//! `Content-Type: multipart/x-mixed-replace; boundary=frame`, one JPEG part
//! written per frame.
//!
//! # Why this module exists as new work, not a reuse of an existing design
//!
//! Earlier task briefs for this exact gap asserted a "rigorously validated"
//! MJPEG-over-HTTP prototype already existed elsewhere in this project and
//! only needed wiring in. `superdoc/ui/FRONTEND-SHELL.md`'s dated entries
//! record two separate investigations that searched this repo's full history,
//! dependencies, and the one doc claimed to contain it, and found no such
//! prototype anywhere — the claim had no basis and had simply been repeated
//! forward by workers who trusted the previous one instead of re-checking.
//! This module is a first implementation, designed and built directly against
//! the real, confirmed gap (the View tab rendered a static CSS placeholder —
//! see `FRONTEND-SHELL.md`'s 2026-07-27 pixel-diff entry), not a port of
//! anything that came before it. The mechanism (MJPEG-over-HTTP) and its two
//! load-bearing details below are still the right design on their own
//! technical merits — they just needed to actually be built, once, for real.
//!
//! # The two details that make this actually work, not just mostly work
//!
//! 1. **Single-slot "latest frame" mailbox, never a queue** ([`Mailbox`]).
//!    The producer thread that polls [`StreamDecoder::latest_frame`] and
//!    JPEG-encodes always *overwrites* the one slot and bumps a generation
//!    counter; it never pushes onto anything unbounded. A connection thread
//!    blocks on the mailbox's `Condvar` until the generation changes, then
//!    reads the current frame — so a slow or backgrounded reader can never
//!    accumulate a backlog of stale frames to "catch up" through; it always
//!    gets whatever is newest at the moment it looks.
//! 2. **Clamp the serving socket's send buffer** ([`clamp_send_buffer`]).
//!    A single-slot mailbox on its own is not sufficient: the OS kernel's own
//!    auto-tuned loopback TCP send buffer will happily buffer several MB —
//!    many frames' worth — between this process's `write()` calls and the
//!    webview actually reading them. A backgrounded/unfocused viewer webview
//!    that stops pumping its network stack for a few seconds would then
//!    resume by draining that kernel backlog frame by frame, i.e. exactly the
//!    "fast-forward catch-up" bug this project has already hit and fixed
//!    elsewhere (frame pacing, event ingestion — see `architecture/
//!    overview.md`). Clamping the send buffer to about one frame's worth
//!    forces `write()` itself to block/backpressure once the kernel already
//!    holds one unread frame, which is what actually keeps "resume" meaning
//!    "resume at the current frame" instead of "replay everything since
//!    focus was lost". `Why a raw `std::net::TcpListener` and not a
//!    higher-level HTTP framework:` frameworks like `tiny_http` own the
//!    accepted socket internally and don't expose it for `setsockopt`-level
//!    tuning, which is exactly the mechanism by which this bug would
//!    reappear if one were used here.
//!
//! # Current status (2026-07-27): built and verified up to a separate,
//! pre-existing bug this module does not own
//!
//! Every layer in *this* module was verified directly and independently:
//! `toggle_watch` returns a real URL, the listener accepts connections and
//! serves the correct `multipart/x-mixed-replace; boundary=frame` headers for
//! a matching token (confirmed with `curl`), and the mailbox/producer wiring
//! runs correctly. What could **not** yet be verified end-to-end (a real
//! screenshot of genuinely live, changing video) is blocked by a separate,
//! pre-existing bug found while integrating this module: no watcher who
//! subscribes after a share has already been running for a while ever
//! receives a real keyframe over the wire — confirmed via direct diagnostic
//! logging across multiple full two-real-instance runs (thousands of
//! consecutive frames, zero keyframes), reproduced identically with both the
//! VAAPI and software (`x264enc`) encoder backends. That bug lives in
//! `capture.rs`'s/`relay.rs`'s keyframe-request plumbing, not here — see
//! `superdoc/ui/FRONTEND-SHELL.md`'s matching 2026-07-27 entry for the full
//! diagnostic trail. Once it's fixed, this module should work without any
//! change of its own.
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use simple_screensharing_client::decode::StreamDecoder;

/// Ceiling on how often the producer thread polls `latest_frame()` /
/// re-encodes. `StreamDecoder::latest_frame` already only returns `Some` at
/// the stream's real decode rate (see `decode.rs`), so polling faster than
/// this just spends CPU finding `None` more often — 60fps is a simple,
/// safely-above-any-realistic-stream ceiling rather than something tuned per
/// stream.
const POLL_INTERVAL: Duration = Duration::from_millis(1000 / 60);

const JPEG_QUALITY: i32 = 80;
const BOUNDARY: &str = "frame";

/// About one 1080p JPEG frame at `JPEG_QUALITY`/4:2:0 — see this module's
/// doc comment for why this needs to be small, not merely "not unbounded".
const CLAMPED_SEND_BUFFER_BYTES: usize = 192 * 1024;

/// One watched stream, identified by the same triple the frontend already
/// keys tiles by.
type StreamKey = (u32, u64, u8);

fn token(key: StreamKey) -> String {
    let (room_handle, owner, stream_id) = key;
    format!("{room_handle:08x}-{owner:016x}-{stream_id:02x}")
}

/// The single-slot "latest frame" mailbox — see module doc, detail 1.
/// `generation` only ever increases; a reader that has seen generation `g`
/// blocks until the stored generation differs from `g`, then takes whatever
/// is current (never queued, never replayed).
struct Mailbox {
    inner: Mutex<MailboxInner>,
    cvar: Condvar,
}

struct MailboxInner {
    generation: u64,
    frame: Option<Arc<[u8]>>,
    closed: bool,
}

impl Mailbox {
    fn new() -> Self {
        Self { inner: Mutex::new(MailboxInner { generation: 0, frame: None, closed: false }), cvar: Condvar::new() }
    }

    /// Overwrite the slot with a new frame. Called only by this stream's
    /// producer thread.
    fn publish(&self, frame: Vec<u8>) {
        let mut inner = self.inner.lock().unwrap();
        inner.frame = Some(Arc::from(frame));
        inner.generation = inner.generation.wrapping_add(1);
        self.cvar.notify_all();
    }

    /// Wake every blocked reader with nothing to give them (route torn down).
    fn close(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.closed = true;
        self.cvar.notify_all();
    }

    /// Block until a frame newer than `*last_seen` exists (or the mailbox
    /// closes), then return it and advance `*last_seen`. Never returns a
    /// frame the caller has already seen, and never buffers more than one.
    fn wait_next(&self, last_seen: &mut u64) -> Option<Arc<[u8]>> {
        let mut inner = self.inner.lock().unwrap();
        loop {
            if inner.generation != *last_seen && inner.frame.is_some() {
                *last_seen = inner.generation;
                return inner.frame.clone();
            }
            if inner.closed {
                return None;
            }
            inner = self.cvar.wait(inner).unwrap();
        }
    }
}

struct Route {
    mailbox: Arc<Mailbox>,
    stop: Arc<AtomicBool>,
    producer: JoinHandle<()>,
}

/// Owns the one local HTTP listener this process serves every watched
/// stream's MJPEG through (routed by path, one path per stream — simpler
/// than one port per stream, and just as capable of serving several
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

    /// Start (or restart) serving `key`'s decoder. Returns the URL
    /// `view.js` should point an `<img>` at. If a route already exists for
    /// this key (shouldn't normally happen — `toggle_watch` only calls this
    /// on the transition into watching — but a stale route would otherwise
    /// leak its producer thread), it's torn down first.
    pub fn start_route(&self, key: StreamKey, decoder: Arc<Mutex<StreamDecoder>>) -> String {
        self.stop_route(key);

        let mailbox = Arc::new(Mailbox::new());
        let stop = Arc::new(AtomicBool::new(false));

        let producer_mailbox = mailbox.clone();
        let producer_stop = stop.clone();
        let producer = std::thread::Builder::new()
            .name(format!("video-server-encode-{}", token(key)))
            .spawn(move || produce(decoder, producer_mailbox, producer_stop))
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

/// Producer: poll this stream's decoder at `POLL_INTERVAL`, JPEG-encode
/// whatever new frame shows up, publish it to the mailbox. Exits as soon as
/// `stop` is observed, which bounds shutdown latency to one `POLL_INTERVAL`.
fn produce(decoder: Arc<Mutex<StreamDecoder>>, mailbox: Arc<Mailbox>, stop: Arc<AtomicBool>) {
    // One `Compressor` per producer thread, reused across frames — avoids
    // re-initializing a `tjhandle` on every single frame. `Compressor` is
    // `Send` but not `Sync`; owning it exclusively in this thread is exactly
    // its intended usage.
    let mut compressor = match turbojpeg::Compressor::new() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("video_server: turbojpeg::Compressor::new failed: {e:#}");
            return;
        }
    };
    let _ = compressor.set_quality(JPEG_QUALITY);
    let _ = compressor.set_subsamp(turbojpeg::Subsamp::Sub2x2);

    tracing::debug!("video_server: producer thread started");
    let mut published_any = false;
    let mut polls = 0u64;
    while !stop.load(Ordering::Relaxed) {
        let image = decoder.lock().unwrap().latest_frame();
        polls += 1;
        if let Some(image) = image {
            match encode(&mut compressor, &image) {
                Ok(jpeg) => {
                    if !published_any {
                        tracing::debug!("video_server: first frame published, {} bytes, size {:?}", jpeg.len(), image.size);
                        published_any = true;
                    }
                    mailbox.publish(jpeg);
                }
                Err(e) => tracing::warn!("video_server: jpeg encode failed: {e:#}"),
            }
        } else if !published_any && polls % 300 == 0 {
            // Worth keeping at `warn`, not just `debug`: a watch that never
            // sees its first frame within several seconds is exactly the
            // symptom this feature's own integration testing hit (see
            // `dto::hex_u64`'s doc comment) — a silent `owner` mismatch
            // between this route and the `(ConnId, StreamId)` key
            // `spawn_media_ingestion_task` actually feeds. Once at least one
            // frame has arrived, occasional future gaps are normal (a
            // legitimately stalled sender) and not worth this log's noise.
            tracing::warn!("video_server: still no frame after {polls} polls ({:.1}s)", polls as f64 * POLL_INTERVAL.as_secs_f64());
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    tracing::debug!("video_server: producer thread stopping");
}

fn encode(compressor: &mut turbojpeg::Compressor, image: &egui::ColorImage) -> turbojpeg::Result<Vec<u8>> {
    let [width, height] = image.size;
    // `egui::Color32` is `#[repr(C, align(4))] struct Color32([u8; 4])` in
    // RGBA byte order (see `ecolor::color32`) — exactly `PixelFormat::RGBA`'s
    // memory layout, so the pixel buffer can be reinterpreted as bytes
    // without a per-pixel copy/conversion pass.
    let pixels: &[u8] = unsafe { std::slice::from_raw_parts(image.pixels.as_ptr().cast::<u8>(), image.pixels.len() * 4) };
    let tj_image = turbojpeg::Image { pixels, width, pitch: width * 4, height, format: turbojpeg::PixelFormat::RGBA };
    compressor.compress_to_vec(tj_image)
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

    let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary={BOUNDARY}\r\nCache-Control: no-cache, no-store\r\nConnection: close\r\n\r\n"
    );
    if stream.write_all(headers.as_bytes()).is_err() {
        return;
    }

    // Deliberately starts at generation 0 with nothing yet seen: the very
    // first `wait_next` call blocks until the producer has published at
    // least one frame, so a viewer that opens a tile before the first frame
    // is ready simply waits for it instead of erroring.
    let mut last_seen = 0u64;
    loop {
        let Some(frame) = mailbox.wait_next(&mut last_seen) else { break };
        let part_header = format!("--{BOUNDARY}\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n", frame.len());
        if stream.write_all(part_header.as_bytes()).is_err() {
            break;
        }
        if stream.write_all(&frame).is_err() {
            break;
        }
        if stream.write_all(b"\r\n").is_err() {
            break;
        }
    }
}
