//! Local REST + SSE transport for the Tauri->Electron shell migration.
//!
//! # Why this exists
//!
//! We're swapping the IPC *transport* the frontend uses (Tauri's
//! `invoke`/`emit` -> plain HTTP/SSE an Electron renderer can `fetch()`),
//! while keeping every `#[tauri::command]` handler in `commands.rs` and every
//! event source in `driver.rs` unchanged in behavior. This module is the new
//! transport, nothing else: every request it serves is dispatched straight
//! into the *existing* command function (the real `#[tauri::command] pub fn`,
//! not a duplicate) via `tauri::AppHandle::state::<Managed>()` — the same
//! `State<'_, Managed>` extraction Tauri's own IPC dispatch performs, just
//! obtained a different way. No command logic is reimplemented here.
//!
//! `Why a raw `std::net::TcpListener`, matching `video_server.rs`, and not an
//! HTTP framework:` see that module's doc comment — same reasoning applies
//! (this workspace has already rejected framework deps for this exact
//! reason), and reusing one raw-socket pattern for both local servers keeps
//! the dependency footprint and the mental model identical across the two.
//!
//! # Endpoint contract
//!
//! - `POST /command/<name>` — one path per Tauri command. Request body is a
//!   JSON object of the command's parameters, keyed by their Rust parameter
//!   name exactly as declared in `commands.rs` (snake_case — e.g.
//!   `{"room_handle": 3, "owner": "00..1", "stream_id": 0}`). Commands with no
//!   parameters accept an empty body (`{}` or a zero-length body, treated the
//!   same). Response:
//!   - `200 OK`, `Content-Type: application/json`, body = the command's
//!     return value JSON-encoded directly (bare — a bool, array, object, or
//!     `null`, not wrapped in an envelope). This is exactly what a
//!     `tauri::invoke()` promise would have resolved to.
//!   - `400 Bad Request`, body `{"error": "..."}` — either the command
//!     returned `Result::Err` (an application-level error, e.g. a bad join
//!     string), or the request itself was malformed (missing/mistyped field,
//!     invalid JSON). This is exactly what a `tauri::invoke()` promise would
//!     have rejected with.
//!   - `404 Not Found`, body `{"error": "..."}` — unknown command name.
//!   - `405 Method Not Allowed` — anything but `POST`.
//! - `GET /events` — one long-lived `text/event-stream` (SSE) connection,
//!   replacing `app_handle.emit()`. Every one of the 6 events `driver.rs`
//!   emits (`room-connected`, `room-connect-failed`, `room-disconnected`,
//!   `bandwidth-overshoot`, `presence-updated`, `destination-state-changed`)
//!   is broadcast over this *one* stream as a standard SSE frame:
//!   ```text
//!   event: <event-name>
//!   data: <json payload, same shape driver.rs used to pass to app_handle.emit>
//!
//!   ```
//!   (blank line terminates the frame, per the SSE spec). A client filters by
//!   the `event:` field, same as `EventSource.addEventListener(name, ...)`.
//!   Every ~15s with no real event, a `: keep-alive\n\n` comment line is sent
//!   (standard SSE ping — a compliant client/`EventSource` ignores lines
//!   starting with `:`); this both lets a client detect a dead connection
//!   promptly and lets *this* server detect a client that vanished without
//!   another event ever occurring to notice on (see [`EventBus`]'s doc
//!   comment for why that matters).
//!
//! Loopback-only (binds `127.0.0.1`), no auth — identical trust posture to
//! `video_server.rs`.
//!
//! `Why a fixed port rather than an ephemeral one (video_server.rs's
//! choice):` `video_server.rs` can afford port-per-process discovery through
//! its own command responses (`toggle_watch`/`stream_video_url` hand back a
//! full URL). This server *is* the channel every command response travels
//! over, so the port has to be knowable *before* the first command call —
//! there's no command response yet to carry it. A fixed, documented port
//! is the simplest thing that lets an Electron main process hard-code the
//! base URL. If this ever needs to run multiple instances side by side, that
//! tradeoff should be revisited (e.g. print the bound port to stdout for a
//! parent process to capture) — not needed for this step.
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Manager};

use crate::commands;
use crate::state::Managed;

/// Fixed loopback port this server binds — see module doc's `Why a fixed
/// port` note.
pub const PORT: u16 = 47823;

/// How long an idle SSE connection waits for a real event before sending a
/// `: keep-alive` comment line — see module doc.
const SSE_KEEPALIVE: Duration = Duration::from_secs(15);

/// Bound on a request's headers (before the blank line) and its body,
/// respectively — this is a trusted-loopback single-purpose server, not a
/// general HTTP implementation, but a client that sends garbage still
/// shouldn't be able to make a connection thread allocate/read forever.
const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

