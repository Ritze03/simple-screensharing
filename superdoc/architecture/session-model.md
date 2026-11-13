# Client Session Model and Destination Lifecycle

Split out of `overview.md` on 2026-11-07 (the hub keeps the module map and a summary). Two topics: the
client's per-launch state (`session.rs`'s `AppState`: rooms, captures, destinations) and the lifecycle of
one outgoing destination (state machine, wire PTS, stall protocol, in `capture.rs`). Auto-reconnect and
resume behaviour built on this model: `client-reconnect-and-watches.md`. Code comments in `capture.rs` cite
the "Per-destination lifecycle" heading below.

## Client session model (rooms / captures / destinations)

The desktop client's *ephemeral* per-launch state lives on `session.rs`'s `AppState` (moved
out of the old single-file `ui.rs`'s `App` in the Tauri-port split — see the `session.rs` and `ui/` table rows above)
in three collections. Nothing here is persisted — only `rooms.rs`'s `RoomStore` (saved lounges)
and `AppSettings` (language, presentation mode, bandwidth preferences, max jitter buffer) survive
a restart, by design.

| Thing | Owner | Identified by | Holds |
| --- | --- | --- | --- |
| `RoomSession` | `AppState::rooms: IndexMap<RoomHandle, _>` | `RoomHandle(u32)` | One joined room = one QUIC connection: `ScreenshareClient`, its (forwarded, non-media) `ClientEvent` receiver, roster, announced streams, `watches`/`audio_watches`, preview cache + textures, its own `next_stream_id`, and `Vec<DestinationId>` back-references to what it's sending. |
| `LocalCapture` | `AppState::captures: IndexMap<CaptureSourceId, _>` | `CaptureSourceId(u32)` | One local GStreamer pipeline (a screen grab or an audio source): its `CaptureHandle`/`AudioCaptureHandle` state machine, its self-preview texture, and its `destinations`. |
| `Destination` | `LocalCapture::destinations: IndexMap<DestinationId, _>` | `DestinationId(u32)` | One room this capture is sent into: the `stream_id` announced on *that* room's connection, this destination's own `VideoParams` (resolution/fps/bitrate/backend), a shared handle onto its `capture::destination::DestinationRuntime` (see below), a `pending: Option<PendingRole>` marking it as a fan-out/transfer target or transfer source still settling, and an `edit: DestEdit` (the row's in-progress scale/framerate/bitrate buffer, applied through `classify_change` on "Apply"). |

Stage 2 added two more pieces of session state, both cross-room by design (see
"Multi-destination captures are live" below):

- **`AppState::bandwidth: BandwidthController`** — Absolute-Maximum mode's live wiring:
  remembers what each destination's encoder was last told (`applied: HashMap<DestinationId, u32>`),
  rate-limits recomputes to one per second (`ALLOC_RECOMPUTE_INTERVAL`), and suppresses
  writes that don't clear a 5%-or-100kbit/s hysteresis threshold (`needs_reapply`). Wraps
  the pure `bandwidth::allocate()` — see that module's row above — with the timing/memory
  behaviour that makes applying its output to live encoders not cause visible bitrate
  oscillation. Never persisted; it describes only this session's live encoders.
- **`AppState::pending_destinations: Vec<PendingDestination>`** — fan-out/transfer
  destinations whose encode branch is still being built (`capture::CaptureHandle::
  spawn_add_destination`), polled once per frame by `poll_pending_destinations`. A
  transfer is two of these events glued together: the *old* destination is marked
  `pending: Some(PendingRole::TransferSource(target_room))`, a *new* one is added in the
  target room marked `PendingRole::TransferTarget(source_room)`, and only once the new one
  confirms live does `poll_pending_destinations` tear the old one down — make-before-break,
  never a bare re-point (a destination's encryption keys/nonce space are room-bound, so
  "the same stream in another room" isn't expressible any other way).

`Why:` (`RoomSession::watches`/`audio_watches` are `Arc<Mutex<HashMap<...>>>`, not plain
`HashMap`s, and `event_rx` is a *forwarded* channel rather than the raw one
`ScreenshareClient::connect` returns) `session.rs`'s `insert_room` spawns one always-on
`spawn_media_ingestion_task` per room that owns the raw connection channel directly and
intercepts every `ClientEvent::MediaFrame` there, feeding `watches`/`audio_watches` — hence
the shared, lockable maps — in real time, completely independent of `App::update()`'s
repaint-driven cadence. Everything else (`ClientEvent::PresenceDelta`, `SubscriberCount`,
`KeyframeRequest`, `ControlClosed`) is forwarded on to a fresh channel, which is what
`RoomSession::event_rx` actually is; `drain_events` keeps handling those exactly as before, at
its normal throttled pace. This closes the "fast-forward catch-up" bug: before this task
existed, `MediaFrame` sat in the same channel as everything else until `drain_events` next ran,
which only happens once every `UNFOCUSED_REPAINT_INTERVAL` (500ms) while the window is
unfocused — so a backlog piled up and then drained in one burst, and `PacingState::decide`
paced each backlogged frame out with a real-time sleep (a literal real-time replay of history).
See `session.rs`'s `spawn_media_ingestion_task` doc comment for the full root-cause writeup and the
option-1-vs-option-2 design choice, and `decode.rs`'s `PacingState::epoch` doc comment for the
complementary, already-separately-merged wall-clock staleness backstop inside the pacing math
itself (a defensive mitigation for a burst that still forms, not a fix for backlog formation).

`Why:` (`spawn_media_ingestion_task` races `raw_events.recv()` against `forward_tx.closed()`,
rather than only detecting a left room via a failed `forward_tx.send()`) the task now owns
`raw_events` — the receiver `RoomSession` used to hold directly — so dropping a `RoomSession`
(`disconnect_room`) has no *direct* effect on it any more. Relying solely on a future failed
send to notice would leave the task, every decoder/GStreamer pipeline it feeds, and the
`net.rs` background readers (which only exit once *their* send into `raw_events` fails) running
for as long as the remote side keeps sending `MediaFrame`s and nothing else — the common case
for an active share, since every one of those frames is consumed here without ever reaching the
forwarding arm. `forward_tx.closed()` resolves the instant `RoomSession::event_rx` drops,
independent of whether any event is in flight, so teardown on disconnect stays as prompt as it
was before this task existed.

`Why:` (captures owned by `App`, and owning their destinations, rather than living under a
room) one capture is meant to eventually feed several rooms at once — one real screen grab,
N independently encoded and independently encrypted outgoing streams, because encryption
keys and nonce spaces are room-bound and can never be shared across rooms. If a room owned
its captures, leaving that room would tear down a pipeline other rooms still use. With this
ownership, leaving a room removes only that room's destinations and the capture survives as
long as any destination remains.

`Why:` (ids are opaque `u32` newtypes from a monotonic counter, not map indices and not the
protocol's own ids) `IndexMap` shifts indices on removal, so an index held across a removal
would silently address a *different* entry; a counter that never rewinds means a stale id can
only miss, never alias. The protocol's `StreamId` is a single byte unique only *within one
connection* — exactly the ambiguity multi-room introduces — so every room-scoped collection
either lives inside its `RoomSession` or is looked up by `(room, stream_id)`, never
`stream_id` alone.

**Multi-destination captures are live.** One capture feeds N rooms: every destination —
including the first — is a dynamic tee branch with its own encoder, room key, `stream_id`
and pump task, driven by `spawn_add_destination`/`spawn_remove_destination` (fan-out and
transfer). See "Per-destination lifecycle" below and `superdoc/features/stream/go-live-and-destinations.md`.

The **sidebar** is genuinely multi-room: "Watched streams" (the cross-room grid) plus the ROOMS list,
each joined room with its sharers as sub-rows (`ui/sidebar.rs`); so are `drain_events`, watch-texture
upload and pop-out viewports. `AppState::active_room` still exists as the room the stage is
showing (set when a room row is clicked, `ui/mod.rs`). The old Stream Control / View content tabs and
the share panel are gone: the **stage** replaced them (`ui/STAGE.md`), and the only tab enums left
are the modal ones, `AddTab` and `SettingsTab` (`ui/modals.rs`).

The remaining single-room seams, listed so later work fixes them deliberately rather than
rediscovering them:

- `AppState::pending_connect` is a single slot: one join can be in flight at a time. (It carries
  the join string it is for, so the sidebar can put the right saved row into the "reconnecting" state.)
- `AppState::status` is global, so one room's `ControlClosed` writes an error line that is not
  attributed to any particular room.

Fixed by the app-shell stage: `App::join_string` is gone — each `RoomSession` now carries
the `join_string` it connected with, which is also the key the sidebar joins saved rooms
to live sessions on (`RoomStore::name_for`).

`Why:` (`disconnect_room` is destination-scoped, not capture-scoped) it walks the leaving
room's `Vec<DestinationId>` back-references and, for each, drops the whole `LocalCapture`
**only** when that was its last destination; otherwise it stops just that one destination.
Dropping the capture unconditionally would tear down a pipeline another room is still
using, which is precisely the invariant `App`-owned captures exist to protect.

`Why:` (`AppState::disconnect_room`/`request_leave_room`/`poll_connect_result` don't touch
`expanded_rooms`/`leave_confirm`/the "+ Add Room" card's fields directly, even though the
pre-split `ui.rs` code they came from did) those three are genuinely UI-only view state —
`session.rs` never had them and must not grow them, since that's exactly the boundary the
UI/lib split needs held clean. `poll_connect_result` instead returns a `ConnectOutcome`
(`None` / `Joined(RoomHandle)` / `Failed { error, from_add_card }`) and `request_leave_room`
returns a `bool` (tore down immediately, or the caller must raise its own confirm dialog);
the UI's (`ui/apply.rs`) same-named handlers are thin wrappers that react to the return value to
update their own UI-only fields. `disconnect_room` similarly has a same-named `App` wrapper
that calls `self.state.disconnect_room(handle)` and then clears `expanded_rooms`/
`leave_confirm` itself. A prior QC pass validated this exact pattern (identical
behavior before/after, verified line-by-line) before it was carried over to the real,
larger split described in the `session.rs` module-map row above.

### In the call / not in the call (row 192, 2026-11-13)

`RoomSession::in_call` is `false` after every connect; `AppState::in_call(handle)` is the accessor (it
also answers for a suspended room, from `ReconnectState::in_call`). The state is this client's own: the
relay and the wire know no call concept.

- `AppState::pick_up(handle)` sets the flag (and discards sounds collected meanwhile, so the pick-up is
  silent). It does **not** start any watch; that is the UI's job (`ui/mod.rs` `App::pick_up_room`, which
  also honours "Automatically watch new streams"). Starting a watch, a listen or a share is likewise left
  to the UI, which offers none while the room is not in the call.
- `AppState::hang_up(handle)` is destination-scoped like `disconnect_room` (same capture rule: dropped
  only with its last destination) and stops every video and audio watch, but keeps the `RoomSession`:
  no `Leave`, no roster change. It clears `rewatch` and `resume_after_stop`, so nothing is pulled back
  later. `request_hang_up` follows the `request_leave_room` pattern: it returns a `bool` (done, or the
  caller raises its own confirm dialog via `leave_needs_confirm`), because `session.rs` holds no UI-only
  state.
- Gated while `!in_call`: `sync_audio_watches`, `apply_rewatch` and the room's sounds
  (`RoomSounds::drain_for(now, in_call)` returns nothing).
- A staged capture (no destination yet) is nothing live, so hang-up does not touch it in `session.rs`;
  `ui/mod.rs` `App::poll_staged` drops it and stops its capture when its room is no longer in the call.

`Why:` (D1) a UI/session flag only, so the running relay and `protocol/` stay untouched and old and new
clients mix freely. `Why:` (D4, user: "Dont disconnect the room, only switch to 'not in call'") the red
button hangs up and keeps the connection, so people still see you and the roster. `Why:` (D2, user:
every connect, automatic and manual, starts not in the call) a reconnect is not a new connect, so the
flag is carried over it (`client-reconnect-and-watches.md`).

## Per-destination lifecycle (state machine, wire PTS, stall protocol)

Lives in `capture.rs`'s **`destination`** submodule (state, pure logic — no GStreamer, no
clock of its own) and its **`branch`** submodule (the GStreamer mechanics plus the
orchestrators joining the two). `session.rs`'s `Destination` holds a `SharedDestination`
(`Arc<Mutex<DestinationRuntime>>`) clone and reads the state to render; `capture.rs`'s
`pump_loop` writes it as frames flow.

### The three-tier change model

Every settings change costs exactly one of three things, and only the third is visible to
the viewer. `destination::classify_change(source, from, to) -> ChangeTier` is the single
place that decides which:

| Tier | Trigger | Mechanism | Viewer sees |
| --- | --- | --- | --- |
| 1 `Bitrate` | bitrate only | live encoder property write | nothing (no state change at all) |
| 2 `BranchSwap` | resolution / fps / encoder backend, **at or below** the source's current fps | `branch::swap_to_new_branch`: build the new branch, wait for **its** first IDR, flip authority in Rust, then remove the old one | nothing (`Rebuilding` → `Live`; there is always exactly one live branch) |
| 3 `SourceRestart` | the capture **source** changes (monitor/window switch, fps above the source's rate, source crash) | `branch::stall_all` → restart the capture → `branch::rebuild_after_source_restart` per destination | a low-framerate pause (`Stalled`), never a disconnect |

`Why:` (fps down is tier 2 but fps up is tier 3) each branch already carries its own
`RateCap`, so anything at or below the source's rate is downconverted inside that one
branch and costs nothing shared. A cap cannot invent frames the source never
delivered, so going *above* the source's rate requires restarting the source element
itself — which is shared, so it hits every co-located destination. This asymmetry is why
the capture runs at `destination::required_source_framerate(...)` = `max(fps)` across its
destinations rather than at any single destination's rate.

`Why:` (`clamp_to_source` never upscales, and clamps *proportionally*) upscaling burns
encode budget on invented pixels and adds no detail, so a requested resolution is clamped
down to at most the capture's native size. Clamping each axis independently would silently
change the aspect ratio when only one axis overshoots (3840x1080 off a 1920x1080 capture
would become 1920x1080, i.e. stretched), so both axes are scaled by the single tightest
overshoot ratio. Framerate is deliberately *not* clamped to the source's rate — asking for
more is legal, it's just tier 3.

### `DestinationState`

`Starting` (branch built, no IDR yet) → `Live` → `Rebuilding{since}` (tier-2 swap in
flight, invisible) / `Stalled{since, reason}` (tier-3, the **only** viewer-visible state) /
`Failed(String)`.

`Why:` (`Rebuilding` is exited only by `finish_rebuild`, never by an IDR) during a swap the
*old* branch is still authoritative and still emitting its own periodic IDRs on the 2s GOP.
Letting those end the rebuild would declare the swap complete before the new branch had
produced anything, defeating make-before-break. `Starting` and `Stalled`, by contrast, do
end on the first IDR — there the authoritative branch *is* the one being waited on.

`Why:` (a failed swap returns to `Live`, not `Stalled` — `branch::SwapOutcome::KeptOld`) a
swap consumes the old branch by value, so a plain `Err` would destroy the caller's only
handle to a branch that is still linked, still `Playing`, and still the destination's sole
source of frames. A settings change that can't be applied must leave the viewer exactly as
they were, never worse.

### Stall protocol

A destination enters `Stalled` on a deliberate source restart (`branch::stall_all`) and,
since 2026-10-14, when a `Live` un-paused branch produces nothing for
`destination::silence_threshold` (`DestinationRuntime::stall_if_silent`). See
`features/stream/capture-sources.md` ("Idle monitors") for why a capture can also have *no first
frame at all*, which is handled before a branch exists (`branch::wait_for_source_frame`).

While `Stalled`, the destination is **never torn down** — no `StopStream`, so the viewer
never sees a disconnect/resubscribe. Instead `capture.rs`'s `send_stall_keepalive` re-sends
the destination's last cached IDR every `STALL_RESEND_INTERVAL` (500ms, ~2fps), freshly
re-encrypted (new nonce every time, per spec §5 — a nonce is never reused, not even for
identical bytes) with an advancing wire PTS, and sets `protocol::MEDIA_FLAG_STALLED` in the
media header.

> **`Why:` `MEDIA_FLAG_STALLED` is advisory-only and must stay that way.** The AEAD's AAD is
> exactly `[PROTOCOL_VERSION, stream_id]` — the media header's `flags` byte is **not**
> covered by it, and cannot be: the relay reads and forwards that header without holding the
> room key, so it is cleartext by design (spec §6.2). Anyone on the path can flip this bit
> without breaking decryption. That is acceptable for a UI hint (worst case: a spurious or
> missing spinner) but means it must **never** drive teardown, subscribe/unsubscribe, key
> handling, resource release, or anything else with a security or protocol consequence. What
> keeps that safe in practice: a stalled destination keeps sending real, *authenticated*
> frames and never sends `StopStream`, so the viewer's actual media state never depends on
> this bit being truthful. `MEDIA_FLAG_KEYFRAME` sits in the same byte with the same
> standing. Documented at every point it is set or read (`protocol.rs`,
> `capture::send_media_payload`, `net::DecryptedMediaFrame::stalled`,
> `decode::StreamDecoder::is_stalled`).

Viewer side: `decode.rs`'s `StreamDecoder::is_stalled()` is true if the flag was seen **or**
no frame has arrived in >3s (`NO_FRAME_STALL_TIMEOUT`). Both halves are needed — the flag
covers a graceful restart (the sender is still sending, so silence never fires), the timeout
covers a sender that died without being able to signal anything. `is_stalled_at(now)` takes
an injected `Instant` so the timing half is testable without sleeping. This stage builds
only the *signal*; the placeholder UI that consumes it (pulsing bar → spinner →
"still waiting" tiers) is a later UI stage. **Update 2026-11-02 (#101):** the tile and pop-out
now show a centred chip from `StreamDecoder::view_state` ("Recovering…" / "Sharer's capture
stalled", the latter driven by this flag, display only) and the viewer acts on *local* evidence
(loss, silence, a decoder that outputs nothing) with keyframe requests, a decoder reset and a
reconnect: `features/video/viewer-recovery.md`. The flag itself still drives nothing.

### The wire-PTS continuity invariant

**Wire PTS is a property of the `Destination`, never of the pipeline.** A GStreamer
pipeline's running time restarts at 0 every time it is rebuilt, so sending buffer PTS
straight onto the wire makes a capture restart look like the stream jumped *backwards*,
which `decode.rs`'s `PacingState` can only read as a discontinuity to hard-reset on — a
visible glitch.

- `pts_epoch: Instant` is set once at destination creation and **never reassigned**.
- `pts_offset_us: u64` is recomputed on every pipeline (re)start:
  `pts_epoch.elapsed() - pipeline_running_time_at_restart` (`DestinationRuntime::on_pipeline_start`,
  which must run *before* any buffer from the new pipeline is timestamped). A tier-2 branch swap does not go through `on_pipeline_start`: `DestinationRuntime::on_branch_swapped(first_buffer_pts_us)` corrects the offset only when the seam would land behind `last_sent_pts_us`, anchored on that value and not on the epoch (see its doc comment in `capture.rs` for why).
- Every send goes through `wire_pts = max(raw + offset, last_sent + 1)` — the `max` is a
  **hard clamp, not a formula**. It guarantees monotonicity even if the offset arithmetic is
  wrong, if a stale pre-restart buffer arrives, or if a stall re-send interleaves with a real
  frame. `capture.rs` funnels every wire timestamp through `DestinationRuntime::advance_pts`,
  so there is exactly one place the invariant can be broken.

The result: a restart reads to the viewer as *a period of low framerate*, not a break —
which is exactly the illusion the stall protocol exists to sustain.

### `EncodeBranch` and the single-`AppSinkStream` rule

`gst_app::AppSink::stream()` is not a view onto an appsink — it *installs* callbacks
(`gst_app_sink_set_callbacks`, which replaces whatever was there), and on this crate's
pinned feature set its `Drop` resets the appsink's callbacks to an empty set. So two
`stream()` calls on one appsink silently break each other: the first's waker is
unregistered by the second, and dropping either leaves the survivor parked (it only wakes
again by luck, since `poll_next` opportunistically `try_pull_sample`s before parking). This
matters because `remove_branch` must itself drain the appsink to EOS while a production
reader is already consuming it.

The fix is structural rather than a rule to remember: `EncodeBranch` owns the branch's
**one and only** `AppSinkStream`, created lazily on first `next_sample()` and reused by
`remove_branch`, which receives it by taking the whole `EncodeBranch` **by value**. Handing
the branch to `remove_branch` *is* the reader's handoff; there is no window in which two
live streams exist. Regression-guarded by
`a_branch_already_being_read_removes_promptly_without_a_second_appsink_stream`, which
asserts on *removal latency* — a clobbered callback set doesn't fail loudly, it just burns
the internal 5s drain timeout and logs "did not drain to EOS" while still returning `Ok`.

Also in `remove_branch`: the removal pad probe fires once per dropped buffer for the whole
drain window, and only the first firing does anything (EOS into an already-EOS'd branch is a
no-op), so a one-shot `AtomicBool` keeps a per-buffer `send_event` — which runs the branch's
whole event chain inline on the tee's streaming thread — out of the hot path.

### Per-branch encoder fallback

`branch::add_branch_with_fallback` (construction) and `branch::add_branch_live`
(construction *plus* "actually produced an IDR within `FIRST_IDR_TIMEOUT`") apply the same
VAAPI→software fallback `start_sharing` applies to the whole pipeline, but **per branch**.
`Why:` the whole-pipeline fallback only ever ran with one encoder in flight. With fan-out
there are N concurrent encode sessions off one capture and VAAPI's concurrent-session limit
is small (commonly 2–4), so the *third* destination can fail on a machine where the first two
succeeded and where `detect_encoder_backend`'s process-wide probe still correctly reports
VAAPI as available. That failure is per-branch and transient — exactly what a process-wide
probe cannot express.

### What is wired vs. what waits for a later stage

**All of it is wired as of stage 2.** Every outgoing destination is a dynamic branch with
its own `DestinationRuntime`, driven for real by its own `destination_pump`: wire PTS, IDR
caching, `Starting` → `Live` on the first IDR, and the stall keepalive ticker. The
branch-backed paths are driven from the UI (`ui/apply.rs`) for real too — `swap_to_new_branch` behind every
tier-2 settings edit, `add_branch_live`/`remove_branch` behind fan-out and transfer.
`bandwidth::allocate` is applied to live encoders in Absolute-Maximum mode. See
`superdoc/features/stream-control.md`.

`branch::rebuild_after_source_restart` (called only from tests) and `CaptureHandle::note_source_restarted` (no callers)
are the orchestrators with no production caller. The stall half is live: `DestinationRuntime::begin_stall`
is reached from `stall_if_silent` and from `branch::stall_all`, which `CaptureHandle::begin_stall` calls when a
capture's source dies (`session.rs` `begin_recovery`). Tier 3 turned out to need no pipeline surgery at
all: measured against the real portal, the source is variable-rate (`framerate=0/1`) with
no ceiling to raise, so `set_source_framerate` just records the higher rate and the
caller's ordinary tier-2 branch swap applies it — nothing stalls, so nothing has to be
stalled (see the `Why:` on `set_source_framerate`). The two caller-less ones stay as the mechanism a *real*
source change (portal session revoked, device unplugged — `StallReason::SourceLost`) would
need, and remain covered by their own tests.

Still not built: the audio mixer rework (audio is still its own capture, not an attachment
on a video destination).

The UI was rebuilt on egui as a room-centric window (the Discord-like layout of `superdoc/ui/APP-SHELL.md`
and `superdoc/ui/STYLING-GUIDE.md`; code in `client/src/ui/`). The Stream Control feature set
(fan-out, make-before-break transfer, per-destination live edits (encoder presets and Quality level),
both bandwidth modes with the overshoot warning) is built (`superdoc/features/stream-control.md`), as are
real decoded video in the room view with per-stream pop-out windows (`superdoc/features/video-pipeline.md`,
`superdoc/features/viewer-window.md`). `superdoc/ui/FRONTEND-SHELL.md` documents the removed HTML
frontend and is kept as history.
