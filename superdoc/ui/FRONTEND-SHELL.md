# Frontend shell (now Electron; built as a Tauri UI port)

**Added 2026-07-26.** The desktop client's UI is being ported from egui to a plain static
HTML/CSS/JS frontend (no framework/bundler), originally shown in a Tauri webview and
**since 2026-07-28 shown in an Electron `BrowserWindow` instead** — see
`superdoc/architecture/overview.md`'s "Process model" section for the two-process
Electron-shell/Rust-sidecar architecture this now runs under, and this file's own matching
2026-07-28 entry below for what that migration changed (in short: almost nothing at the
frontend-code level — the same `window.__TAURI__.core.invoke`/`event.listen` calls this
whole document describes are now serviced by an Electron-bridged REST/SSE transport
instead of real Tauri IPC, transparently to every file below). This document covers the
**shell** — sidebar, nav, ROOMS/Outgoing sections — built at
`simple-screensharing-client/frontend/`. It is the HTML/CSS/JS counterpart to
`APP-SHELL.md` (the egui version, now dead code but still the source of the *behavioral*
rules) and follows the same structural rules that doc describes (always-on sidebar, nav is
a pure view-switcher, ROOMS is the greedy scroll region, Outgoing is capped). Read
`APP-SHELL.md` first for the *behavioral* rules — this doc only covers what's specific to
the HTML/CSS/JS implementation, its real build history, and (below) the Electron
migration and the later visual redesign.

**Update 2026-07-26 (later same day): Stream Control / View / Settings tab content
built.** The shell above is unchanged; this update fills in the three tab panels that
were previously placeholder text. See "Tab content" below.

**Update 2026-07-26 (later still: real backend wiring).** Every `// TODO: replace with
invoke(...)` stub across `shell.js`/`stream-control.js`/`view.js`/`settings.js` now
calls the real `#[tauri::command]`s in `src-tauri/src/commands.rs`, and startup wires
`window.__TAURI__.event.listen(...)` for all 6 events `driver.rs` emits
(`room-connected`, `room-connect-failed`, `room-disconnected`, `presence-updated`,
`destination-state-changed`, `bandwidth-overshoot`), set up once in `shell.js`'s
`wireEvents()`. The mock/real fallback pattern described below is otherwise unchanged
and still fully intact — every mock branch still works standalone in a plain browser.
Command-argument keys are camelCase (`{ roomHandle, ownerId }`, …) — Tauri's
`#[tauri::command]` macro auto-converts a Rust fn's snake_case parameter names to
camelCase IPC argument keys by default, and this codebase calls `invoke()` manually
(no generated/specta bindings), so every call site had to get that casing right by
hand, verified per-command against `commands.rs`'s exact parameter names.

**Mock data shapes were changed to match the real DTOs field-for-field**, not the other
way around (the DTO is the real contract): room/stream/destination/settings objects now
use `dto.rs`'s exact snake_case field names (`join_string`, `room_handle`,
`active_stream_count`, `destination_id`, `capture_id`, `bitrate_kbps`, `presentation_mode`,
…) instead of the earlier camelCase guesses, `kind: 'video'|'audio'` instead of a
boolean `isAudio`, and the bandwidth-mode enum string `'per_stream'` (underscore, matching
`dto::bandwidth_mode_str`) instead of `'per-stream'` (hyphen). The Settings tab's
presentation-mode dropdown values changed from a guessed `vsync`/`reactive`/`continuous`
to the real `auto_vsync`/`immediate`/`mailbox` (`dto::presentation_mode_key`). No DTO
inlines a room's live roster + watch state together, or a cross-room "every watched
stream" list, or "just this room's members" — those three real-branch fetches (in
`shell.js`'s `loadRoomMembers`, `view.js`'s `fetchWatchedStreams`, and
`stream-control.js`'s `fetchRoomMembers`) each do their own client-side join of
`list_saved_rooms`/`room_detail`'s `members`/`streams`/`watched` arrays — documented
inline at each join site.

`Why real command names sometimes differ from what stream-control.js originally
guessed:` `start_video_share`/`start_audio_share` take only a `room_handle` — framerate/
bitrate/audio-source are process-wide settings applied via `set_framerate`/`set_bitrate`/
`select_audio_source` *before* the share starts, not per-call arguments, so
`startVideoShare`/`startAudioShare` in `stream-control.js` now call those first. Fan-out
(`start_destination`) takes a `capture_id`, not a `destination_id` — it starts a new
branch off the same capture, not a copy of one destination — so `destinationCard`'s
`onFanout` handler was changed to pass `dest.capture_id`. The leave-confirm flow calls
`leave_needs_confirm` first to *decide* whether to show the dialog, then either
`leave_room` (no confirm needed) or `force_leave_room` (confirmed) — not `leave_room`'s
own boolean return value as the primary signal, per `commands.rs`'s doc comment on why
`leave_needs_confirm` is its own query.

**A known gap, left honestly unresolved rather than faked:** no DTO/event carries a
per-watched-stream "is this actually decoding right now" signal. The MJPEG-over-HTTP
frame path itself (see `video_server.rs`) was out of scope for this command/event wiring
pass and has since been built — see `superdoc/architecture/overview.md`'s `Why:` note and
this file's 2026-07-27 entries below for the mechanism, its status, and a newly-found
pre-existing bug (unrelated to the frame-delivery mechanism itself) currently blocking
real video from actually rendering end-to-end. Every real View-tab tile still renders in
the `'live'` visual state regardless of whether frames are actually flowing; the
`'stalled'` placeholder is only reachable via the mock branch's demo toggle until a later
worker wires a real per-stream health signal.

> **2026-07-28 update: the "real video from actually rendering end-to-end" gap above is
> closed** — the MJPEG design this paragraph describes was itself replaced by a raw
> H.264-over-HTTP relay decoded client-side with WebCodecs (see this file's 2026-07-27
> "later still" entries below and `superdoc/features/video-pipeline.md`), and every claim
> below in this document that a View tile shows "only a static CSS gradient placeholder"
> or that "no `<img>`/`<canvas>` element" exists is now **historical**, not current — real
> tiles render real `<canvas>` elements fed by `window.TileVideo`. The per-stream
> "is this actually decoding" health-signal gap named in this paragraph is still open;
> only the underlying "there's no real video at all" gap closed.

**2026-07-27 — fixed:** a real backend bug found while QC-verifying this against the
actual compiled app (not caused by, or fixable from, this frontend-only pass):
`src-tauri/src/state.rs`'s `Managed::new()` constructs `session::AppState::default()` —
which calls `net::build_client_endpoint()`, binding a `quinn`/Tokio UDP socket — from
`tauri::Builder::default().manage(Managed::new())`, i.e. before Tauri's Tokio runtime is
entered (the exact same class of "no reactor running" trap `driver.rs`'s own module doc
warns about for `tokio::spawn`, just hit one call earlier in the chain). The result:
`AppState.endpoint` was permanently `Err("failed to bind local UDP socket: no async
runtime found")` for the whole process lifetime, so **every** `join_room` call failed
immediately in any real launch of the app, regardless of environment.

Fixed in `src-tauri/src/main.rs`: `Managed::new()` is now constructed inside
`tauri::async_runtime::block_on(async { Managed::new() })` before `.manage()`, instead of
being called bare. `block_on` lazily creates (or reuses) Tauri's one shared Tokio runtime
and *enters* it for the call, giving `build_client_endpoint()` the reactor it needs —
available even this early, before `.setup()`/`.run()`. `session.rs`/`state.rs` were left
untouched; the fix is entirely `main.rs`-local (see its `Why:` note at the fix point).
Verified against a real relay server + the real compiled Tauri app in an isolated
headless Wayland compositor: `join_room` now round-trips to a real `Joined` outcome
instead of the "no async runtime found" error.

**2026-07-27 — later the same day — fixed, second instance of the same bug class:** the
real compiled app crashed on the very next click after the fix above — clicking
"Reconnect"/"Join" panicked at `src/session.rs:1854` (`AppState::start_connect`'s raw
`tokio::spawn`) with the identical "no reactor running" message, this time inside a
webkit2gtk `register_uri_scheme` FFI callback (Tauri's IPC delivery mechanism on Linux),
which cannot unwind — a fatal process abort, not a recoverable panic. Root cause: Tauri
only enters its managed Tokio runtime for `async fn` `#[tauri::command]`s (it wraps their
execution in `tauri::async_runtime::spawn` before polling); a plain `fn` command — which
`join_room` was — is instead called directly on whatever thread delivered the IPC
message, no reactor entered, so any raw `tokio::spawn` reached from inside one panics.
Reproduced deterministically with a unit test (`commands::tests::
join_room_outside_a_tokio_runtime_does_not_panic`) that builds a `tauri::test` mock app,
drops the throwaway runtime `Managed::new()` was built inside, and calls the real
`#[tauri::command] join_room` wrapper with no runtime entered — hits the exact panic
message/location pre-fix.

Tracing every other sync command in `commands.rs` turned up the identical defect in
`leave_room`, `force_leave_room` (both via `disconnect_room`), `start_video_share`,
`start_audio_share`, `stop_share`, `start_destination`, `transfer_destination` (via
`add_destination`), `stop_destination`, `toggle_watch`, and `toggle_listen` — every one of
these transitively reaches a raw `tokio::spawn` in `session.rs`/`capture.rs`, so every one
would have crashed the same way on its first real click. Fixed the same way in all of
them: wrap the call into `AppState`/`RoomSession` in `tauri::async_runtime::block_on(async
{ ... })` at the `#[tauri::command]` wrapper (see `join_room`'s `Why:` note in
`commands.rs` for the full reasoning) — the same already-established pattern as the first
fix above, applied at the call site instead of turning these into `async fn` commands
(which would have forced their return types to `Result<_, _>` for every one taking
`State<'_, Managed>`, per Tauri's async-command macro rule, touching every frontend call
site's error handling for no behavioral gain — `block_on` here never actually blocks on
I/O, since each of these methods only needs the reactor to *issue* the `tokio::spawn`, not
to await its result). `session.rs`/`capture.rs` stay untouched — same shared-crate
boundary reasoning as the first fix. Also hardened `driver.rs`'s `tick()` — which locks
the same `Managed::state` mutex every 250ms from its own always-running background task —
to recover from a poisoned lock (`.unwrap_or_else(|poisoned| poisoned.into_inner())`)
rather than propagate the poison and kill the polling loop too, in case a future command
panics for an unrelated reason while holding the lock.

Verified: `cargo test --workspace` clean (162 passed including the new regression test),
and the real compiled app launched via an isolated `XDG_CONFIG_HOME` and stayed alive and
rendering for 15+ seconds with no crash.

**For whoever eventually removes the egui fallback:** this is the second time the same
"a Tauri command/hook ran on a thread with no Tokio reactor entered" bug class has bitten
this app for real (`main.rs`'s `Managed::new()`, then `commands.rs`'s ten sync commands).
Both instances were only found by actually running the compiled app, not by reading the
code — `cargo build`/`cargo test` are silent about it, since the type system has no way to
express "this fn needs an entered reactor". If a third instance turns up, it's very likely
another sync `#[tauri::command]` (or another `Builder::setup`/`.manage()` closure) that
reaches a raw `tokio::spawn`/`Handle::current()` through a few calls of indirection — grep
`session.rs`/`capture.rs`/`audio.rs` for `tokio::spawn` and cross-check every caller
chain from `commands.rs` is either already `async fn` or already wrapped in `block_on`.

**2026-07-27 — later still — fixed: poll-driven re-render dropped in-flight clicks in
Stream Control.** QC driving the real compiled app with `ydotool` found the Stop button
and the Per-Stream Maximum bandwidth-mode toggle intermittently ate correctly-targeted
clicks — no crash, no hang, just a click that silently did nothing. Root cause, confirmed
by reading `stream-control.js`: its `render()` did `root.innerHTML = ""` then rebuilt
*every* card in the tab from scratch on every single call — and it was called on every
`presence-updated`/`destination-state-changed` event `driver.rs`'s 250ms poll emits (via
`shell.js`'s `wireEvents()` → `window.StreamControl.refresh()` → `init()` → `render()`),
even when the event had nothing to do with most of what got rebuilt (e.g. one
destination's state flipping tore down the Stop button on every *other* destination's
card too, and the bandwidth-mode toggle along with it). Every button was a freshly
`document.createElement`d node with a freshly `addEventListener`'d handler each time —
exactly the pattern that drops a click landing between the old node's removal and the new
node's attachment. `driver.rs` itself already diffs before emitting (see its `Snapshot`
struct/`tick()`) — the bug was entirely on the frontend side, re-rendering the *whole tab*
in response to an event that was itself already known to be meaningful for only one small
part of it.

