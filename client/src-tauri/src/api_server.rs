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
//! Loopback-only (binds `127.0.0.1`) is *not* sufficient on its own: any
//! webpage open in the user's regular browser can still reach a loopback
//! port via `fetch()`, regardless of origin, so this server requires a
//! shared-secret bearer token on every request (see `API_TOKEN` below) —
//! that's what makes the wildcard `Access-Control-Allow-Origin: *` below
//! safe, not the loopback bind by itself.
//!
//! `Why an ephemeral port, matching video_server.rs, rather than the fixed
//! port this module used to bind:` running two sidecar instances
//! side by side (e.g. a "share" instance and a "watch" instance launched
//! together for manual testing) would otherwise have the second instance
//! fail to bind. Like `video_server.rs`, we bind port 0 and let the OS pick
//! a free ephemeral port, then announce it on stdout (`API_PORT=<port>`,
//! printed the same way and at the same time as `API_TOKEN=<hex>`) for the
//! parent (Electron) process to capture before making its first request —
//! there's no command response yet to carry it, so stdout is the only
//! channel available this early.
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Duration;

use rand::RngExt;
use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Manager};

use crate::commands;
use crate::state::Managed;

/// Fallback/default port, no longer used to bind (see module doc's `Why an
/// ephemeral port` note) — kept only in case some caller still wants a
/// documented "typical" value to reference.
#[allow(dead_code)]
pub const PORT: u16 = 47823;

/// Shared-secret bearer token, generated once at startup and required on
/// every `/command/*` and `/events` request (see module doc). Set exactly
/// once by [`spawn`] before the accept loop starts; every connection thread
/// reads it via [`api_token`].
static API_TOKEN: OnceLock<String> = OnceLock::new();

/// The current process's API token, generated by [`spawn`] before this
/// server starts accepting connections. Panics if called before `spawn` —
/// there is no valid state in which a connection could be handled without
/// this being set already.
fn api_token() -> &'static str {
    API_TOKEN.get().expect("api_server::spawn must set API_TOKEN before accepting connections")
}

/// 32 bytes of cryptographically random data, hex-encoded (64 hex chars).
fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Constant-time-ish comparison isn't critical here (this guards a local
/// IPC channel, not a network-facing auth boundary against a timing-attack
/// capable adversary), but a plain `==` on `String` is simplest and correct.
fn token_is_valid(provided: Option<&str>) -> bool {
    provided.is_some_and(|t| t == api_token())
}

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

/// A small ordered, bounded queue of emitted events, broadcast to every SSE
/// reader via `Condvar`, replacing `app_handle.emit()`.
///
/// `Why a bounded queue, not a single overwritable slot (as an earlier
/// version of this struct was):` a single-slot "overwrite + bump a
/// generation counter" mailbox — mirroring `video_server.rs`'s `Mailbox` —
/// was tried first, on the theory that coalescing same-kind duplicate
/// notifications is harmless (it is: nothing here promises a reliable,
/// ordered log of every state transition, just "go re-fetch the thing this
/// describes"). But `driver.rs`'s tick loop always publishes *several
/// different* events within one synchronous call — critically, its
/// `stream-stats-updated` fires **unconditionally every tick**, always last,
/// right after any diff-triggered `presence-updated`/`destination-state-changed`
/// that tick also happened to produce. With a single slot, that ordering is
/// fatal, not just lossy: `stream-stats-updated` deterministically overwrites
/// the diff event before any reader can plausibly wake, re-lock, and observe
/// it — there is no OS yield point between the `publish()` calls, so a reader
/// essentially never wins that race. Confirmed directly against this real
/// struct (`repro_tests::same_tick_publishes_starve_all_but_the_last_event`):
/// publishing `presence-updated` then `stream-stats-updated` back-to-back, a
/// fresh reader that then calls `wait_next` sees *only*
/// `stream-stats-updated` — `presence-updated` is gone, always, not
/// occasionally. That was the real cause of "ROOMS sidebar active-stream
/// count stuck at 0 until an unrelated manual refresh" and "Stream Control's
/// stopped-destination card never disappears": both events fire correctly in
/// `driver.rs`, and the frontend listener for each is correct and already
/// wired up (see `shell.js`'s `wireEvents`), but the event essentially never
/// survives the trip through this bus.
///
/// A small ring buffer with a monotonic sequence number per entry fixes this:
/// every distinct `publish()` gets its own slot, and each reader tracks its
/// own read cursor (`last_seen`, a sequence number, not a generation) so it
/// drains every event published since it last read, in order, not just
/// whatever was newest at wake time. `MAX_BUFFERED` bounds memory for a
/// reader that's gone away without cleanup (matching the old struct's
/// bounded-memory property) — only a reader that falls more than
/// `MAX_BUFFERED` events behind (a full second-plus of ticks with the default
/// `TICK_INTERVAL`) can still lose events, which is the same "a dropped
/// notification is fine, the next one or a periodic re-fetch catches up"
/// tradeoff as before, just no longer triggered by the *ordinary*, every-tick
/// case above.
pub struct EventBus {
    inner: Mutex<EventBusInner>,
    cvar: Condvar,
}

