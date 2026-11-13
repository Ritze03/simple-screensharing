# Native Client: Reconnect, Resume and Stopped Streams

Split out of `implementation-notes.md` on 2026-11-07. Decisions and the *why* of how `session.rs` / `net.rs` behave when a connection drops, a stream stops or the app exits (tasks #36, #40). The model they act on: `session-model.md`. Vocabulary: `meta/TERMINOLOGY.md` (Suspended room, Wanted stream).

## Native client: stopped streams, AV1 size, staged-modal meter (2026-10-14, task #40)

Found by I19 in a real two-client run.

- **A stopped stream ends the watch.** `drain_events` ignored the relay's `StreamStopped` for
  `RoomSession::watches`, so the viewer kept a frozen 0 fps tile (and "Watched streams 1"). It now
  calls `RoomSession::end_watch_of_stopped` (before `note_stream_stopped`, which forgets the name and
  label): the watch entry, hence the decoder, goes; `ui::video::VideoViews::sync` then drops the GPU
  `VideoView` on the next frame. The selection moves on. No `unsubscribe` is sent (the relay already
  dropped the stream and its subscriber list). The relay sends `StreamStopped` for every stream of a
  member that leaves or drops, so that case is covered too. The former Electron shell (removed) did this in
  `driver::prune_stale_watches`; the egui path never had it.
- **Re-share resumes the watch (decision).** The stopped watch is remembered in
  `RoomSession::resume_after_stop` for `RESUME_AFTER_STOP` (30 s) and matched like a reconnect's
  (`WantedStream`: owner name + kind + label, `rewatch_one`), so the same person going live again
  takes the dead tile's place with no click, with the same selection and volume.
  `Why resume:` stop/start is how a sharer switches monitor or fixes a stuck share; a viewer who was
  watching means to keep watching, and the reported symptom was the new share showing up as a separate
  card. `Why only 30 s (not the reconnect's 60 s):` a deliberate stop is not an outage; minutes later an
  automatic subscribe would be a surprise. A user's own "stop watching" never queues a resume.
  Separate from `rewatch` because each entry has its own deadline and a different cause.
- **AV1 1080p arrives as 1082 lines.** See `features/video/decode-and-display.md` ("Visible size"): the
  sender (VCN4) really encodes 1082 lines with no render size; the receiver crops to the announced
  size.
- **Staged "Set up your stream" modal: the meter never moved.** Level taps exist only after
  `UiAction::StartLevelTaps`, and only the bar's audio popover sent it; a staged capture has no audio
  share to read a level from either. `stream_settings::sync_level_taps` now starts a tap for every audio
  source while the staged modal is open (restarted when the source list changes) and stops them on
  any footer button / Escape / click-outside; the Source card reads the chosen source's tap
  (`cx.source_level`). Known gap: if the shell closes the modal by itself (capture or room vanished)
  the taps run until the next `StartLevelTaps`/`StopLevelTaps`; fixing that needs a hook in
  `ui/mod.rs`.

## Native client: auto-reconnect and clean exit (2026-10-14, task #36)

Found by I20 in real runs: after a relay restart the egui client sat on "Not connected" until a
manual Connect + Watch, and a killed/closed client lingered in the roster for 60-75 s (relay idle
timeout) because nothing sent `Leave` or closed the QUIC connection. Both fixed in `session.rs` /
`net.rs`; the browser core already behaved this way.

- **Dropped room = suspended, not torn down.** `drain_events` used to call `disconnect_room` on
  `ClientEvent::ControlClosed`. It now calls `suspend_room`: the `RoomSession` goes, a
  `ReconnectState` in `AppState::reconnects` takes its place (kept `RoomHandle`, last-known roster
  and streams, attempt count, next retry, last error, cause). `is_reconnecting(join_string)` is true
  for the whole outage (also between attempts), `Status` stays `Connecting` (the UI only repaints
  on that), so the existing UI shows its Reconnecting banner with no edits. Only for rooms still in
  the saved list; user Leave / Forget / Cancel and `shutdown` never reconnect.
  `Why the handle is kept:` everything keyed by `RoomHandle` (a destination's `room`,
  `active_room`, the UI's staged shares and views) stays valid across the outage with no remapping.
  `Why no session while reconnecting:` the UI derives Reconnecting from "no session for a saved
  room but `is_reconnecting`", so a half-dead session would show as joined.
- **Retry policy** (`session::retry_delay`, pure): first retry immediate, then 0.5 s, 1, 2, 4, 8,
  capped at 15 s, forever. Per-attempt timeout 8 s (`RECONNECT_ATTEMPT_TIMEOUT`; a manual connect
  keeps 15 s) so a dead relay cannot hold the single connect slot and starve the backoff. The
  failure kind comes from the QUIC application close code (`net::CloseCause`, read with
  `ScreenshareClient::close_cause()` / `CloseCause::from_error`): `0x200` relay shutdown retries
  at once, `0x102` room full retries but never faster than 5 s (the error is kept in
  `last_error`, `ReconnectState::room_full()`), `0x101` malformed stops and surfaces
  (`ConnectOutcome::Failed`, `Status::Error`), anything else backs off. `connect()` now returns a
  typed `JoinRejectedError` instead of a bare `bail!`, and falls back to the close code if the
  relay's close beat the `JoinRejected` frame.
- **Our shares come back by rebuilding the branch, not re-pointing it.** A branch is bound to the
  old connection (client clone, nonce space, announced `StreamId`; the new connection has a fresh
  `ConnId`). `suspend_destination` removes the branch (`spawn_remove_destination`) and records
  the params; the *capture* (screen grab, portal session) keeps running, so there is no new picker
  prompt. After the reconnect `add_destination_with_params` rebuilds each branch on the new room
  (the same primitives `retry_destination` uses): no `capture.rs` change needed. Audio shares are
  bound to the connection as a whole, so they are stopped and restarted with the source they were
  started with (`AppState::audio_share_sources`). A capture left with no destination is invisible
  to the UI; cancelling/forgetting the room stops it. Transfer halves are not resumed.
- **Watches come back by name.** `WantedStream { owner_name, kind, label }` (the browser's
  `stableKey` minus the room); `match_wanted` picks the first not-yet-watched stream of a member
  with that name, so two "Screen"s of one owner map to two streams. Unmatched ones wait in
  `RoomSession::rewatch` for 60 s (`REWATCH_GRACE`, the browser's resume window) and resolve as
  re-announces arrive, because a sharer that reconnected is indistinguishable from one that
  stopped and started again, and after a relay restart the owners rejoin at different moments.
  Selected watch and per-owner volume carry over; mute and pop-out do not.
- **The call state is carried (2026-11-13).** `ReconnectState::in_call` records `RoomSession::in_call` when
  the connection drops and `restore_after_reconnect` puts it back on the new session; while suspended,
  `AppState::in_call` reads it from there. `Why:` (D2, user: every connect starts not in the call, a
  reconnect is not a new connect) an in-the-call room stays in the call and an idle one stays idle.
  Watches and shares are only resumed for an in-call room (`apply_rewatch` is gated on the flag), and a
  hang-up clears `rewatch` so nothing is pulled back after a reconnect. `session-model.md` "In the call".
- **Clean exit.** `disconnect_room` (user Leave, Forget) sends `Leave` and closes the connection
  (`ScreenshareClient::leave_and_close`, code `0x300`). `AppState::shutdown` /
  `shutdown_blocking(timeout)` do that for every room, close the endpoint with a bounded (500 ms)
  `wait_idle` so the close frames really leave the socket, and stop the captures alongside (a
  pipeline can take seconds to drain; it must not delay the leave). `install_shutdown_signal_handlers`
  turns SIGTERM/SIGINT/SIGHUP into a window close so the normal `on_exit` path runs; a second
  signal or 4 s without exit force-quits. `Why via the window:` the state belongs to the UI
  thread, so a signal task can only ask that thread to run the shutdown.
- **Known gaps.** (1) `capture.rs` binds a capture's room-preview task to the *first*
  destination's client; after a reconnect it keeps failing on the dead connection, so others see
  no thumbnail of our share until it is restarted. Fix: `ensure_preview_task` restarts the task
  when its client's connection is closed, which needs `preview_sink` kept (cloned) instead of
  `take()`n. (2) A relay that dies without a `CONNECTION_CLOSE` (SIGKILL, power loss) is only
  noticed on the next packet or after the idle timeout (60 s then, 20 s since 2026-11-02, `features/relay-transport.md`).
- **UI wiring (task #37).** The banner's Retry now / Cancel are `UiAction::RetryNow(key)` /
  `CancelReconnect(key)` (`reconnect_now` / `cancel_reconnect`; Cancel also aborts a first-time
  connect); the old `CancelConnect` action is gone. The banner adds "Attempt N · Retrying in N s"
  (or "Trying now…", "Room full, still trying") and one short "Last error" line from
  `reconnect_info`. The People panel lists the last-known roster dimmed, with our own row
  prepended from the display name (the lib's copy leaves us out). `on_exit` calls
  `shutdown_blocking(1.5 s)` and `App::new` installs the signal handlers. A staged (not yet
  live) share survives an outage; one whose room is really gone has its capture stopped.
  "Live" (People pill, sidebar cast icon, "Live in N rooms") now means a destination that is
  on air (`Cx::my_phase` / `on_air_rooms`); while Starting the UI says so, and the share card
  carries the idle-screen hint as a tooltip. `Why:` a share waiting for an idle screen's
  first frame used to read as Live although nobody saw anything.