Fixed in `stream-control.js` only (no changes to `driver.rs`, `commands.rs`, or
`components.js`): `render()` now reconciles instead of rebuilding. Each top-level card
(Bandwidth, Active Shares, Start a Stream, Room-scoped) keeps its DOM node across a
`render()` call unless a cheap `JSON.stringify` signature of the data it actually displays
changed; a new `syncChildren()` helper reuses/moves existing nodes into place in the
`.sc-grid` instead of tearing them down. Active Shares goes further with keyed
per-destination reconciliation (`destCardEls: Map<destination_id, {el, sig}>`): a
destination's card — Stop button included — is only rebuilt when *that destination's own*
signature changes, and is *moved* (a no-op `insertBefore` if already in place) rather than
recreated when only its position in the list needs to shift. This means a `presence-updated`
event about a totally different room, or one destination's state changing, no longer
touches any other destination's card or the bandwidth-mode toggle at all.

Verified against the real compiled app (isolated `XDG_CONFIG_HOME`, real relay on
`127.0.0.1:4433`, real screen capture, `ydotool` driving real synthetic clicks) with a
throwaway churn client repeatedly joining/leaving the same room to generate genuine
`presence-updated` traffic from the real 250ms poll while clicking: the Per-Stream
Maximum/Absolute Maximum bandwidth-mode toggle was alternately clicked 15 times in rapid
succession against live churn traffic in three separate sessions — all three landed on the
exact expected final state (parity-checked: 15 alternating clicks starting on Absolute
must end on Absolute), i.e. 45/45 real clicks registered correctly with zero drops. The
Stop button was verified individually (not chained into a rapid restart loop, see the
caveat below) across multiple clean sessions with live churn traffic active, registering
correctly every time it was clicked in isolation.

**A separate, real bug found during this QC pass — reproducible, was NOT yet fixed at the
time this paragraph was first written (see the fixed-entry right below it for the
resolution):** repeating the *same* Stop→Share Screen restart cycle back to
back with no pause between attempts, or issuing the 15-click bandwidth-mode-toggle stress
sequence and then immediately clicking Stop, reliably (3/3 attempts) leaves the app in a
state where **every** further `#[tauri::command]` invocation (Stop, either bandwidth-mode
button, presumably any other real command) silently never resolves — not a crash, not a
per-click drop, but every backend-bound button going permanently unresponsive for the rest
of the process's life, while pure-client-side actions (switching Stream
Control/View/Settings tabs) keep working fine. This means the webview and its click
handling are fine; something in the Rust-side command/driver layer is getting wedged
(`ps -T` on the process showed every `tokio-rt-worker` thread idle in `futex_wait`, not
spinning — consistent with something awaiting a lock/channel that will now never resolve,
not a busy deadlock or a panic — no panic was logged).

**2026-07-27 — later still — fixed: the Stop→Share-restart permanent hang above, root
cause found and fixed.** The prime suspects flagged in the paragraph above
(`driver.rs`'s `tick()` holding `managed.state.lock()` across `apply_bandwidth_allocation()`,
or `set_bandwidth_mode` racing that lock) turned out to be **wrong** — investigated and
ruled out by reading `bandwidth.rs`'s allocator write path
(`CaptureHandle::set_destination_bitrate` is just an `UnboundedSender::send`, never
blocking) and `capture::branch::remove_branch` (its own drain wait is already a properly
`tokio::time::timeout`-guarded async wait, not a blocking call). The real cause is
structurally different and lives one layer down, in `capture.rs`'s pipeline startup:

`capture::start_pipeline` (called by `start_sharing_with_source`, itself spawned via a raw
`tokio::spawn` from `session::AppState::start_video_share`) calls GStreamer's
**synchronous, blocking** `Element::state(timeout)` to wait for the new pipeline to reach
`Playing`, passing a nominal 5-second `ClockTime` timeout. That timeout is **not reliable
under real contention** — reproduced directly (not inferred) with a diagnostic Rust harness
(`src-tauri/src/commands.rs`'s `#[ignore]`d `hang_repro_stop_share_cycles`/
`hang_repro_bandwidth_toggle_then_stop` tests, built for this investigation and kept as
permanent manual regression coverage) that calls the real `#[tauri::command]` wrapper
functions against a real relay server and real xdg-desktop-portal screencast capture, with
`eprintln!` breadcrumbs temporarily added around every await point in the portal/pipeline
startup path (removed once the culprit line was found). Starting a new capture immediately
after the *previous* one's portal session/pipeline finished tearing down — exactly what
"Stop, then Share again, no pause" does — can leave `pipewiresrc`'s state change stuck deep
in GStreamer/PipeWire's own synchronization for far longer than the 5s argument promises:
measured directly anywhere from several seconds up to indefinitely (never returned) across
otherwise-identical repeated attempts in the same process.

`Why this wedges *every* command, not just the one that triggered it:` a call that can
genuinely never return must never run on one of Tokio's own runtime worker threads — but
`start_pipeline` was being called inline from `start_sharing_with_source`, an `async fn`
already executing on that pool (reached via the raw `tokio::spawn` inside
`AppState::start_video_share`), so a stuck `Element::state()` call permanently parked
whichever worker thread happened to run it. The default multi-thread Tokio runtime has a
small, *fixed* worker pool; `driver.rs`'s 250ms poll loop and every `block_on`-wrapped
`#[tauri::command]`'s spawned continuation (see the `block_on` fix above) all share that
exact pool. Enough repeated stalls (empirically, as few as one more restart cycle) leaves
zero workers able to run *anything* — the poll loop stops polling, and every command's
`block_on` future can no longer be driven — which is exactly "every subsequent command
silently never resolves" and "every `tokio-rt-worker` thread idle in `futex_wait`" (the
stuck threads are genuinely blocked in GStreamer's own C-level wait, which is a real OS
futex wait, not spinning; a caught panic was never involved, hence no panic logged).

**Fix**, entirely in `capture.rs`: wrap the call to `start_pipeline` in
`tokio::task::spawn_blocking` at its one call site (`start_sharing_with_source`), and
`.await` that instead of calling it inline. This doesn't fix GStreamer's own timeout
unreliability (out of this app's control), but it confines a stuck state-change to one
throwaway thread on Tokio's separate, elastic blocking-thread pool instead of permanently
starving the small, fixed async worker pool the rest of the app depends on. `session.rs`
was not touched — `start_video_share`'s raw `tokio::spawn` still just fires the (now
`spawn_blocking`-protected) future and returns immediately, same shape as every other spawn
in that file. `Why not also convert `start_pipeline` into a genuinely async wait (e.g.
polling the pipeline's bus for `ASYNC_DONE` instead of GStreamer's blocking `state()`):`
considered, but that's a materially bigger, GStreamer-idiom-crossing change for the same
outcome `spawn_blocking` already gives with a two-line diff — `spawn_blocking` is the
correct-and-minimal fix for "a call that might block for an unbounded time on a worker
thread pool that can't afford to lose one," which is exactly this bug.