// ---------------------------------------------------------------------
// The event bus SSE reads from and driver.rs publishes to.
// ---------------------------------------------------------------------

/// Generalizes `video_server.rs`'s single-slot `Mailbox` from "latest video
/// frame" to "latest emitted event": the same "overwrite the one slot, bump a
/// generation counter, block a reader on `Condvar` until it changes" shape,
/// just carrying a `(event name, JSON payload)` pair instead of a JPEG.
///
/// `Why this can coalesce two distinct events into one delivery (and that's
/// an accepted tradeoff, not a bug):` `driver.rs`'s tick loop can publish
/// several different events (e.g. `presence-updated` then
/// `destination-state-changed`) within the same lock scope, faster than any
/// reader could possibly observe the first one — same as a producer thread in
/// `video_server.rs` can encode a new frame before a slow reader drained the
/// last one. A reader that was blocked mid-`wait_next` when both publishes
/// happened only ever sees the *last* one. This is fine here for the same
/// reason it's fine for video frames: every one of these 6 events exists to
/// tell the frontend "go re-fetch the thing this describes" (see
/// `driver.rs`'s `destination-state-changed`/`"removed"` `Why:` note — the
/// payload is frequently not even meaningful beyond "something changed"), not
/// to deliver a reliable, ordered log of every state transition. A dropped
/// *notification* just means the next one (or the client's own periodic
/// re-fetch) catches the frontend up; nothing here promises delivery of every
/// individual event.
pub struct EventBus {
    inner: Mutex<EventBusInner>,
    cvar: Condvar,
}

#[derive(Default)]
struct EventBusInner {
    generation: u64,
    event: Option<(String, Value)>,
}

impl EventBus {
    pub fn new() -> Self {
        Self { inner: Mutex::new(EventBusInner::default()), cvar: Condvar::new() }
    }

    /// Called by `driver.rs` in place of the `app_handle.emit(name, payload)`
    /// calls it used to make.
    pub fn publish(&self, name: &str, payload: Value) {
        let mut inner = self.inner.lock().unwrap();
        inner.event = Some((name.to_string(), payload));
        inner.generation = inner.generation.wrapping_add(1);
        self.cvar.notify_all();
    }