/// How many recent events `EventBus` keeps for a reader to catch up on. Only
/// needs to cover a handful of events per tick, generously multiplied — see
/// struct doc's `Why a bounded queue` note for what this actually bounds.
const MAX_BUFFERED: usize = 64;

#[derive(Default)]
struct EventBusInner {
    /// Sequence number the *next* published event will get. Doubles as "how
    /// many events have ever been published", so a fresh reader can start
    /// its cursor here to see only what's published from now on (no replay
    /// of history from before it connected — matching what `app_handle.emit()`
    /// did).
    next_seq: u64,
    /// Ring buffer of the most recent events, oldest first, each tagged with
    /// the sequence number it was published at.
    buffer: std::collections::VecDeque<(u64, String, Value)>,
}

impl EventBus {
    pub fn new() -> Self {
        Self { inner: Mutex::new(EventBusInner::default()), cvar: Condvar::new() }
    }

    /// Called by `driver.rs` in place of the `app_handle.emit(name, payload)`
    /// calls it used to make.
    pub fn publish(&self, name: &str, payload: Value) {
        let mut inner = self.inner.lock().unwrap();
        let seq = inner.next_seq;
        inner.next_seq += 1;
        inner.buffer.push_back((seq, name.to_string(), payload));
        if inner.buffer.len() > MAX_BUFFERED {
            inner.buffer.pop_front();
        }
        self.cvar.notify_all();
    }

    /// The sequence number a brand-new reader should start its `last_seen`
    /// cursor at, so it only ever sees events published from this point on.
    pub fn subscribe(&self) -> u64 {
        self.inner.lock().unwrap().next_seq
    }