**Verified:** `cargo build --workspace` and `cargo test --workspace` clean (162 passed,
0 failed, same count as before this change plus the 2 new `#[ignore]`d regression tests —
including `commands::tests::join_room_outside_a_tokio_runtime_does_not_panic`, confirming
this change doesn't reintroduce the earlier "no reactor running" crash). Pre-fix, the
`hang_repro_stop_share_cycles` harness reproduced the wedge reliably by cycle 2 (the *second*
restart) against a real relay and real portal capture. Post-fix, the same harness ran **15**
rapid Stop→Share cycles (no pause between attempts, exactly the reported trigger) against a
real relay and real portal capture with **zero** command-response hangs — every single
`start_video_share`/`stop_share`/`list_saved_rooms` call across all 15 cycles resolved in
under 100 microseconds, even on the cycles (8 of 15) where the new capture itself was slow
enough under GStreamer/PipeWire churn that it never reached `Active` within the harness's
30s per-cycle budget — proving the actual regression (the whole backend wedging) is fixed,
independent of that separate, lower-severity real-world capture-startup flakiness (GStreamer/
PipeWire genuinely can take a while, or fail once, to settle a brand-new capture right after
tearing down the previous one — left as-is, since it isn't the "permanent, every-command"
bug this task was about, and the fix here already ensures it can never cascade into one).
The second reported trigger — 15x rapid bandwidth-mode-toggle then Stop, with **no**
capture restart — was also tested with its own harness (`hang_repro_bandwidth_toggle_then_stop`)
and did **not** independently reproduce a hang, before or after this fix: every code path it
exercises (`apply_bandwidth_allocation`, `set_bandwidth_mode`, `CaptureHandle::stop`'s
`remove_branch`/portal-close) was confirmed non-blocking by direct code reading, and none of
them touch `start_pipeline`. Most likely the original QC report's "OR" bundled two
observations that actually share one root cause reached only via a restart (e.g. an
exploratory restart after that Stop that wasn't captured in the written repro steps) — left
as an open, low-confidence note for whoever next touches this area, since a negative result
here doesn't prove no bug exists in that exact sequence, only that this investigation's
harness couldn't trigger one.

`Why the real compiled Tauri app (webview + `ydotool`) wasn't the primary verification
vehicle this pass, unlike prior QC entries in this doc:` attempted first, but a *pre-existing,
unrelated* frontend issue (the "+ Add Room" join flow's `Join` button not visibly completing
a connection in this build — sidebar `ROOMS` stayed "No rooms yet" across several careful,
coordinate-verified click attempts, despite the relay showing real bandwidth having flowed
during earlier session activity) blocked driving a full click-through repro. Rather than
spend further time debugging an unrelated frontend click issue, verification instead called
the real `#[tauri::command]` wrapper functions directly (`join_room`, `start_video_share`,
`stop_share`, `set_bandwidth_mode`, exactly what the frontend's `invoke()` calls reach) via
`tauri::test::mock_builder`, against a real relay server child process and real
xdg-desktop-portal screencast capture — same backend code path a real click exercises,
same real network/GStreamer/portal behavior, just invoked directly instead of through
rendered pixels. `driver.rs`'s own 250ms poll loop runs for real during these tests too
(reimplemented against `tauri::test`'s `MockRuntime` as `commands.rs`'s private `test_tick`,
since `driver::spawn`/`driver::tick` are hard-wired to the real `AppHandle<Wry<...>>` for
their `emit()` calls — `test_tick` mirrors everything else in `tick()` on the same 250ms
cadence). The frontend click issue itself was not investigated further — flagging it here
so the next worker doesn't waste time assuming it's this same backend bug resurfacing.

**2026-07-27 — later still — visual/UX polish pass against the real compiled app.**
Full critical audit driving the real compiled Tauri app (not the mock/Playwright path)
with `ydotool`/`grim` on the live desktop — two real client instances, a real relay, real
portal screen capture, real presence between them — through empty/populated/joined/
disconnected/reconnecting sidebar states, both bandwidth modes plus a real overshoot
banner, the leave-confirm modal, and a real watched stream on the View tab. Compared
throughout against the design-handoff screenshots for rounding/spacing/accent language.
Overall verdict: the shell reads as a real, finished product already — elevation, spacing,
the warning banner, the leave-confirm popover and the Discord/Slack-style dot/pill
vocabulary all match the handoff's intent closely, modulo the already-documented deliberate
monospace-driven deviations above. Three real bugs turned up and were fixed (all
frontend-only, no `.rs` touched); two more gaps were found and deliberately left alone.

*Fixed 1 — disconnected room names truncated to unreadable.* A disconnected row's head
line carried the name *and* the Reconnect pill *and* the forget button on one line; the
auto-generated fallback name (`Room <hex prefix>`, this doc's own "Auto-generated
saved-room names" note is the common case for any room joined without a custom
name) plus that pill left no room for the name, which ellipsized down to `Room 011309…`
— losing exactly the hex suffix that's the only way to tell two such rooms apart. Fixed in
`components.js`'s `renderRoomRow` (disconnected state only: Reconnect + forget now render
on the same line as "not connected" instead of the name's line) and a new
`.room-sub-meta-disconnected` rule in `shell.css`. Joined/reconnecting rows are unchanged.
Verified against the real app with two long real auto-generated names side by side.

*Fixed 2 — an expanded room's member roster went blank on the next presence update.*
`shell.js`'s `refreshRooms()` re-fetches bare `SavedRoomRow`s (no roster) on every
`presence-updated` event — which fires routinely, on real churn like someone else
joining/leaving — and re-renders immediately; an already-expanded room's `.members` from
the previous render doesn't exist on the fresh row objects, so the member list flashed back
to "No other members yet." even though nothing about the roster actually changed.
Reproduced with two real clients (one expanded, the other reconnecting) and confirmed it
did **not** self-correct on its own. Fixed by having `refreshRooms()` re-run
`loadRoomMembers()` for every room in `state.expandedRoomIds` before rendering.

*Fixed 3 — several settings toggles looked completely dead after a real, successfully-
applied click.* The bandwidth-mode pill switch, both Absolute-mode sliders, the room-rename
Save button, and the Settings tab's language pill switch each `await`ed their real
`invoke(...)` call but never updated the module-level local copy of the setting they'd just
changed — only the *mock* branch did. `stream-control.js`'s bandwidth card uses a
signature-based reconciliation (`JSON.stringify({ bandwidth, ... })`) to decide whether to
rebuild; since the real branch left `bandwidth` untouched, the post-click `render()` computed
an identical signature and silently skipped rebuilding the toggle — it looked like the click
did nothing at all. (`settings.js`'s language/presentation-mode row has no such
reconciliation gate, but the same missing update made it rebuild from stale data, which
reads identically to the user.) Confirmed this was a pure reactivity bug and not a rejected
backend call by relaunching the app after clicking Per-Stream Maximum with no other
refresh in between: the fresh `init()` on startup showed Per-Stream Maximum correctly
selected, proving the backend had accepted the change all along. Fixed by having
`setBandwidthMode`/`setAbsoluteTotal`/`setAbsoluteFloor`/`renameRoom` (in
`stream-control.js`) and `setLanguage`/`setPresentationMode` (in `settings.js`) write the new
value into local state on *both* branches, not just the mock one. Verified live: every one of
these now updates the instant it's clicked, and the mock/Playwright path (which already did
this correctly) is unaffected.

*Left alone 1 — the sidebar's per-room expand lists every member, including yourself, not
just "each other member" as this doc's own ROOMS section specifies.* Confirmed with two
real clients in the same room: expanding it from either side lists *both* members, self
included. Root cause: `shell.js`'s `loadRoomMembers` maps `RoomDetail.members` with no
self-filter, and `RoomDetail`/`dto::Member` (`src-tauri/src/dto.rs`) carries no
self-identifying `conn_id` for the frontend to filter against — there's no reliable
frontend-only way to know which row is "me". Correctly fixing this needs a small DTO
addition on the backend, which is a `.rs` change and out of this pass's scope; flagging it
here rather than guessing (e.g. matching on display name, which breaks the moment two
members share one) since a data-shape fix belongs with whoever next touches `dto.rs`.

*Left alone 2 — the Settings tab's language switch doesn't actually translate the frontend's
own UI text.* Clicking Deutsch does flip the toggle (now correctly, per the fix above) and
does call `set_language`, which changes `rust_i18n`'s locale for the Rust side — but no
string in `frontend/scripts/*.js` is ever looked up through a translation table, so every
label stays in English regardless. This is a real, pre-existing feature gap (frontend i18n
was never wired up when the tab was ported from egui), not a visual/CSS issue — building
out string translation for the whole frontend is a feature-sized piece of work, well beyond
a polish pass, so it's flagged here rather than attempted.

*Not reached — the tiered Rebuilding/Stalled destination-card visuals and the View tab's
stalled-tile placeholder.* Both render correctly from the mock data path (verified via
Playwright: `Gaming Night`'s `APPLYING…` badge, orange dot) but weren't reliably
triggerable against the real backend in the time available — the real stall protocol needs a
live resolution/fps change to land in exactly the right window, and repeated attempts
didn't land it. Left unverified against the real app rather than assumed fine; a future QC
pass with more time budget for retries should confirm the real-backend visuals match what
the mock path already shows.

Self-check: `cargo build --workspace` clean (same pre-existing warning count, unaffected —
pure frontend change). Mock/browser path re-verified after all fixes via Playwright against
a plain `python3 -m http.server` (no `window.__TAURI__`): bandwidth mode toggle, language
toggle, and all four sidebar room states (joined/expanded, joined/collapsed, disconnected,
reconnecting) all still render and respond correctly with the new name-truncation layout.

**2026-07-27 — later still — fixed: clicking Stop genuinely halted the pipeline but the
destination stayed stuck showing `LIVE` in the UI, sometimes for minutes.** Reported during
final regression QC, reproduced on two separate real app instances (one after Stop→Share
restart cycling, one on the very first-ever Stop click on a completely fresh app) — encoder
CPU dropped to 0% and the viewer's frame froze (the pipeline really did stop), but the
destination card never disappeared: repeated Stop clicks, waiting 8+ seconds, and even
switching tabs away and back did nothing, though on one instance it self-cleared after
several minutes of unrelated interaction.

The strong lead going in was that this was the same bug *class* as `471670b`'s
`start_pipeline`/`Element::state()` fix (a blocking GStreamer call running inline on an async
task, starving Tokio's small worker pool) but on the stop/teardown path instead — the prime
suspect being `capture::branch::remove_branch`'s drain wait or `CaptureHandle::stop`'s
`self.pipeline.set_state(gst::State::Null)` (a GStreamer downward state change, which is
specified as synchronous and can therefore block for an unpredictable time on real
hardware/portal contention, exactly like the already-fixed `Element::state()` wait). **That
hypothesis was investigated and ruled out, not assumed** — `eprintln!` breadcrumbs were
temporarily added at every await point in `CaptureHandle::stop`'s full call chain
(`destination.stop()`, `pipeline.send_event(Eos)`, `capture_task` join, `set_state(Null)`,
`preview_handle.stop()`, `portal.session.close()`) and exercised with a new diagnostic
harness (`src-tauri/src/commands.rs`'s `#[ignore]`d `stop_destination_actually_clears_from_state`/
`stop_reap_stress_cycles`, kept as permanent manual regression coverage, same shape as the
existing `hang_repro_*` tests but polling `AppState.captures` after `stop_share` returns
instead of just timing the command call itself — see `Why the existing hang_repro tests
couldn't have caught this` below) against a real relay and real portal capture: across more
than 15 real `stop_share` calls, `CaptureHandle::stop` — `set_state(Null)` included — never
took more than ~30ms end to end, and `AppState.captures` was reaped within one 250ms driver
poll tick (never more than ~300ms), every single time. The backend genuinely tears the
destination down and updates its own state almost immediately; the bug is not there.

`Why the existing hang_repro tests couldn't have caught this:` `hang_repro_stop_share_cycles`
and `hang_repro_bandwidth_toggle_then_stop` only assert that the `stop_share` **command call**
returns quickly — and it always does, because `session::AppState::stop_share` fires a
`tokio::spawn` and returns immediately (see `session.rs`'s own doc comment on it: "the
`LocalCapture` stays in the map, in `Stopping`, so the UI can show that state;
`poll_capture_states` reaps it once the stop lands"). Neither test ever checked whether the
reap actually *happened* afterward, which is exactly the thing this bug broke.

The real root cause was two separate, compounding gaps in `src-tauri/src/driver.rs`'s
`tick()` — the mechanism the whole Tauri port relies on to turn "`AppState` changed" into "the
frontend re-fetches" (see this module's own doc comment: "diffs a few cheap snapshots against
what it saw last tick, and emits a Tauri event for whatever changed"):

1. The per-destination diff loop only compares `label` for `destinationId`s **present in
   both** the last tick's snapshot and this tick's snapshot — it had no code path at all for
   "this `destinationId` was in the last snapshot and is gone from this one" (i.e. the
   destination was actually just removed from `AppState`). A key vanishing produces no
   iteration in a `for (id, dest) in &capture.destinations` loop, so it was silently
   invisible to a "did the value at this key change" comparison. Confirmed directly by
   reading the loop — there was no removal branch to reach a breakpoint in.
2. Even where `AppState` *would* otherwise get a second, independent nudge — `room.streams`
   shrinking (which separately drives a `presence-updated` event) once the relay round-trips
   the `StopStream` control message the destination's pump sends on teardown — that never
   fires either, for the exact client whose own stream just stopped: read directly in the
   relay's source (`simple-screensharing-server/src/relay.rs`'s `ClientToRelay::StopStream`
   handler), `guard.broadcast(&RelayToClient::PresenceDelta { .. }, Some(conn_id))` passes
   the stopping client's own `conn_id` as the *excluded* recipient — a deliberate,
   reasonable-on-its-own echo-suppression choice ("you already know your own stream
   stopped locally"), except this frontend's only mechanism for learning about it *is* that
   echo.

`Why "navigate away and back to the tab" didn't clear it either, contrary to what the QC
report assumed:` read directly in `shell.js`'s `setActiveTab` — switching tabs only toggles
`.tab-panel[hidden]`; it has never called `StreamControl.refresh()`/`init()`. The Stream
Control tab only ever re-fetches on `DOMContentLoaded` (first load) or in response to a
`presence-updated`/`destination-state-changed` event (`shell.js`'s `wireEvents()`) — both of
which, per the two gaps above, never fired for your own stopped stream. This also explains
"self-cleared after several minutes of unrelated interaction" on one instance: *some other*
real event (a different room's roster changing, another destination's state changing)
eventually fired one of those two events anyway, triggering a full refetch that incidentally
picked up the true, already-correct backend state — not the bug clearing itself, but an
unrelated event papering over it. `stream-control.js`'s keyed DOM reconciliation
(`destCardEls`) was checked too and is not at fault — it correctly drops a card whose
`destination_id` is missing from a fresh `list_active_streams` result; it just was never
being asked to.

**Fix**, entirely in `src-tauri/src/driver.rs`'s `tick()`: after building each tick's
`dest_snapshot`, a new pass finds every `destinationId` present in `last.destinations` but
missing from `dest_snapshot` and emits `destination-state-changed` for it with a synthetic
`"removed"` state label. This is safe because `shell.js`'s listener for that event ignores
the payload entirely and just re-fetches (`refreshStreams()` + `StreamControl.refresh()`) —
so the exact label doesn't matter, only that the event fires at all. `session.rs`/`capture.rs`
were not touched (the backend's own bookkeeping was already correct and fast); `relay.rs`'s
self-echo suppression was also left as-is (a reasonable design choice on its own, not a bug —
the frontend just can't rely on it as its only removal signal). `Why not also fix relay.rs to
echo `StreamStopped` back to the sender:` that would fix this one symptom but leaves the
underlying gap (the diff loop has no removal detection) unfixed for the next thing that
relies on it; the `driver.rs` fix is the smaller, more general, and more clearly-correct one —
`tick()`'s whole job is turning `AppState` diffs into events, and "a key disappeared" is a
diff it was simply missing a case for.

**Verified**, in order:
- Reproduced the original bug's root cause path *not* being what it looked like: 15+ real
  `stop_share` calls against a real relay + real portal capture, via the new
  `stop_destination_actually_clears_from_state`/`stop_reap_stress_cycles` harnesses, all
  showed `AppState.captures` reaped within ~300ms — ruling out a backend hang before writing
  the fix (see the root-cause section above).
- `cargo build --workspace` and `cargo test --workspace` clean (162 passed, 0 failed, same as
  before plus the two new `#[ignore]`d harnesses above), including the pre-existing
  `hang_repro_stop_share_cycles` regression test re-run against a real relay + real portal
  post-fix with zero hangs (same result as before this change — this fix doesn't touch any
  of that code path).
- **Real, live proof against the actual compiled Tauri app** (not mocked, not Playwright):
  launched via an isolated on-disk saved-room config, joined a real relay at `127.0.0.1:4433`,
  driven with real `ydotool` clicks and `grim` screenshots on the live desktop. Clicked
  **Share Screen → Stop 15 times in a row** (fresh `start_video_share`/`stop_share` cycle each
  time, real portal capture each time, no persisted permission — matching "several fresh
  shares" from the task, plus covering the "very first Stop on a fresh app" and "repeated
  Stop→Share restart cycling" repro conditions from the original report in one run): **every
  single one** of the 15 cycles cleared the destination card from `ACTIVE SHARES` back to "No
  active shares" within well under a second of the click (screenshotted after a fixed 400ms
  wait each time, consistently already cleared) — zero stuck cards, zero misses.

**The second reported symptom — "a viewer who was already watching keeps showing the OLD
stale frame after the sharer restarts" — was investigated and is `NOT reproducible in the
current Tauri frontend, documented here rather than guessed at`:** read `frontend/scripts/
view.js` in full, plus `commands.rs`'s `pop_out_stream`. Neither the View tab's tiles nor the
pop-out window (which just opens a second full copy of `index.html`, with a `?popout=1` query
string that no JS in the frontend ever reads) render a real decoded video frame at all yet —
every tile, mock or real, shows only a static CSS gradient placeholder
(`renderTile`'s `media-tile-mock-frame`, `view.js`'s own header comment: "decoding actual
frames onto this tile is the separate MJPEG-over-HTTP mechanism ... explicitly out of scope").
Grepped the whole client tree for `mjpeg`/`MJPEG`: the only hits are this same forward-looking
comment in `view.js` and `driver.rs`'s module doc — no such mechanism is implemented anywhere.
There is therefore no code path today by which a Tauri-frontend viewer tile could show a stale
*decoded frame*, because it never shows a real one in the first place. This symptom either
belongs to the older egui frontend (`src/ui.rs`, which does have a real decode pipeline —
`decode.rs` — and a real `media_frame` widget, per `APP-SHELL.md`) or is a real gap that will
only become reproducible once a future worker wires the MJPEG-over-HTTP (or equivalent) frame
path into `view.js` — at which point it should be re-tested for this exact staleness pattern
(a plausible root cause once that mechanism exists: an `<img>`/canvas element whose source
isn't explicitly cleared/reset on a `destination-state-changed`-style "this stream restarted"
signal, the same class of "nobody told the frontend to re-fetch/reset" bug as the fix above —
worth checking for specifically when that path is built). Left undone rather than faked; no
`.js`/`.rs` change was made for this second symptom.

**2026-07-27 — later still — resolved a QC/investigation discrepancy: the View tab shows NO
real decoded video, confirmed both by code reading and by a live two-instance test.** One QC
pass reported "verified with a pixel-diff test that decoded video frames actually change over
time (real video, not a placeholder)"; a later worker's grep-based finding (the paragraph
above) said the opposite. Re-investigated from scratch rather than trusting either report:

- Read `frontend/scripts/view.js`, `src-tauri/src/commands.rs`, `src-tauri/src/driver.rs`,
  `src-tauri/src/dto.rs` in full. Confirmed again: `renderTile` only ever appends a
  `media-tile-mock-frame` `<div>` with a static CSS `linear-gradient` background (`view.css`
  has no `@keyframes`/`animation` on that class — the *only* animated element in the file is
  the unrelated `.media-tile-spinner`, used solely by the `'stalled'` mock-demo state). No
  `<img>`/`<canvas>` element, no MJPEG/HTTP-serving command, no frame-serving endpoint anywhere
  in `commands.rs`/`driver.rs`/`dto.rs`. The later worker's finding was correct.
- **Live proof, not just code reading**: built `cargo build --workspace --release`, launched
  two real Tauri instances against a real relay (`127.0.0.1:4433`), joined the same fresh room
  from both (real `ydotool` clicks + clipboard-paste join links, real desktop), started a real
  portal screen capture from instance 1 ("Sender", went `LIVE` at `1920x1080@30fps`), and
  watched it from instance 2 ("Watcher")'s View tab. Screenshotted the resulting tile
  (`grim -g`) twice, 4 seconds apart, and diffed with Pillow: **`ImageChops.difference` bbox
  was `None` and max/mean pixel delta were both exactly `0`** — the tile is byte-for-byte
  identical across time. Real decoded video (even of a near-static desktop) would show at
  minimum JPEG-encode noise and cursor movement; a flat zero-diff over 4 real seconds is only
  possible for a static, non-video element. This directly falsifies the QC pass's pixel-diff
  claim — whatever that pass diffed, it wasn't two frames of the same live tile (the most
  likely explanation: the mock/demo `'stalled'` tile's CSS spinner *does* animate, and is easy
  to mistake for "video changing over time" if the diff wasn't scoped to a real, `'live'`-state
  tile specifically).
- **Verdict: the later worker's finding stands, confirmed with real end-to-end evidence, not
  just a grep.** The View tab renders a mock/placeholder gradient for every tile, real or mock,
  today. No code change was made for this finding by itself — see the next entry for why the
  follow-up "implement it" step was not attempted in this pass either.

**2026-07-27 — later still — did NOT implement the MJPEG-over-HTTP video-frame path; stopped
and flagged instead, because its premise doesn't check out.** The task that motivated the
investigation above also asked, if the gap were confirmed real (it was), to wire in "the
already-validated MJPEG-over-HTTP mechanism" — single-slot latest-frame mailbox, `socket2`
clamped send buffer, `turbojpeg` encoding, said to have been "rigorously validated earlier in
this port" at "30fps sustained, 3 concurrent streams" in `superdoc/brainstorm/
ui-redesign-multiroom-streaming.md`. That document was read in full for this pass: it contains
**no** mention of MJPEG, HTTP frame serving, `turbojpeg`, `socket2`, or any such prototype
anywhere — it's exclusively about the sidebar/multi-room UI redesign and an unrelated fps bug
(whose "Mailbox" is `wgpu::PresentMode::Mailbox`, a GPU present mode, not a frame-delivery
mailbox — a plausible source of the name collision). A broader search turned up nothing either:
no `turbojpeg` dependency in any `Cargo.toml`/`Cargo.lock` in this repo, no `video_server.rs` or
similarly-named module anywhere, no commit in `git log --all` (this repo or the sibling relay
server) ever touching MJPEG/turbojpeg/`x-mixed-replace`. The phrase "MJPEG-over-HTTP,
already-prototyped/explicitly out of scope" does appear in four places — this doc, `view.js`'s
header comment, `driver.rs`'s module doc, and `architecture/overview.md` — but every one of
them *asserts* it as established fact and links to no actual prototype; they all appear to
trace back to one original unverified claim that later workers (including the one who wrote
this doc's own earlier entries) repeated without re-checking, exactly the "doc that lies is
worse than no doc" failure mode this project's documentation discipline warns about. **Given
that, actually building the MJPEG path in this pass would mean designing a new subsystem from
scratch (HTTP server, JPEG encode strategy, mailbox wiring, Tauri command/DTO surface, frontend
`<img>` wiring, plus the sustained-playback and focus-loss/backlog regression tests the task
asked for) while writing it up as "reusing an already-proven design" — a false provenance this
doc should not record. That's a scope decision (design-from-scratch vs. wire-in-existing)
beyond what this pass was asked to make unilaterally, so it was intentionally left undone and
flagged here rather than either faked or silently expanded in scope.** No `.js`/`.rs` change
was made in this pass. `Why leave the four false "already-prototyped" claims in place rather
than delete them here:` deleting them would erase the trail for whoever picks this up next;
instead, each should be corrected to point at this entry the next time that specific file is
touched, so the correction lands with full context rather than as a drive-by edit.

**2026-07-27 — later still — built the MJPEG-over-HTTP video-frame path for real (new work,
not the fictional "already-validated" prototype the previous entry debunked); found and fixed
one real, pre-existing, unrelated bug it depends on; found and stopped at a second, deeper
pre-existing bug that still blocks real video from actually rendering end-to-end.** Full
mechanism and both `Why:` notes (the single-slot mailbox, the clamped send buffer) are
documented in `src-tauri/src/video_server.rs`'s own module doc — this entry covers what was
found and the current status, not the design itself.

- **Built:** `src-tauri/src/video_server.rs` (new module — raw `std::net::TcpListener`, one
  path-routed MJPEG stream per watched `(room_handle, owner, stream_id)`, single-slot
  generation-counted mailbox, `socket2`-clamped ~192KB send buffer, `turbojpeg` JPEG encoding
  of `StreamDecoder::latest_frame()`'s `egui::ColorImage` at ~60fps polling ceiling). Wired to
  `commands::toggle_watch` (starts/stops the route in lockstep with the watch itself, returns
  the URL) and a new `commands::stream_video_url` query command (lets `view.js` rebuild its
  tile list on reload without needing to have been the caller that started the watch).
  `frontend/scripts/view.js` now renders a real `<img>` pointed at that URL per real tile,
  falling back to the existing mock gradient only when `window.__TAURI__` is absent.
  `libturbojpeg` was confirmed present via `pkg-config --exists libturbojpeg` (3.2.0) per the
  task's instruction; `TURBOJPEG_SOURCE=pkg-config` was pinned in a new `.cargo/config.toml`
  because `turbojpeg-sys`'s default (build libjpeg-turbo from source) needs a NASM assembler
  this machine doesn't have — a build-environment fix, not a design substitution.
- **Found and fixed (real, pre-existing, blocking bug #1 — u64 IDs over the JSON boundary):**
  the very first live two-instance test produced a decoder that polled forever and never saw a
  frame. Root cause: `owner`/`conn_id` (`ConnId` = `u64`) crossing `window.__TAURI__.core.invoke`
  as a bare JSON number silently loses precision above `2^53` (IEEE-754 `f64`) — `toggle_watch`'s
  `owner` came back a *different* `u64` than the one `MediaFrame`s actually arrived tagged with,
  so `spawn_media_ingestion_task` could never find the watch entry to feed. This exact class of
  bug was already flagged as a known limitation for the *mock* path
  (`stream-control.js`'s `MOCK_MEMBERS_BY_ROOM` comment, "not something fixable here") but had
  never actually been hit/fixed on the real path before, because nothing had ever exercised real
  decode under Tauri until this pass. Fixed by sending `owner`/`conn_id` as lowercase-hex strings
  instead of raw numbers across the whole Tauri JSON boundary — see `src-tauri/src/dto.rs`'s
  `hex_u64` module doc for the full detail and every touched call site
  (`commands.rs`'s `toggle_watch`/`toggle_listen`/`set_volume`/`pop_out_stream`/
  `stream_video_url`, `dto.rs`'s new `MemberDto`/`StreamInfoDto`/`StreamKey`, and
  `view.js`/`stream-control.js`'s corresponding read sites). Confirmed fixed: after this change,
  `MediaFrame`s reliably route to the correct decoder (verified via temporary diagnostic logging,
  removed before this pass ended) — hundreds of consecutive frames all landing on the
  watch entry, zero misses, across several fresh two-instance runs.
- **Found, then investigated further later the same day and confirmed NOT to reproduce
  (originally logged as blocking bug #2 — no watcher who subscribes after a share has
  already started ever receives a decodable keyframe):** even with bug #1 fixed,
  `StreamDecoder::latest_frame()` never produced a single frame in any live test — because,
  confirmed via the same temporary diagnostic logging (checked *every* `MediaFrame`, not
  sampled), **not one keyframe was ever observed on the wire**, across 5 independent
  two-real-instance runs (real relay, real portal capture, both a fresh room each time and a
  reused one), totaling several thousand consecutive frames and multiple minutes of runtime.
  This is upstream of `video_server.rs` entirely — plain H.264, no watcher would ever be able to
  decode this either. Traced as far as: the relay *does* send a debounced
  `RelayToClient::KeyframeRequest` on first subscribe (confirmed by relay bandwidth counters
  showing real sustained multi-MB/s traffic, and is separately unit-tested on the server); the
  sender's `drain_events` *does* receive `ClientEvent::KeyframeRequest` and find its
  `capture_feeding` destination (confirmed via temporary logging: `capture_feeding found=true`);
  `capture.rs`'s `DestCmd::KeyframeRequest` handler *does* run and calls `force_keyframe()` on a
  present branch/pad (confirmed via temporary logging: `branch_present=true`); `force_keyframe`'s
  `UpstreamForceKeyUnitEvent` is accepted downstream without the pipeline's own
  "event was not accepted" warning ever firing. And yet the next buffer out of the encoder is
  never actually `DELTA_UNIT`-free on the wire. Reproduced identically with
  `E2E_FORCE_BACKEND=software` (forcing `x264enc` instead of `vah264enc`), ruling out a
  VAAPI-specific cause — this is not a hardware-encoder quirk. `capture.rs`'s own
  `encoder_valve_gates_output_and_force_key_unit_event_forces_a_keyframe` unit test claims this
  exact mechanism works, in isolation; something about the real multi-process, real-network,
  already-running-for-a-while integration breaks it, and pinning down what specifically (pad
  identity after a branch swap? a race between `refresh_route` and the keyframe event? the
  relay's own fragment-forwarding dropping/misrouting the keyframe fragments specifically?) is
  its own investigation, squarely in `capture.rs`/`relay.rs` territory and unrelated to the
  frame-*delivery* mechanism this pass was scoped to build. **Stopped here rather than
  improvising a fix into unfamiliar pipeline internals** — this is the "genuine architectural
  rework / real pre-existing bug in unrelated code" case this project's worker instructions say
  to flag rather than grind on solo.
  **Update, later the same day: investigated and confirmed NOT to reproduce.** A follow-up
  investigation ran 12 real end-to-end tests across both encoder backends (`x264enc` software,
  `vah264enc` hardware), synthetic and real `xdg-desktop-portal` capture, and small and large
  keyframes. A watcher subscribing well after a share has been running (past a GOP boundary)
  reliably receives a real keyframe within 20-70ms every time — `force_keyframe()`'s existing
  mechanism in `capture.rs` works correctly. No production code needed to change; the original
  diagnostic trail above turned out not to generalize (root cause of that specific run's
  zero-keyframes observation was not pinned down, but it does not reproduce against current
  code). Locked in as permanent regression coverage:
  `late_subscriber_after_unwatched_period_gets_a_real_keyframe` and
  `late_subscriber_after_unwatched_period_gets_a_real_keyframe_real_portal` in
  `src/end_to_end_test.rs`. `video_server.rs`'s module doc has been corrected to match (it
  previously cross-referenced this entry as an open bug).
- **Consequently NOT completed:** the "genuinely live video, confirmed by screenshot" and
  focus-loss/backlog self-checks the task asked for. Everything *upstream* of a real keyframe
  (HTTP serving, multipart framing, mailbox, clamped buffer, JPEG encoding, the whole Tauri
  command/DTO surface) was verified directly and independently of bug #2 — e.g. `curl`ing a
  route's URL directly returns the correct `HTTP/1.1 200 OK` / `multipart/x-mixed-replace;
  boundary=frame` headers the instant a watch starts, matching a route's token exactly — so
  `video_server.rs` is not itself unverified, just unable to prove the visual end-to-end result
  until bug #2 is fixed elsewhere. `cargo build --workspace` / `cargo test --workspace` are both
  clean; the mock/browser Playwright path (`window.__TAURI__` absent) still renders its
  gradient-placeholder tiles with zero console errors, confirmed via a live Playwright run
  against a local static server for `frontend/`.
- Two of the four false "already-prototyped" claims flagged by the previous entry were touched
  and corrected in this pass (this file's own gap note above, and `driver.rs`'s module doc); the
  other two (`view.js`'s original header comment — since rewritten entirely — and
  `architecture/overview.md`) are corrected in this same pass's doc updates there.

## Tab content

Built at `frontend/scripts/stream-control.js` / `view.js` / `settings.js`, styled by
`frontend/styles/stream-control.css` / `view.css` / `settings.css`, and mounted into
`index.html`'s three `<section class="tab-panel">` root `<div>`s
(`#stream-control-root`, `#view-root`, `#settings-root`). All three now call the real
`#[tauri::command]`s in `src-tauri/` when `window.__TAURI__` is present (see the
"real backend wiring" update above), and still fall back to mock data in a plain
browser.

### The mock/real fallback pattern

Every data touchpoint in these three files (and this applies to any future frontend
work too) is a small `async function` shaped like:

```js
async function fetchActiveDestinations() {
  if (window.__TAURI__) {
    const rows = await window.__TAURI__.core.invoke("list_active_streams");
    rows.forEach((d) => { d.room_label = /* looked up by room_handle */ ""; });
    return rows;
  }
  return MOCK_DESTINATIONS.slice();
}
```

— a real `invoke(...)` call on one branch, an in-memory mock fallback on the other (this
particular example now calls the real `list_active_streams` command — the name/shape
this file originally guessed, `list_active_destinations`, didn't match the real command
surface; see the "real backend wiring" update above). Write-y actions
(editing a destination, starting a share, renaming a room, flipping a settings toggle)
follow the identical shape: the mock branch mutates local state directly and calls
`render()`, so the UI stays interactively alive with zero backend present.

`Why:` this is a hard requirement, not a nicety — the whole point of building against
mock data first is that a plain browser (no Tauri shell at all) can render and be driven
by Playwright for fast visual iteration, and that has to survive **after** a later
worker fills in the real `invoke()` calls, not just during this build. Writing the
branch structure now (rather than a bare mock array + a `// TODO` comment, which is what
`shell.js`'s stage-1 pass did) means the next integration pass only ever fills in one
branch's body — it never has to restructure a call site to add the real path. Verified
by reloading with `window.__TAURI__` undefined (its actual state in every plain-browser
Playwright run) and confirming every button, slider, and toggle across all three tabs
still works.

### Stream Control

Fixes the specific problem flagged in the old egui tab's design critique (see
`superdoc/brainstorm/ui-redesign-multiroom-streaming.md` and the design-handoff
screenshots at `04a`/`04b`/`09`/`10` in the reviewed bundle): Active Shares rendering as
a tiny stub buried after two big forms (Bandwidth, Start a Stream), in a stack of
content-sized cards of visibly different widths. The new layout:

1. **Bandwidth** — a *compact* strip: the mode toggle + a one-line summary
   (`N destinations · streaming to N rooms — about Nx bandwidth`) up top, that mode's
   numeric controls (sliders, not a big form) below, and the overshoot warning banner
   (`Components.warningBanner`, new — mirrors `theme::warning_banner`) when Absolute
   mode's floors don't fit the total.
2. **Active shares** — the prominent card, right after Bandwidth: an equal-width grid
   (`.sc-active-shares-grid`, CSS `auto-fit`/`minmax`) of destination cards
   (`Components.destinationCard`, new), each showing a status dot + state badge (`Live` /
   `Applying…` for Rebuilding / `Waiting for capture…` for Stalled / `Starting` /
   `Failed`), inline bitrate edit (Per-Stream mode only — Absolute mode shows the
   allocator's computed share instead, read-only), and Transfer / "Also send to…"
   (fan-out) / Stop actions.
3. **Start a stream** — a compact inline form (room picker over joined rooms, framerate,
   bitrate in Per-Stream mode only), not a tall vertical stack.
4. **Room-scoped row** — Members/rename and Share Audio as two equal-width cards, kept
   visually separate from the cross-room Active Shares list via their own row and a
   "Room-scoped settings for: [room picker]" header, per the redesign brief's "room-scoped
   content kept separate from the cross-room destination list."

Every card in the tab uses the new generic `Components.card(label, children)` /
`.card` CSS class rather than the old `.tab-placeholder-card` (which capped itself at
`--panel-max-width` — exactly the content-sized-card shape being fixed here); `.card`
has no width opinion of its own, so equal-width sizing is entirely the caller's grid's
decision (`.sc-grid` is a single-column flex stack, so every card is the same width).

**New shared primitives added to `components.js`:** `card`, `warningBanner`,
`pillSwitch` (generic two-option exclusive switch), `bandwidthModeToggle` (a
`pillSwitch` wrapper for the Absolute/Per-Stream Maximum pair), `destinationCard`. All
follow the existing factory-function style: pure DOM builders wired only to the
callbacks they're given, no dependency on any tab's own script.

**Mock data:** 3 destinations across 3 rooms — Movie Night (`Live`), Gaming Night
(`Rebuilding`/"Applying…"), Book Club (`Stalled`/"Waiting for capture…") — with Movie
Night and Book Club sharing a `captureId` (a pre-existing fan-out) so the "rooms this
capture already feeds" exclusion rule is visible without any clicks. Bandwidth mode
defaults to **Absolute** with a total (8000 kbps) and floor (3000 kbps) that overshoot by
1000 kbps — a deliberate deviation from the real default (`stream-control.md`: Per-Stream
is the production default), made purely so the overshoot banner is visible on first load
without an extra click in a Playwright review. Toggling to Per-Stream works and hides it,
as expected.

**Deliberate simplifications vs. the full feature doc:** Transfer and fan-out are
mocked as a fixed-delay state machine (new destination appears `Starting` with a
pending-role label, flips to `Live` and — for Transfer — removes the source after ~1-1.2s)
rather than driven by any real IDR/branch-readiness signal, since there's no real capture
here to wait on. The bandwidth allocator is a simplified even-split-with-floor
calculation, not the real weighted `bandwidth::allocate`.

### View

An auto-tiled grid (`.view-grid`, same `auto-fit`/`minmax` approach as Active Shares) of
mock watched-stream tiles, chrome-free per `APP-SHELL.md`'s `theme::media_frame` note
("letterboxed against PANEL_SUNKEN, no card border or title, because a video surface
must never read as a settings widget") — `.media-tile` has no card border, just a sunken
background. Each tile has a top overlay (owner/room/resolution label + a pop-out icon
button) and either a mock gradient standing in for a decoded frame, or — for one tile in
`stalled` mock state — a spinner + "Waiting for capture…" placeholder.

`Why the tiered restart placeholder is simplified:` the full design (per
`ui-redesign-multiroom-streaming.md`) is a 3-tier placeholder (nothing visible under
800ms, a spinner from 800ms-6s, a firmer message past 6s). This build renders only the
single "spinner + message" placeholder state for a mock-stalled tile — there's no real
capture-restart signal to time against in a mock-only build, and building the full timer
machinery against fake data would be simulating a feature rather than proving the visual
states. The three visual states (live tile, stalled-placeholder tile, empty grid) are all
present and reachable, which is what a visual/interactive review needs; wiring the real
tiering is stage-3 backend work.

The empty state (no watched streams) defaults to **not** shown — mock data starts
non-empty so the interesting tiled layout is visible on load — but is reachable via a
`dev:`-prefixed toggle button (`.view-demo-toggle`), the same pattern `shell.js`
established for the sidebar STREAMS section's empty/populated states.

### Settings

Confirmed working, no gaps found: the language pill-switch (English/Deutsch, now via
the shared `pillSwitch` primitive) and the presentation-mode dropdown, on the new
`.card` primitive. Per `APP-SHELL.md`'s own note, this tab needed the least work — no
settings were added, removed, or redesigned, only relocated onto the shared card
primitive and wired to the mock/real fallback pattern.

## What's here

- `frontend/index.html` — the shell markup: sidebar (header, nav, ROOMS,
  "+ Create/Join a Room", STREAMS) + a right content area with one
  `<section class="tab-panel">` per nav tab.
- `frontend/styles/tokens.css` — **the single source of truth for design tokens**,
  ported 1:1 from `src/theme.rs`'s color/spacing/rounding constants, plus new
  sidebar-specific elevation/wash tokens from the design handoff (not yet in
  `theme.rs`). Every other frontend file — this shell's own CSS, and later Stream
  Control / View / Settings styles — should reference these custom properties by role
  (`var(--accent)`, `var(--rounding)`, …), never a raw hex/px value. If `theme.rs`
  itself changes, port the change here in the same commit (and vice versa) so the two
  don't drift.
- `frontend/styles/fonts.css` — local `@font-face` declarations (see "Fonts" below).
- `frontend/styles/shell.css` — sidebar/nav/rooms/streams layout rules.
- `frontend/scripts/components.js` — shared UI primitives (`window.Components`):
  `renderNavButton`, `statusDot`, `renderRoomRow`, `renderStreamRow`, `renderEmptyState`,
  `primaryButton`/`secondaryButton`/`dangerButton`/`iconButton`, plus (added building
  the tab content below) `card`, `warningBanner`, `pillSwitch`, `bandwidthModeToggle`,
  `destinationCard`. Mirrors `theme.rs`'s widget-helper pattern (`primary_button`,
  `nav_button`, `status_dot`, …) — reuse these rather than hand-rolling button/card/dot
  markup. Pure with respect to the DOM: each factory returns an element and wires only
  the callbacks it's given: no global state, no dependency on any tab's own script.
- `frontend/scripts/shell.js` — the shell's own behavior: nav tab switching, ROOMS/
  STREAMS rendering, room-row expand/collapse, the "+ Create/Join a Room" inline card
  (Join/Create `pillSwitch` tabs — see `APP-SHELL.md`'s 2026-07-28 entry), and the
  leave-confirm modal, against real `#[tauri::command]`s (`list_saved_rooms`,
  `list_active_streams`, `join_room`, `create_room`, `leave_needs_confirm`/`leave_room`/
  `force_leave_room`, `toggle_watch`, `room_detail`) with a mock fallback per file. Also
  the one place every Tauri event listener is registered (`wireEvents`, in `init`) —
  the sidebar's own state reacts directly; `stream-control.js`/`view.js` expose a small
  `refresh()`/`setOvershoot()` this module calls into rather than each tab duplicating
  its own `listen()` calls.
- `frontend/scripts/stream-control.js` / `view.js` / `settings.js` +
  `frontend/styles/stream-control.css` / `view.css` / `settings.css` — the three tabs'
  real content. See "Tab content" above for what each renders and the mock/real
  fallback pattern they all follow.

No `.rs` file, `Cargo.toml`, or `src-tauri/` content was touched building the original
shell/tab content, nor by the later real-backend-wiring pass (frontend-only scope both
times) — the frontend is still a standalone static site (open `frontend/index.html`
through a local HTTP server — see "Local preview" below — for the mock path) that a
real Tauri scaffold (`src-tauri/`, see `commands.rs`/`dto.rs`/`driver.rs`) now also
serves for the real path.

## Fonts

**2026-07-29 — follow-up fix: snapped every remaining `font-weight: 500`/`600` site to a
real vendored weight (400 or 700).** The 2026-07-28 overhaul below switched the app to
Geist, but only vendored two static weights (400/700) and only re-pointed a few sites
(`<h1>`, nav-pill labels, `.tab-panel h2`) at 700 — roughly 15 other sites across
`settings.css`, `status.css`, `shell.css`, `stream-control.css`, and `view.css` were left
at `font-weight: 600` (a couple at `500`), which the browser can't render as a real Geist
face and instead synthesizes (typically fake-bolding the 400 face), visibly blurrier at
this shell's small (10–15px) UI text sizes. Reported as "font rendering looks weirdly not
sharp in some areas" — confirmed by zoomed real-app screenshots (Playwright `_electron`)
comparing a 700-weight site (crisp) against a 600-weight site (visibly softer/haloed
edges) before touching anything; see before/after crops of `.settings-row-label`
("Max jitter buffer"), `.settings-row-value` ("3.5s"), and `.room-name` ("Test") for the
clearest examples.

Fix, applied per-site by what the element actually is (not a blanket substitution):

- **Sans (`--font-sans`) contexts → 700** (loud labels/headlines/buttons, matching the
  app's existing "emphasis means bold" pattern — `.mode-toggle-btn-active`, `.nav-btn`):
  `.settings-row-label`, `.status-failed-headline`, `.empty-state-headline`,
  `.dest-room-label`, `.sc-ghost-row`, `.room-name`, `.ghost-add-room-row`, `.btn` (the
  shared button primitive), `.room-panel-title` + `.room-panel-title-input` (kept in sync
  so edit-mode doesn't shift weight).
- **Sans contexts → 400** (quiet/secondary text, or a toggle's idle state that already
  gets its emphasis from a real-700 active state): `.mode-toggle-btn` (idle; `
  .mode-toggle-btn-active` was already real 700 — no need for idle to also carry a
  synthesized middle weight), `.view-room-filter-chip`, `.media-tile-nameplate`.
- **Mono (`--font-mono`) contexts → 400, not 700**: `.settings-row-value`,
  `.sc-bandwidth-field-value`, `.dest-metric-value`, `.dest-bitrate-value`. Important
  distinction from the sans fix above — Geist Mono only vendors the Regular (400) weight
  (see below), so bumping these to 700 would still synthesize; 400 is the *only* real face
  available in mono context, regardless of how loud the value is meant to read.
- **Deliberately left alone**: `viewer.css`'s `.viewer-tile-nameplate` (still
  `font-weight: 500`) — that pop-out window's `body` rule sets
  `font-family: system-ui, sans-serif`, not Geist (see the "left on its own stack" note
  below), so its 500 renders against the OS system font, not a synthesized Geist weight —
  a different, separately-scoped situation this pass didn't touch.

**2026-07-28 — font overhaul: Geist (sans) is now the app default; Geist Mono is
reserved for "value" text.** Originally `body { font-family: var(--font-mono) }` made
Geist Mono the font for literally everything (all body text, buttons, inputs, since
`button, input { font-family: inherit }`) — wrong, monospace should only be used where
proportional-width digits would visually shift a layout. Two vendored files that had sat
unused since an earlier pass (`frontend/fonts/Geist-Regular.ttf` /
`Geist-Bold.ttf`) supplied the fix:

- **Geist** (`--font-sans` in `tokens.css`) is now `body`'s font, hence the default for
  the whole shell. Registered as one family, two static weights — 400
  (`Geist-Regular.ttf`) and 700 (`Geist-Bold.ttf`) — so `font-weight: 700` text (the
  sidebar brand `<h1>`, nav-pill labels, `.tab-panel h2` page titles — the last bumped
  from 600 to 700 specifically so it picks up the real Bold face) gets a true bold glyph
  instead of the browser's synthetic fake-bold.
- **Geist Mono** (`--font-mono`, unchanged token) is now scoped to "value" text only —
  numeric/tabular readouts that change at runtime and would shift with a proportional
  font: resolution, framerate, bitrate, timers, jitter-buffer ms, IDs, and join-link
  (`server|room-id|key`) strings. Classes carrying `font-family: var(--font-mono)`:
  `.status-failed-sub`, `.stream-meta`, `.media-tile-fps`, `.dest-metric-value`,
  `.dest-computed-share`, `.dest-fps-readout`, `.dest-bitrate-value`,
  `.sc-bandwidth-field-value`, `.settings-row-value`, and `.field-input-mono` (a
  modifier class added to just the two join-link `<input>`s — `shell.js`'s
  `renderRoomsEmptyState` and `renderAddRoomCard` — not the generic `.field-input` every
  other text field also uses). Only the Regular (400) weight is vendored for Geist Mono;
  `font-weight: 700` mono text still falls back to synthetic bold — unchanged from
  before, still acceptable per the original note below.
- `viewer.html`'s standalone pop-out page (`viewer.css`) was deliberately left on its own
  `system-ui, sans-serif` stack — out of scope for this pass, flagged rather than changed
  since it wasn't asked for and the page is simple enough that the inconsistency is low-
  stakes; worth revisiting for consistency later.

The design handoff calls for Geist Mono (body/UI) + Material Symbols Outlined (icons)
via Google Fonts CDN, which doesn't work offline or under a CSP-locked Tauri build. All
three families are vendored locally into `frontend/fonts/` instead:

- **Geist / Geist Mono** — `Geist-Regular.ttf`/`Geist-Bold.ttf` and
  `GeistMono-Regular.ttf`, copied from a local asset cache
  (`ritz/resources/assets/…`) after a coordinator pointed at it —
  this file's first pass had already fetched a variable-weight woff2 directly from
  Google Fonts (network access was available), which worked but was replaced with this
  local copy to avoid a live download step. Geist Mono has only the Regular (400) weight
  available; elements styled `font-weight: 700` in mono context get the browser's
  synthetic bold rather than a true bold face — swap in a real Geist Mono Bold file
  later if that ever looks off. Geist (sans) has both weights vendored, so bold sans
  text always gets a true bold glyph.
- **Material Symbols Outlined** — `MaterialSymbolsOutlined.woff2`, fetched from
  `fonts.gstatic.com` at build time and vendored as-is (kept over the local asset
  cache's `GeistMonoNerdFont-Regular.otf` alternative: a Nerd Font requires mapping each
  icon to a specific codepoint by hand, with no way to confirm the mapping without a
  render pass — see `APP-SHELL.md`'s own "Icon glyphs" note on how easily that goes
  wrong with egui's bundled fonts; not worth the risk for the handful of icons this
  shell needs when the ligature-based Material Symbols font already renders correctly).
- `frontend/fonts/LICENSES/` carries both fonts' license terms (SIL OFL for Geist,
  Apache 2.0 for Material Symbols) alongside the files they cover, per each license's
  redistribution terms.
- `--font-sans`/`--font-mono` in `tokens.css` each fall back to a system stack
  (`ui-sans-serif, system-ui, sans-serif` / `ui-monospace, "JetBrains Mono", "Cascadia
  Code", Consolas, monospace`) if the vendored font ever fails to load.

## Known deliberate deviations from the design handoff

- **Room-row state body text moved to its own line for `disconnected`.** The handoff's
  mockup (a proportional font) fits `<name> not connected [Reconnect] [x]` on one line
  at the sidebar's width. Geist Mono is a monospace face — much wider per character —
  and the same text on one line squeezed the room name down to nothing (down to a
  single visible letter in testing). `not connected` now renders on its own indented
  sub-line under the name instead. `reconnecting` never had this problem once fixed:
  per `APP-SHELL.md`'s row-state table, that state's body text is `—` (none) — the
  pulsing dot alone carries the state, so there was no long string to squeeze the name
  in the first place.
- **Sidebar width bumped to 288px** (from the handoff's ~268px) for the same
  monospace-width reason.
- Icons are Material Symbols Outlined ligatures (`expand_more`, `chevron_right`,
  `close`, `play_arrow`, `volume_up`), not the exact glyph technique `theme.rs`/`ui.rs`
  use (painted discs for the status dot, restricted Unicode carets for expand/collapse).
  The status dot itself *is* still a painted/CSS shape (`.status-dot-*` in `shell.css`),
  matching `theme::status_dot`'s reasoning — a font glyph can't give an exact,
  font-independent pulse animation.

## A layout bug worth knowing about (fixed, but a trap for later CSS edits)

`.add-room-card` (and any other element whose class sets its own `display` and is also
toggled via the `hidden` attribute) can silently stop hiding: `[hidden] { display: none
}` is a UA-stylesheet rule with the same specificity as a class selector, so a later
stylesheet's `.foo { display: flex }` overrides it by load order, not by being "more
specific." Every such element needs an explicit `.foo[hidden] { display: none }`
override alongside its own display rule — `shell.css` has one for `.add-room-card` and
one for `.modal-overlay`; add a matching rule for any new toggle-shown element that also
sets its own `display`.

## Local preview

`frontend/index.html` uses `<link>`/`<script src>` with relative paths, which most
browsers block under `file://` (and Playwright refuses `file://` navigation outright).
Serve it over a local HTTP server instead, e.g. `python3 -m http.server` from inside
`frontend/`, then open `http://localhost:<port>/index.html`.

**2026-07-27 — later still — `video_server.rs`'s MJPEG-over-HTTP path replaced with a
direct H.264 relay** (backend-only change; `frontend/` itself untouched by this pass —
a separate worker builds the WebCodecs consumer against the new contract). The
decode-with-GStreamer-then-JPEG-re-encode round trip described in the entry above this
one is gone: `video_server.rs` now relays the raw Annex-B H.264 access units `net.rs`
already hands `session.rs` straight over the wire, length-prefixed, for the browser to
decode itself with WebCodecs' `VideoDecoder`. `toggle_watch`'s returned URL and
`stream_video_url`'s shape are unchanged — only what's served at that URL changed. Full
wire format, the codec-string discovery mechanism (parsed from the first keyframe's SPS
NAL), and the `Why:` reasoning are documented in `video_server.rs`'s own module doc; see
`architecture/overview.md`'s matching 2026-07-27 entry for the shorter summary. The
no-keyframe-on-late-subscribe concern from the entry above was investigated further the
same day and confirmed not to reproduce (see that entry's later-the-same-day update) — it
is not a blocker for real end-to-end video.

**2026-07-28 — the WebCodecs consumer landed, jitter buffer added, and a client-side
fullscreen/borderless viewer built (frontend catches up to the H.264 relay above).**

The "separate worker builds the WebCodecs consumer against the new contract" gap named in
the entry above is closed: `frontend/scripts/tile-video.js` (extracted out of `view.js` so
it could be shared with the new viewer window — see below) parses the H.264-over-HTTP wire
format and drives a real `VideoDecoder`, rendering onto a real `<canvas>` per tile. Every
claim earlier in this document that a tile shows "only a static CSS gradient placeholder"
is now historical, not current (see the correction note inline above, near this file's
"known gap" paragraph). A client-side jitter buffer (RFC 3550-style EWMA, buffers
still-encoded chunks rather than decoded frames, ceiling sourced from a new
user-configurable `max_jitter_buffer_ms` Settings-tab slider) was added the same day. Full
design, wire format, and the encoder-tuning/codec-string interaction are documented in
`superdoc/features/video-pipeline.md` — not duplicated here.

**Electron migration.** The app shell this whole document describes moved from a Tauri
webview to an Electron `BrowserWindow`. At the frontend-code level this was close to a
non-event: every file below still calls `window.__TAURI__.core.invoke(...)`/
`window.__TAURI__.event.listen(...)` exactly as documented throughout this page — a new
`electron/preload.js` rebuilds that same `window.__TAURI__` shape on top of Electron's
`ipcRenderer`/`contextBridge`, and `electron/main.js` forwards `invoke` calls to the Rust
sidecar's new `POST /command/<name>` REST endpoint and relays its `GET /events`
Server-Sent-Events stream back out as the same named events `wireEvents()` already
listened for. See `superdoc/architecture/overview.md`'s "Process model" section for the
full two-process architecture and the `Why:` behind keeping the Tauri command surface
alive underneath a new transport rather than rewriting it. One genuinely new,
Electron-only bridge was added alongside `__TAURI__`: `window.__ELECTRON__`, for window
management that has no Tauri equivalent — see the next entry.

**Fullscreen/borderless multi-window viewer.** `frontend/viewer.html`/
`frontend/scripts/viewer.js` is a new, separate, focused page (not part of this shell's
tabbed layout) that Electron opens in its own `BrowserWindow`, showing one or several
watched-stream tiles fullscreen or borderless-maximized — triggered from the View tab's
per-tile selection checkboxes plus "Fullscreen selected"/"Borderless selected" buttons.
Reuses `tile-video.js`'s decode pipeline rather than re-implementing it. Full design
(the `window.__ELECTRON__` IPC surface, the query-string handoff, the tiling-Wayland-WM
mitigations found and fixed against a real compositor) lives in
`superdoc/features/viewer-window.md`, not duplicated here.

**2026-07-28 — later still — visual redesign against the "Stream Control Redesign
Options" design handoff, on top of the already-built shell/tab content above (no `.rs`
change).**

A second design-handoff pass restyled the shell and both real tabs. In commit order:
a shared 4-family status vocabulary (`frontend/styles/status.css`,
`Components.statusBadge` — `progress`/`live`/`degraded`/`failed`, matching
`capture::DestinationState`'s real variants, each family visually distinct enough to read
without reading the label text — see `status.css`'s own header comment for the full
per-family rationale, e.g. why `live` is a deliberately *static*, non-animated dot);
restyled empty states and the leave-confirm modal onto that same vocabulary; the sidebar's
outgoing-destinations section restyled and re-labelled **"Outgoing"** in the UI (per
option 1c — the DOM ids/JS variable names stayed `streams`/`STREAMS` throughout, a
label-only rename that deliberately didn't churn working selector/variable names); the
Stream Control tab's destination cards restyled to option 1a (status accent bar, a
sparkline fed by `stream-stats-updated`, room chips); an expanded room's detail moved from
an inline expand-in-place row to a **slide-over panel** (`shell.js`'s room-detail
slide-over, option 1f), fixing two small pre-existing bugs surfaced while building it
(the panel double-listing the viewer's own self as a member, and a leave-modal member-count
miscount); and the View tab restyled as the **"Watch grid"** (option 1e — computed grid
columns rather than a fixed layout, hover-overlay tile chrome, and **held-slot
placeholders**: a tile sent to a pop-out or the fullscreen/borderless viewer window keeps
its spot in the grid, labelled "in a separate window" / "fullscreen" / "borderless" rather
than just disappearing — `view.js`'s `poppedOutIds`/`viewerHeldIds`).

This pass also fixed a real, previously-shipped bug found during it: the nav's active-pill
indicator didn't visually transition on a tab switch (a CSS transition that was a no-op
because the property it animated never actually changed value between the two states) —
fixed in `shell.css`/`shell.js`, no behavioral change to tab switching itself.

Verified via Playwright screenshots against the mock/no-backend path across all three tabs
and every documented sidebar/destination/tile state (joined/reconnecting/disconnected room
rows, all four status-badge families, the room-detail slide-over open/closed, and the Watch
grid's held-slot placeholders) — not verified against the real compiled app with live
`ydotool` clicks the way several 2026-07-27 entries above were; a future QC pass following
that same real-app methodology against this redesign is worth doing but wasn't part of this
pass's scope.

---

**2026-07-28 — later still — design-fidelity correction: the redesign above shipped the
right structure with the wrong surface.**

The pass documented immediately above was reviewed by the project owner, who reported:
*"check if the design was implemented fully. I'm missing some settings and borders on the
stream. It looked way more futuristic in the mockup. Also, some of the colors are not
right."* An earlier structural-only audit had passed the redesign clean. Both are correct
and that is the interesting part — the failure mode is worth recording, because a
structural check cannot catch it by construction.

**Root cause: the port used the wrong source of truth.** `tokens.css` described itself as
"a 1:1 CSS translation of `src/theme.rs`", and it was — but `theme.rs` is the palette of
the *old* egui client, i.e. the thing the redesign was commissioned to replace. The
handoff's `shots/*.png` compound this: they sit in the same folder as `theme.rs` and
`ui.rs` and are screenshots of the **old app** — the designer's *input*, not the mockup.
The actual design is the HTML prototype, `Simple Screensharing Redesign.dc.html` (option
**1i**, "Clickable — full window at 1440×900", is the assembled screen). Porting colours
from `theme.rs`/`shots` reproduced the old greys underneath the new layout:

| role | shipped (old `theme.rs`) | prototype |
|---|---|---|
| `--panel-base` (page) | `#1E1F22` | `#1B1C1F` |
| `--panel-sunken` | `#17181B` | `#16171A` |
| `--panel-raised` (card) | `#27282D` | `#24252A` |
| `--border` (card edge) | `#3A3B40` | `#34353B` |

**Why "structurally complete" was still visually wrong.** The old egui theme's vocabulary
is flat fills and 1px strokes. The prototype's is *lit*: accent **gradients** with coloured
drop-glows, green hairline **rings** on live surfaces, 45° **hatch** textures on degraded
ones, sunken insets inside raised cards. A port taking `theme.rs` as its source has no
token for any of that, and an audit asking "is the card there? is the label accent
coloured?" answers yes to every question while the result stays flat. `tokens.css` now
carries an explicit **Effects** block (`--accent-gradient`, `--accent-glow`,
`--tile-ring-live`, `--thumb-ring-live`, `--success-glow`, `--screen-hatch`,
`--card-gradient`, `--slider-fill`, …) so this layer is a named part of the design system
rather than something an implementer can silently omit.

What was corrected, by the owner's three complaints:

- **"borders on the stream"** — Watch tiles (`.media-tile`) had *no* outline at all. The
  prototype gives every live tile a green hairline **ring**
  (`box-shadow: 0 0 0 1px rgba(80,200,120,.2)`), not a `border`. The distinction is why it
  went missing: `view.css`'s own header (correctly) says a video surface must never read as
  a settings widget and carry no card border — and the implementation concluded there
  should be no outline whatsoever. A ring sits *outside* the box, so it lights the tile
  without insetting the video or reading as chrome. Broadcast preview thumbs gained the
  matching ring + bloom (`--thumb-ring-live`); the sidebar's ROOMS and OUTGOING lists
  gained the **raised wells** (option 1c: "the room list sits in a raised well inside
  it") they never had; section headings gained the trailing hairline **rule** that makes
  them read as banded groups rather than floating text.
- **"colors are not right"** — the repalette above, plus: destination cards were filled
  with `--panel-sunken`, i.e. *darker than the page*, so every active share read as a hole
  punched in the surface instead of a card resting on it; preview thumbnails were a
  saturated blue gradient rectangle rather than a dark screen well; accent surfaces used
  light-on-accent text where the prototype uses dark (`--primary-text` on `#4C6EF5`).
- **"missing some settings"** — genuinely missing *content*, not styling: the prose
  explaining what the presentation modes trade off (without it the dropdown is three
  meaningless words), and the closing **info/about card**, which is the only place the app
  states its local-only privacy property. Settings is now a stack of one-concern cards
  (LANGUAGE / PRESENTATION MODE / PLAYBACK / about) per the prototype, instead of one
  "SETTINGS" card of label-left rows. No control was added or removed —
  `max_jitter_buffer_ms` postdates the mockup and keeps its slider, in a PLAYBACK card.

**Also fixed in this pass — a real layout bug, reported separately by the owner.** The
bandwidth slider rows ("Total maximum" / "Floor per stream") each rendered as their own
flex row, so a slider's start X was a function of *that row's own label width* — the two
tracks began at different X and the group read as ragged. `.sc-bandwidth-controls` is now
one grid whose rows are `display: contents`, so every row resolves against the same three
column tracks; the label column is `max-content`, not a fixed px width, because the German
strings run ~30% longer and a guessed width would either clip them or leave an English
gutter. The same treatment was applied to the four per-destination tuning rows
(`.dest-edit-grid`: Bitrate / Tune / Quality Level / Rate Control), which had the identical
defect. **Trap for later edits:** with `display: contents` every row must contribute
*exactly* as many children as there are columns — the three `pillSwitch` rows have no value
readout and needed an empty spacer cell, or grid auto-placement spills the next row's label
into the value column.

**Deliberate deviations from the prototype** (the owner explicitly granted latitude to
depart from the mockup where it improves the result — it is a reference for direction, not
a spec to replicate):

- Cards and destination cards carry an **ambient shadow**. The prototype is a flat mockup
  and paints none, but a card that casts nothing reads as a lighter rectangle rather than a
  surface.
- A consistent **`:focus-visible` accent halo** across buttons/inputs/rows. The prototype
  has no focus states at all; the port had a mix of UA defaults and `outline: none`.
- The Watch room filter's active segment is a **neutral** raised chip, not the accent
  gradient — accent is reserved for actions, and a room filter is navigation.
- No **version number** in the about card. The prototype prints "Simple Screensharing 0.4",
  but that is the designer's placeholder and no command exposes the real crate version to
  the frontend (`Cargo.toml` says `0.1.0`). Printing an invented version is worse than
  printing none. **Open follow-up:** wiring a real `get_app_version` command is a small
  backend addition, deliberately not made in this frontend-only pass.

**Chromium cannot paint a filled range track.** The prototype's sliders show an accent
gradient up to the thumb; `::-moz-range-progress` has no Chromium equivalent, and Chromium
is the engine behind *both* the Electron shell and the Tauri webview target — so CSS alone
left every slider a flat grey groove. Rather than hand-roll a div-based slider (losing
keyboard support, native drag, and the a11y semantics of a real `input[type=range]`),
`shell.js` publishes the value as `--range-pct` and CSS draws the fill as a hard-stop
gradient. A `MutationObserver` handles sliders created later by a tab's own `render()`, so
no tab has to remember to ask for a repaint.

Verified by side-by-side comparison of the prototype against fresh screenshots at the
mockup's own 1440×900, before and after, across Stream Control (both bandwidth modes, with
live/rebuilding/stalled/failed destinations), the Watch grid (idle, hover, waiting), the
room-detail slide-over, the leave-confirm modal and Settings — **and**, unlike the pass
above, against the **real compiled Electron app** driven by Playwright (`_electron`) with
the Rust sidecar attached, confirming the fixes hold in the shipping shell and not only in
the mock/no-backend browser path.

## 2026-07-28 — native `<select>` replaced with a custom-styled `Components.dropdown`

Every `<select>` in the frontend rendered its open option popup as unthemed OS/Chromium
chrome — stark white background, generic system font, no dark surface/border/accent
styling and no open/close animation — because a native select's popup is platform UI that
CSS cannot restyle cross-platform (Electron on Linux/Windows/macOS each render it
differently, and none of them can be reached from author stylesheets). The project owner
asked for the dropdowns to "roll out" and actually match the app's dark theme, which a
native select structurally cannot do.

**`Components.dropdown(options, value, onChange, opts)`** (new, in `components.js`,
alongside `pillSwitch` — see that function's own doc comment for the full parameter
shapes) rebuilds the same trigger+popup shape out of ordinary elements instead:

- The closed state is a real `<button class="dd-trigger">` (native keyboard focus, no
  a11y shim needed) showing the current option's label plus a `expand_more` chevron that
  flips 180° when open — same icon/rotate idiom `.dest-advanced-summary`'s disclosure
  triangle already used elsewhere in this file.
- Opening reveals a `.dd-list` of `<button role="option">` rows (real, individually
  focusable/clickable elements, not `role="option"` divs with no native affordance) with a
  ~140ms `transform: scaleY() + opacity` "roll out" transition, matching this codebase's
  existing 120–160ms transition durations (`tokens.css`/`view.css`/`shell.css` all sit in
  that range already — see `dropdown.css`'s own header comment). The selected row is
  accent-colored with a checkmark, the same visual language `pillSwitch`'s active state
  uses.
- Closes on selecting an option, clicking outside, or Escape (restores focus to the
  trigger). A native `<button>` already fires `click` for both Enter and Space, so no
  custom keydown handling was needed to open it.
- `Why the option list is appended to `<body>` only while open, not rendered as a
  `position: absolute` child of the dropdown itself:` several call sites nest a dropdown
  inside `overflow: hidden` ancestors — `.dest-card`'s rounded-corner clip in particular
  (`stream-control.css`) — so a plain absolutely-positioned child would be silently
  clipped at the card's edge the moment the list is taller than the remaining card height.
  Appending to `<body>` with `position: fixed` and a boundingClientRect-computed
  left/top (flipping to open upward with a simple "not enough room below, more room
  above" check — not full popover collision logic) sidesteps that entirely, and the list
  is removed from `<body>` again on close so a card that gets torn down while the picker
  is closed leaves nothing behind; found and fixed during verification (see the trap
  below).
- The returned root exposes `.value` (get/set, mirrors `select.value` — setting it does
  *not* fire `onChange`, matching native `<select>` scripted-assignment behavior),
  `.setOptions(newOptions)` (rebuild the list in place, e.g. the audio-source picker's
  periodic re-poll in `addAudioRow`), and `.focus()`. Plain `.hidden` works unmodified
  since the root is an ordinary element.

**Trap hit during this port, worth knowing for future edits:** `.dd { display: block }`
and the browser's own default `[hidden] { display: none }` are the *same CSS specificity*
(one selector each) — since UA rules always lose to author rules of equal specificity,
`.dd`'s own rule silently beat the built-in hidden behavior, so a `.hidden = true` dropdown
(the "+ Also send to…" fan-out picker in `destinationCard`) stayed visibly rendered.
Fixed with an explicit `.dd[hidden] { display: none }` in `dropdown.css`, which wins on
specificity regardless of source order. Any future custom component that also relies on
the native `hidden` attribute should check for this same trap if it sets its own top-level
`display` on the wrapper.

Converted every `<select>` in the frontend (`grep -n 'el("select"' frontend/scripts/*.js`
found all of them): the Framerate pickers in `destinationCard`'s edit panel and the
Start-a-Stream form, the room-transfer and fan-out pickers and the audio-source picker in
`destinationCard`/`addAudioRow` (`components.js`), the room-scoped-settings room picker and
the Start-a-Stream room picker (`stream-control.js`), the Settings tab's presentation-mode
picker (`settings.js`), and the room-detail panel's audio-source picker (`shell.js`). Each
call site kept its exact prior value semantics (same options, same selected value, same
change callback) — this was a like-for-like visual/structural swap, not a behavior change.
Existing sizing classes (`.dest-room-select`, `.settings-select`, etc.) were re-pointed
from the old `<select>` element onto the new `.dd-trigger` button, since class-based CSS
selectors don't care which element carries them but the handful of `select`-*tag*
selectors (`.sc-start-field select`, `.sc-room-scoped-header select`) had to be rewritten
to target `.dd-trigger`/`.dd` instead, tag selectors having no reason to still match.

Verified against the real (mock-data) frontend served over local HTTP and driven with
Playwright: every converted dropdown opens with the dark trigger/list styling and the
roll-out transition, reflects and updates the selected value on click, closes on outside
click and Escape, and the fan-out/audio-source pickers correctly escape their `.dest-card`
ancestor's `overflow: hidden` instead of being clipped by it.

### 2026-07-29 — dropdowns were shorter than buttons

**Owner's ask, verbatim:** *"dropdowns should be taller (same as buttons)."* The
`.dd-trigger` base rule (`dropdown.css`) itself already rendered close to the app's
standard `.btn` height (real Electron `getBoundingClientRect()` measurement: 33px vs
`.btn`/`.btn-primary`'s 32px — e.g. `.settings-select`, unmodified, already matched). The
mismatch the owner was seeing was `.dest-room-select` (`stream-control.css`) — the sizing
class every Room and Framerate picker on a destination card carries alongside `.dd-trigger`
— whose own `padding: 3px 4px` (a leftover from when it styled a native `<select>`, never
revisited when this component was rebuilt as a `.dd-trigger` button on 2026-07-28 above)
overrode `.dd-trigger`'s padding via ordinary same-specificity cascade (`stream-control.css`
loads after `dropdown.css` in `index.html`), rendering at ~23px real height — visibly
shorter than the "Go live"/"Cancel" buttons sitting directly below it on the same staged
card. Fixed by bumping `.dest-room-select`'s padding to `8px 8px`, landing at 33px real —
matching `.dd-trigger`'s own already-correct height and the `.btn` reference alongside it.

**Verified** against the real Electron app + real compiled sidecar (`getBoundingClientRect()`
on the Stream Control tab's staged "Go live" card and the Settings tab's Presentation Mode
picker):

| control | before | after | reference (`.btn`/`.btn-primary`) |
|---|---|---|---|
| `.dest-room-select` (Room / Framerate pickers) | 23px | 33px | 32px |
| `.settings-select` (Presentation Mode picker) | 33px (unchanged) | 33px | 32px |

No native `<select>` remains anywhere in the frontend to check separately (confirmed via
`grep -n '"select"' frontend/scripts/*.js` — only doc-comment mentions, zero live
`el("select", ...)` call sites; every control already routes through
`Components.dropdown`), so this is the complete set of dropdown/select-like controls in the
app.

---

**2026-07-28 — later still — room-detail panel gained a "Live streams" section (project
owner: "In the tree view (in ROOMS), show the actual running streams, so i can connect to
them from there. Plus the current amount of people watching.").**

`shell.js`'s room-detail slide-over (`renderRoomPanel`) gained a new **Live streams**
section between MEMBERS and AUDIO: one row per announced stream in the room (`room_detail`'s
`RoomDetail.streams` / `StreamInfoDto`, `src-tauri/src/dto.rs` — already exposed to the
frontend, no backend change needed for this part), each showing the owner's display name,
the stream's label, kind, and resolution, and a Watch/Stop-watching icon that calls the same
`toggle_watch` command the MEMBERS section and the View tab's "NOT ON SCREEN" rail
(`view.js`'s `playNotOnScreen`) already use — see the new `toggleStreamWatch`/
`renderRoomPanelStreamRow` functions. Starting a watch also switches the active tab to
**View** (`setActiveTab("View")`) so "connect to them from there" actually lands the user
somewhere they can see it; stopping a watch does not switch tabs. This is a superset of
what MEMBERS already showed: MEMBERS caps out at one Video stream per member
(`loadRoomMembers`'s pre-existing limitation), while Live streams lists every stream
`room_detail` reports, video or audio, including a member sharing more than one.

**The "current amount of people watching" half of the request could not be implemented —
this is a genuine relay wire-protocol gap, not a client-side oversight.** Investigated
`src/protocol.rs`'s `RelayToClient::SubscriberCount { stream_id, count }` (already exists on
the wire) and its handling in `src/net.rs`/`src/session.rs`/`src/capture.rs`
(`notify_subscriber_count`, `DestCmd::SubscriberCount`): the relay sends this message **only
to the stream's own owner**, to drive that owner's local encoder-pause-at-zero-subscribers
optimization — confirmed both in code (`src/session.rs`'s `ClientEvent::SubscriberCount`
handler forwards it to the matching *local* capture handle only) and explicitly in
`superdoc/architecture/protocol-spec.md` §9: *"The relay sends both messages only to the
stream's owner; other members never receive another stream's subscriber count."* So a
client viewing someone else's stream in the ROOMS tree has no data — real or derivable —
for how many people are watching it. No DTO/frontend change could paper over this; `dto.rs`'s
`StreamInfoDto` was deliberately left without a `watcher_count` field rather than adding one
with nothing to populate it from.

**Follow-up cross-repo task, if the project owner wants this closed:** the relay
(`simple-screensharing-server`) would need to start broadcasting a per-stream subscriber
count to *every* room member, not just the owner — either by widening `SubscriberCount`'s
existing owner-only delivery to room-wide, or by adding a `watcher_count` field directly onto
`StreamInfo`/`PresenceDelta`'s `StreamStarted` variant (`protocol.rs`) and updating it via a
new delta message on subscribe/unsubscribe. Either shape is a wire-format change both sides
of the room ↔ relay connection must agree on: `simple-screensharing-server`'s relay/protocol
code (out of scope for this worktree) plus this client's `src/protocol.rs` (`RelayToClient`/
`StreamInfo`), `src/net.rs` (event forwarding), `src/session.rs` (state bookkeeping), and
`src-tauri/src/dto.rs` (`StreamInfoDto`, to actually surface the new count to the frontend).
Until that lands, the Live streams section above shows a static note in place of a count:
*"Watcher counts aren't shown yet — the relay only reports them to a stream's own owner."*

Verified against the real (mock-data) frontend served over local HTTP and driven with
Playwright: opening Movie Night's room-detail panel shows the new Live streams section with
Ana's "Screen" video stream, clicking its Watch icon flips it to "Stop watching" state and
switches the active tab to View. Not verified against a real two-client relay session (no
live server/second client available in this pass) — the `toggle_watch`
call itself is unchanged from the already-shipped, already-tested MEMBERS/View-tab code
paths, so this is a UI-composition change riding on proven plumbing, not new wire behavior.

---

**2026-07-28 — later still — app icon actually wired up (asset had been copied into the
repo in an earlier session but nothing referenced it).**

The mark lives at `frontend/assets/icons/mark-<N>.png`, shipped as loose files in the
Electron app resources (not compiled into the `src-tauri` Rust binary — confirmed no
`include_bytes!`/`build.rs` icon embedding exists there). Two consumers, two resolutions:

- `electron/main.js`'s `APP_ICON` constant (`mark-256.png`) is passed as the `icon:` option
  to both the main `BrowserWindow` (`createWindow`) and each per-stream pop-out
  `BrowserWindow` (`openPopoutWindow`) — pop-outs get it too because each is its own
  top-level OS window with its own taskbar entry on Linux/Windows. 256px was picked over
  32/64/512/1024 because Electron only takes one PNG and scales it itself; on Linux a
  reasonably large source lets the window manager/taskbar downscale cleanly, and nothing
  here needed a larger source than that.
- `frontend/index.html` and `frontend/viewer.html` (the pop-out page's own `<head>`) each
  got a `<link rel="icon" href="assets/icons/mark-32.png" />` tab/window-icon favicon.

**Kept only the two resolutions actually consumed above — 32 and 256 — and `git rm`'d
64/128/512/1024.** Why: the project owner explicitly asked to trim the asset set to "only
the resolutions we'll need for displaying in the GUI and for a tray icon (not yet
implemented)" and to not keep dead resolutions "just in case." A tray icon isn't built yet
(out of scope here), but the kept 32px file already doubles as a suitable future tray
source (Linux/Windows tray icons want roughly 16-32px, and Electron scales a supplied
source down further if needed) — so no separate tray-only resolution needed to be kept
around unused. Checked repo-wide for anything else that might want a specific icon size
(`electron-builder`/`electron-forge` config, a `.desktop` file) — none exists, so there was
no packaging-manifest size requirement to account for. `src-tauri/icons/icon.png` and
`tauri.conf.json`'s `bundle.icon` are a separate, vestigial leftover from before the
Electron migration (`src-tauri` never shows its own window — `"visible": false` — so that
icon is unused); left untouched, out of scope for this change.

Verified against the real Electron app (built sidecar + `electron .`, per
`superdoc/claude-instructions/testing-and-verification.md`): the main window's taskbar/WM
icon shows the mark instead of Electron's default icon.

---

**2026-07-28 — later still — the mark now also renders inside the shell's own UI, not
just as the window/taskbar icon.** `index.html`'s sidebar brand header (`.app-brand`, ~
line 20) previously held a generic Material Symbols "cast" glyph inside a `.app-icon`
gradient badge as a placeholder — the real mark had never actually been wired into the
app's own chrome. Swapped that `<span class="material-symbols-outlined">cast</span>` for
`<img src="assets/icons/mark-32.png">` (32px source at the badge's native 22×22px
display size — no need for the 256px file here, unlike the window-icon consumers above
which want a large source for the OS to downscale).

`Why the gradient badge/glow was dropped, not kept underneath:` the mark
(`mark-32.png`/`mark-256.png`) is already a full-color image — two overlapping rounded
rectangles in a blue-to-green gradient, on a transparent background — not a flat
single-color glyph the way the placeholder "cast" icon was. It supplies its own color
and contrast on the dark sidebar rail directly; a colored gradient badge sitting behind
an already-colored gradient mark read as a swatch-on-a-swatch, not a logo. Checked what
the mark actually looks like (`Read` on the PNG) before deciding, rather than assuming
either way. `.app-icon` is now just a plain 22×22px flex box sizing the `<img>`.

Verified against the real Electron app (built sidecar + Playwright `_electron`, per
`superdoc/claude-instructions/testing-and-verification.md`): the mark renders crisply
next to the "Simple Screensharing" title in the sidebar header.

---

**2026-07-29 — sidebar header centered and given even box-spacing, logo bumped up.** The
project owner asked for the logo+title (`.app-brand`) to be treated as one box, centered
inside `.sidebar-header`, with equal spacing on all four sides around that box (including
around the now-bigger logo) — the previous layout left `.app-brand` left-aligned inside
`.sidebar-header` with asymmetric padding (`12px 12px 10px`, less on the bottom than the
other three sides).

- `.sidebar-header` gained `display: flex; justify-content: center;` (matching how the
  rest of the codebase centers a single flex child rather than reaching for
  `margin: 0 auto` on the child) and its padding shorthand became a single equal value,
  `18px` on all four sides — up from the old `12px 12px 10px`, since the bigger logo below
  needed more breathing room to read as "generous/balanced" rather than "cramped."
- `.app-icon` (and the `<img>`'s `width`/`height` attributes in `index.html`, kept in sync
  so there's no layout jump before CSS applies) grew from 22px to 30px — "a bit bigger" per
  the owner's wording, picked by rendering it against the real title text and confirming it
  didn't dominate.

Verified against the real Electron app (built sidecar + Playwright `_electron`, per
`superdoc/claude-instructions/testing-and-verification.md`): `getBoundingClientRect()` on
`.sidebar-header`/`.app-brand` confirmed the left/right gaps around `.app-brand` are equal
(43.89px each) and match the computed padding exactly, and the top/bottom gaps are each
18px, matching the same padding value — i.e. all four sides of the "box" are genuinely
even, not just visually close. Screenshot confirmed the bigger logo reads proportionate
next to the title, and the rest of the sidebar (ROOMS list, nav, Outgoing section) is
unaffected by the padding change.