    /// Block until an event newer than `*last_seen` exists, or `timeout`
    /// elapses first (`None`). See module doc for why a timed wait, not a
    /// plain blocking one like `video_server.rs`'s `Mailbox::wait_next`: an
    /// SSE connection has no per-client teardown signal equivalent to
    /// `VideoServer::stop_route`, so a client that vanishes with no further
    /// event ever published would otherwise block its serving thread forever.
    fn wait_next(&self, last_seen: &mut u64, timeout: Duration) -> Option<(String, Value)> {
        let mut inner = self.inner.lock().unwrap();
        loop {
            if inner.generation != *last_seen
                && let Some(event) = inner.event.clone()
            {
                *last_seen = inner.generation;
                return Some(event);
            }
            let (guard, result) = self.cvar.wait_timeout(inner, timeout).unwrap();
            inner = guard;
            if result.timed_out() {
                return None;
            }
        }
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------
// Server lifecycle
// ---------------------------------------------------------------------

/// Start the accept loop on a background thread. `app` is used to reach
/// `Managed` (via `AppHandle::state::<Managed>()`, the same extraction
/// mechanism Tauri's own IPC dispatch uses) from whatever thread ends up
/// handling a given connection.
pub fn spawn(app: AppHandle) {
    std::thread::Builder::new()
        .name("api-server-accept".into())
        .spawn(move || match TcpListener::bind(("127.0.0.1", PORT)) {
            Ok(listener) => {
                tracing::info!("api_server: listening on http://127.0.0.1:{PORT}");
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { continue };
                    let app = app.clone();
                    std::thread::spawn(move || handle_connection(stream, &app));
                }
            }
            Err(e) => tracing::error!("api_server: failed to bind 127.0.0.1:{PORT}: {e:#}"),
        })
        .expect("spawn api-server accept thread");
}

struct Request {
    method: String,
    path: String,
    body: Vec<u8>,
}

/// Parse the request line + headers (just enough to get method, path and
/// `Content-Length`), then read exactly that many more bytes as the body.
fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while head.len() < MAX_HEADER_BYTES {
        if stream.read(&mut byte).ok()? == 0 {
            return None;
        }
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let text = String::from_utf8_lossy(&head);
    let mut lines = text.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();

    let mut content_length = 0usize;
    for line in lines {
        if let Some((key, value)) = line.split_once(':')
            && key.trim().eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }
    if content_length > MAX_BODY_BYTES {
        return None;
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        stream.read_exact(&mut body).ok()?;
    }
    Some(Request { method, path, body })
}

fn handle_connection(mut stream: TcpStream, app: &AppHandle) {
    let _ = stream.set_nodelay(true);
    let Some(req) = read_request(&mut stream) else { return };

    if req.method == "OPTIONS" {
        write_preflight(&mut stream);
        return;
    }

    if req.path == "/events" {
        handle_sse(stream, app);
        return;
    }

    if let Some(name) = req.path.strip_prefix("/command/") {
        if req.method != "POST" {
            write_error(&mut stream, 405, "method not allowed, use POST");
            return;
        }
        let body: Value = if req.body.is_empty() {
            Value::Object(serde_json::Map::new())
        } else {
            match serde_json::from_slice(&req.body) {
                Ok(v) => v,
                Err(e) => {
                    write_error(&mut stream, 400, &format!("invalid JSON body: {e}"));
                    return;
                }
            }
        };
        match dispatch(name, &body, app) {
            Ok(value) => write_json(&mut stream, 200, &value),
            Err(err) => write_error(&mut stream, err.status, &err.message),
        }
        return;
    }

    write_error(&mut stream, 404, "not found");
}

fn handle_sse(mut stream: TcpStream, app: &AppHandle) {
    let headers = "HTTP/1.1 200 OK\r\n\
         Content-Type: text/event-stream\r\n\
         Cache-Control: no-cache, no-store\r\n\
         Connection: keep-alive\r\n\
         X-Accel-Buffering: no\r\n\
         Access-Control-Allow-Origin: *\r\n\
         \r\n";
    if stream.write_all(headers.as_bytes()).is_err() {
        return;
    }

    let managed = app.state::<Managed>();
    // Same "start at generation 0, block for the *next* event" behavior as
    // `video_server.rs`'s `Mailbox` — no replay of whatever happened before
    // this client connected, matching what `app_handle.emit()` did (a
    // listener registered after an emit never saw it either).
    let mut last_seen = 0u64;
    loop {
        match managed.events.wait_next(&mut last_seen, SSE_KEEPALIVE) {
            Some((name, payload)) => {
                let data = serde_json::to_string(&payload).unwrap_or_else(|_| "null".to_string());
                let frame = format!("event: {name}\ndata: {data}\n\n");
                if stream.write_all(frame.as_bytes()).is_err() {
                    break;
                }
            }
            None => {
                if stream.write_all(b": keep-alive\n\n").is_err() {
                    break;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------
// Command dispatch
// ---------------------------------------------------------------------

struct ApiError {
    status: u16,
    message: String,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self { status: 400, message: message.into() }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self { status: 404, message: message.into() }
    }
}

fn get_string(body: &Value, key: &str) -> Result<String, ApiError> {
    body.get(key).and_then(Value::as_str).map(str::to_string).ok_or_else(|| ApiError::bad_request(format!("missing or non-string field `{key}`")))
}

fn get_u32(body: &Value, key: &str) -> Result<u32, ApiError> {
    body.get(key)
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| ApiError::bad_request(format!("missing or invalid u32 field `{key}`")))
}

fn get_u8(body: &Value, key: &str) -> Result<u8, ApiError> {
    body.get(key)
        .and_then(Value::as_u64)
        .and_then(|v| u8::try_from(v).ok())
        .ok_or_else(|| ApiError::bad_request(format!("missing or invalid u8 field `{key}`")))
}

/// Wrap a plain (non-`Result`) command return value as a dispatch success.
fn ok<T: Serialize>(value: T) -> Result<Value, ApiError> {
    serde_json::to_value(value).map_err(|e| ApiError { status: 500, message: format!("failed to serialize response: {e}") })
}

/// Wrap a `Result<T, String>` command return value: `Ok` -> success (same as
/// `ok`), `Err` -> the same "application-level failure" the frontend would
/// have seen as a rejected `invoke()` promise.
fn result<T: Serialize>(r: Result<T, String>) -> Result<Value, ApiError> {
    match r {
        Ok(v) => ok(v),
        Err(e) => Err(ApiError::bad_request(e)),
    }
}

/// Route `name` to the matching `#[tauri::command]` function in
/// `commands.rs` and call it directly — no logic is duplicated here, this is
/// purely argument marshalling. See module doc for the request/response
/// shape every arm follows.
fn dispatch(name: &str, body: &Value, app: &AppHandle) -> Result<Value, ApiError> {
    let managed = app.state::<Managed>();
    match name {
        "list_saved_rooms" => ok(commands::list_saved_rooms(managed)),
        "list_active_streams" => ok(commands::list_active_streams(managed)),
        "room_detail" => ok(commands::room_detail(managed, get_u32(body, "room_handle")?)),
        "join_room" => result(commands::join_room(managed, get_string(body, "join_string")?, get_string(body, "display_name")?)),
        "toggle_watch" => {
            result(commands::toggle_watch(managed, get_u32(body, "room_handle")?, get_string(body, "owner")?, get_u8(body, "stream_id")?))
        }
        "leave_needs_confirm" => ok(commands::leave_needs_confirm(managed, get_u32(body, "room_handle")?)),
        "leave_room" => ok(commands::leave_room(managed, get_u32(body, "room_handle")?)),
        "force_leave_room" => ok(commands::force_leave_room(managed, get_u32(body, "room_handle")?)),
        "rename_room" => result(commands::rename_room(managed, get_string(body, "join_string")?, get_string(body, "new_name")?)),
        "select_active_room" => ok(commands::select_active_room(managed, get_u32(body, "room_handle")?)),
        "get_settings" => ok(commands::get_settings(managed)),
        "set_language" => ok(commands::set_language(managed, get_string(body, "language")?)),
        "set_presentation_mode" => result(commands::set_presentation_mode(managed, get_string(body, "mode")?)),
        "start_video_share" => ok(commands::start_video_share(managed, get_u32(body, "room_handle")?)),
        "start_audio_share" => ok(commands::start_audio_share(managed, get_u32(body, "room_handle")?)),
        "stop_share" => ok(commands::stop_share(managed, get_u32(body, "capture_id")?)),
        "list_audio_sources" => result(tauri::async_runtime::block_on(commands::list_audio_sources())),
        "select_audio_source" => ok(commands::select_audio_source(managed, get_u32(body, "id")?, get_string(body, "name")?)),
        "set_bitrate" => ok(commands::set_bitrate(managed, get_u32(body, "kbps")?)),
        "set_framerate" => ok(commands::set_framerate(managed, get_u32(body, "fps")?)),
        "start_destination" => ok(commands::start_destination(managed, get_u32(body, "capture_id")?, get_u32(body, "room_handle")?)),
        "transfer_destination" => {
            ok(commands::transfer_destination(managed, get_u32(body, "destination_id")?, get_u32(body, "target_room")?))
        }
        "edit_destination" => ok(commands::edit_destination(
            managed,
            get_u32(body, "destination_id")?,
            get_u32(body, "scale_percent")?,
            get_u32(body, "framerate")?,
            get_u32(body, "bitrate_kbps")?,
        )),
        "stop_destination" => ok(commands::stop_destination(managed, get_u32(body, "destination_id")?)),
        "set_bandwidth_mode" => result(commands::set_bandwidth_mode(managed, get_string(body, "mode")?)),
        "set_bandwidth_total" => ok(commands::set_bandwidth_total(managed, get_u32(body, "total_kbps")?)),
        "set_bandwidth_floor" => ok(commands::set_bandwidth_floor(managed, get_u32(body, "floor_kbps")?)),
        "stream_video_url" => {
            result(commands::stream_video_url(managed, get_u32(body, "room_handle")?, get_string(body, "owner")?, get_u8(body, "stream_id")?))
        }
        "toggle_listen" => {
            result(commands::toggle_listen(managed, get_u32(body, "room_handle")?, get_string(body, "owner")?, get_u8(body, "stream_id")?))
        }
        "set_volume" => result(commands::set_volume(
            managed,
            get_u32(body, "room_handle")?,
            get_string(body, "owner")?,
            get_u8(body, "stream_id")?,
            get_u32(body, "volume_percent")?,
        )),
        "pop_out_stream" => {
            result(commands::pop_out_stream(app.clone(), get_u32(body, "room_handle")?, get_string(body, "owner")?, get_u8(body, "stream_id")?))
        }
        other => Err(ApiError::not_found(format!("unknown command `{other}`"))),
    }
}

// ---------------------------------------------------------------------
// Response writing
// ---------------------------------------------------------------------

fn write_json(stream: &mut TcpStream, status: u16, value: &Value) {
    let body = serde_json::to_vec(value).unwrap_or_else(|_| b"null".to_vec());
    write_response(stream, status, "application/json", &body);
}

fn write_error(stream: &mut TcpStream, status: u16, message: &str) {
    let body = serde_json::to_vec(&serde_json::json!({ "error": message })).unwrap_or_else(|_| b"{}".to_vec());
    write_response(stream, status, "application/json", &body);
}

fn write_response(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        _ => "Error",
    };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n",
        body.len()
    );
    if stream.write_all(header.as_bytes()).is_err() {
        return;
    }
    let _ = stream.write_all(body);
}

/// Respond to a CORS preflight `OPTIONS` request (any path) with `204 No
/// Content` and the headers needed to authorize the real `POST`/`GET` that
/// follows — see module doc's CORS note.
fn write_preflight(stream: &mut TcpStream) {
    let header = "HTTP/1.1 204 No Content\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Access-Control-Allow-Methods: POST, GET, OPTIONS\r\n\
         Access-Control-Allow-Headers: Content-Type\r\n\
         Connection: close\r\n\
         \r\n";
    let _ = stream.write_all(header.as_bytes());
}