    /// Block until an event at or after `*last_seen` exists, or `timeout`
    /// elapses first (`None`). Returns (and advances `*last_seen` past) the
    /// single oldest such event still in the buffer — call this in a loop to
    /// drain everything a reader missed, in publish order, rather than only
    /// ever the newest. See module doc for why a timed wait, not a plain
    /// blocking one like `video_server.rs`'s `Mailbox::wait_next`: an SSE
    /// connection has no per-client teardown signal equivalent to
    /// `VideoServer::stop_route`, so a client that vanishes with no further
    /// event ever published would otherwise block its serving thread forever.
    fn wait_next(&self, last_seen: &mut u64, timeout: Duration) -> Option<(String, Value)> {
        let mut inner = self.inner.lock().unwrap();
        loop {
            if let Some((seq, name, payload)) = inner.buffer.iter().find(|(seq, _, _)| *seq >= *last_seen) {
                *last_seen = seq + 1;
                return Some((name.clone(), payload.clone()));
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
    // Generate and announce the shared-secret token before anything else:
    // the Electron main process captures this line from this process's
    // stdout (spawning it as a sidecar child) to learn the token it must
    // send back on every request. Must happen exactly once, and strictly
    // before the accept loop below can hand off a connection.
    let token = API_TOKEN.get_or_init(generate_token);
    println!("API_TOKEN={token}");

    // Bind an ephemeral port (0 = "OS picks a free one"), same as
    // `video_server.rs`, so multiple sidecar instances can run side by side
    // without a bind conflict — see module doc's `Why an ephemeral port`
    // note. Announce the actual assigned port on stdout, same pattern and
    // timing as `API_TOKEN=` above, so the Electron parent process can
    // capture it before making its first request.
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind api_server ephemeral port");
    let port = listener.local_addr().expect("api_server listener local_addr").port();
    println!("API_PORT={port}");

    std::thread::Builder::new()
        .name("api-server-accept".into())
        .spawn(move || {
            tracing::info!("api_server: listening on http://127.0.0.1:{port}");
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let app = app.clone();
                std::thread::spawn(move || handle_connection(stream, &app));
            }
        })
        .expect("spawn api-server accept thread");
}

struct Request {
    method: String,
    path: String,
    body: Vec<u8>,
    /// The bearer token, from either an `Authorization: Bearer <token>` or
    /// `X-Api-Token: <token>` header (see module doc's auth note) — `None`
    /// if neither was present.
    token: Option<String>,
}

/// Parse the request line + headers (just enough to get method, path,
/// `Content-Length` and the auth token), then read exactly that many more
/// bytes as the body.
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
    let mut token = None;
    for line in lines {
        let Some((key, value)) = line.split_once(':') else { continue };
        let key = key.trim();
        let value = value.trim();
        if key.eq_ignore_ascii_case("content-length") {
            content_length = value.parse().unwrap_or(0);
        } else if key.eq_ignore_ascii_case("authorization") {
            token = value.strip_prefix("Bearer ").map(str::to_string);
        } else if key.eq_ignore_ascii_case("x-api-token") {
            token = Some(value.to_string());
        }
    }
    if content_length > MAX_BODY_BYTES {
        return None;
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        stream.read_exact(&mut body).ok()?;
    }
    Some(Request { method, path, body, token })
}

fn handle_connection(mut stream: TcpStream, app: &AppHandle) {
    let _ = stream.set_nodelay(true);
    let Some(req) = read_request(&mut stream) else { return };

    if req.method == "OPTIONS" {
        write_preflight(&mut stream);
        return;
    }

    // Every non-preflight request must carry the shared-secret token (see
    // module doc's auth note) - checked here, before dispatch to either the
    // SSE stream or a command, so neither ever runs for an unauthenticated
    // caller.
    if !token_is_valid(req.token.as_deref()) {
        write_error(&mut stream, 401, "missing or invalid API token");
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
    // Start the cursor at "now" — no replay of whatever happened before this
    // client connected, matching what `app_handle.emit()` did (a listener
    // registered after an emit never saw it either). See `EventBus::subscribe`.
    let mut last_seen = managed.events.subscribe();
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

/// Optional `String` field, defaulting to `""` when absent — for
/// `join_room`'s `room_name` param (an unset room name, same as an omitted
/// one, means "leave the existing/id-derived naming alone" — see
/// `join_room_impl`'s doc comment).
fn opt_string(body: &Value, key: &str) -> String {
    body.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn get_bool(body: &Value, key: &str) -> Result<bool, ApiError> {
    body.get(key)
        .and_then(Value::as_bool)
        .ok_or_else(|| ApiError::bad_request(format!("missing or non-boolean field `{key}`")))
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
        "join_room" => result(commands::join_room(managed, get_string(body, "join_string")?, get_string(body, "display_name")?, opt_string(body, "room_name"))),
        "cancel_connect" => ok(commands::cancel_connect(managed)),
        "create_room" => result(commands::create_room(managed, get_string(body, "server")?, get_string(body, "room_name")?)),
        "toggle_watch" => {
            result(commands::toggle_watch(managed, get_u32(body, "room_handle")?, get_string(body, "owner")?, get_u8(body, "stream_id")?))
        }
        "leave_needs_confirm" => ok(commands::leave_needs_confirm(managed, get_u32(body, "room_handle")?)),
        "leave_room" => ok(commands::leave_room(managed, get_u32(body, "room_handle")?)),
        "force_leave_room" => ok(commands::force_leave_room(managed, get_u32(body, "room_handle")?)),
        "rename_room" => result(commands::rename_room(managed, get_string(body, "join_string")?, get_string(body, "new_name")?)),
        "set_room_auto_connect" => {
            result(commands::set_room_auto_connect(managed, get_string(body, "join_string")?, get_bool(body, "auto_connect")?))
        }
        "forget_room" => result(commands::forget_room(managed, get_string(body, "join_string")?)),
        "select_active_room" => ok(commands::select_active_room(managed, get_u32(body, "room_handle")?)),
        "get_settings" => ok(commands::get_settings(managed)),
        "get_start_stream_defaults" => ok(commands::get_start_stream_defaults()),
        "set_start_stream_defaults" => result(commands::set_start_stream_defaults(
            get_u32(body, "framerate")?,
            get_u32(body, "bitrate_kbps")?,
            get_string(body, "tune")?,
            get_string(body, "quality_level")?,
            get_string(body, "rate_control")?,
        )),
        "set_language" => ok(commands::set_language(managed, get_string(body, "language")?)),
        "set_display_name" => ok(commands::set_display_name(managed, get_string(body, "display_name")?)),
        "set_presentation_mode" => result(commands::set_presentation_mode(managed, get_string(body, "mode")?)),
        "set_max_jitter_buffer_ms" => ok(commands::set_max_jitter_buffer_ms(managed, get_u32(body, "ms")?)),
        "set_catch_up_speed_percent" => ok(commands::set_catch_up_speed_percent(managed, get_u32(body, "percent")?)),
        "start_video_share" => ok(commands::start_video_share(managed, get_u32(body, "room_handle")?)),
        "stage_video_share" => ok(commands::stage_video_share(managed)),
        "go_live" => ok(commands::go_live(managed, get_u32(body, "capture_id")?, get_u32(body, "room_handle")?)),
        "list_staged_captures" => ok(commands::list_staged_captures(managed)),
        "start_audio_share" => {
            result(commands::start_audio_share(managed, get_u32(body, "room_handle")?, get_u32(body, "for_capture_id")?))
        }
        "set_audio_share_muted" => {
            ok(commands::set_audio_share_muted(managed, get_u32(body, "capture_id")?, get_bool(body, "muted")?))
        }
        "stop_share" => ok(commands::stop_share(managed, get_u32(body, "capture_id")?)),
        "list_audio_sources" => result(tauri::async_runtime::block_on(commands::list_audio_sources())),
        "select_audio_source" => ok(commands::select_audio_source(managed, get_u32(body, "id")?, get_string(body, "name")?)),
        "start_audio_level_tap" => {
            result(tauri::async_runtime::block_on(commands::start_audio_level_tap(managed, get_u32(body, "source_id")?)))
        }
        "poll_audio_level_tap" => ok(commands::poll_audio_level_tap(managed, get_u32(body, "tap_id")?)),
        "stop_audio_level_tap" => ok(commands::stop_audio_level_tap(managed, get_u32(body, "tap_id")?)),
        "set_bitrate" => ok(commands::set_bitrate(managed, get_u32(body, "kbps")?)),
        "set_framerate" => ok(commands::set_framerate(managed, get_u32(body, "fps")?)),
        "set_tune" => result(commands::set_tune(managed, get_string(body, "tune")?)),
        "set_quality_level" => result(commands::set_quality_level(managed, get_string(body, "quality_level")?)),
        "set_rate_control" => result(commands::set_rate_control(managed, get_string(body, "rate_control")?)),
        "start_destination" => ok(commands::start_destination(managed, get_u32(body, "capture_id")?, get_u32(body, "room_handle")?)),
        "transfer_destination" => {
            ok(commands::transfer_destination(managed, get_u32(body, "destination_id")?, get_u32(body, "target_room")?))
        }
        "edit_destination" => result(commands::edit_destination(
            managed,
            get_u32(body, "destination_id")?,
            get_u32(body, "scale_percent")?,
            get_u32(body, "framerate")?,
            get_u32(body, "bitrate_kbps")?,
            get_string(body, "tune")?,
            get_string(body, "quality_level")?,
            get_string(body, "rate_control")?,
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

#[cfg(test)]
mod repro_tests {
    //! Regression coverage for the "ROOMS sidebar active-stream count stuck
    //! at 0" / "Stream Control stopped-destination card never disappears"
    //! bugs (2026-07-28) — root-caused to `EventBus`'s old single-slot
    //! design: `driver.rs`'s tick loop always publishes several distinct
    //! events within one synchronous call (critically, `stream-stats-updated`
    //! fires unconditionally *every* tick, always last, right after any
    //! diff-triggered `presence-updated`/`destination-state-changed` that
    //! tick also produced), and a single overwritable slot meant the
    //! always-on event deterministically clobbered the diff event before any
    //! reader could plausibly observe it — see `EventBus`'s doc comment for
    //! the full `Why:`. This test pins the fixed, queue-based behavior: every
    //! distinct event published in a burst is still delivered, in order.
    use super::EventBus;
    use serde_json::json;

    #[test]
    fn same_tick_publishes_are_all_delivered_in_order() {
        let bus = EventBus::new();
        let mut last_seen = bus.subscribe();

        // Mirrors driver.rs's real per-tick publish order: a diff-triggered
        // event, then the *unconditional* stream-stats-updated that fires
        // every single tick, all inside the same synchronous function call
        // with no reader synchronization in between.
        bus.publish("presence-updated", json!({"activeStreamCount": 1}));
        bus.publish("destination-state-changed", json!({"state": "removed"}));
        bus.publish("stream-stats-updated", json!({"tick": 1}));

        let timeout = std::time::Duration::from_millis(50);
        let names: Vec<String> = std::iter::from_fn(|| bus.wait_next(&mut last_seen, timeout)).map(|(name, _)| name).collect();

        // All three survive, in publish order — not just the last one.
        assert_eq!(names, vec!["presence-updated", "destination-state-changed", "stream-stats-updated"]);
    }

    #[test]
    fn a_fresh_subscriber_never_replays_events_published_before_it_connected() {
        let bus = EventBus::new();
        bus.publish("presence-updated", json!({"before": true}));

        let mut last_seen = bus.subscribe();
        bus.publish("presence-updated", json!({"before": false}));

        let (_, payload) = bus.wait_next(&mut last_seen, std::time::Duration::from_millis(50)).expect("event published after subscribing");
        assert_eq!(payload, json!({"before": false}));
    }
}
