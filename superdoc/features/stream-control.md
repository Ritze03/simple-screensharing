# Stream Control (desktop client)

> **Status (2026-10-14).** The client is now one native egui binary. The **backend** half of this
> page is current: `client/src/capture.rs` (with its inline `destination` and `branch` modules), `session.rs`,
> `bandwidth.rs`, `presets.rs`, `rooms.rs`. That covers "The shape of a capture", fan-out,
> transfer, the three tiers, "Capture/encode performance" (#39, with the rotated-output check from
> #35), "Idle monitors" and the silence watchdog (#32), resuming, the encoder-tuning backend
> mapping, "Video codec selection", and the bandwidth modes. The **UI** half of this page
> (everything that names `frontend/scripts/*.js`, `stream-control.js`, `src-tauri/`,
> `commands.rs`, `window.__TAURI__`, `.media-tile-*` CSS, Electron) describes the **removed Electron
> frontend** and is history: "Go live" staging as a UI flow, the "START A STREAM" form, "What the
> tab renders" and "Coverage", the edit-panel bug write-ups, the whole audio-card story
> (from "Sharing audio" to the end, except the backend facts about `pw-loopback`/node ids), and the
> "bitrate doesn't update" display bug. The egui equivalents are `client/src/ui/stream_settings.rs`,
> `ui/action_bar.rs`, `ui/room.rs`, `ui/modals.rs` (see `superdoc/ui/APP-SHELL.md`).
> The 320x180 "self-preview" branch drawn in the diagrams still exists but nothing consumes it
> (`superdoc/features/self-preview.md`).

**Added 2026-07-26.** Stage 2 of the three-stage UI build (see `superdoc/ui/APP-SHELL.md`):
the Stream Control tab's real feature set: starting and editing outgoing streams,
**multi-destination fan-out**, **transfer** between rooms, and the two **bandwidth modes**
with `bandwidth::allocate` wired into live encoders. The plan it implemented is
`superdoc/brainstorm/ui-redesign-multiroom-streaming.md` (final plan items 4, 5 and 7).

---

## The shape of a capture, after this stage

One capture is **one screen grab, one GStreamer pipeline, N destinations**:

```text
GPU domain (Linux portal + VA stack, the normal case; 2026-10-14, #39):
pipewiresrc(DMA-BUF) ! vapostproc(video-direction=auto) ! VAMemory NV12 ! tee
    ├─ queue ! videorate(1fps)  ! vapostproc ! BGRx ! videoconvert ! RGB ! appsink   (JPEG thumbnails, fixed)
    ├─ queue ! videorate(12fps) ! vapostproc(320x180) ! BGRx ! videoconvert ! RGB ! appsink   (self-preview, fixed; UNUSED, see self-preview.md)
    ├─ queue ! valve[RateCap] ! vapostproc ! VAMemory NV12 ! capssetter(fps) ! va*enc ! appsink   ← destination A (room 1)
    └─ queue ! valve[RateCap] ! vapostproc ! VAMemory NV12 ! capssetter(fps) ! va*enc ! appsink   ← destination B (room 2)

System domain (Windows, x264enc machines, tests):
<platform source> ! videoconvert ! capsfilter(format=BGRx) ! tee
    ├─ queue ! videorate(1fps)  ! RGB ! appsink   (JPEG thumbnails, fixed)
    ├─ queue ! videorate(12fps) ! videoscale(320x180) ! RGB ! appsink   (sender self-preview, fixed; UNUSED, see self-preview.md)
    ├─ queue ! valve[RateCap] ! videoconvert ! videoscale ! capssetter(fps) ! encoder ! appsink   ← destination A (room 1)
    └─ queue ! valve[RateCap] ! videoconvert ! videoscale ! capssetter(fps) ! encoder ! appsink   ← destination B (room 2)
```

A destination's fps is a **cap** (`branch::RateCap`, a pad probe on its valve), never a floor:
no branch ever duplicates a frame. Why both the domains and the cap exist, with the
measurements: "Capture/encode performance" below.

Every destination branch is **dynamic** (`capture::branch::add_branch`), added and removed
on the live `Playing` pipeline. Each has its own encoder, its own bitrate, its own pause
valve, its own `DestinationRuntime` (state machine + wire-PTS clock + cached IDR), its own
`stream_id`, and its own room key and nonce sequence. They share only the raw frames coming
off the `tee`.

`Why the pipeline no longer has a fixed encoder branch:` a statically-built branch can only
ever serve one destination, and it cannot be branch-swapped (you cannot swap out an element
`gst::parse::launch` welded into the pipeline). Keeping one would have meant implementing
every operation twice — per-destination settings changes, transfer, fan-out, per-branch
VAAPI fallback — with the fixed branch's version unable to use the branch-swap primitive at
all. With every destination dynamic there is exactly one code path and the *first*
destination is not special. `start_sharing` now builds the pipeline and then adds its first
destination through the same `spawn_destination` every later one goes through.

`Why one pump task per destination rather than one per capture:` each destination's samples
go to a different room, under a different key, on a different `stream_id`, and a swap
*consumes* the branch by value (that ownership transfer is the appsink-reader handoff —
see `EncodeBranch`'s "single `AppSinkStream`" rule in `architecture/overview.md`). One task
per destination gives each branch exactly one owner and lets a swap be `await`ed without
the UI thread ever blocking. The capture keeps one task of its own
(`capture_loop`) for the two things that genuinely are per capture: the pipeline bus and
the self-preview branch.

---

## "Go live": staging a capture with zero destinations, then attaching the first one (2026-07-28)

**Owner's request, verbatim:** *"Lets make it so when i press 'Share another screen', it
opens the stream UI with the settings visible, but the stream is not running yet. In the
corner there should be a 'Go live' button, which actually goes live with it. The current
settings, when pressing 'Go live' should be saved as standards for the next time. Also,
remove the preview, since it doesnt show the current stream anyways."*

Before this, "Share Screen"/"Share another screen" called `AppState::start_video_share`
directly: one call that acquired the capture source (the OS portal prompt), built the
pipeline, added the *first* destination's encode branch, and announced the stream to its
room, all in one shot — there was no way to see the real, portal-negotiated resolution or
adjust settings before something was already being sent.

**The architectural question, and why the answer turned out to be "no restructuring
needed":** the fan-out primitive (`AppState::add_destination` / `capture::branch::
add_branch_live`, see below) already adds a destination branch to an *already-running* tee.
The only question was whether a capture could start with **zero** branches on that tee to
begin with — i.e., whether "capture running, tee up, nothing subscribed to it yet" is a
state the pipeline already tolerates, or something that needed careful new pipeline-
construction work. Tracing `capture::start_sharing`'s old body (`start_sharing_with_source`)
showed the pipeline is fully up, `Playing`, and reporting real negotiated caps *before* the
first destination's branch is ever added — the old code just immediately chained
`spawn_destination` right after. Splitting there needed no pipeline surgery at all:

- **`capture::start_capture(framerate) -> Result<CaptureHandle>`** (new) is the "staged"
  half: acquires the capture source, builds and starts the pipeline, wires up the
  capture-level bus/self-preview pump — everything `start_sharing` used to do up to and
  including "pipeline confirmed `Playing`, real caps known" — and returns a `CaptureHandle`
  whose `destinations` list is empty. `capture::start_sharing`/`start_sharing_from_test_source`
  (unchanged public signatures) now call a shared `start_capture_with_source` helper for that
  part, then immediately attach their first destination the way they always did — the
  "share and go live in one call" behavior those two entry points still provide (still used
  by `AppState::start_video_share`, which is now dead code from the UI's perspective but
  kept, along with its own real-hardware hang/stress tests in `src-tauri/src/commands.rs`,
  rather than deleted mid-investigation).
- **`AppState::stage_video_share() -> CaptureSourceId`** (new, `session.rs`) is the
  `start_video_share`-shaped wrapper: allocates a capture id, spawns `capture::start_capture`,
  inserts a `LocalCapture` with an **empty** `destinations` map. No room, no `stream_id`, no
  `AnnounceStream` — nothing is sent anywhere yet.
- **`AppState::go_live(capture_id, room_handle)`** (new) *is* `add_destination`'s fan-out
  primitive, just used for the first branch instead of the second: it raises the capture's
  source-framerate floor if the staged card's Framerate control was set above what the
  pipeline actually started at (`CaptureHandle::set_source_framerate` — no restart needed,
  see "Editing a live destination" below for why a branch's own `videorate` already serves
  higher framerates off a lower-floor source), builds a `VideoParams` from the *current*
  process-wide `framerate`/`bitrate_kbps`/`tune`/`quality_level`/`rate_control` (the same
  fields the staged card's controls write via `set_framerate`/`set_bitrate`/`set_tune`/
  `set_quality_level`/`set_rate_control` — unchanged commands, same "process-wide, applied
  before the call" shape `start_video_share` always used), and calls the existing
  `add_destination_with_params` with a new `PendingRole::GoingLive` (mechanically identical
  to `PendingRole::FanOut`, kept as its own variant only so the UI says "Going live…" instead
  of "Also sending here…" while it waits for the first IDR).

**The one real subtlety: the periodic room-preview thumbnail.** `preview::spawn_preview_task`
needs a `client`/`stream_id` to send into — neither exists for a staged, destination-less
capture. `CaptureHandle` now holds the tee's low-rate preview branch as an **unconsumed**
`preview_sink: Option<gst_app::AppSink>` until the first destination is attached (safe to
leave unread indefinitely: that appsink is `max-buffers=1 drop=true`, so it never applies
backpressure upstream — see `build_pipeline`). A new `CaptureHandle::ensure_preview_task`
starts the preview task the first time *any* destination is registered (called from both
`register_destination`, now taking an `Option<&ScreenshareClient>`, and inline in
`start_sharing_with_source`'s own single-call path) and is a no-op on every destination after
that — exactly matching the pre-existing behavior that only a capture's first-ever
destination gets the periodic thumbnail at all.

**Frontend.** `stream-control.js`'s "Share another screen" ghost row now calls a new
`stageVideoShare()` (pushes the persisted `get_start_stream_defaults` into the process-wide
settings first, then `stage_video_share`) instead of scrolling to a separate form — the old
"START A STREAM" card (see the section below) is gone. The staged capture renders as a card
in the same Active Shares grid, via `components.js`'s `destinationCard(dest, handlers,
{staged: true, ...})`: same Preset/Framerate/Bitrate/Advanced-tuning panel a live
destination's edit gets, plus a Room picker (nothing else has anywhere to put "which room
Go Live will use"), and a "Go live"/"Cancel" action pair in place of Transfer/Stop — "Cancel"
is just `stop_share` on the capture id, since a staged capture is `ShareStatus::Active` with
zero destinations, exactly the state `stop_share` already knows how to tear down. Editing a
staged card's controls calls the same `set_framerate`/`set_bitrate`/`set_tune`/
`set_quality_level`/`set_rate_control` commands the old form did — `stagedEditState`
(capture_id -> edit buffer) is the new form's counterpart to `destEditState`. Persisting
those values as the new defaults (`set_start_stream_defaults`) now fires exactly once, at
"Go live" (`goLive()`), not on every keystroke like the old form's `saveStartFormDefaults`
did — per the owner's explicit request. New backend surface: `stage_video_share`, `go_live`,
`list_staged_captures` (`dto::StagedCaptureRow` — capture id, `starting`/`ready`/`stopping`
state, real negotiated width/height once known, and the process-wide encoder settings, since
a staged capture has no destination of its own to own more specific values than that), all
wired into `src-tauri/src/commands.rs`/`main.rs`/`api_server.rs` the same way every other
command pair is. `driver.rs` gained a `staged-capture-changed` event (same diff-and-emit,
"vanished this tick" shape as `destination-state-changed`) so the frontend refreshes when a
staged capture finishes coming up or disappears (went live, or was cancelled).

**Preview thumbnail removed, live or staged, per explicit request.** `components.js`'s
`destinationCard` no longer renders a `.dest-thumb` column at all — it was always a static
icon/gradient placeholder (see its own now-removed doc comment: "a real live self-preview
needs a backend addition this frontend-only pass doesn't make"), never a live decode. The
owner confirmed it was useless specifically because it never showed the actual stream, and
asked for it gone outright, not just for the new staged card. All of `.dest-thumb`'s CSS
(size, ON AIR corner plate, live/stalled ring treatments) was deleted from
`stream-control.css` along with it.

**Verified against the real app** (Electron + the real `src-tauri` sidecar over its
REST+SSE `api_server`, driven with Playwright's `_electron`, not the frontend's mock
branch — this environment's `xdg-desktop-portal` did not block on an interactive consent
dialog, so the full flow could be driven end to end without a human click):
clicking "Share another screen" staged a real capture (portal prompt resolved, real
1920×1080 negotiated resolution shown, card state `ready`) while `list_active_streams`
stayed empty — nothing was live. Changing the staged card's Framerate from 120 to 60 fps
was reflected immediately in `list_staged_captures`. Clicking "Go live" produced a real,
`live`-state entry in `list_active_streams` at `framerate: 60`, and `get_start_stream_
defaults` picked up `framerate: 60` immediately afterward — confirming persistence fires at
"Go live", not before. Stopping that destination returned `list_active_streams` to empty
with no residual state. Separately: `cargo build --lib -p simple-screensharing-client` and
`cargo build -p simple-screensharing-client-tauri` both pass; `cargo test -p
simple-screensharing-client --lib` (159 passed, 0 failed, 13 ignored — no regressions); the
two real-hardware regression tests this session's earlier freeze/burst fixes added
(`capture::tests::applying_a_preset_never_bursts_a_branch_above_its_configured_framerate`
and `end_to_end_test::late_watch_after_all_three_tuning_controls_toggled_gets_a_real_
keyframe_every_time`, the latter run 10/10 rounds) both still pass — the "split pipeline
construction from the first destination" refactor above introduced no new hang/burst.

`Known, pre-existing, unrelated quirk noticed during verification:` a fanned-out or
just-gone-live destination's `list_active_streams` row reports `width: 0, height: 0` even
though the real negotiated size is known (`CaptureHandle::source_caps`) — `poll_capture_
states` only backfills a fresh destination's `params` from its real runtime state on the
*first* destination of a capture (the `start_video_share` path), not one added later via
`add_destination`/`add_destination_with_params` (fan-out, transfer, and now `go_live`).
This predates this feature — plain "Also send to…" fan-out has always had it — and is out
of scope here; noted for whoever picks up the fan-out geometry-readout gap next.

---

### Two real regressions in "Go live", fixed: resolution stuck at 0×0, and GPU (VAAPI) encoding silently disabled (2026-07-28)

Follow-up to the "Go live" flow above — the "known, pre-existing quirk" noted there turned
out to be exactly right, and a second, previously-unnoticed bug shared its root shape.

**Bug 1 — resolution 0×0 after "Go live" (was correct while staged).** Root cause,
confirmed: `AppState::go_live` (`session.rs`) built the new destination's `VideoParams`
with a hardcoded `width: 0, height: 0`, and — unlike `capture::start_sharing_with_source`
(the still-alive-but-dead-from-the-UI `start_video_share` path), which internally rebuilds
a fresh `VideoParams` from `handle.source_caps()` *after* the pipeline is up — nothing ever
resolves that placeholder back to a real value for a destination attached via
`add_destination_with_params` (fan-out, transfer, and `go_live` all go through it). The
capture's real negotiated size **was** known at the moment `go_live` ran (it's exactly what
the staged card had just displayed, via `CaptureHandle::source_caps`) — it just wasn't being
read. `AppState::add_destination`'s own "no sibling destination to copy" fallback had the
identical bug for the same reason (reachable if a capture with zero destinations is ever
fanned out via the public `add_destination` entry point directly, not just via `go_live`).
Fix: both call sites now read `handle.source_caps()` off the capture's own `CaptureHandle`
when there's no sibling destination to copy from, instead of defaulting to `0, 0`.

**Bug 2 — GPU (VAAPI) hardware encoding silently disabled, falling back to CPU (software
x264).** Root cause, confirmed via `git log -p -L` on the three call sites: `session.rs`'s
`go_live`, `add_destination`'s fallback, and `start_video_share` have *always* built their
`VideoParams` with `backend: capture::EncoderBackend::default()` — a static `Software`, not
a call to the real hardware probe `capture::detect_encoder_backend()` (cached VAAPI-vs-
software detection, used correctly by `capture::start_sharing_with_source` and every other
internal `capture.rs` call site). Whether this mattered depended entirely on whether
anything downstream re-derives the backend — traced and confirmed it does **not**:
`CaptureHandle::spawn_add_destination` → `spawn_destination` → `branch::add_branch_live`
uses `VideoParams.backend` (via `to_encode_settings()`) exactly as given, straight into
which encoder element (`vah264enc` vs `x264enc`) gets built. So for `go_live`/
`add_destination` (which call `spawn_add_destination` directly with whatever `session.rs`
passed), the static `Software` default was real and load-bearing — every "Go live" share
silently ran CPU encoding regardless of this machine's real VAAPI capability.
`start_video_share`'s own `EncoderBackend::default()`, by contrast, is harmless/vestigial:
that path calls `capture::start_sharing`, whose `start_sharing_with_source` builds its
*own* internal `VideoParams` (real `source_caps()` + real `detect_encoder_backend()`) and
never looks at the placeholder `session.rs` constructed — it's used only as `Destination`
bookkeeping during the brief `Starting` state before the real runtime params land. Left
unchanged for that reason (fixing it would be a no-op; the doc comment on `go_live`
explains the asymmetry). Fix: `go_live` and `add_destination`'s fallback now call
`capture::detect_encoder_backend()` (made `pub(crate)`, previously private to `capture.rs`)
instead of `EncoderBackend::default()`.

**Verified on real hardware** (this machine: AMD Radeon RX 7900 XTX, `vah264enc` via the
`va` GStreamer plugin). Added
`end_to_end_test::go_live_attaches_the_first_destination_with_real_resolution_and_backend`
(`src/end_to_end_test.rs`, `#[ignore]`d like this module's other real-relay tests): spawns a
real relay server, starts a real zero-destination `TestSource` capture (`capture::
start_capture_from_test_source`, a new test-only sibling of `start_sharing_from_test_source`
so this doesn't need an interactive portal prompt or touch any real monitor), calls the
real `AppState::go_live`, and asserts the resulting destination's live params show the real
640×480 negotiated size and `EncoderBackend::Vaapi` (this machine's real
`detect_encoder_backend()` result) — not `0, 0`/`Software`. Confirmed this test fails
against the pre-fix code exactly as described (`left: (0, 0), right: (640, 480)`) by
temporarily reverting the `session.rs` change and rerunning it, then confirmed it passes
again after restoring the fix, with the real `capture.rs` log line `VAAPI hardware H.264
encoder 'vah264enc' is available` and `backend Vaapi` in the output. `cargo build --lib
-p simple-screensharing-client` and `cargo build -p simple-screensharing-client-tauri` both
pass; `cargo test -p simple-screensharing-client --lib` is 159 passed / 0 failed / 14
ignored (one more ignored than before — the new test); the pre-existing real-hardware
`end_to_end_test::` suite (`retry_destination_preserves_prior_encoder_tuning`,
`resubscribe_after_pause_resumes_encoder_and_frames_flow`,
`late_watch_after_all_three_tuning_controls_toggled_gets_a_real_keyframe_every_time`,
`real_portal_share_fans_out_and_resumes`, etc.) and `capture::tests::vaapi_pipeline_links_
and_produces_encoded_output`/`detect_encoder_backend_returns_without_panicking` all still
pass — no regressions from either fix.

---

## Bug: "Go live" visually reset Tune/Quality Level/Rate Control to Balanced (2026-07-29)

**Owner's report, verbatim:** *"When i set a config before starting the stream and then
start a stream, it resets to 'Balanced' again."* — change Quality Level (or Tune/Rate
Control) on a staged card, press "Go live", and the resulting live card showed
"Balanced"/"Best Quality"/"Constant" instead of what was actually picked.

**Root cause: a frontend display-only bug, not a backend regression.** `getDestEdit(dest)`
(`frontend/scripts/stream-control.js`) is the per-live-destination local cache the
Preset/Advanced-tuning controls render from; when it has no cached entry yet for a
`destination_id` it falls back to hardcoded defaults (`best_quality`/`balanced`/
`constant`) — the same fallback a fresh page load intentionally uses before any real value
is known. The mock path's `mockGoLive` explicitly seeded this cache right when creating the
new live destination, but the **real** (`window.__TAURI__`) branch of `goLive` never did:
it called `go_live`, `set_start_stream_defaults`, re-fetched destinations, and rendered —
without ever writing the new destination's actual tuning into `destEditState`. Confirmed
`dto::DestinationRow` (`src-tauri/src/dto.rs`) carries no `tune`/`quality_level`/
`rate_control` fields at all, so `destEditState` seeded at creation time is the *only* place
the frontend can ever recover these values from — nothing later re-derives them from a
fetch. The backend itself was never affected: `session::AppState::go_live` builds its
`VideoParams` from `self.tune`/`self.quality_level`/`self.rate_control`, which
`applyStagedTuning`'s `set_tune`/`set_quality_level`/`set_rate_control` calls had already
set on the staged card, before "Go live" was ever pressed — the encoder kept running at the
picked setting the whole time; only the UI's redisplay of it was wrong.

**Fix:** the real branch of `goLive` now seeds `destEditState` for the destination(s) that
`fetchActiveDestinations()` shows as newly live for this `captureId`, the same shape
`mockGoLive` already used, using the `edit` object already in scope from
`getStagedEdit(staged)`. `go_live` attaches exactly one new destination per call
(`session::AppState::go_live` calls `add_destination_with_params` once), so matching on
`capture_id` (scoped to `kind === "video"`, since a paired audio destination shares the same
`capture_id` via `paired_capture_id`) is unambiguous. `mockGoLive`'s own comment, which had
claimed the real branch already did this (it didn't — that was the bug), now points at the
real branch's matching `destEditState.set(...)` instead of restating the rationale a second
time.

**Verified on real hardware** via the `run` skill's Playwright `_electron` driver, with a
real local relay server (`simple-screensharing-server`, spawned headless the same
`setsid script -qc ...` way `end_to_end_test.rs` does) so a real room could be joined:
staged a real capture, opened Advanced tuning, picked Low Latency / Quality / Adaptive
(all three away from their defaults), pressed "Go live", and confirmed the live card showed
`[Low Latency, Quality, Adaptive]` — not reset. Reverting just the `stream-control.js`
change (`git stash`) and rerunning the identical script reproduced the reported bug exactly:
the live card showed `[Best Quality, Balanced, Constant]` despite the same picks on the
staged card. `cargo build --lib -p simple-screensharing-client` and `cargo build -p
simple-screensharing-client-tauri` both pass; `cargo test -p simple-screensharing-client
--lib` is 160 passed / 0 failed / 15 ignored, no regressions.

---

## Fan-out: one capture, several rooms

**UI:** every destination row has an "Also send to…" room picker. Picking a room adds a
second destination off the *same* capture (rooms this capture already feeds are disabled in
the list — a second stream of the same picture in one room is never what anyone means).

**Bandwidth is multiplied, deliberately.** The bandwidth card states it outright whenever
more than one destination is live: *"Streaming to N rooms — about Nx the outgoing bandwidth
of one stream."* Fan-out is not free replication: the raw frames are shared, but each room
gets a separately encoded and separately encrypted stream, because a room key can never be
reused across rooms (`net.rs`). The plan doc confirms this multiplication as intended
behaviour, not a bug — so the UI's job is to make it impossible to be surprised by it.

**Leaving a room** now removes only that room's destinations; the capture survives as long
as any destination remains (`App::disconnect_room`). That is the whole reason captures are
owned by `App` and not by a room.

---

## Transfer: make-before-break

**UI:** every destination row has a "Transfer to…" room picker. While a transfer is in
flight, *both* rows are visibly labelled — the old one "Transferring to <room>…", the new
one "Transferring from <room>…" — rather than the stream silently appearing somewhere else.

**Mechanism** (`App::transfer_destination` → `poll_pending_destinations`):

1. Mark the existing destination `PendingRole::TransferSource(target)`. It keeps streaming,
   unchanged, throughout.
2. Add a destination in the target room off the same capture
   (`CaptureHandle::spawn_add_destination`) — a new branch, a new `stream_id`, a new
   `AnnounceStream` in the target room.
3. Only once that branch has produced its first IDR (i.e. the new room genuinely has
   something decodable) does the old destination get stopped (`stop_destination` → branch
   drained and removed, `StopStream` sent to the old room).
4. If the new destination fails, the marker is cleared and the old one carries on. A failed
   transfer leaves the user exactly where they were, never off the air.

`Why this reuses the branch add/remove primitives and never touches the capture source:`
nothing about the screen grab changes — the same `tee` feeds both destinations during the
overlap. Routing a transfer through the source-restart path would stall *every* destination
of that capture (tier 3, below) to accomplish something that costs one extra branch for a
second. Re-pointing the existing stream at another room is not expressible at all:
encryption keys and nonce spaces are room-bound, so "the same stream, elsewhere" always
means new bytes under a new key on a newly announced stream.

---

## Editing a live destination: the three tiers, never bypassed

Every user-driven settings edit goes through **one** entry point,
`CaptureHandle::change_destination`, which clamps the request to the source
(`destination::clamp_to_source`, never upscale) and then classifies it with
`destination::classify_change`:

| Tier | Trigger | What happens | Viewer sees |
| --- | --- | --- | --- |
| `Bitrate` | bitrate only | live `bitrate` property write on that branch's encoder | nothing |
| `BranchSwap` | resolution / fps (≤ source rate) / encoder backend | `branch::swap_to_new_branch`: build the replacement alongside the original, wait for its first IDR, flip, drop the old one. A forced keyframe follows the flip (the readiness IDR was consumed proving the branch). | nothing (row shows "Applying…") |
| `SourceRestart` | requested fps **above** the rate the capture was started at | `set_source_framerate` records the higher rate, then the destination's own branch is swapped to match | nothing — see below |

`Why the classification is not skippable at the call site:` the tiers differ by orders of
magnitude in cost and in what the viewer sees — a property write nobody notices, a
make-before-break swap nobody notices, or a shared source restart that stalls **every**
destination on that capture at once. "This is just a bitrate, I'll write it directly" at
some call site is exactly how a change silently takes the wrong tier. The one exception is
the bandwidth allocator's own writes (`set_destination_bitrate`), which are tier 1 by
construction — the allocator only ever changes bitrates.

### Tier 3 turned out to need no pipeline surgery at all

`set_source_framerate` records the higher rate on the capture's `SourceCaps` and does
nothing else. No stall is raised, because nothing stalls.

`Why:` the tier was designed on the assumption that the shared source has a hard rate
ceiling only a restart could raise. Measured against a real portal capture, it doesn't:
the source negotiates `framerate=0/1` — variable, damage-driven — and was observed
delivering ~94 fps under motion and ~30 fps on a static desktop. The selected framerate
reaches the source only as `pipewiresrc`'s `keepalive-time` (since #39 capped at 30 fps, see
below), which is a *heartbeat for the idle case*, not a ceiling. A destination asking for
more is served by its own branch's `RateCap`, which passes at most that many real frames
per second (it never pads); two branches off one real portal capture were measured holding
31.5 fps and 5.5 fps simultaneously. So nothing is being withheld that a source change
would unlock, and the ordinary tier-2 branch swap applies the new rate.

Both mechanisms that *could* have changed the live source's rate were measured and
rejected: a pre-`tee` `videorate`+capsfilter (the original design) **stalls a real portal
source dead** — 0 fps past the tee, nothing on the bus — while behaving perfectly against
`videotestsrc`; and writing `keepalive-time` on a live `pipewiresrc` kills the stream
outright (every branch dropped to 0 fps and never recovered).

The stall protocol itself (`begin_stall`/`note_source_restarted`/
`rebuild_after_source_restart`) stays, tested, as the mechanism a *genuine* source change
would need — the user picking a different monitor, or the portal session being revoked.

Lowering happens automatically once a change settles (`App::retune_source_framerate` →
`destination::required_source_framerate`, i.e. `max(fps)` across the capture's
destinations).

---

## Capture/encode performance: 120-240 fps of *unique* frames (2026-10-14, #39)

**Requirement (owner):** "at least 120 FPS sharing works or even 240 FPS". The viewer's
decoder and presenter already reached 240; the *unique* frames (real screen content) did
not. Measured on this machine (AMD RX 7900 XTX, VA-API; capture DP-1 at 279.75 Hz running
a fullscreen mailbox `vkcube`; 1080p, 8 Mbps; `client/scripts/session.sh`, the
`stats` tracer, `client/scripts/portal-probe.py`).

### Four root causes, each measured

1. **The portal delivered exactly half the output's rate (140 of 280 fps) because of the
   PipeWire buffer pool, not the compositor and not the portal's own scheduling.**
   `xdg-desktop-portal-wlr` negotiates a pool of two buffers by default. Anything that holds
   one of them back (here: `pipewiresrc`'s `keepalive-time`, which keeps its last buffer so
   it can resend it; a slow consumer does the same) leaves the portal one buffer to fill
   while we process the other, so it waits for a release every frame: exactly one frame per
   two vblanks. `portal-probe.py`, DMA-BUF path, cursor mode and keepalive swept:

   | keepalive-time | min-buffers | portal rate |
   | --- | --- | --- |
   | 0 | default | 279 fps |
   | 33 ms | default | **140 fps** (every cursor mode: hidden, embedded, default) |
   | 33 ms | 3, 4, 8 | 279 fps |

   Fix: `pipewiresrc min-buffers=6` (`PORTAL_MIN_BUFFERS`). The cursor mode is not a factor
   (re-checked with `cursor_mode` 1 and 2 at default buffers: still 140). The old
   `keepalive-time` was `1000 / selected fps` (4 ms at 240), which also fired on every
   hiccup of a 3.6 ms-cadence source and injected resends that were indistinguishable from
   real frames ("pipewiresrc 200 fps", of which ~55 unique); it is now capped at a 30 fps
   heartbeat (`STATIC_KEEPALIVE_FPS`), which is all a static screen needs.
2. **Shared-memory buffers.** With `pipewiresrc ! videoconvert` the only caps on offer are
   system memory, so the portal hands out SHM buffers and the compositor must read the whole
   8 MB frame back to RAM. Any GPU element downstream makes `pipewiresrc` offer DMA-BUF
   (`AR24` with the AMD tiled modifier); the same portal then delivers 280 fps at a fraction
   of the CPU cost. This is the **GPU capture domain** (`capture::CaptureDomain::Gpu`):
   `pipewiresrc ! vapostproc video-direction=auto ! video/x-raw(memory:VAMemory),format=NV12
   ! tee`. One GPU pass does the BGRx to NV12 conversion *and* the orientation, the PipeWire
   buffer is released immediately, and the VA encoders take the VAMemory frames with no
   download. The CPU `videoconvert` BGRx to NV12 it replaces cost ~4.7 ms per frame (a ~210
   fps ceiling on the thread that also runs the encoder); on the CPU path the branch now
   measures only ~40 fps with this machine under load (`SSS_CAPTURE_PATH=system`). The
   System domain remains for Windows and software-encoder machines, and a GPU pipeline that
   fails to start falls back to it (`start_capture_with_source`).
3. **`videorate` padding fed on itself.** A branch's `videorate drop-only=false` duplicated
   frames up to the target fps. When the encoder could not sustain the target (VA H.264
   ~165 fps, AV1 ~113-215 fps at 1080p) the leaky queue dropped real frames, `videorate`
   answered each gap with a run of duplicates, the encoder encoded those, and the queue
   dropped yet more real frames: 3 unique fps inside a "169 fps" stream. Fix: the branch has
   no `videorate` any more. `branch::RateCap` (a pad probe on the valve's src) admits at most
   `fps` frames per second and never invents any; a `capssetter` stamps `framerate=N/1` onto
   the caps for the encoder's rate control (a `0/1` makes `va*enc` assume 30 fps). A slow
   encoder now just gets fewer, all real, frames.
   - **Why not `videorate drop-only=true` / `max-rate`:** it keeps an output grid that falls
     behind real time whenever the branch was blocked (a saturated encoder pushing back, a
     paused valve), then passes *every* frame until it has caught up. A 30 fps cap on a
     60 fps source after a 1.5 s block ran at 66, 60, 54 fps in the three half-seconds that
     followed (`client/scripts/videorate-burst.py` reproduction; pinned in the other direction by
     `rate_cap_does_not_burst_after_a_gap`). `RateCap` is the same grid plus "a frame more
     than one period behind resynchronises it".
   - **Queues drop old, never new, and never the IDR:** the raw queue after the tee is
     `leaky=downstream` (drops the *oldest* raw frame). The encoded-side appsink used to be
     `drop=true`, which discards the oldest *access unit* (possibly the IDR a new viewer
     needs, or a reference frame); it is now `drop=false max-buffers=8`, so a stalled
     consumer blocks the encoder and the raw queue absorbs it.
4. **Encoder ceilings** are hardware limits (see the table); the cap just stops them from
   starving. Observed in the real app (rate control `vbr`, Balanced) with the viewer's VA
   decoder running on the same GPU: H.264 ~168 fps, AV1 ~186 fps, H.265 holds the full 240.
   Standalone (no decoder), `vaav1enc` does ~216 at `target-usage=4` but only ~116 at
   `target-usage=7`: AV1's `QualityLevel::Performance` mapping to `target-usage=7` was
   *slower*, not faster, on this driver. Fixed in #46: Performance is now `target-usage=2`,
   see "Encoder limits made visible" below.

### Result (release build, `session.sh`, 30 s windows, receiver on DP-2)

`capture` = `pipewiresrc` out, `into branch` = after the branch's queue (real frames),
`enc out` = encoder output, all fps; viewer = tooltip "decoded / shown" and dropped.

| codec @ fps | capture | into branch | enc out | viewer decoded/shown | dropped |
| --- | --- | --- | --- | --- | --- |
| H.264 @120 | 277 | 274 | 118 (120.0 in every undisturbed 4 s bucket) | 120 / 120 (113, 96 in the first two samples, CPU load) | 0 |
| H.264 @240 | 279 | 169 | 169 (encoder-bound) | 164-170 | 0 |
| H.265 @120 | 280 | 279 | 120.0 | 120 / 120 | 0 |
| H.265 @165 | 279 | 279 | 164.5 | 164-166 / 164-165 | 0 |
| H.265 @240 | 280 | 280 | **240.0** | 238-240 / 238-240 | 0 |
| AV1 @120 | 280 | 280 | 120.0 | 120 / 120 | 0 |
| AV1 @240 | 280 | 186 | 186 (encoder-bound) | 186 / 186 | 0 |

Before (I19): H.264 @240 3 unique fps / 169 "encoder out"; AV1 @240 3 / 183; H.265 @240
58-90 unique; H.264 @120 56 unique; the portal ceiling 140. After: every number in the
"into branch" column is real, unique frames. The bitrate shown for H.265 @120 in I19's
screenshot was 379 kbps (a stream of duplicates); it is now ~8 Mbps of real content.

### Live-verified: rotated outputs (#35)

DP-2 is rotated 180 degrees in MangoWC. The patched portal **does** send the transform:
`portal-probe.py` prints `image-orientation=rotate-180` from `pipewiresrc`. In the GPU domain
the source `vapostproc video-direction=auto` follows that tag (it is the orientation
stage, with no extra pass). Sharing DP-2 for real, the viewer shows the desktop the way the
screen is laid out (sharer window left, viewer window right, title bars on top);
sharing DP-1 is unchanged (upright cube). The System domain keeps #35's lazily spliced
stage.

### "The first capture session in a fresh process is slow": gone with the buffer-pool fix

I19 saw 46-58 fps in the first session of a fresh client, 139-235 in later ones. Reproduced
on the pre-#39 binary (same scripts, one `session.sh` with two H.264 @120 shares in one
client process): **first share 55.9 fps at `pipewiresrc`, second share 139.3 fps**, with the
branch itself at ~56 fps both times (the CPU `videoconvert` ceiling under load). In the
first share the capture rate is *pinned to the downstream rate* (flat 54-58 for the whole
30 s), i.e. the portal is being throttled by a consumer that holds buffers, which is the
2-buffer pool of root cause 1 above; in the second share the pool evidently had more slack.
The exact reason a fresh process's first share fares worse was not isolated further. What is
established: with `min-buffers=6` the capture rate is 277-280 fps in **every** first share of
a fresh process (7 fresh processes, GPU domain) and also 279 fps in the first of three
shares on the CPU domain, where the branch is still CPU-bound at ~40 fps. VA init, plugin
warm-up and the portal itself are not it: a fresh `portal-probe.py` process (always a first
session) gets 140 or 280 fps from its first second, depending only on keepalive and
`min-buffers`.

### Operating notes

- `SSS_CAPTURE_PATH=system` forces the CPU domain (A/B measurement, or a machine where
  DMA-BUF into `vapostproc` misbehaves).
- `client/scripts/portal-probe.py` gained `--src "<pipewiresrc props>"`, `--watch name,name`
  (per-element buffers/s for named elements in `--tail`) and prints every `image-orientation`
  tag; `probe-run.sh` forwards them as `SRC_PROPS` / `WATCH`.
- AV1 reports 1920x1082 in the viewer both before and after: a `vaav1enc` crop quirk, not
  from this change.

## Encoder limits made visible; Performance = `target-usage=2` (2026-10-14, #46)

### `target-usage` on Mesa radeonsi is not the VA spec's order

Raw throughput, `client/scripts/enc-bench.sh`, 1080p NV12, 8 Mbit/s (CBR and VBR gave the
same numbers), RX 7900 XTX, Mesa 26.2.4, GStreamer 1.28.8:

| target-usage | 1 | 2 | 3 | 4 | 5 | 6 | 7 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `vah264enc` | 246 | 307 | 307 | 172 | 172 | 172 | 172 |
| `vah265enc` | 482 | 536 | 536 | 476 | 476 | 476 | 476 |
| `vaav1enc` | 526 | 535 | 532 | 217 | 217 | 117 | 117 |

The driver groups the values into bands, and 7 (the spec's "fastest") was the slowest AV1
band and no faster than 4 for H.264. `QualityLevel::Performance` (Gaming and Low-latency
presets) now maps to **2**, the fastest band for all three codecs; Balanced (4) and Quality
(1) are unchanged. **Why 2 and not 1:** 1 is slower and its rate control overshoots (H.264
wrote 65 % more bytes than at 2 for the same bitrate). Quality cost of 2 is noise (H.265
PSNR 40.83 dB at 2, 40.84 at 4, 40.90 at 7 on a `testsrc2` clip). Trade-off: a driver that
follows the VA order (Intel iHD) gets a slower encode for Performance; the hint below still
tells the user when that caps the framerate. Full table and reasoning:
`EncoderBackend::launch_fragment` in `client/src/capture.rs`.

Real app, vkcube on DP-1 (280 Hz), 240 fps target, sender stats tracer: AV1 Gaming 107.8 fps
encoder output before, 240.1 after; H.264 Gaming 239.9 after.

### The encoder-limit hint

`capture::destination::EncoderLimit` (on `DestinationRuntime`, so the UI reads it through
`Destination::runtime` with no `session.rs` change): every 500 ms the destination pump
compares the encoder's output fps against `min(frames offered to the branch, target fps)`.
Offered frames are counted by a probe on the branch bin's sink pad, *before* its leaky
queue. Below 90 % for more than 3 s → limited; at or above 90 % for more than 3 s →
cleared; any non-Live state (a rebuild for a codec switch, a pause) resets it.
**Why against the offered rate, not the target alone:** the portal sends a frame only when
the screen changes, so an idle or slower monitor under a 240 target would otherwise read as
an encoder limit. No per-GPU tables: everything is measured at run time.

UI: the action bar's Live status shows `Screen · 172/240 fps` in the warning colour and its
tooltip (now always present, with the full status) adds "Encoding 172 of 240 fps" plus a
pointer to Stream settings. The stream-settings Video card shows an Info banner under
Framerate, "Your encoder reaches about N fps with H.264 on this machine.", with a "Use
H.265" button when H.265 is in `AppState::supported_codecs()` and not already the codec;
it applies through the normal `EditDestination` path. Measured: H.264 @240 Balanced → hint
at 171-173 fps; "Use H.265" → viewer 240-241 fps, hint gone.

## Cancelling the screen picker is silent (2026-10-14)

Dismissing the portal's picker (Esc, Cancel, or the picker being killed) used to surface
as `screencast portal 'Start' request failed: ... Invalid session`. Measured against the
real `xdg-desktop-portal-wlr-patched` + `wlr-share-picker`: the `Start` call itself fails,
~1 s after the dismissal, with `org.freedesktop.DBus.Error.AccessDenied: Invalid session`
(not a `Response` code). `capture::is_portal_user_cancel` classifies that, plus the
standard `ResponseError::Cancelled` and `PortalError::Cancelled`, as a dismissal;
everything else (`Failed`, `NotFound`, any other D-Bus error, missing portal) is still a
real failure. A dismissal becomes `capture::PickerCancelled` (find it with
`is_picker_cancelled`), which `session` turns into `SHARE_START_CANCELLED`: the capture is
removed and `share_error` stays empty.

---

## Idle monitors: a capture can go without a first frame (2026-10-14)

**Symptom:** sharing an idle monitor (nothing repainting) through the real portal ended
with `VAAPI encode branch ... produced no IDR (... within 5s); retrying on software`, then
`encode branch did not drain to EOS within 5s; removing it anyway`, and the share vanished.
A monitor that repaints worked.

**Root cause (measured, not inferred):** `xdg-desktop-portal-wlr` captures with
`wlr-screencopy`'s `copy_with_damage`, and the compositor only answers when the output
commits a new frame. On a monitor where nothing repaints it doesn't answer even the
*first* request, so `pipewiresrc` never receives a buffer. `GST_DEBUG=pipewiresrc:6` on
idle DP-1: stream reaches `streaming`, two buffers allocated, then `popped buffer (nil)`
forever; 0.0 fps at the tee for 20 s. One pointer movement onto the monitor produced a
frame, and from then on `keepalive-time` held ~29 fps with the pointer gone, for as long
as it was watched (60 s). So `keepalive-time` is correct but can only *repeat* a buffer it
already has — it cannot supply the first one. (Side effect seen in the same trace: with
`keepalive-time` set and no buffer ever received, `pipewiresrc` spins on a nil pop instead
of sleeping — ~500k log lines/s at `pipewiresrc:6`. Not fixable from here.)

**What the old code did with that:** built the first destination's encode branch against a
silent tee, burned the hardware encoder's 5 s IDR deadline, burned another 5 s on the
software retry, and reported the *encoder* as the failure. The drain-to-EOS warning was a
second bug: `remove_branch` injected its EOS from a `BLOCK_DOWNSTREAM` probe that only fires
when a buffer is pushed, so on a silent source every removal waited out its full 5 s.

**The fix:**

- `branch::wait_for_source_frame` — before any branch is built, wait for the `tee` to see a
  buffer (a one-shot `BUFFER` probe on its sink pad). `branch::add_branch_live_waiting`
  takes a `SourceWait`: `Indefinite` for a destination's first branch and for
  `rebuild_after_source_restart` (idle is a normal state there, and a stalled destination's
  viewers are fed the cached IDR meanwhile); `Bounded(SOURCE_FRAME_TIMEOUT)` for tier-2
  swaps (`add_branch_live`), where the old branch is still running and a dead source should
  fail back to it. The wait ends with an error if the pipeline goes to `Null`, so a stopped
  capture never leaves a task parked. The encoder's 5 s IDR deadline now only ever measures
  the encoder.
- `remove_branch` also installs an `IDLE` probe, so EOS is injected even when no buffer is
  in flight.
- `CaptureHandle::awaiting_first_frame` / `AppState::capture_awaiting_first_frame` expose
  "this capture has never produced a frame" so a UI can say *why* a destination is still
  `Starting` ("waiting for the shared screen to change"). **Nothing renders it yet.**
- `session::poll_pending_destinations` no longer raises `share_error` for a destination
  whose capture/destination the user already stopped, and stops (instead of adopting) a
  destination that finishes coming up after it was cancelled. Both windows used to be ~5 s
  and are now unbounded on an idle monitor.

**What the user sees:** sharing an idle monitor does not fail. The destination stays
`Starting` until anything on that monitor changes (pointer onto it, a window, a clock),
then comes up in ~0.2 s. Verified for real: waited 7.2 s, nudge at 7.0 s, viewer's first
frame 185 ms after subscribing, 24.5–28.4 media fps over the next 60 s untouched
(`capture::tests::real_portal_idle_monitor_share_reaches_a_viewer_and_stays_alive`).

`Why not get the first frame some other way:` it needs a compositor repaint, which a
client can't ask for through the portal. A one-shot `wlr-screencopy` `copy` (no damage)
would return immediately but bypasses the portal's consent flow; a tiny layer-shell surface
on the captured output would force a commit but is invasive. Both are options if "starts
only after the first repaint" turns out not to be good enough in practice.

### Silence watchdog (the stall protocol's other entry)

Until now only a *deliberate* source restart ever entered `Stalled`. A source that simply
went quiet left the viewer with nothing. `DestinationRuntime::stall_if_silent` (driven by
the pump's 500 ms ticker; threshold `destination::silence_threshold`, ≥3 s) now moves a
`Live`, un-paused destination to `Stalled(SourceLost)` after that much silence, so the
cached-IDR keepalive starts; the first frame back also forces an IDR (a delta can't end a
stall). With `keepalive-time` a healthy static screen never trips it. The silence clock is
restarted on resume-from-pause and after a swap so those gaps don't count.

---

## Resuming a paused destination

A destination with zero subscribers pauses its own encoder by closing that branch's
`valve` (idle-stream optimization), and resumes when a subscriber returns. Resuming must
put the viewer back at the **live edge**.

`Why that needs saying (history):` until 2026-10-14 the branch's `videorate drop-only=false`
held the destination's configured framerate on a static screen by duplicating the last frame
it saw. The `valve` sat directly *upstream* of that `videorate`, so a pause was
indistinguishable from a silent source — and with `max-duplication-time=0` (no limit) the
moment the valve reopened, `videorate` synthesised one duplicate of the frozen pre-pause
frame for **every frame period of the entire pause** before letting a live frame through.
Measured on a 35 s pause: ~1000 stale duplicates encoded, encrypted and sent, which the
viewer's `decode::PacingState` then paced out by their (evenly spaced, entirely valid) PTS
— a real-time replay of the whole pause showing one frozen image. The user-visible symptom
was *"resuming streams doesn't work; the streamer always has to restart"*. It was first
bounded with `BRANCH_MAX_DUPLICATION_TIME_NS` (1 s); #39 removed the cause: a branch no
longer duplicates anything (`branch::RateCap` only drops), so a paused branch simply
produces nothing and the first live frame after the valve reopens is the live edge, with
no bound to tune. The `RateCap` grid resynchronises on a gap rather than chasing it (a
`videorate drop-only` grid does chase it: see the burst measurement in "Capture/encode
performance"). Measured before the change: 136 ms from resubscribe to first decoded frame,
after both a 35 s and a 120 s pause.

---

## Encoder tuning controls

**Added 2026-07-28.** Three more per-destination live-editable settings, alongside
resolution/framerate/bitrate: **Tune** (Low-Latency / Best Quality — `BestQuality` default),
**Quality Level** (Performance / Balanced / Quality — `Balanced` default), and **Rate
Control** (Constant / Adaptive / Quality — `Constant` default). Defined as `TuneMode`,
`QualityLevel`, `RateControlMode` in `capture.rs`, exposed via `edit_destination`'s
`tune`/`quality_level`/`rate_control` params (snake_case strings, same convention as every
other `edit_destination` param) and as `pillSwitch` controls in the destination edit panel
(`stream-control.js`/`components.js`).

`Why the defaults are `BestQuality`/`Balanced`, not either speed/quality extreme` — the
original product requirement (from this feature's owner) was explicit: "Low latency should
be an option, but quality also has to be achievable. By default, it should be a good mix."
The pipeline's original hardcoded behavior was `tune=zerolatency` + `speed-preset=ultrafast`
— the fast/low-quality *extreme*, not a middle ground, and an audit (2026-07-28) confirmed
this as a real product bug against that requirement. The fix: `TuneMode::BestQuality` (no
`tune=zerolatency`; trades a little latency for meaningfully better compression) and a new
`QualityLevel::Balanced` variant — `speed-preset=medium`/`target-usage=4`, the actual
midpoint between `Performance`'s `ultrafast`/`7` and `Quality`'s `veryslow`/`1` (`medium` is
also x264's own upstream default preset) — are now the `#[default]`s, with `LowLatency` and
`Performance` demoted to explicit opt-ins for whichever extreme a user actually needs.
`RateControlMode::Constant` (CBR) stays the default untouched: CBR-by-default is the correct
choice for live-streaming bandwidth predictability on its own merits, not an extreme that
needed fixing. Regression coverage for the new mapping:
`capture::tests::quality_level_maps_to_speed_preset_and_target_usage` (asserts `Balanced` →
`medium`/`4` on both backends) and `capture::destination::tests::tuning_knob_changes_are_tier_two`.

### Simple/advanced split in the edit panel (2026-07-28)

**The problem:** these three switches (plus the bitrate slider) started out fully
unconditionally visible on every destination's edit panel, all the time — reasonable for the
"testing-oriented encoder tuning knobs" they were originally built as, but a real UX bug
against this project's own requirement ("make it easy for regular users and powerful for
advanced users"): a regular user opening a destination card to just check/nudge the bitrate
was shown Tune/Quality Level/Rate Control jargon they don't need.

**The fix — two tiers, per destination card:**

- **Simple (always visible):** a new **Preset** `pillSwitch` — *Low-Latency / Balanced /
  Quality* — sitting above the existing Bitrate slider. This is a convenience layer over the
  same three raw fields below, not a fourth field of its own: picking a preset sets
  `tune`/`quality_level`/`rate_control` together through the same `onEditApply`/
  `edit_destination` call any individual raw switch already used. Mapping (`components.js`'s
  `destinationCard`, `PRESETS` const):

  | Preset | `tune` | `quality_level` | `rate_control` |
  | --- | --- | --- | --- |
  | Low-Latency | `low_latency` | `performance` | `constant` |
  | Balanced (default/middle option) | `best_quality` | `balanced` | `constant` |
  | Quality | `best_quality` | `quality` | `quality` |

  The active preset button is computed live from the destination's current
  tune/quality_level/rate_control (highlight whichever preset's 3 fields match exactly); if
  the Advanced section was hand-tuned to some other combo, none of the 3 preset buttons
  shows selected — an intentional, unvalidated state, not an error.

  `Why: "balanced" as a `quality_level` string` — at the time this preset selector was
  written, `parse_quality_level` (`dto.rs`) only accepted `performance`/`quality`; a
  `QualityLevel::Balanced` variant parsed from `"balanced"` was landing concurrently from a
  sibling branch (`fix/balanced-encoder-defaults`). The frontend was written against the
  string the backend would accept once merged, rather than blocking on it. That branch has
  since landed `QualityLevel::Balanced` (`"balanced"`, mapping to `speed-preset=medium`/
  `target-usage=4`) as the backend's own `#[default]`, and extended the raw Quality Level
  `pillSwitch` in the Advanced section below to a three-way Performance/Balanced/Quality
  switch — so this preset selector's "Balanced" button and the raw switch's own "Balanced"
  option now agree, and picking either applies cleanly server-side.

- **Advanced (collapsed by default):** the raw Tune/Quality Level/Rate Control `pillSwitch`
  rows, unchanged in behavior/values, moved inside a native `<details class="dest-advanced">`
  /`<summary>` disclosure ("Advanced tuning", chevron affordance in `stream-control.css`).
  `<details>` was chosen over hand-rolled show/hide state because this component library has
  no existing disclosure primitive (`pillSwitch` is an exclusive-choice picker, not a
  toggle-visibility one) and because each destination card gets its own `<details>` element,
  so expand/collapse state is naturally independent per card with zero extra state to manage.

  `Why the caller threads `advancedOpen` back in:` `stream-control.js` rebuilds a
  destination's card element from scratch (a fresh DOM subtree) on *any* edit-state change,
  including a click on a control inside Advanced itself — a brand-new `<details>` always
  starts closed, so without extra care the section would visibly snap shut right after the
  very click that used it. The fix: before replacing a card, the caller reads the outgoing
  `.dest-advanced` element's `.open` and passes it back in as `opts.advancedOpen` to the new
  `destinationCard(...)` call, so the disclosure survives its own re-render. The
  `dest-edit-error` line (shown when an `edit_destination` call fails) stays outside/below the
  `<details>` regardless of its state — it can be set by the always-visible Bitrate slider
  too, not just an Advanced-only control, so it must stay visible independent of whether
  Advanced is expanded.

**Why these are tier 2 (`BranchSwap`), not tier 1 (a live property write) — including for
`RateControlMode::Adaptive`, which turns out to be a no-op on one backend:** none of
`x264enc`/`vah264enc`'s tuning properties (`speed-preset`/`target-usage`,
`tune`, `rate-control`/`pass`) are safely writable on an already-`Playing` element the way
`bitrate` is — `EncoderBackend::launch_fragment` bakes them into the element's
*construction* string. So "change one of these" is definitionally "build a new encoder,"
which is exactly what a branch swap already exists to do safely (make-before-break, wait
for the new branch's first IDR, flip authority, drop the old one — see "Editing a live
destination" above). Routing these three through the *same* `classify_change`/
`swap_to_new_branch` machinery every other tier-2 edit uses means they inherit the
readiness gate and the `KeptOld`-on-failure safety net for free, with zero new
state-machine code.

### Four bugs found in owner testing of the edit panel, fixed (2026-07-28)

The first hands-on test of the panel above (by this project's owner, on a real share)
turned up four separate defects. All four are fixed; recorded here because three of them
are the kind of mistake that is easy to make again, and the fourth was a genuine
stream-killer with a non-obvious cause.

**1. The whole panel only appeared in Per-Stream Maximum mode.** A single flag,
`perStreamBitrateEditable` (`stream-control.js`), gated *both* "should this destination
have an edit panel at all" and "should that panel contain a Bitrate slider". Those are two
different questions and only the second one has anything to do with the bandwidth mode:
the allocator owns **bitrate**, and only bitrate, so Preset/Framerate/Tune/Quality Level/
Rate Control are equally meaningful and equally safe to apply in either mode. Split into
two flags — `encoderTuningEditable` (any non-audio destination, both modes) gates the
panel, `perStreamBitrateEditable` (Per-Stream + non-audio) gates only the Bitrate row.
Absolute mode's read-only `Allocated: N kbps` line moved out of the panel's `else` branch
so it still renders now that the panel itself always does. In Absolute mode an edit that
doesn't touch bitrate sends the destination's *currently reported* bitrate rather than the
edit buffer's copy, so touching the preset can't briefly stomp the allocator's live value
with a stale number.

**2. No way to change framerate on a running stream.** `edit_destination` has always taken
a `framerate`, and `applyDestinationEdit` has always sent one — but hardcoded to the value
the destination already had, so it was a no-op on every call and there was no control
anywhere in the tab. Added a **Framerate** row to the always-visible half of the panel
(alongside Preset/Bitrate, not behind Advanced — framerate is a regular-user concern, the
raw Tune/Rate Control knobs are not). It is a `<select>` rather than a `pillSwitch`: it
offers the same nine values the "Start a Stream" form does (15…280), which is far too many
for a pill row, and reusing that form's own control keeps "the framerate picker" one
recognisable thing in both places. A destination whose current rate isn't one of the nine
gets it appended, so the control never misreports what is actually running.

**3. The Bitrate slider snapped back to the old value after every change** — while the
backend really had applied it (observed server-side). Two causes stacked:

- The slider was constructed from `dest.bitrate_kbps` (the `DestinationRow` DTO) rather
  than from the local edit buffer. The card is rebuilt from scratch whenever its `sig`
  changes and the edit buffer *is* part of that `sig`, so applying a value immediately
  re-rendered the card — rebuilding the slider from the value the server had last
  reported, i.e. the old one.
- That stale value never self-corrected, because a bitrate-only edit is **tier 1** (a live
  encoder property write): the destination never leaves `live`, so `driver.rs` never emits
  `destination-state-changed`, so `shell.js` never calls `StreamControl.refresh()`, so
  `mockDestinations` is never re-fetched. The DTO's `bitrate_kbps` stayed wrong
  indefinitely.

Fixed by making the local edit buffer (`destEditState`) the single source of truth for
*every* control in the panel — bitrate and framerate included, joining the three tuning
knobs that were always caller-held because `DestinationRow` has no fields to read them back
from. A successful apply also mirrors the applied values onto the local DTO copy so the
card's `BITRATE`/`FRAMERATE` readouts agree with its controls.

**4. Changing the preset made the FPS shoot to 300+ and sometimes killed the stream.**
The serious one. Two independent defects in the tier-2 branch swap, both of which scale
with **how long the share has already been running** — which is why single-knob changes on
a freshly started capture had never shown them.

*Ruled out first, so it doesn't get re-investigated:* the framerate echoed back in every
`edit_destination` call is **not** a feedback loop. `DestinationRow::framerate` comes from
`destination_rows()` → `Destination::live_params()` → the runtime's configured
`VideoParams::framerate`, never a measured rate; verified by instrumenting the real
pipeline, where it stayed pinned at the configured value throughout.

- **The runaway rate: `videorate` replayed the whole share.** Every branch this codebase
  builds is added to an *already running* pipeline (a tier-2 swap, a fan-out/transfer
  destination, a post-source-restart rebuild — never at pipeline construction), and it
  inherits the capture's segment. Its `videorate` anchored its output timeline on that
  segment start rather than on the first buffer it actually received, decided it was late
  by the capture's entire elapsed lifetime, and raced to catch up — emitting one real
  encoded access unit per frame period of that whole history as fast as the encoder could
  produce them, each of which `destination_pump` then encrypts, fragments and pushes at the
  relay. Fixed with `videorate skip-to-first=true` on every encode branch
  (`BRANCH_SKIP_TO_FIRST`). Note this is **not** what `max-duplication-time` bounded — that
  governed duplication between two buffers the branch actually received (the pause/resume
  replay bug it was added for) and was already set to one second while the branch replayed
  ten. (Since #39 the branch has no `videorate`; `branch::RateCap` anchors its grid at the
  first admitted frame, which is the same guarantee.)

- **The wire clock broke at the seam.** Because that replacement branch's timeline starts
  back near the capture's start, every buffer it produced mapped to a wire PTS *below* the
  last one already sent, so `DestinationRuntime`'s monotonic clamp took over and the stream
  advanced **+1µs per frame** until the branch's raw PTS climbed back past where the old
  branch left off — i.e. for the whole age of the share. Nothing goes backwards (the clamp
  works), but minutes of frames arriving effectively simultaneously is not something a
  viewer's pacer can render. Fixed by re-anchoring at the swap's flip:
  `DestinationRuntime::on_branch_swapped(first_buffer_pts_us)`, called from
  `branch::swap_to_new_branch` inside the same lock as the authority flip. It anchors on
  the last PTS actually put on the wire (not on the destination's epoch, whose subtraction
  saturates to zero against a source handing out large PTS values) and it corrects the
  offset **only** when the seam really does land in the past, so a swap whose timeline is
  already continuous pays nothing.

*How this was verified:* a real-hardware regression test,
`capture::tests::applying_a_preset_never_bursts_a_branch_above_its_configured_framerate`,
drives a genuine 1920×1080 desktop capture through this machine's real encoder backend
(VAAPI here), lets it run, then applies the "Balanced" preset — `tune` + `quality_level` +
`rate_control` changed together in one edit, asserted to classify as `BranchSwap` — while
draining the branch exactly the way `destination_pump` does, including running every buffer
through the destination's real wire clock. Red before the fix: **peak 76fps** on a
3-second-old capture and **181fps** (~350fps instantaneous) on a 10-second-old one against
a configured 30, with 180 of 181 post-swap frames clamped to +1µs. Green after: a flat
30.5fps peak and zero clamped frames. `DestinationRuntime`'s side is additionally covered
by two pure unit tests (`a_branch_swap_keeps_real_frame_spacing_when_the_new_branch_
restarts_its_timeline`, `a_branch_swap_that_continues_the_timeline_leaves_the_wire_clock_
alone`).

*Two existing tests had to be adjusted*, both because they were leaning on the very burst
that was removed. `a_long_pause_resumes_at_the_live_edge_instead_of_replaying_the_gap` read
its pre-pause PTS only from what was still in flight *after* closing the valve, which the
start-up burst used to guarantee; it now samples a short window of live output first.
`a_failed_swap_keeps_the_old_branch_and_never_stalls` asserted the original branch was
"still producing" after a failed swap — but the 1×1 negotiation failure it injects sends
`NOT_NEGOTIATED` back up through the shared `tee` and kills `videotestsrc` outright, so
nothing on that pipeline can produce anything afterwards; it passed only because a
straggler from the burst happened to be queued. It now asserts what `KeptOld` actually
promises: the caller gets its *original* branch back, still wired into the pipeline and not
nulled out.

### A follow-up freeze after the burst fix above, root-caused and fixed (2026-07-28)

The project owner tested the burst fix above for real and reported a *different, worse*
symptom: "changing the FPS still sometimes hangs the screenshare, same for switching
presets. It gets especially bad when the Hz jump is big. Changing presets seems almost
random. The freeze happens on the sharer side." — a genuine hang (the pipeline stops
producing entirely), not the burst.

**What it was not**, despite extensive real-hardware investigation (hundreds of branch
swaps stress-tested against a real `ximagesrc` capture — small/large framerate jumps, every
tune/quality/rate_control combination, both encoder backends, cold and minutes-warm
branches, rapid back-to-back "double-fire" edits): the branch-swap machinery itself
(`branch::swap_to_new_branch`, `add_branch`/`add_branch_live`, `remove_branch`) never hung
in isolation. One real, bounded, worst-case cost *was* found and characterized along the
way — a software `x264enc` with `QualityLevel::Quality` (`speed-preset=veryslow`) at a low
framerate can take several real seconds (measured up to ~5s, right at
`FIRST_IDR_TIMEOUT`'s boundary) to produce its first frame, because how long its lookahead
buffer takes to fill is wall-clock time divided by framerate — but that path was already
handled correctly (a clean, timely error, no hang), so it wasn't *the* bug.

**What it actually was**, caught with a live `gdb` backtrace while reproducing
`end_to_end_test::late_watch_after_all_three_tuning_controls_toggled_gets_a_real_keyframe_
every_time` (which was stuck for 4+ minutes with zero progress before the fix below): the
sender-side self-preview tee branch (`SELF_PREVIEW_SINK_NAME`, `SELF_PREVIEW_BRANCH_
FRAMERATE` = 12fps — see "Self-Preview" doc) carried the **full negotiated source
resolution** (e.g. 1920×1080) all the way to `capture::sample_to_color_image`, which
converts each sample with a plain per-pixel Rust loop, synchronously, on `capture_loop`'s
own task — the same task/runtime every destination's `destination_pump` (and therefore
every branch-swap `ApplyParams` reply) shares. At 12fps that is up to twelve
multi-megapixel synchronous conversions per second, with no `.await` anywhere inside the
loop to let anything else on the runtime run in between. The self-preview doc comments
already called this "a small preview panel"/thumbnail and the UI already scales the
resulting texture down for display — the pipeline itself just never actually enforced a
small size, so every conversion paid the full source's pixel count for nothing. The
backtrace showed the runtime's own worker thread caught mid-loop inside
`sample_to_color_image` at the exact moment the test believed a branch swap was hung; a
`spawn_blocking`-based hardening (below) around the swap machinery's own two remaining
unbounded GStreamer calls did **not** by itself fix this — the starvation source was
self-preview, not the swap.

*Why "worse with a bigger Hz jump" and "presets look almost random":* neither framerate nor
preset choice caused this loop to run — the self-preview branch's own rate is fixed at
12fps regardless of the destination's configured framerate — but both raise overall CPU
pressure on the same runtime (a bigger jump or a heavier preset means more encoder work
competing for the same threads), which widens the window in which a self-preview
conversion happens to be in flight exactly when a swap's reply is due, i.e. it makes an
existing but previously-rare starvation coincidence easy to hit. This is also why it wasn't
caught by the burst-fix pass above (that pass never touched self-preview or changed timing
around it) and wasn't reproducible through the branch-swap machinery alone, however hard it
was stress-tested in isolation.

**The fix**, `capture.rs`: added `SELF_PREVIEW_MAX_WIDTH`/`SELF_PREVIEW_MAX_HEIGHT` (320×180)
and a `videoscale ! video/x-raw,width=...,height=...` stage to the self-preview branch in
`build_pipeline`, so the appsink — and therefore `sample_to_color_image` — never sees more
than a genuine thumbnail's worth of pixels, regardless of the real source resolution.
Verified: the same previously-4+-minutes-stuck test now completes all 10 rounds in ~27s,
repeatably, across multiple runs.

**Hardening added alongside** (defense in depth, not the root-cause fix, but a real gap
closed while investigating): `branch::remove_branch`'s final `pipeline.remove`/
`bin.set_state(Null)`/`release_request_pad` tail and `branch::add_branch_live`'s
`add_branch_with_fallback` call were the only two remaining *unbounded* blocking GStreamer
calls in the whole swap path (every other wait in this module — the IDR wait, the EOS
drain — already had a timeout). Both now run on a `tokio::task::spawn_blocking` thread,
bounded by `BRANCH_TEARDOWN_TIMEOUT` (10s) / `BRANCH_CONSTRUCTION_TIMEOUT` (20s)
respectively: if a GStreamer/driver-internal state change ever does stick, the destination
now gets a loud, timely error instead of an unbounded stall of the async runtime it shares
with every other destination and the bus watcher.

*Permanent regression coverage:* `capture::tests::self_preview_branch_is_capped_to_a_small_
thumbnail_regardless_of_source_size` (fast, deterministic — asserts the self-preview
branch's negotiated caps never exceed the thumbnail size, given a 1920×1080 source) is the
primary guard, since a timing-based hang detector would itself be exactly the kind of
flaky-under-load test this bug already made hard to catch. The pre-existing
`late_watch_after_all_three_tuning_controls_toggled_gets_a_real_keyframe_every_time`
(`#[ignore]`d, real relay + real capture) now also serves as a real-world confirmation that
a rapid tune → quality_level → rate_control sequence stays fast.

### A third freeze report, this time genuinely on the viewer side — root-caused and fixed (2026-07-29)

After the two fixes above (the runaway-fps burst, then the sender-side self-preview
starvation freeze), the project owner reported a *third* symptom, worded almost identically
to the second but this time pinned to the other end of the pipe: "Sometimes, when doing big
FPS jumps (setting a different fps as the streamer), it still freezes (on the viewer side)."
Both prior fixes are sender-side (`capture.rs`); this one is not — it lives entirely in
`decode.rs`, and neither prior regression test exercises it, because both of the
pre-existing branch-swap regression tests
(`capture::tests::applying_a_preset_never_bursts_a_branch_above_its_configured_framerate`,
`end_to_end_test::late_watch_after_all_three_tuning_controls_toggled_gets_a_real_keyframe_
every_time`) deliberately hold `framerate` **constant** across their swap (only
`tune`/`quality_level`/`rate_control` change) and neither has a viewer actively decoding
across the seam — they prove the sender keeps producing correctly, not that an
already-watching viewer keeps *displaying* correctly through a real fps change.

**Root cause**: `decode.rs`'s video pipeline (`appsrc ! queue ! h264parse ! avdec_h264 !
videoconvert ! appsink`) built its `queue` element with `ElementFactory::make("queue").build()`
— GStreamer's plain defaults (`leaky=no`, `max-size-buffers=200`, `max-size-time=1s`). That is
not a "drop and continue" element, it is a *bounded, blocking* one: once full, it blocks the
thread pushing into it rather than dropping anything. `avdec_h264` here is software-only (no
hardware decode path in this pipeline), so a big fps jump — or really any sustained rate above
what this viewer's CPU can decode in real time — fills that queue within about a second, and
every subsequent `appsrc.push_buffer()` call in `pump_loop` then blocks synchronously until
decode frees up space. From `StreamDecoder::feed()`'s side this is invisible (it's a
non-blocking `try_send` into its own small bounded channel, which just starts silently
dropping once full, exactly as designed) — but the actual GStreamer pipeline keeps grinding
through an ever-growing, ever-more-stale FIFO backlog in strict order at whatever rate decode
can sustain, which is a real, multi-second, repeatable visible freeze followed by slow-motion
catch-up, not the graceful degradation `decode.rs`'s own `PacingState` is designed to provide.
`PacingState` cannot help here at all — it only ever sees a frame *after* GStreamer has already
decoded it, so a backlog stuck in this queue, upstream of the decoder, never reaches it.

This is the exact same failure class `capture.rs`'s `ENCODER_QUEUE_MAX_BUFFERS` doc comment
already describes and fixes on the *encode* side ("without a small bounded/leaky config there,
a real capture source's incoming raw-frame rate spiking under motion can outpace the encoder
and the queue's own defaults, blocking the whole pipeline solid rather than dropping raw
frames") — `decode.rs` simply never had the receive-side analog; every encode-branch queue in
`capture.rs` is `leaky=downstream` with a small `max-size-buffers`, but the one queue in
`decode.rs` was not.

**The fix**: the decode pipeline's `queue` now sets `leaky=downstream` +
`max-size-buffers=DECODE_QUEUE_MAX_BUFFERS` (4 — a little more slack than the encode side's
`ENCODER_QUEUE_MAX_BUFFERS=2` since this queue holds small compressed access units rather than
raw frames, so a little extra jitter tolerance costs negligible memory). A struggling decoder
now skips stale, no-longer-relevant access units and keeps tracking the live edge instead of
blocking. Dropping a compressed access unit does mean visible corruption until the next real
IDR (a delta frame whose reference got skipped can't decode cleanly) — but that's bounded by
the stream's own GOP length (seconds, self-healing) rather than an unbounded/growing-latency
block, and matches the tradeoff this project already accepts for a stall/pause/resubscribe gap
elsewhere (see `destination::STALL_RESEND_INTERVAL`'s doc comment).

**Repro and verification**: real relay + real `videotestsrc`-backed capture + a real
`StreamDecoder`, forcing the software backend at 1920×1080 (VAAPI has enough headroom on the
dev machine to hide the bug entirely — the repro needs decode to actually be the bottleneck) —
`end_to_end_test::big_fps_jump_mid_stream_keeps_viewer_frames_flowing`. A viewer already
watching gets jumped 15fps → 280fps (`SourceRestart` tier) then back down 280fps → 15fps
(`BranchSwap` tier); asserts the gap between two consecutive newly-decoded frames never exceeds
2s. Before the fix: max gap **3.1-3.5s**, only 4-12 frames decoded out of ~2000 received over an
8s window. After the fix: max gap **0.5-0.8s**, 22-45 frames decoded over the same window — a
real, measured improvement, not just "no longer times out". The two pre-existing branch-swap
regression tests and `go_live_attaches_the_first_destination_with_real_resolution_and_backend`
were re-run and still pass; this fix touches only `decode.rs`, nothing sender-side.

### Backend mapping — real, and asymmetric between backends

| Control | `x264enc` (Software) | `vah264enc` (Vaapi) |
| --- | --- | --- |
| Tune | `tune=zerolatency` (LowLatency) / unset (**BestQuality, default** — x264enc has no explicit "best quality" value, only the *absence* of the low-latency one) | **No equivalent control.** The field is still accepted on any destination — it simply has no effect until/unless that destination's live branch is `Software` |
| Quality Level | `speed-preset`: `ultrafast` (Performance) / `medium` (**Balanced, default** — x264's own upstream default preset) / `veryslow` (Quality) | `target-usage`: `2` (Performance; `7` until #46, which is slower on Mesa radeonsi — see "Encoder limits made visible") / `4` (**Balanced, default**) / `1` (Quality) |
| Rate Control | No native enum — `Constant` (default)/`Adaptive` are both plain `bitrate=` (**`Adaptive` is a deliberate no-op relative to `Constant`** — x264enc's CBR is already frame-adaptive within its budget, so there's no meaningfully different x264enc setting to reach for); `Quality` switches to `pass=qual quantizer=21` (bitrate-target `bitrate=` is then ignored) | `rate-control=cbr`/`vbr`/`cqp` — a genuine three-way native enum, no asymmetry |

`Why the `Adaptive`/`Software` no-op is documented rather than faked with some invented
x264enc setting:` a real asymmetry between backends is more honest recorded plainly than
silently pretended symmetric — a destination on the software backend showing "Adaptive"
selected and behaving identically to "Constant" is the accurate picture of what's actually
happening, not a bug to paper over.

**Superseded 2026-07-30 — see the section below.** The `Rate Control` row above (three-way
`Constant`/`Adaptive`/`Quality`) describes the *original* design; it has since collapsed to a
two-way `Cqp`/`Qvbr` choice. Left in place rather than rewritten because the "why" reasoning
above (the `Adaptive`/software no-op asymmetry, CBR-by-default for predictability) is still
accurate history for the design that predated the collapse.

### Rate control collapses from three modes to two: CQP / QVBR, new default, new Gaming preset (2026-07-30)

**Why:** the project owner's own real-world usage of all three modes: `Constant` (CBR)
"wastes bandwidth for no visible gain" on easy content (CBR pads to the target bitrate
regardless); `Adaptive` (VBR) showed visible "pop-in" artifacts — traced to
`target-percentage` never being set, so VBR's real target swings between its default 66%
and 100% of the configured ceiling as content changes; `Quality` (CQP) was "the best of the
three, but barely uses the available bandwidth" and never sharpens a static/still image over
time — expected, since CQP targets a fixed quantizer (`qpi`/`qpp`/`qpb`, left at the element
default of 26) with no bandwidth-ceiling awareness and no scene-change/refinement feature at
all (`vah264enc`'s full property list has none to reach for).

**The fix:** `RateControlMode` is now `Cqp`/`Qvbr` (wire strings `"cqp"`/`"qvbr"`) — `Constant`
and `Adaptive` are gone, not just hidden. `Cqp` keeps the old `Quality` mode's behavior
unchanged (still the "resourceful"/predictable-bandwidth choice). `Qvbr` is new: `vah264enc`'s
`rate-control=qvbr` ("quality-defined VBR") — a mode this codebase had never used before,
confirmed to exist via `gst-inspect-1.0 vah264enc` on the project's real AMD RX 7900 XTX. QVBR
targets quality (`qpi=20`, a deliberate step down from CQP's 26) while still treating
`bitrate` as a real ceiling and `cpb-size` (set to 1x the configured bitrate) as the VBV
buffer — quality-driven but genuinely bandwidth-bounded, which is what "adaptively use as much
of the available bandwidth as possible for good quality" actually requires given what this
specific encoder element supports. **`Qvbr` is now the default** (not `Cqp` — the owner's
explicit call: maximize quality by default, not conserve bandwidth by default).

The Advanced panel's raw `Rate Control` `pillSwitch` now shows two plain-language options,
"Efficient" (Cqp) and "Max Quality" (Qvbr), replacing the old three raw mode names. The four
`ENCODER_PRESETS` (`components.js`) were re-assigned per-preset rather than blanket-defaulted:

| Preset | `tune` | `quality_level` | `rate_control` | GOP |
| --- | --- | --- | --- | --- |
| Low-Latency | `low_latency` | `performance` | **Efficient (Cqp)** | 2000ms |
| Balanced (default) | `best_quality` | `balanced` | **Max Quality (Qvbr)** | 2000ms |
| Quality | `best_quality` | `quality` | **Max Quality (Qvbr)** | 2000ms |
| Gaming (new) | `low_latency` | `performance` | **Efficient (Cqp)** | **750ms** |

Gaming uses Cqp over Qvbr deliberately: constant motion rarely gives Qvbr's implicit
"spend leftover bandwidth budget on a static/still frame" upside anything to work with, while
Cqp's fixed per-frame quantizer holds up more predictably under continuous motion. Gaming's
750ms GOP (vs. every other preset's 2000ms) trades a little bandwidth for faster recovery from
motion-induced artifacts.

**GOP is now preset-driven, not a single global constant.** Previously every preset shared one
hardcoded `GOP_SECONDS = 2` regardless of its other settings. `gop_millis: u32` is now threaded
end-to-end: `capture::destination::VideoParams`/`branch::EncodeSettings` →
`classify_change` (a `BranchSwap` trigger, same tier as the other three tuning knobs) →
`add_branch`'s frame-count formula → `session::DestEdit`/`AppState` →
`dto::StagedCaptureRow`/`edit_destination` → a new `set_gop_millis` command → the frontend.
Deliberately **not** persisted to `settings.toml` — it's a preset-specific tradeoff, not a
standalone remembered setting, so `stageVideoShare()` always resets it to 2000ms before
staging a fresh capture.

**Left unwired, flagged rather than guessed at:** `ref-frames` — the same IPC-plumbing cost as
`gop_millis` for a knob that wasn't asked for; a real follow-up if lower ref-frame counts turn
out to matter for Gaming's decode latency.

Regression coverage: `rate_control_mode_maps_per_backend` (rewritten to assert `qpi`/
`cpb-size` are genuinely different between Cqp/Qvbr on real VAAPI, not just that the enum
value round-trips) and `tuning_knob_changes_are_tier_two` (extended to cover `gop_millis`
too), both in `capture.rs`.

### Investigated: "For QVBR, the max bitrate is entirely ignored" — confirmed real, root cause is a driver limitation, not a simple bug (2026-07-30)

**Owner's report, verbatim:** *"For QVBR, the max bitrate is entirely ignored."*

**Investigated live, not guessed at.** First ruled out the property-string theory from the bug
report: constructed the exact same `vah264enc ... rate-control=qvbr bitrate=N qpi=20
cpb-size=N` fragment via `Gst.parse_launch` with the properties in every plausible order
(`bitrate` before/after `qpi`/`cpb-size`, `rate-control` set last) and read every property back
off the live element both at construction and while `Playing` — `bitrate`, `cpb-size`, `qpi`,
`rate-control` all held exactly the configured values in every ordering, at every point. This
codebase's own pipeline string was never the problem; GStreamer's property-string parsing has no
ordering quirk here.

**Then measured actual encoded output**, real `vah264enc` on this machine's real AMD RX 7900 XTX
(radeonsi/Mesa, VA-API 1.24), with a pad probe summing real encoded bytes/sec at 1920×1080/30fps,
across content complexity and two very different `bitrate` ceilings (2000 vs 8000 kbps) at the
shipped `qpi=20`:

| Content | Ceiling 2000 kbps | Ceiling 8000 kbps |
| --- | --- | --- |
| Static test pattern (`smpte`) | 19034 kbps | 19207 kbps |
| Moving content (`ball`) | 75 kbps | 75 kbps |
| Worst-case incompressible noise (`snow`) | 159239 kbps | 159239 kbps |

**The ceiling has zero measurable effect on output, in every case, confirmed on the live
element.** This is not "QVBR rarely needs to hit the ceiling on real content" (the static-pattern
row is *already* 9-10x over an 8000 kbps ceiling — the opposite of a ceiling that rarely binds,
it's a ceiling that never binds at all) and it is not fixable by retuning `qpi` (no `qpi` value
changes whether `bitrate`/`cpb-size` are honored — they simply aren't, regardless).

**A/B confirmation this is `qvbr`-specific, not a general VAAPI/property bug**: the same harness
against `rate-control=cbr` and `rate-control=vbr` on identical `smpte` content genuinely scaled
with the ceiling (`cbr`: 1886 kbps @ ceiling 2000, 7922 kbps @ ceiling 8000; `vbr`: 1878 / 7922,
same numbers) — proving this machine's VA-API driver/hardware *can* and *does* respect a bitrate
ceiling, just not under `rate-control=qvbr`. `rate-control=cqp` (this project's other mode) showed
the identical "ignores everything but its own quantizer" shape as `qvbr` (18295 kbps regardless of
ceiling) — expected and correct for `cqp`, which was never supposed to respect a ceiling; the
finding is that on this driver, `qvbr` behaves like `cqp` (quantizer-only, no real ceiling) rather
than like its own documented "quality within a bandwidth ceiling" contract. `GST_DEBUG=va*:6`
shows `vah264enc` computing and logging a max/target bitrate internally (`_ensure_rate_control`:
"Max bitrate: 3030 kbps, target bitrate: 2000 kbps") and a driver warning
(`Driver does not support attribute VAConfigAttribEncQuantization`) — GStreamer believes it's
telling the driver to cap bitrate; the driver silently doesn't. This is a genuine driver/hardware
limitation of AMD's Mesa radeonsi VA-API QVBR implementation, external to this codebase, not a
property-ordering bug, not a units mismatch, and not fixable by tuning `qpi`.

**The real fix would trade one already-rejected problem for another.** `rate-control=vbr` *does*
respect the ceiling (measured: static/simple content ~19 kbps, detailed-static content scaling up
to ~5900 kbps under a 6000 kbps ceiling, worst-case noise content briefly exceeding the ceiling by
~58% under a 6s burst — ordinary encoder HRD-compliance slop, not a broken cap) — appearing to be
the "real fix." But `vbr` is exactly the mode this same file's "Rate control collapses from three
modes to two" section (immediately above, same date) documents this project deliberately moving
*away* from, because the owner's own real-world testing found `vbr`'s (then `Adaptive`'s) bitrate
swinging between its `target-percentage` and the ceiling as content changes produced visible
"pop-in" artifacts. `qvbr` was adopted specifically *because* it promised quality-driven,
ceiling-bounded encoding without that swing — a promise this investigation now shows this
hardware's driver does not keep. Switching back to `vbr` would make the ceiling real again at the
cost of reintroducing the exact swing/pop-in behavior this project already rejected once; staying
on `qvbr` keeps the smooth quality but the ceiling stays fictional. `target-percentage` retuning
(tested 50 vs 66, the only two values in its valid 50-100 range near the low end) made no
measurable difference in these tests, so it isn't a free way out of the tradeoff either — but this
was only measured as aggregate bitrate over fixed windows, not as subjective visual "pop-in", so
it doesn't rule out `vbr` with a different `target-percentage` being an acceptable middle ground.

**Not fixed — escalated per this task's own instructions.** No code change was made in
`capture.rs`: this is the "genuine `qpi`-vs-`bitrate` design tension" (in fact one level worse —
not a tuning tradeoff but a hardware capability gap) that the task explicitly said to stop and
report on with real data rather than guess at, and the two known real options (keep `qvbr`'s smooth
quality with a non-functional ceiling, or switch to `vbr` and reintroduce its previously-reported
swing/pop-in) directly re-litigate a tradeoff this project already made a considered call on once.
Whoever picks this up next should decide with the owner: accept `qvbr`'s ceiling as
aspirational/best-effort (and say so in the UI/docs), switch to `vbr` and re-test for the pop-in
artifact specifically (possibly with a lower `target-percentage` than the untuned 66% that caused
the original complaint), or investigate a non-default combination (e.g. `vbr` with a tightly-set
`cpb-size` as a hard VBV cap) not yet tried here.

### Follow-up: switched "Max Quality" to `vbr` with a tight `cpb-size` + high `target-percentage` — ceiling now genuinely works, pop-in measurably reduced (2026-07-30)

**The owner's decision, given the tradeoff above:** option (b) — try `vbr` again, but with a
tighter `cpb-size` than the naive 1x-bitrate buffer, on the hypothesis that a smaller VBV buffer
forces more frequent, finer-grained rate adjustment and might avoid the original pop-in mechanism
via a different lever than `target-percentage` alone.

**What changed in `capture.rs`:** `RateControlMode::Qvbr`'s VAAPI branch now emits
`rate-control=vbr` (was `rate-control=qvbr`), with `target-percentage=95` (`VBR_TARGET_PERCENTAGE`)
and `cpb-size = bitrate_kbps * 3 / 8` (`VBR_CPB_SIZE_NUMERATOR`/`VBR_CPB_SIZE_DENOMINATOR`) instead
of the old `qpi=20 cpb-size=bitrate_kbps` (1x buffer). The `qpi` property is dropped entirely —
it's a `qvbr`/`icq`-specific quality-factor knob, meaningless under plain `vbr`. `Cqp`/"Efficient"
is untouched. The enum variant keeps the name `Qvbr` despite no longer emitting literal
`rate-control=qvbr` — see `capture.rs`'s doc comment on `RateControlMode` for the full reasoning
(short version: the wire string `"qvbr"` is both the frontend's `rateControl` value in
~15 call sites and the on-disk persisted field in `AppSettings`, so renaming it would break
existing users' saved settings for a purely cosmetic gain; the user-facing "Max Quality" label is
unaffected either way).

**Verified live, real hardware, real `vah264enc`, same AMD RX 7900 XTX** — not the full two-party
Electron+relay+viewer path this time (a sender's own `DestinationStatsDto::bitrate_kbps` merely
echoes the *configured* bitrate, not a measured one — see that field's doc comment in `dto.rs` —
so seeing the real achieved bitrate requires a second party's `received_bitrate_kbps`, out of scope
for this pass's effort budget), but the exact same GStreamer element and properties
`launch_fragment` constructs, driven directly via `gst-launch-1.0` against real decoded video
(not synthetic test patterns — a real 1280×720/30fps clip, plus a noise-boosted variant to force
genuinely demanding content):

- **Ceiling now genuinely works**: on demanding real content, a 2000 kbps ceiling produced
  ~1911 kbps actual output (95.6% of ceiling — matches the tuned `target-percentage=95` almost
  exactly) and an 8000 kbps ceiling produced ~7641 kbps (95.5%) — output scales cleanly with the
  ceiling, both landing right at the configured target-percentage, unlike `qvbr`'s measured
  complete non-response to the same 4x ceiling change. This matches/slightly improves on the prior
  investigation's ~1886/7922 kbps figures for plain untouched `vbr`.
- **Bitrate stability (the actual pop-in proxy)**: encoded a 19s clip that cuts from static/simple
  content to noise-boosted demanding content partway through (a real complexity change, not a
  synthetic spike), at a fixed 4000 kbps ceiling, once with this fix's tuning
  (`target-percentage=95`, `cpb-size=0.375x`) and once with the old `Adaptive`-era untuned baseline
  (`target-percentage` unset/66%, `cpb-size=0`/auto ~1x). Measured achieved bitrate in 500ms
  windows across the steady-state demanding-content portion (excluding the initial keyframe spike,
  which is identical scene-cut cost either way):

  | | tuned (this fix) | untuned baseline |
  | --- | --- | --- |
  | mean | 4443.6 kbps | 4818.9 kbps |
  | stdev | 506.9 kbps | 787.2 kbps |
  | coefficient of variation | 0.114 | 0.163 |
  | mean frame-window-to-window swing | 550.8 kbps | 862.1 kbps |
  | max swing | 1668.5 kbps | 2165.5 kbps |

  The tuned combination cuts stdev by ~36%, mean swing by ~36%, and max swing by ~23% relative to
  the untuned baseline that originally caused the reported pop-in — a real, measured improvement in
  bitrate stability, consistent with (though not itself proof of) less visible pop-in.
- **`Cqp`/"Efficient" unaffected**: confirmed via the updated `rate_control_mode_maps_per_backend`
  unit test (real `gst::init()` + real `vah264enc` element construction) — `Cqp` still emits
  `rate-control=cqp`, untouched `qpi=26`, `cpb-size=0`; only `Qvbr`'s VAAPI branch changed.
- `cargo test --workspace` passes (183 + 12 tests, 0 failed).
- A real Electron app launch (built sidecar + real `electron/main.js`, driven via Playwright's
  `_electron`) loaded with zero console errors after this change, confirming no regression to app
  startup; driving a full two-party watch session to read a viewer's real
  `received_bitrate_kbps` was not done in this pass — flagged above as the honest verification gap.

**Why these specific values**: `target-percentage=95` sits at the high end of the property's
50-100 range so the real encode target tracks close to the configured ceiling rather than the
default 66% (the original pop-in's diagnosed cause). `cpb-size`'s 3/8 (0.375x) fraction was chosen
as a value meaningfully tighter than the old 1x-bitrate buffer per the task's guidance
(0.25x-0.5x range) — not separately swept against other fractions in this pass; the measured
result above already shows a real improvement at this value, so no further tuning was done, but a
tighter (e.g. 0.25x) or looser (0.5x) value remains unexplored if the current setting turns out to
need further adjustment after real-world (non-scripted) use.

### Interaction with the View tab: codec-string re-derivation

Toggling any of these three controls on a *live* destination triggers a real `BranchSwap`
— a new encoder with (potentially) a different H.264 profile/level. `x264enc` in
particular picks its AVC profile/level automatically from `speed-preset` with no explicit
`profile=` set anywhere in this pipeline, so a Performance→Quality toggle can genuinely
change the bitstream's profile out from under an already-connected WebCodecs watcher. This
was a real, real-hardware-verified bug (a watcher connecting *after* all three controls had
been toggled got stuck at 0fps) — the fix lives entirely on the View tab's delivery side
and is documented in full, including the regression coverage and a since-closed
investigation into a flaky-looking recurrence, in `superdoc/features/video-pipeline.md`'s
"Encoder-tuning interaction" section. Nothing about `capture.rs`'s own tuning-control
mechanism needed to change for that fix.

---

## Video codec selection: H.264, H.265, AV1 (2026-07-30, backend/wire only — Stage 1 of 2)

**Scope of this entry:** this is the *backend + wire protocol* half of adding a codec
selector to the stream control UI (approved after a codec-comparison discussion this
session settled on offering all three codecs, not picking one). A separate Stage 2 builds
the frontend selector on top of the contract documented here — `frontend/` was
deliberately untouched by this stage.

### The `VideoCodec` enum and its threading

`capture::VideoCodec` (`H264`/`H265`/`Av1`, `#[default] = H264`) is threaded through the
*exact same* per-destination settings pipeline `TuneMode`/`QualityLevel`/`RateControlMode`
already use: `VideoParams` → `EncodeSettings` → `DestEdit` → `dto.rs`'s
`parse_video_codec`/`video_codec_str` (snake_case wire strings: `"h264"`/`"h265"`/`"av1"`,
same convention as `parse_quality_level` etc.) → `rooms::AppSettings::
StartStreamDefaults::codec` (persisted, `#[serde(default)]` so every existing user's
on-disk `settings.toml` — which predates this field — still loads cleanly, defaulting to
`H264`, same reasoning as why `RateControlMode`'s wire strings didn't change after the VBR
fix: never break an existing config).

A codec change is unconditionally a tier-2 `BranchSwap` (`capture::destination::
classify_change`) on both backends — there is no live property that could ever make
swapping the encoder *element itself* (`vah264enc` → `vah265enc`, etc.) a tier-1 write.

New/changed `src-tauri` commands (mirroring `set_quality_level`/`set_rate_control`):

- `set_codec(codec: String)` — process-wide codec for the *next* video capture started
  (same "applied before `start_video_share`" shape as `set_tune` etc.).
- `edit_destination(..., codec: String)` — codec is now a 7th param, alongside the existing
  `tune`/`quality_level`/`rate_control`.
- `set_start_stream_defaults`/`get_start_stream_defaults` — `codec` added to both the
  request and `StartStreamDefaultsDto`.
- **`supported_codecs() -> Vec<&'static str>`** (new) — see below.

### Software-fallback gating: `capture::supported_codecs`

Only `x264enc` (H.264) is confirmed installed in software-encoder mode on this machine — no
`x265enc`/`aomenc`/`svtav1enc`/etc. is assumed present, so `EncoderBackend::Software`
**silently always produces H.264** regardless of what `VideoCodec` was requested
(`EncoderBackend::actual_codec`, the single source of truth for "what will this backend
*actually* emit" — used both by `branch::add_branch`'s output-caps selection and by the
software-backend real-hardware test `software_backend_always_produces_h264_regardless_of_
requested_codec`). This mirrors how `EncoderBackend::Vaapi` already silently ignores
`TuneMode` — a no-op, not an error.

Stage 2's frontend must call the new `supported_codecs` command (wraps `capture::
supported_codecs(capture::detect_encoder_backend())`, returning `["h264"]` on
`Software` or `["h264", "h265", "av1"]` on `Vaapi`) to grey out/disable H.265/AV1 in the
selector on a machine that would silently downgrade them, rather than let a user pick an
option that quietly does something else. `dto::StagedCaptureRow` also carries this same
list (`supported_codecs: Vec<&'static str>`) alongside the staged card's other encoder
settings, so the "Share Screen" staged-card flow doesn't need a second round trip.

`Why a static function of the backend, not a live per-codec hardware probe:` every VAAPI
machine tested this session has both `vah265enc`/`vaav1enc` alongside `vah264enc` (same
`va` GStreamer plugin) — `detect_encoder_backend`'s existing `vah264enc`-only probe is
already the "is VAAPI usable at all" gate; a machine where `vah264enc` works but the other
two mysteriously don't would still fail safely via `branch::add_branch_with_fallback`'s
existing per-branch software fallback (dropping that branch to H.264), not crash. A second
probe pipeline per codec was judged not worth the complexity for this stage.

### `vah265enc`/`vaav1enc`: confirmed via `gst-inspect-1.0` on this dev machine (AMD RX 7900
XTX, RDNA3, GStreamer 1.28.5)

Both elements' `rate-control` (`cbr`/`vbr`/`cqp`/`qvbr`), `bitrate`, `key-int-max`, and
`target-usage` properties have the **exact same names, ranges, and enum values** as
`vah264enc`'s — re-verified directly, not assumed — so `EncoderBackend::launch_fragment`
only needs to swap the element name per codec, not remap any property. Their `mbbrc`
property (mentioned as a possible per-codec wrinkle before this stage started) defaults to
`"auto"`/`"disabled"` on both and was left untouched — nothing in this pipeline's existing
`RateControlMode`/`QualityLevel` mapping needs it.

Output caps differ by codec, not just by element (`branch::add_branch`'s caps immediately
downstream of the encoder):

| Codec | Encoder element | Output caps |
| --- | --- | --- |
| H.264 | `vah264enc` / `x264enc` | `video/x-h264,stream-format=byte-stream,alignment=au` |
| H.265 | `vah265enc` | `video/x-h265,stream-format=byte-stream,alignment=au` |
| AV1 | `vaav1enc` | `video/x-av1,stream-format=obu-stream,alignment=tu` (no Annex-B start codes at all — a genuinely different framing, not just a different media type) |

### The `CodecInfo` wire chunk: generalized to all three codecs, still purely bitstream-derived

`src-tauri/src/video_server.rs`'s `CodecInfo` chunk (the in-band WebCodecs codec string
every watch connection gets first — see that module's own doc comment) now derives
`avc1.PPCCLL` / `hev1.<profile>.<compat>.<tier><level>[.<constraints>]` /
`av01.<profile>.<level><tier>.<bitdepth>` strings, one parser per codec
(`parse_avc_codec_string`/`parse_hevc_codec_string`/`parse_av1_codec_string`), dispatched
by `derive_codec_string`.

**The dispatch mechanism — and the one genuine ambiguity in this stage's design:**
`protocol.rs`'s `AnnounceStream`/`StreamInfo` carry no codec field (confirmed at this
stage's start, deliberately left that way — see "Explicitly NOT in scope" below), so
`video_server.rs` has no wire-level hint telling it which codec a given watched stream is
even using. Two different answers were possible here:

1. **Derive the codec purely from the bitstream's own structure** (what was built).
   `capture.rs` deterministically builds H.264/H.265 branches with `stream-format=
   byte-stream` (Annex-B, every access unit starts with a `00 00 01`/`00 00 00 01` start
   code) and the AV1 branch with `stream-format=obu-stream` (no start codes, ever) — so
   "does this access unit begin with an Annex-B start code" is a **provably reliable**
   AV1-vs-{H.264,H.265} discriminator, not a guess. Disambiguating H.264 from H.265 within
   the Annex-B branch is *not* as clean: there is no structural guarantee, only a
   **heuristic** (`looks_like_hevc_param_set_nal`: re-interpret each NAL's header as a
   H.265 SPS and check whether the resulting `nal_unit_type` lands in the real SPS value —
   hand-verified against this machine's actual `vah264enc`/`vah265enc` output not to
   collide either way, but not proven safe for every possible encoder's output in general).
2. **Add a codec field to `protocol.rs`'s wire types** — would remove the ambiguity
   entirely, but is a real cross-cutting change to the network protocol shared with the
   (not-yet-built) relay server and browser-viewer sibling repos, explicitly out of scope
   for this stage's brief.

Option 1 was chosen (matches this module's existing design philosophy of deriving
everything from the bitstream rather than trusting a caller-declared value that could go
stale — see the module's own "Codec-string discovery" doc section), but the H.264/H.265
heuristic is a real, flagged judgment call, not a closed question — documented in full,
including the exact byte-level reasoning, in `video_server.rs`'s own "H.265/AV1
codec-string derivation" section doc comment.

Regression coverage: `capture::tests::vaapi_h265_pipeline_links_and_produces_encoded_
output`/`vaapi_av1_pipeline_links_and_produces_encoded_output` (real hardware, actually
launch the `vah265enc`/`vaav1enc` pipelines and assert encoded output), `capture::tests::
quality_level_maps_to_target_usage_on_h265_and_av1`/`rate_control_mode_maps_on_h265_and_
av1` (real hardware property-mapping), and `video_server.rs`'s
`parse_hevc_codec_string_reads_real_vah265enc_sps`/`parse_av1_codec_string_reads_real_
vaav1enc_sequence_header`/`derive_codec_string_dispatches_by_bitstream_shape` — the latter
three against byte-for-byte real `vah265enc`/`vaav1enc` capture output (not synthesized),
cross-checked against an independent decoder (`ffprobe -v trace`) for the AV1 case where
hand-derived bit-tracing first produced a wrong expected value.

### Explicitly NOT in scope for this stage

No `frontend/` changes (`pillSwitch`, `components.js`, `view.js` — Stage 2). No change to
`protocol.rs`'s `AnnounceStream`/`StreamInfo` (see the wire-chunk section above for why,
and the ambiguity that choice leaves open). No live per-codec VAAPI availability probing
beyond the existing `vah264enc`-only one.

---

## The "START A STREAM" form gets the same options as editing a live destination, and remembers them (2026-07-28)

**Superseded (2026-07-28, later the same day):** the standalone "START A STREAM" card this
section describes is gone — replaced by the staged-capture "Go live" flow in "'Go live':
staging a capture with zero destinations, then attaching the first one" above, which reuses
`destinationCard`'s Preset/Advanced-tuning/Framerate/Bitrate panel instead of a separate
form. The mechanisms this section documents (process-wide `tune`/`quality_level`/
`rate_control` on `AppState`, the `set_tune`/`set_quality_level`/`set_rate_control` commands,
`rooms::StartStreamDefaults`/`get_start_stream_defaults`/`set_start_stream_defaults`) are
all still exactly as described below and still exactly how the new flow persists its
settings — only *when* persistence fires changed (now: once, at "Go live", not on every
form edit). Left in place rather than deleted: the reasoning for *why* each of these pieces
exists is still accurate and still the reason the new flow could reuse them as-is.

**Owner's request, verbatim:** *"The 'START A STREAM' UI should offer the same options as I
have after creating a stream and editing it. Right now, it only offers to change the frame
rate. These settings (only for the start stream part) should be saved, so they can be used
the next time."*

Before this, `renderStartStreamCard` (`stream-control.js`) offered only a Room dropdown, a
Framerate dropdown, and — Per-Stream mode only — a manual Bitrate field: strictly less than
the destination edit panel above (which also has the Preset selector and the Advanced
Tune/Quality Level/Rate Control disclosure), even though both panels apply to the exact same
`VideoParams` shape. Two changes closed that gap:

**1. Same controls, shared code.** The Preset `pillSwitch` mapping (`ENCODER_PRESETS` —
Low-Latency/Balanced/Quality → tune+qualityLevel+rateControl, see "Encoder tuning controls"
above) moved out of `destinationCard` to module scope in `components.js`, alongside a small
`activeEncoderPreset(tune, qualityLevel, rateControl)` helper, and both are now exported on
`window.Components`. `renderStartStreamCard` renders the identical Preset row plus an
identical `<details class="dest-advanced">` Advanced-tuning disclosure (Tune/Quality
Level/Rate Control `pillSwitch` rows) using the same CSS classes and the same collapsed-by-
default/reopen-across-rerender convention `destinationCard`'s Advanced section already
established (`priorAdvancedEl`/`advancedOpen`, read off the outgoing `startCardEl` before
building its replacement).

**2. Wiring a form that has no destination yet.** `edit_destination` (the mechanism the edit
panel uses) needs an existing `destination_id` — a stream that has not started yet has none.
Rather than invent a third settings-application mechanism, this follows the *existing*
pattern this file's own header comment already documents for framerate/bitrate: **process-
wide settings, applied via a pre-start command, read by `start_video_share` when it builds
the first destination's `VideoParams`.** `AppState` gained three new fields — `tune: TuneMode`,
`quality_level: QualityLevel`, `rate_control: RateControlMode` (`session.rs`), mirroring
`framerate`/`bitrate_kbps` exactly — and three new commands, `set_tune`/`set_quality_level`/
`set_rate_control` (`src-tauri/src/commands.rs`, `dto::parse_tune_mode`/`parse_quality_level`/
`parse_rate_control_mode` for the string→enum step, same convention `edit_destination` already
uses). `start_video_share` (`session.rs`) now reads `self.tune`/`self.quality_level`/
`self.rate_control` into the new destination's `VideoParams` instead of hardcoding
`TuneMode::default()` etc. `startVideoShare` (`stream-control.js`) calls all five setters —
`set_framerate`/`set_bitrate`/`set_tune`/`set_quality_level`/`set_rate_control` — before
`start_video_share`, same shape as the pre-existing framerate/bitrate pair.

**3. Persistence.** New `rooms::StartStreamDefaults` (framerate, bitrate_kbps, tune,
quality_level, rate_control), nested as `AppSettings::start_stream_defaults` — the same
"local TOML settings file, loaded at startup, written on every change" mechanism
`save_bandwidth_settings`/`save_max_jitter_buffer_ms` already use, not a new one.
`TuneMode`/`QualityLevel`/`RateControlMode` (`capture.rs`) gained `Serialize`/`Deserialize`
derives (`#[serde(rename_all = "snake_case")]`, matching `dto.rs`'s existing wire strings
exactly) so they can be stored directly, the same way `BandwidthMode`/`PresentationMode`
already are, rather than going through a string-parsing layer just for this one field.
`session::save_start_stream_defaults(...)` follows the existing load-modify-save shape
(loads the current `AppSettings`, replaces just this nested struct, saves — never clobbers
the sibling settings). Two new Tauri commands, `get_start_stream_defaults`/
`set_start_stream_defaults` (`src-tauri/src/commands.rs`, plus their `dto::
StartStreamDefaultsDto`) — a dedicated pair rather than folding into `get_settings`'s
`SettingsDto`, because `SettingsDto`'s `bitrate_kbps`/`framerate` fields mirror **live
`AppState`** (reset to `DEFAULT_BITRATE_KBPS`/`DEFAULT_FRAMERATE` every launch — see the
`session.rs` module doc's "every launch starts fresh" note), which is a different question
from "what did the form last save", and there is no live `AppState` field for
tune/quality_level/rate_control to mirror in the first place (those only exist per-
destination, via each running share's own edit buffer). `stream-control.js`'s `init()` calls
`get_start_stream_defaults` and merges the result onto `state.startForm`'s hardcoded
first-run fallback (which matches the backend's own `#[default]`s exactly: 30fps/8000kbps/
`best_quality`/`balanced`/`constant`); every Preset click, Advanced-tuning pill click,
framerate change and bitrate change calls `set_start_stream_defaults` with the form's full
current state (`applyStartFormTuning`) — deliberately excluding `roomHandle`, which is a
per-launch UI convenience, not a remembered encoder preference.

**Verified against the real app** (Electron + the real `src-tauri` sidecar over its REST+SSE
`api_server`, not the frontend's mock branch): the Preset/Advanced-tuning controls render
identically to the edit panel's; changing Tune/Quality Level/Rate Control/Framerate writes
through to the real `~/.config/simple-screensharing/settings.toml`
`[start_stream_defaults]` block; closing the app and launching a fresh Electron+sidecar
process reloads exactly those values into the form (confirmed via the Advanced-tuning
panel's active pill state and the Framerate readout after the relaunch). Separately, joining
a real relay-backed room and clicking "Share Screen" with Framerate set to 60 in the form
produced a live destination reporting `framerate: 60` via `list_active_streams` — direct
confirmation the pre-start-command mechanism (identical code path for tune/quality_level/
rate_control) actually reaches the encoder, not just the settings file.

---

## Bandwidth modes

Two named modes (plan doc, round-2 Q6), persisted in `AppSettings` as **independent**
fields:

- **Per-Stream Maximum** (default) — the user sets each destination's bitrate on its own
  row. Streams don't affect each other. New work, not "today's behaviour": the app
  previously had a single global bitrate applied to the next capture started.
- **Absolute Maximum** — one **per-client aggregate** total across every destination in
  every room. Starting another stream anywhere shrinks every other destination's share.

`Why both modes' numbers are stored independently and never merged:` they mean different
things (a total vs. one stream's rate). Switching modes must be lossless, so the
Absolute total lives in `AppSettings::absolute_total_kbps` and each destination's manual
rate lives on its row (`DestEdit::bitrate_kbps`) — and while Absolute mode is on, the
allocator writes live bitrates straight into the encoders without ever touching the manual
values. Switching back to Per-Stream re-applies each destination's own rate
(`App::reapply_manual_bitrates`). Per the plan doc's own correction, manual per-destination
rates are **not** persisted across launches (nothing about active streams is); the mode and
the Absolute total/floor are.

### The allocator wiring

Once per frame, `App::apply_bandwidth_allocation`:

1. builds an `AllocInput` from **every** active video destination across all rooms and all
   captures — weight `width*height*fps` from the *live* runtime params, `floor_kbps` from
   the persisted per-destination floor, `ceiling_kbps` = the project's 40 Mbit/s per-stream
   maximum;
2. calls the pure `bandwidth::allocate()`;
3. writes each destination's share into its encoder as a tier-1 property write.

Destinations whose branch doesn't exist yet (still starting, or a fan-out/transfer target
still proving itself) are excluded: they have no encoder to write to, and including them
would shrink everyone else's share for a stream that may never come up.

**Rate limit — one recompute per second** (`ALLOC_RECOMPUTE_INTERVAL`). `Why:` `allocate`
is pure and cheap, but *applying* it writes into a live encoder's rate controller, and both
`x264enc` and `vah264enc` need time to converge on a new target. Recomputing every repaint
(up to 240/s) hands the rate controller a new target faster than it can settle on the last
one, which reads as quality oscillation rather than an adjustment. The inputs also change in
bursts — joining a room, starting a fan-out destination, a transfer's make-before-break
overlap where old and new destination briefly coexist — so a fixed ~1s cadence collapses
each burst into a single re-apply. One second is the plan doc's own figure and is well below
human-noticeable for a bandwidth change.

**Hysteresis — write only past 5% *or* 100 kbit/s** (`needs_reapply`). `Why both bounds,
combined with OR:` at 8000 kbit/s, 5% is 400 kbit/s and waiting for a swing that large would
leave real headroom unused; at 400 kbit/s, an absolute 100 kbit/s threshold is a quarter of
the stream and would make the destination unresponsive to allocation changes entirely.
Triggering on whichever bound is hit first keeps the threshold proportionate at both ends
while still filtering the ±1 kbit/s churn largest-remainder rounding produces when an
unrelated destination's weight shifts slightly.

Both live in `BandwidthController`, which owns no `App` state and calls no clock of its own
(`now` is passed in) — the same determinism/testability stance as `bandwidth::allocate` and
`decode.rs`'s `PacingState`, and the reason the timing behaviour can be tested without
sleeping.

### 2026-07-28 — "the bitrate doesn't update when Total Maximum changes" — frontend display bug, not a backend one

**Reported:** dragging Absolute-Maximum mode's "Total maximum" slider never changed a
destination's displayed bitrate, while Per-Stream mode's own per-destination slider did.

**Investigated for real, not just by reading code** (the mechanism above reads as
structurally correct on paper, which is exactly why this needed a real-hardware check):

- The decision layer (`BandwidthController::tick`'s rate limit + hysteresis) is already
  covered by `ui.rs`'s `bandwidth_wiring_tests` — re-ran them: all pass, and they
  demonstrably do react to a `total_kbps` change once the rate limit clears.
- Whether a decided write reaches a *real* encoder is covered, for real GStreamer (not a
  mock), by `capture::branch::tests::
  setting_one_branchs_bitrate_writes_through_to_that_encoder_only` — re-ran it against a
  real pipeline: passes, and the test reads the bitrate back off the live element, not off
  a detached handle. Confirms the backend really does write the new bitrate into the real
  encoder.
- `driver.rs`'s tick loop calls `AppState::apply_bandwidth_allocation()` **unconditionally
  every 250ms tick**, not gated behind a room being connected or any UI tab being open —
  confirmed by reading `driver.rs::tick` step 4, no early-return before it.
- So the backend was never the problem: `set_bandwidth_total` writes the new total,
  `apply_bandwidth_allocation` picks it up on its next scheduled tick (≤1s later, per the
  rate limit) and writes the real encoder property when the change clears hysteresis.

**Root cause, entirely in the frontend:** the destination card's **"Bitrate" metric**
(`frontend/scripts/components.js`, the `metric("Bitrate", ...)` row shown on every
destination card in both modes) reads `dest.bitrate_kbps` off `stream-control.js`'s
`mockDestinations` cache. That cache is only ever refreshed by re-fetching
`list_active_streams`, which only happens on a `destination-state-changed`/
`presence-updated` event. The allocator's write is a **tier-1 property write**
(`capture::CaptureHandle::set_destination_bitrate`) — deliberately chosen *because* it has
no lifecycle consequence, so it never changes `DestinationState` and never fires either
event. Per-Stream mode never showed this symptom because `applyDestinationEdit` has a
call site to optimistically mirror the user's own edit onto `dest.bitrate_kbps` the instant
it's sent (see that function's own `Why:` comment); an allocator-driven change happening
entirely inside the backend, off the back of a slider in a *different* mode, has no such
call site to patch from. Exactly the same "a live change with no accompanying event" shape
as the already-fixed 2026-07-27 "Stop never clears" bug in `driver.rs` (see that entry) —
different symptom, same root cause class.

Note this is **not** the same thing as the "Allocated: N kbps" line also shown in Absolute
mode (`updateActiveSharesCard`'s `computedShareKbps`) — that one is a **frontend-local
recompute** off `bandwidth.absolute_total_kbps` (itself updated optimistically by
`setAbsoluteTotal`) and *does* update immediately on every slider release. It was the
"Bitrate" metric specifically that stayed stale.

**Fix:** rather than adding another diff-suppressed event (more plumbing for a value that
already changes on roughly the same cadence as the FPS counters below it), the destination's
live bitrate now rides the *existing* `stream-stats-updated` payload, which is already
unconditional every driver tick (never diff-suppressed — see `dto::StreamStatsUpdated`'s
own doc comment) and already keyed by `destination_id` on the frontend for the FPS readout.
`dto::DestinationStatsDto` gained a `bitrate_kbps: Option<u32>` field (`None` for audio),
populated in `driver.rs` from `dest.live_params().bitrate_kbps` — the same real,
runtime-cached value `dto::destination_rows` already trusted for everything else. On the
frontend, `stream-control.js`'s `updateStats()` now also writes that value straight into
`mockDestinations` (so the cache agrees with the backend for any subsequent full render)
and directly into the card's `.dest-metric-bitrate-value` DOM node (a new dedicated class on
that span, in `components.js`), the same direct-DOM-write pattern already used for the FPS
readout and sparkline — bypassing the sig-diffed `render()` path entirely so a stats tick
never triggers a full card rebuild.

**Verified:** `cargo build --lib -p simple-screensharing-client` and
`cargo build -p simple-screensharing-client-tauri` clean; `cargo test -p
simple-screensharing-client --lib` — 153 passed, 0 failed, 13 ignored (real-hardware tests,
unchanged), same pass count as before this change. The real-GStreamer bitrate-write test
above was re-run specifically (not just the full suite) as the closest existing coverage to
"does a live bitrate write really reach the encoder," both before touching anything (to rule
out a regressed backend) and after (to confirm the fix didn't disturb it) — passed both
times.

### Overshoot

`allocate` never starves a destination below its floor and never silently exceeds the
user's total (plan doc, round-1 Q8): when the floors don't fit, everyone still gets their
floor and the excess is reported as `Allocation::overshoot_kbps`. The bandwidth card
renders that as a `theme::warning_banner` (a new tinted, `WARNING`-stroked primitive) with
the actual numbers — *"Over budget by X kbps: the guaranteed floors need Y kbps but the
maximum is Z kbps. No stream is starved below its floor."*

---

## What the tab renders

Top to bottom: **Active shares** (every destination across every room) → **Bandwidth**
(mode toggle + that mode's settings + fan-out multiplication note) + overshoot banner →
**Start a stream** (room picker over *joined* rooms, framerate, bitrate when Per-Stream
mode is on). The SHARE AUDIO card that used to sit beside the room-scoped row below was
removed on 2026-07-28 — see "Audio is a row on the share" below — and the room-scoped
row itself (a room picker plus a Members/rename card) was removed the same day; see
"Room-scoped settings row removed" below.

**Active shares moved above Bandwidth (2026-07-29).** Owner's request: swap the two
cards' order. The overshoot banner moved together with the Bandwidth card rather than
staying pinned to the top — it's a bandwidth-budget warning contextually tied to that
card, so it stays visually adjacent to what it explains. `stream-control.js`'s `render()`
now calls `syncChildren(gridEl, [updateActiveSharesCard(alloc), updateBandwidthCard(),
bannerText ? updateOvershootBanner(bannerText) : null])` — `syncChildren` (local to
`stream-control.js`) is positional/order-driven DOM reconciliation (`insertBefore` by
index), so the reorder is a pure array-order change with no other code affected.

### Room-scoped settings row removed (2026-07-28)

Owner's request, verbatim: *"remove the Room-scoped settings ('Room-scoped settings
for'), since it just clutters the UI and is barely useful."*

Removed the whole "Room-scoped settings for:" row: its room-picker header, the MEMBERS
card (member list + `conn_id` display), and the room-name rename field/Save button/
"Saved." hint beneath it (`stream-control.js`'s `updateRoomScopedRow`/
`renderRoomScopedRow`/`renderMembersCard`, plus the `.sc-room-scoped-*`/`.sc-members-
card`/`.sc-member-row`/`.sc-rename-row`/`.sc-saved-hint` CSS in `stream-control.css`).

**Why it was safe to delete outright, not just trim:** it was a straight duplicate of
functionality the sidebar's room detail side panel (`shell.js`'s `renderRoomPanel`,
opened by clicking a room in the sidebar's ROOMS list) already provides — the same
members list and the same room-name rename field, reached one click away instead of
buried in a second, disconnected room picker on this tab. Confirmed by reading
`renderRoomPanel` before deleting anything: it independently fetches/renders members
(`room-panel-members`) and has its own rename flow (`renamePanelRoom`, its own
`rename_room` invoke call) — no code path here was the *only* place either piece of
functionality lived.

Also removed as now-orphaned: `mockMembersByRoom`/`MOCK_MEMBERS_BY_ROOM`,
`fetchRoomMembers`, `renameRoom` (this tab's own copy — `shell.js`'s `renamePanelRoom`
is a separate function with its own `rename_room` call and was left untouched), and the
`state.scopedRoomHandle`/`state.roomName`/`state.savedHint` fields. `state.startForm
.roomHandle` (the *Start a stream* card's own room picker) is unrelated and was kept,
including `init()`'s default-to-first-joined-room logic for it (re-keyed off
`state.startForm.roomHandle` instead of the deleted `state.scopedRoomHandle`).

Each non-audio destination card carries the encoder edit panel described under "Encoder
tuning controls" above: **Preset** and **Framerate** always visible in *both* bandwidth
modes, **Bitrate** as a third always-visible row in Per-Stream mode only, and
Tune/Quality Level/Rate Control behind the collapsed "Advanced tuning" disclosure. In
Absolute mode the Bitrate row is replaced by the read-only `Allocated: N kbps` line the
allocator drives.

**Equal-width tuning toggles (2026-07-29).** Tune (2 options), Quality Level (3), and Rate
Control (3) each render as their own `pillSwitch` and, before this fix, each button sized to
its own label text — "Low Latency"/"Best Quality"/"Performance"/etc. all landed at visibly
different widths within and across the three rows. Fixed with a `.dest-tuning-toggle`
modifier class (`components.js`, styled in `stream-control.css`), applied only to these
three `pillSwitch` instances: a fixed `width: 342px` on the track plus `flex: 1 1 0` on
`.mode-toggle-btn` inside it, so a row's segments split that width evenly and all three
rows' tracks measure identical. Same scoped-modifier pattern as `APP-SHELL.md`'s
`.add-room-tabs` (Join/Create tabs) and `settings.css`'s `.settings-segmented` (language
switch) — a class on this particular `pillSwitch` instance, not a change to
`.mode-toggle`/`.mode-toggle-btn` themselves, which the bandwidth-mode toggle, language
switch, and encoder preset picker still rely on sizing to content.

`Why 342px total, not width: 100%`: unlike `.add-room-tabs`, these rows are
`justify-self: start` in `.dest-edit-grid` (see that class's own `Why a fixed px` comment
in `stream-control.css`, which set the 96px label column all six Room/Preset/Framerate/
Tune/Quality Level/Rate Control rows now share) — not meant to stretch across their grid
cell, since unrelated rows (Room/Preset/Framerate) can be much wider. 342px was sized to
comfortably fit "Performance"/"Low Latency", the longest labels in play, in the tightest
case (3 segments), and confirmed by real-app `getBoundingClientRect()` measurement in
Electron: all three tracks measure exactly 342px, with each row's buttons equal-width to
its own siblings (Tune's two segments ~165.5px each; Quality Level's and Rate Control's
three segments ~109.3px each).

`Why a shared fixed total instead of sizing each row to its own content`: the owner's ask
was "all the triple selections should have the same width" for the whole block, which
necessarily means Tune's two (wider) segments and Quality Level/Rate Control's three
(narrower) segments are *not* the same per-button width as each other — only the track
total is shared. This mirrors the `.dest-edit-grid` label-column fix's own approach: one
fixed number every row agrees on, rather than each row solving its own layout
independently and drifting apart.

The destination list is deliberately **cross-room**, like the sidebar's STREAMS section: a
destination is bandwidth the user is paying for right now, and hiding it behind a room
selection would make fan-out invisible. Each row shows a status dot + label folding the
capture's start/stop state, the pending transfer/fan-out role, and the live
`DestinationState` (`Starting` / `Live` / `Applying…` for `Rebuilding` / `Waiting for
capture…` for `Stalled` / `Failed`) — the minimal honest surfacing of the stall protocol.
The full tiered restart placeholder (800ms / 6s tiers) is stage 3, on the View tab.

A `Failed` row's "Retry" action (`stream-control.js`'s `retryDestination`) calls a
dedicated `retry_destination` Tauri command (`commands.rs` → `AppState::retry_destination`
in `session.rs`), not `stop_destination` + `start_destination`.

**Why:** that pairing used to be the implementation, and had two real bugs. First, it
didn't preserve the failed destination's last-configured bitrate/tune/quality_level/
rate_control — `start_destination` only derives new-destination params by copying a
sibling still on the capture, so for a capture's *only* destination (the common case)
there was no sibling to copy from and it silently reset to session defaults. Second,
worse: `stop_destination` tears down the *whole capture* once its destination count hits
≤1 — correct when the user deliberately stops their only stream, wrong for a retry where
only that one branch died — so `start_destination` afterward bailed silently (capture no
longer `Active`) and retry actually failed outright for the single-destination case, the
most common one. `retry_destination` reads the failed destination's own params before
removing it, tears down just that one branch without touching `ShareStatus`, and re-adds
a replacement in the same room with those same params — never touching the rest of the
capture. Regression coverage:
`end_to_end_test::retry_destination_preserves_prior_encoder_tuning` (real relay + real
GStreamer capture, asserts both tuning preservation and that the capture stays `Active`
for the single-destination case).

The mode toggle is a pair of `selectable_label`s rather than a new theme component:
`theme.rs` has no radio-group helper, and a two-option exclusive choice is exactly what the
Settings tab's language switcher already expresses this way.

---

## Coverage

- `capture::branch::tests::setting_one_branchs_bitrate_writes_through_to_that_encoder_only`
  — a tier-1 write reaches the live encoder element of the branch it targets and no other's,
  and the branch keeps producing across it (real GStreamer).
- `capture::branch::tests::pausing_one_branch_does_not_pause_its_sibling` — per-destination
  valves: subscriber counts are per stream, so a pause must not take a fanned-out sibling
  down.
- `capture::branch::tests::sibling_destinations_keep_their_own_rate_through_a_swap_and_a_removal`
  — fan-out isolation by *measured rate*: pausing, swapping and removing one destination
  each leave a sibling's framerate undisturbed.
- `capture::branch::tests::a_long_pause_resumes_at_the_live_edge_instead_of_replaying_the_gap`
  — a destination paused for having no subscribers resumes at the live edge instead of
  replaying the whole pause as duplicated stale frames (see "Resuming a paused
  destination" below).
- `capture::tests::applying_a_preset_never_bursts_a_branch_above_its_configured_framerate`
  — the runaway-fps regression: a real 1920×1080 desktop capture on this machine's real
  encoder backend, run for ten seconds, then given a preset change (all three tuning fields
  at once) and drained the way `destination_pump` drains it. Asserts both that the
  replacement branch stays at its configured rate and that its frames carry real
  presentation times rather than falling through the wire clock's monotonic clamp. See
  "Four bugs found in owner testing of the edit panel" above.
- `capture::destination::tests::a_branch_swap_keeps_real_frame_spacing_when_the_new_branch_restarts_its_timeline`
  and `…::a_branch_swap_that_continues_the_timeline_leaves_the_wire_clock_alone` — the
  wire-clock re-anchor's arithmetic, and that it stays a no-op on a seam that was already
  continuous.
- `capture::tests::adding_a_branch_never_renegotiates_upstream_of_the_tee` — the
  `RECONFIGURE` guard that keeps a real portal source alive when a destination is added.
- `capture::tests::live_portal_capture_feeds_a_destination_branch` (`#[ignore]`d) — the
  whole real capture path against an actual portal session; the only test that can observe
  a real source at all.
- `ui::bandwidth_wiring_tests::*` — convergence onto `allocate()`'s output, the per-client
  aggregate shrinking existing shares when a stream starts anywhere, and that the rate limit
  and hysteresis each genuinely suppress writes (deterministic, injected `Instant`s).
- `ui::tests::fan_out_transfer_and_absolute_bandwidth_across_three_rooms` (`#[ignore]`d,
  needs a relay child process) — the whole feature against a real relay and a real capture:
  fan-out into a second room, both destinations reaching `Live`, Absolute-Maximum applied to
  both, make-before-break transfer to a third room, and the tab's real egui layout code run
  headlessly over each of those states including mid-transfer.

## Sharing audio: an attachment on a screen share's card

**Added 2026-07-28.** **Superseded the same day** — the section below described audio as
its own `LocalCapture` *and* its own card, started from a standalone SHARE AUDIO panel.
The capture is still its own `LocalCapture` (that part is unchanged); the **UI is not**.
See "Audio is a row on the share, not a card of its own" below for what ships now, and
"The 2026-07-28 audio bugs" for why the first pass of this never actually worked.

The **streamer's** side keeps the opt-in the project owner asked for: *"The streamer
should be able to enable audio on his share."* Audio is still its own `LocalCapture` with
its own `StreamKind::Audio` stream id on the wire, started via `list_audio_sources` →
`select_audio_source` → `start_audio_share`. Your own outgoing audio destination still
appears in your OUTGOING sidebar list with a `volume_up` glyph — that is your own share,
not a viewer-facing artifact. (A 2026-07-28 investigation initially misread the
`kind === "audio"` branches in `components.js` as the viewer-facing "separate audio
stream" to delete; they were not that, and were left alone at the time. They have since
been removed for a different and correct reason — see below.)

What changed is entirely on the **watcher's** side: a viewer never sees another person's
audio as a separate stream, row, or "Listen" action. Watching someone's video
automatically subscribes and plays their audio, and stops it again when the watch stops.
The full architecture, the rejected alternatives, and the four cases that broke the
previous "latch the pairing at watch time" approach are in
`superdoc/features/video-pipeline.md`'s "Audio follows video" section — including why
audio is played inside the Rust sidecar rather than relayed to the browser like video is.

Note this leaves the two halves deliberately asymmetric, and that is the point: *sending*
audio is an explicit choice the sharer makes about their own machine, so it has UI;
*receiving* it is not a choice at all, so it has none (beyond a per-tile volume/mute knob
on the video tile in the View tab).

---

## The 2026-07-28 audio bugs: the sharer was broadcasting their microphone

**Added 2026-07-28.** The "bundle audio with video" pass shipped with a passing synthetic
end-to-end test and did not work at all in real use. The owner's report: the audio share
sat on **STARTING** forever, and the viewer heard **nothing** even though a volume slider
had appeared on their tile. Three separate defects, all in
`simple-screensharing-client`:

### 1. `pipewiresrc path=<node id>` silently captured the microphone

`audio.rs::build_pipeline` built `pipewiresrc path={loopback_output_node_id}`, where the
id came from `pw-dump`'s top-level `id` field.

- `path` is **deprecated** (`gst-inspect-1.0 pipewiresrc`); `target-object` replaces it.
- Both take *"the source name/serial"* — a node's **`object.serial`** or its `node.name`.
  Neither takes `pw-dump`'s registry `id`, which is a *different number*: measured live on
  the dev machine, the loopback output node was registry id `333`, `object.serial`
  `18643`. The registry id therefore matched nothing.
- **A `pipewiresrc` whose target does not resolve does not fail.** `node.autoconnect`
  defaults to `true`, so the session manager links it to the **default audio source** —
  the microphone. Confirmed live with `pw-link -l`: the capture stream came up linked to
  `easyeffects_source:capture_FL`.

So the shared application's audio was never captured, and the sharer's mic was being
Opus-encoded and broadcast to the room instead. Nothing downstream could tell: the
pipeline reached `PLAYING`, packets flowed, the viewer's decoder ran.

`Why the tests missed it:` the end-to-end test drove its synthetic source at `volume=0`,
on the reasoning that "every assertion here is about pipeline liveness and subscription
state, none of which depend on amplitude". That reasoning was the bug — a test that
cannot distinguish "audio is flowing" from "audio is not flowing" cannot protect an audio
feature. It now runs at `volume=0.02` (inaudible, but *not nothing*) and asserts the
sender's real capture level and the receiver's real packet count.

**Fixes:** `target-object=<object.serial>`; `audio::wait_for_capture_link` polls `pw-dump`
after the pipeline starts and refuses the share unless a link genuinely exists between the
loopback output node and our own capture stream node (found by the `client-name` we set).
That check is not paranoia — a mis-targeted capture is a *privacy* failure, and it is the
only place that can detect one. It doubles as the real "capture started" gate:
`Element::state()` reports `Ok(StateChangeSuccess::Async)` for a live source still stuck
waiting to negotiate, which the old `if let Err(..)` check read as success.

`channels=2` is now pinned with a caps filter too: without it PipeWire negotiated **mono**
and linked only `output_FL`, so shared music silently lost its right channel.

### 2. "STARTING" forever

`Destination::state()` derives everything from `runtime`, the *video* encode branch's
shared state, and maps `runtime: None` to `Starting`. An audio destination never has a
runtime (there is no per-destination audio geometry or stall machinery), so for video
`Starting` meant "no IDR yet" and cleared within a beat, while for audio it was permanent.

**Fix:** `LocalCapture::destination_state(dest)` folds the capture's own `ShareStatus` in
for audio captures (`Starting` while `start_audio_sharing` is in flight, `Live` once it
returns `Ok` — which now means *verified* capture, per the link check above).
`dto::destination_rows`, `driver.rs`'s state-change diff and `ui.rs` all go through it.

### 3. The volume slider had nothing to control

Not a separate bug — with no application audio ever captured, there was nothing to hear
and nothing for the slider to change. It works as designed once (1) is fixed.

---

## Audio is a row on the share, not a card of its own

**Added 2026-07-28.** Owner's request, verbatim: *"attach the Audio part to the
Screenshare and not make it its own separate card. Just extend the streams card, add a
horizontal separator and then the following line: [SmallVolumeFeedbackVisualizer]
[StreamedAudioApplicationName] [Mute/Unmute](Fixed width, on text change) [Stop] — Keep
the modern look."*

Before this, audio showed up **twice** as a thing of its own: a standalone SHARE AUDIO
card in the room-scoped row, and a full destination card in ACTIVE SHARES structurally
indistinguishable from a screen share's. Both are gone.

A screen share's card now ends with a `.dest-divider` hairline and one extra row:

```text
[■■▪··] ssshare-noise ...................... [ Mute ] [ Stop ]
 meter   captured application               fixed-w   audio only
```

- **Meter** (`.dest-audio-meter`, 5 bars) — driven by `audio_level` on the existing
  `stream-stats-updated` tick, written straight into the DOM by `stream-control.js`'s
  `updateStats()` (never through the sig-diffed render path, same as the FPS readout and
  the sparkline). The level comes from a `level` element in the capture pipeline, folded
  into `audio::AudioLevel` (an `AtomicU32` of `f32::to_bits` — one writer, no history,
  lock-free for a GStreamer streaming thread) and surfaced as
  `DestinationStatsDto::audio_level`.
- **Application name** — `DestinationRow::label`, which for an audio capture already was
  the picked source's name.
- **Mute/Unmute** — `commands::set_audio_share_muted` → `AudioCaptureHandle::set_muted`,
  a `volume name=mute` element in the capture pipeline. Placed deliberately **before**
  `level`, so muting flattens the meter too: the meter's job is to answer "is my audio
  going out right now", and a bar still bouncing while muted answers that wrongly. Muting
  with `volume` rather than by pausing the send keeps a continuous Opus stream flowing, so
  the viewer's jitter buffer never sees a mute as a multi-second dropout to conceal.
  The button is a **fixed 76px** (`.dest-audio-mute`, widened from 64px on 2026-07-29 to fit
  "Unmute" at the tab's now-standard 32px button size), sized for the longer word, so the
  label swap never shoves Stop sideways.
- **Stop** — plain `stop_destination(<audio destination id>)`. Safe to reuse as-is: an
  audio capture always has exactly one destination, so `stop_destination` takes its
  `destinations.len() <= 1` branch and stops that capture only. The screen share is a
  different `capture_id` and is untouched.

With no audio attached, the same slot shows `No audio attached` plus a dashed
**+ Add audio** chip that expands in place into the source picker — the same
inline-reveal pattern as the card's existing "+ Also send to…" fan-out chip. It re-lists
`list_audio_sources` **on open**, because which applications are producing audio changes
by the second.

### Pairing audio to a card

`LocalCapture::audio_for_capture: Option<CaptureSourceId>`, set from
`start_audio_share(room_handle, for_capture)` and exposed as
`DestinationRow::paired_capture_id`. The frontend matches on `paired_capture_id` **and**
`room_handle`.

`Why an explicit link and not "same room":` one capture can be fanned out to several
rooms, and someone sharing two monitors into one room is two cards but potentially one
audio share — "same room" would make both cards claim the same audio row, and Stop on
either would silently kill audio for both. Recording the pairing at the moment the user
clicks "+ Add audio" on a specific card is unambiguous and costs one `Option`.

### What was removed, and the one entry point that went with it

- `stream-control.js`'s `renderShareAudioCard()` and the `.sc-share-audio-*` styles.
- The `.sc-ghost-row` "Share audio" row. It started a *standalone* audio share, which now
  has no card to render in — leaving it would have been the one way to create an audio
  share the UI cannot show.
- `components.js`'s `dest.kind === "audio"` branches in `destinationCard` (thumb glyph,
  head glyph, audio-only metric row, failed-card head). Audio rows never reach that
  function any more.

An audio destination that somehow has no matching video card (a share torn down out from
under it, or one started by the legacy egui frontend) renders nothing in ACTIVE SHARES,
but is still listed and stoppable in the sidebar's OUTGOING list — so it can never be
stranded invisibly.

### Real-world verification (2026-07-28)

Not the synthetic test this time. Two real Electron clients against a real relay, sharing
a real screen plus a throwaway decorrelated-noise application, with the audible result
measured off the output device's monitor:

| viewer volume | measured RMS at the speakers | expected |
|---|---|---|
| 0%   | −30.80 dBFS (source only) | baseline |
| 100% | −28.09 dBFS (**+2.71 dB**) | +3.01 dB (one more equal-power copy) |
| 200% | −24.29 dBFS (**+6.51 dB**) | +6.99 dB (source + 4× power) |
| 0%   | −30.80 dBFS | exactly back to baseline |

Sharer-side Mute produced exactly the baseline figure and `audio_level` went to `0.0`.
The sidecar logged `audio capture stream '…-cap' (node 86) is linked to loopback output
node 355` — i.e. the tap, not the microphone. `Why noise and not a tone:` a 440 Hz sine
reproduced ~100 ms late by the viewer phase-cancels against the original unpredictably
(measurements swung between +5.8 dB and −3.6 dB); two decorrelated noise copies add in
power, which is monotonic and reproducible to 0.01 dB.

### Freshness investigation (2026-07-28)

Owner report: *"the Audio source dropdown should update more often (at least, when i open
it, but also periodically maybe)? Right now, when an audio source appears, after opening
the page, it doesn't reflect that (entry missing)."*

Investigated the whole path end to end rather than trusting a static read:

- **`audio::list_audio_sources`** (`src/audio.rs`) shells out to `pw-dump` fresh on every
  call — no cache, no memoization, nothing built once at startup. Verified live: started
  the sidecar once, queried `list_audio_sources` (baseline: 3 real apps), then — without
  restarting the sidecar — launched a throwaway `gst-launch-1.0 audiotestsrc ! audioconvert
  ! pipewiresink client-name=claude-test-new-source` and queried again on the *same running
  process*. The new node appeared immediately, correctly excluded PipeWire `Client` objects
  that carry lookalike `media.class` props but aren't `PipeWire:Interface:Node` (a red
  herring hit mid-investigation — a `pw-dump` skim without the `obj_type` filter makes it
  look like a node is missing when it's actually a non-node object correctly skipped).
- **`commands::list_audio_sources`** and **`api_server.rs`'s `/command/list_audio_sources`**
  (the real Electron-shell transport, per this doc's "Electron migration" note above) both
  call straight into the same function, `.await`ed fresh per request — no caching layer at
  the Tauri-command or HTTP-dispatch level either.
- **`components.js`'s `addAudioRow`** already re-invokes `onRefreshAudioSources` and rebuilds
  the `<select>` from the fresh result on every "+ Add audio" click — see the `Why` comment
  right above it, which documents a *previous* bug (a stale closure over the mock-data array)
  already fixed before this pass.

**Conclusion: no caching/staleness bug found anywhere in the current on-open path** —
backend, Tauri IPC, HTTP/Electron IPC, and frontend all re-query live on every open, verified
against real `pw-dump` output, not assumed from source alone. The core "when I open it"
behavior the owner asked for was already correct.

**What was missing, and what shipped:** the owner also asked for it to refresh *while the
picker stays open* ("periodically maybe"), which nothing implemented — the list was fetched
once per open, not while idling open afterward. Added periodic polling to `addAudioRow`
(`components.js`): a 5s `setInterval`, started when the picker opens, that re-calls
`onRefreshAudioSources` and rebuilds the `<select>` (preserving the current selection if it's
still present in the fresh list). Stops itself once the picker is hidden/torn out of the DOM
(`picker.isConnected` check) or once "Share" is clicked — same self-stopping `isConnected`
pattern `statusBadge`'s degraded-elapsed-time ticker already uses in this file, since this
codebase's render paths have no uniform unmount hook to call a `.destroy()` from instead.

### Room-detail panel's Audio section removed; real "Auto-Connect" (join room on app
startup) added (2026-07-28, corrected)

**This replaces an earlier, wrong version of this same entry.** A previous pass
misread the owner's request and wired "Auto-Connect" to mean "auto-start audio sharing"
— checking it called `startPanelAudioShare` for whichever source was selected in the
panel's own room-scoped Audio picker (a source `<select>` fixed to whichever room the
panel was open for, duplicating `fetchPanelAudioSources`/`startPanelAudioShare`
independently of `stream-control.js`'s destination-card audio row). The owner's actual
correction: *"The Auto Connect wasnt supposed to be for the Audio. It was supposed to
be for connecting to the room on startup. The Audio part in the room settings makes no
sense, remove it."*

**What shipped instead:**

1. **The whole AUDIO section is gone** — the source dropdown, the "No audio sources
   found" fallback, and the old (wrongly-wired) checkbox, all removed from
   `renderRoomPanel` (`shell.js`). Also removed as now-dead: `panelAudioSources`/
   `panelAudioSourceId`/`panelAudioAutoConnect` (module-level state) and
   `fetchPanelAudioSources`/`startPanelAudioShare` (their fetch/select/start helpers) —
   nothing else in `shell.js` referenced them. Audio sharing for a room is already
   reachable one tab over, on the video destination's own card in Stream Control (the
   "+ Add audio" row) — a second, room-scoped audio picker living in this panel was
   redundant even before the miswiring bug, so nothing replaces it here.
2. **A real "Auto-Connect" checkbox** — persisted per saved room, meaning what the name
   says: join this room automatically the next time the app starts, independent of
   whatever happened to be connected when it last closed. Lives in the room-detail
   panel's "Room name" section (next to the rename row), reusing the same
   `.room-panel-checkbox-input`/`.room-panel-checkbox-box`/`.room-panel-autoconnect-row`
   visual treatment the old (removed) audio checkbox used — same custom-checkbox styling
   in `shell.css`, just re-pointed at a real feature instead of recreated. The
   audio-row-specific CSS (`.room-panel-audio-row` and its `.dd` fit rule) and the
   `-disabled` checkbox-row variant (the new checkbox is never disabled) were deleted as
   dead; the checkbox classes themselves were kept and reused.

**Backend (new):**

- `rooms::SavedRoom` (`src/rooms.rs`) gained `auto_connect: bool`
  (`#[serde(default)]`, so a `rooms.toml` written before this field existed still
  parses, defaulting to `false`). `RoomStore::set_auto_connect(join_string, value)` sets
  it for the matching saved room (no-op, not an error, if the join string isn't saved —
  mirrors `name_for`'s "miss stays a miss" contract).
- `AppState::set_room_auto_connect` (`src/session.rs`) calls that and persists
  (`room_store.save()`), same load/set/save shape as `rename_saved_room`.
- New Tauri command `set_room_auto_connect(join_string, auto_connect)`
  (`src-tauri/src/commands.rs`), registered in **both** dispatch surfaces —
  `main.rs`'s `tauri::generate_handler!` list (native Tauri IPC) and
  `api_server.rs`'s `dispatch()` match (the Electron-migration REST bridge every real
  run of this app actually goes through, see that module's doc). Missing the second
  registration was caught during this task's own real-backend verification pass (see
  below) — the frontend's `invoke()` call resolved to `{"error":"unknown command
  \`set_room_auto_connect\`"}` until both were added.
- `dto::SavedRoomRow` gained `auto_connect: bool`, populated from `SavedRoom::
  auto_connect` in `saved_room_rows()` (`false` for the "live room with no saved entry
  yet" synthesized rows, which have no `SavedRoom` to read it from).
- **Startup auto-connect:** `AppState` gained `auto_connect_queue: Vec<String>` — a
  queue, not "just call `start_connect` once per flagged room in `.setup()`", because
  `pending_connect` is a single slot (one in-flight connect at a time); firing several
  `start_connect`s back-to-back at startup would silently overwrite/orphan every one but
  the last (its `oneshot` result arrives with nothing left to read it, and the room —
  even on a successful handshake — never makes it into `self.rooms`). `main.rs`'s
  `.setup()` now enqueues every saved room with `auto_connect == true`; the driver's
  tick loop (`driver.rs`) calls the new `AppState::poll_auto_connect_queue()` right
  before `poll_connect_result()` each tick, which drains one queued join string into
  `pending_connect` whenever that slot is free — so a startup batch of auto-connect
  rooms joins one at a time as each previous one resolves, not all at once.
- `AppState::default()` was confirmed (by reading it, not assumed) to start `rooms`
  empty and reconnect nothing on its own — there was no pre-existing "resume whatever
  was connected last session" mechanism to hook into or conflict with. This flag is the
  only thing that makes a saved room rejoin itself at launch.

**Verified against the real app, not the mock/browser branch** (per this task's
explicit instruction): `cargo build --lib -p simple-screensharing-client`,
`cargo build -p simple-screensharing-client-tauri`, and `cargo test -p
simple-screensharing-client --lib` (157 passed, 0 failed, up from 153 — 4 new
`rooms::tests` covering `set_auto_connect`'s found/not-found cases and the legacy-TOML
default) all pass. `node --check frontend/scripts/shell.js` passes.

Live verification ran the real sidecar binary (`target/debug/
simple-screensharing-client-tauri`, the exact binary Electron spawns) directly against
its real REST `api_server` — driven with `curl` using the printed `API_TOKEN`/
`API_PORT`, the same transport `electron/preload.js` bridges every `invoke()` call
through — with `XDG_CONFIG_HOME` pointed at an isolated scratch dir (never touching the
shared `~/.config/simple-screensharing/rooms.toml` this machine's other live sessions
use) and a real join string against the project's already-running relay server on
`127.0.0.1:4433`:

1. `list_saved_rooms` showed the seeded room with `auto_connect: false`.
2. `set_room_auto_connect` (join_string, true) returned success; `list_saved_rooms`
   immediately reflected `auto_connect: true`, and the on-disk `rooms.toml` in the
   isolated config dir showed `auto_connect = true` — the persistence round-trip.
3. Killed the sidecar and started a fresh instance against the same config dir (a real
   cold restart, not a simulated one) with two saved rooms, one `auto_connect = true`
   (`AutoVerify`) and one `false` (`NotAuto`). The very first `list_saved_rooms` poll
   after the new process came up showed `AutoVerify` already `state: "joined"` — reached
   with zero manual `join_room` calls from the test — while `NotAuto` stayed
   `"disconnected"`. This is the actual startup-auto-connect behavior working, not just
   the flag persisting.

An attempted GUI (mouse-driven, screenshot) pass through the real Electron app was
started but abandoned: the test desktop turned out to be actively shared with other
concurrent agents' sessions (windows tiling across the same free monitors, a shared
`~/.config/simple-screensharing/rooms.toml` another agent was concurrently writing to),
and a stray click during window-position discovery corresponded with this task's own
sidecar process receiving `SIGTERM` from something outside this task's control. Given
that, further mouse automation was judged too likely to interfere with other agents'
in-flight work and was dropped in favor of the direct-REST verification above, which
exercises the identical real backend/command surface the GUI would have, without the
shared-desktop risk. The AUDIO section's removal and the new checkbox's presence/wiring
were confirmed by code inspection instead (`grep` across `shell.js` turns up zero
remaining references to any of the removed audio-panel identifiers or CSS classes).

### Room-detail panel: title is now click-to-edit, the separate "ROOM NAME" section is gone (2026-07-28)

Owner request: *"when the room settings are open, just let me click the title, to edit the
name. Looks cleaner."* The panel used to show the room name twice — once as a plain,
non-interactive `.room-panel-title` span in the header, and again in a dedicated "ROOM
NAME" section further down the body (its own label, a pre-filled `<input>`, and a "Save"
button calling `renamePanelRoom`). The second section is now gone; the header title itself
is the editor.

`shell.js`'s `renderRoomPanel` (~line 606) now calls a new `renderRoomPanelTitle(room)`
helper (defined just above `renderRoomPanel`) instead of building the plain title span
inline. Clicking the title swaps it for a text `<input>` pre-filled with the current name,
in the same header slot (`.room-panel-title-wrap`). Commit/cancel behavior:

- **Enter** commits — calls `renamePanelRoom(room.join_string, <trimmed value>)`, same as
  the old "Save" button did.
- **Escape** reverts without saving (re-renders the panel, discarding the edit).
- **Blur** (clicking away) also commits.

`Why blur commits rather than discards:` the task explicitly left this an open call, with
a lean toward "saving on blur is the friendlier default since users often click away
expecting their edit to stick." Went with that — Escape is the one deliberate
discard-and-revert path; clicking away is treated as "done editing," not "abandon this."
A `settled` flag inside the closure makes sure only one of commit/cancel actually fires
even though Enter's keydown handler and the input's blur handler can both observe the same
interaction (Enter itself doesn't reliably blur the input first).

The privacy note ("Only you see this name. It never leaves your machine.") that used to be
a permanent fixture under the old section now only renders while the title is actively
being edited (`.room-panel-title-privacy-note`, appended next to the input) — not shown at
rest, keeping the header clean per the "looks cleaner" ask, without dropping the
disclosure itself. The underlying claim was re-verified against `rename_saved_room`
(`src/session.rs`): it only touches the local `RoomStore` file
(`room_store.upsert` + `.save()`), never sends the new name over the wire.

Styling: hover/focus affordance on the title reuses `var(--wash-hover-1)`, the same
hover-highlight token `.btn-secondary`/`.btn-icon`/`.room-row` already use elsewhere in
this file, so clickable-title behavior reads as part of the same visual language rather
than a one-off. New classes: `.room-panel-title-wrap`, `.room-panel-title-input`,
`.room-panel-title-privacy-note` (`shell.css`); the old `.room-panel-rename-row` rule was
deleted (dead now that the "ROOM NAME" section is gone), `.room-panel-privacy-note` itself
was kept since the new title editor still uses it.

**Verified:** `node --check frontend/scripts/shell.js` passes. Two verification passes:

1. Mock-mode pass (`frontend/index.html` loaded directly in a browser, no Tauri backend,
   `window.__TAURI__` undefined so `renamePanelRoom` takes the mock branch): opened a
   room's side panel, clicked the title, confirmed it became an input pre-filled with the
   current name, typed a new name and pressed Enter (title updated, sidebar room row
   updated too), reopened the editor and clicked away to blur (also committed), reopened
   once more and pressed Escape (reverted, no save) — all four outcomes matched the
   behavior described above.
2. **Real-backend pass** (the one that actually matters for shipping this): built this
   worktree's sidecar (`cargo build -p simple-screensharing-client-tauri`), launched the
   real Electron shell against it with `XDG_CONFIG_HOME` pointed at an isolated scratch
   dir (so it could never touch the real `~/.config/simple-screensharing/rooms.toml` a
   live session elsewhere on the machine might be using), joined a real room against the
   project's already-running relay server on `127.0.0.1:4433`, opened its panel, clicked
   the title, typed a new name, pressed Enter, and confirmed the rename round-tripped
   through the real `rename_room` Tauri command all the way to disk — the on-disk
   `rooms.toml` in the isolated config dir showed `name = "Renamed For Real"` after the
   edit, not just an in-memory/mock state change.

---

### Sidebar ROOMS list: inline member/LIVE tree replaces "{count} active", LIVE badge styling, disconnected-dot size fix (2026-07-28)

Three owner requests against the sidebar ROOMS list (`frontend/scripts/components.js`'s
`renderRoomRow`, `frontend/styles/shell.css`).

**Member/LIVE tree.** The room row's meta slot used to show a `"{count} active"` summary
for joined rooms. It's now removed (joined falls through to the same "—" default the
`reconnecting` state already used) and replaced with an always-visible tree below the row:
one indented line per current member (`- <display_name>`), with a `[LIVE]` pill next to any
member who owns a stream. "Owns a stream" reuses the exact same per-stream roster
`renderRoomPanelStreamRow` (`shell.js`) already reads for the room-detail panel's "Live
streams" section — `room.streams` (every announced stream, `{owner, stream_id, kind, ...}`,
populated by `loadRoomMembers` from `room_detail`) tested against each member's `conn_id` —
not re-derived. New markup: `.room-member-tree` / `.room-member-tree-row` /
`.room-member-tree-name` (`shell.css`).

`Why:` (data-fetch tradeoff) `list_saved_rooms` (`dto::SavedRoomRow`) doesn't inline a
roster, so `room.members`/`room.streams` only existed for whichever single room's
detail panel happened to be open (`loadRoomMembers`, fetch-on-open). Showing the tree for
*every visible joined room* in the sidebar needed a real behavior change:
`shell.js`'s `refreshRooms()` now calls `loadRoomMembers` (one `room_detail` round-trip)
for every joined room in parallel, not just the open panel's room, on every refresh —
including on `presence-updated`, which fires routinely. Accepted rather than adding a
cheaper "roster summary" DTO/command: this app's room/member counts are small (a friends
screensharing app), so the added chatter is a handful of extra calls per refresh, not a
scaling risk. Revisit if room/member counts ever grow enough for this to show up as
actual latency.

**LIVE badge styling.** Owner: match the "Stop" destination-card button's pill shape/
weight (`.btn.btn-danger.dest-action-btn`: `padding: 3px 8px`, `border-radius:
var(--rounding-md)`, danger-colored outline border + text — see `stream-control.css`'s
`.dest-action-btn` and `shell.css`'s `.btn-danger`), but with a dot + "LIVE" text instead
of an icon + "Stop" label. New classes `.room-member-live-badge` / `.room-member-live-dot`
(`shell.css`) copy that padding/radius/border treatment. Color: a **genuine red dot**
(`var(--danger)`), not this app's usual green `--success` "live" family
(`.status-dot-joined`/`.status-badge-live`, `status.css`) — the owner's "with a red dot"
instruction was explicit about wanting red for this specific element, so it's taken
literally rather than folded into the green convention.

**Disconnected-dot size fix.** `.status-dot-disconnected` used to override `width`/`height`
to `calc(var(--dot-size) - 3px)` — 3px smaller than `.status-dot-joined`/
`.status-dot-reconnecting`, which both render at the full `var(--dot-size)`. That override
is now removed; the disconnected dot keeps its existing hollow-ring look (`background:
transparent; border: 1.5px solid var(--text-faint)`) but at the same size as every other
status dot. Confirmed by screenshot comparison (mock mode) that this was the entire cause
of the size mismatch — no separate icon/glyph was involved.

**Verified:** `node --check` on `components.js`/`shell.js` passes. No backend/Rust files
were touched (the member/stream data was already surfaced via the existing `room_detail`
command through `loadRoomMembers`), so no `cargo build`/`cargo test` pass was needed for
this change. Ran the app in mock mode (`frontend/index.html` served statically, no Tauri
backend, driven with Playwright — `window.__TAURI__` undefined) with an enriched
Gaming Night mock entry (3 members, 2 concurrently live) added alongside Movie Night's
existing single-member/single-stream entry specifically to exercise a multi-member,
mixed-LIVE tree. Screenshots confirmed: the ROOMS list shows the member tree with `[LIVE]`
pills instead of "N active" text; the pills render as small red-outline pills with a red
dot, matching the "Stop" button's pill weight; and the "Old Room" (disconnected) status dot
is now the same size as the joined/reconnecting dots. Test window stayed off `HDMI-A-1`
(confirmed free monitors via `xrandr --listmonitors` first, per this session's safety
constraint) — a Playwright-driven static-file view has no on-screen window placement to
begin with, but the constraint was checked before starting regardless.

---

### Investigated: "Streaming Audio doesn't work anymore" report; fixed "+ Add audio" being unreachable on a staged card (2026-07-29)

Owner report: audio sharing, previously working, stopped working. Investigated the whole
real (non-mock) chain rather than guessing from the day's changelog:

- **Dispatch registration** — `start_audio_share`/`select_audio_source` are present, with
  matching arg names, in both `src-tauri/src/main.rs`'s `generate_handler!` list and
  `api_server.rs`'s REST dispatch table. Not the "registered in one but not the other"
  class of bug that hit `set_room_auto_connect` earlier this session.
- **The resolution/GPU-encoding `go_live`/`add_destination` fix (`b0394bb`, this session)
  does not touch the audio path at all** — `add_destination_with_params` bails out early
  (`if capture.kind != StreamKind::Video { return; }`) before it ever reaches the
  `handle.source_caps()` call that fix added, and `start_audio_share` (`session.rs`) is a
  wholly separate code path that never calls `add_destination`/`go_live`. Confirmed by
  reading, not assumed.
- **`src/audio.rs` has had zero functional changes** since `e8d8385` (the "it was
  broadcasting the microphone, not the app" fix earlier this session) — only a test-only
  `volume=0.02` tweak (`47b0e2d`). The capture/link/announce pipeline itself is untouched.
- **Real-hardware proof the pipeline itself still works:** ran
  `end_to_end_test::watching_video_automatically_delivers_the_sharers_audio_too` (real
  relay, real capture, real PipeWire loopback+link, real Opus decode) — passed, with real
  audio flowing (sender capture level 0.366, viewer received 4 Opus packets). Separately,
  drove the real Electron app + real built sidecar with Playwright's `_electron` driver,
  calling `select_audio_source`/`start_audio_share` against a real running application
  (TeamSpeak) exactly as the frontend does: reached `state: "live"` within ~1s, no error.
  **The backend audio-sharing chain is not regressed.**
- **The actual regression: an unreachable UI entry point.** This session's "Go live"
  staging redesign (`a4be944`) made a fresh screen share land in a **staged** card (no
  destination yet) instead of going live immediately, and — in the same commit —
  `components.js`'s `destinationCard` was written to skip the audio row entirely while
  `opts.staged` is true, with the reasoning "audio attaches to a room, and a staged card
  doesn't have one until Go live picks it." That reasoning doesn't hold: a staged card's
  own Room picker (`stream-control.js`'s `getStagedEdit().roomHandle`, defaulted to the
  first joined room, changeable via `onStagedRoomChange`) already resolves the exact
  `room_handle` "Go live" itself sends — nothing about it is unavailable pre-Go-live. The
  practical effect: since `a4be944` landed, **"+ Add audio" doesn't exist anywhere in the
  UI from the moment a share starts until "Go live" is pressed** — not disabled, not
  hidden-with-a-hint, just structurally absent. A user used to attaching audio as part of
  starting a share (the pre-staging-redesign flow) hits a wall with no visible affordance
  and no error, which reads exactly as "audio sharing doesn't work anymore." Confirmed by
  loading the real (mock-data) UI in a browser and staging a fresh share: the resulting
  card showed Resolution/Framerate/Bitrate/Room/Preset/Framerate/Advanced tuning/Go
  live/Cancel — no audio row of any kind. This is a legitimate mock-data check per this
  project's testing policy: it's pure DOM/CSS structure, not real Tauri/backend behavior.

**Fix (`frontend/scripts/components.js`, `frontend/scripts/stream-control.js`):**

- `destinationCard`'s audio-row block now renders for `dest.kind !== "audio"` unconditionally
  (dropped the `&& !opts.staged` exclusion) — a staged card gets the same "No audio
  attached" / "+ Add audio" / attached-audio row a live card does.
- `stream-control.js`'s `stagedCardEl` now looks up any audio already paired to the staged
  capture (`mockDestinations.find(d => d.kind === "audio" && d.paired_capture_id ===
  staged.capture_id)` — no room match needed, unlike the live-card lookup, since a staged
  capture has exactly one room, not a fan-out set) and passes a staged-specific
  `onAddAudio: (d, source) => startAudioShare(edit.roomHandle, source, d.capture_id)` —
  using the staged card's own Room-picker value, not a `dest.room_handle` field a staged
  capture's DTO (`StagedCaptureRow`) doesn't have. `onAudioMute`/`onAudioStop`/
  `onRefreshAudioSources` are reused unchanged (they operate on capture/destination ids,
  identical either way). No backend/Rust change was needed — `start_audio_share`
  (`session.rs`) already accepted any valid `room_handle` + optional `for_capture_id`; the
  only gap was the frontend never calling it from a staged card at all.

**Height (the separate, already-diagnosed styling ask):** the staged card's "+ Add audio"
now needs to read at the same deliberate size as its "Go live"/"Cancel" (`.dest-actions-
staged .dest-action-btn`, bumped to a measured 32px tall earlier this session), not the
small routine-button sizing `.dest-chip-add-btn` uses on a live card (measured 26px — a
live card already has Transfer/Stop for its "routine" affordances, so a small audio button
there doesn't read as under-weighted the way it would on a staged card, which has nothing
else routine to compare against). Added `.dest-audio-add-btn-staged` (new modifier class,
applied only when `opts.staged`, mirroring the existing `.dest-actions-staged .dest-action-
btn` scoped-modifier pattern rather than changing `.dest-chip-add-btn`/`.dest-audio-add-btn`
globally, which live cards also use): `padding: 6px 14px 6px 12px; font-size: 14px;
font-weight: 700`. Landed on those numbers by measuring the *real rendered* height
(`getBoundingClientRect().height`) after each attempt rather than trusting the padding math
— a first pass at `padding: 7px …` measured 34px, not 32px, confirming this session's
existing lesson (font-size changes line-height in ways padding numbers alone don't
predict) applies here too. Final measurement: staged "+ Add audio" is exactly 32px,
matching `.dest-go-live-btn`; the live-card "+ Add audio" is untouched at 26px.

**Verified:**

- `cargo build --lib -p simple-screensharing-client` and `cargo build -p
  simple-screensharing-client-tauri` pass (no Rust changed, but confirmed clean).
- `cargo test -p simple-screensharing-client --lib`: 160 passed, 0 failed, 15 ignored — no
  regressions.
- `cargo test -p simple-screensharing-client --lib -- --ignored`, including
  `watching_video_automatically_delivers_the_sharers_audio_too` (real audio flowing, see
  above): all pass.
- `node --check` on both touched JS files passes.
- Real-app pass (Playwright `_electron` driver against the real built sidecar + a real
  running relay, `xrandr --listmonitors` checked first per this session's HDMI-safety
  rule, window confirmed off `HDMI-A-1`): `select_audio_source`/`start_audio_share` against
  a real running application reached `state: "live"` in under 1.5s with no error — the
  exact backend call the fixed staged-card button now makes.
- Mock-data browser pass (legitimate for this pure-DOM/CSS half of the change, per this
  project's testing policy): staged a fresh share, confirmed "+ Add audio" now renders on
  it, measured its height at 32px against `.dest-go-live-btn`'s 32px — exact match — and
  confirmed the live-card instance is unaffected (still 26px).
- Driving an actual staged **video** capture through the real portal consent flow (rather
  than mock data) to click-test the fixed button end-to-end was not attempted: `xdg-
  desktop-portal`'s `ScreenCast` picker is a native, interactive system dialog outside
  Playwright's Electron driver's reach (this codebase's own real-hardware Rust tests work
  around this with a `videotestsrc`-backed test-only capture seam,
  `capture::start_sharing_from_test_source`, which has no exposed Tauri command). The
  real-app coverage above exercises the exact same `start_audio_share` backend call the
  button makes (proven working with real hardware), and the frontend wiring change is a
  parameter substitution into that same already-real-verified call — not a new code path.

### Follow-up same day: the picker was reachable but "Share" silently no-op'd — a `String`/`Number` mismatch in the dropdown value compare (2026-07-29)

The fix above made "+ Add audio" **reachable**; it did not make it **work**. Next owner
report, seconds after confirming the picker opened: *"Pressing the Share button just does
nothing right now"* — a source ("Floorp") visibly selected in the dropdown, no error, no
loading state, no card change, on a **live** card (staged not yet confirmed broken/working
at report time).

**Root cause, confirmed by reading before touching anything:** `dto::AudioSourceDto.id` is
`u32` (`src-tauri/src/dto.rs`) — it round-trips through Tauri/REST JSON as a JS `Number`,
never a string. `Components.dropdown()`'s `.value` getter (`components.js`) returns
`currentValue` **raw, unstringified** — it's set from `o.value` (`= s.id`, a `Number`) the
instant an option is clicked. `addAudioRow`'s "Share" button handler compared:

```js
const source = sources.find((s) => String(s.id) === select.value);
```

`String(262) === 262` is `false` — always, for every real source, since `s.id` is a `Number`
on both sides of the `===` and only one side went through `String(...)`. `source` was
therefore always `undefined`, the bare `if (source) handlers.onAddAudio && …` guard silently
did nothing (no `try/catch`, no error path exists there at all), and `stopAudioSourcePolling()`
still ran — so the picker just closed with no observable effect, exactly matching "Share does
nothing." This is the same shape of bug this project has hit before with loose `===` across a
serialization boundary — see `dropdown()`'s own `findOption` (`String(o.value) === String(v)`,
both sides stringified), which is what the *rendering* path already did correctly; the Share
handler was the one caller that didn't follow that pattern.

**Both card types were equally broken** — `addAudioRow` is one shared implementation for
staged and live cards, so this wasn't staged-vs-live at all (unlike the "unreachable" bug
above, which genuinely was staged-only). The owner's report happened to be on a live card
first; testing confirmed the staged card was identically broken before the fix and identically
fixed after.

**Fix (`frontend/scripts/components.js`, one line):**

```js
const source = sources.find((s) => String(s.id) === String(select.value));
```

Matches `dropdown()`'s own `findOption` convention: stringify both sides of an id compare that
crosses a JSON-serialization boundary, never just one.

**Verified — real Electron + real built sidecar, Playwright `_electron` driver, per this
project's testing policy (not mock/browser):** staged a real capture (real `xdg-desktop-portal`
consent already granted from earlier in this session, so the picker reached a real `READY`
card with no portal prompt needed this run), used "+ Add audio," selected the real,
already-running **Floorp** browser (per the owner's explicit instruction — no synthetic
beep/tone) from the real `list_audio_sources` result, clicked "Share":
- **Staged card:** row correctly went from "No audio attached" to a live "Floorp / Mute /
  Stop" row.
- **Live card** (staged the same capture, pressed "Go live" first, then repeated the same
  "+ Add audio" → select Floorp → Share flow): same correct result, "Floorp / Mute / Stop"
  row appeared on the live destination card.
- One earlier attempt (staged card, first try) showed the picker collapsing back to "No audio
  attached" with a too-short 1.5s wait after the click — not a repeat of this bug: the backend
  call (`select_audio_source` → `start_audio_share`) genuinely takes a variable amount of time
  to bring the real PipeWire capture up, and the frontend UI update, in the real branch, relies
  entirely on the backend's `destination-state-changed` event round-trip (`startAudioShare`'s
  real branch calls no `render()` of its own — unlike `setAudioShareMuted` right below it in the
  same file, which does). A longer wait on the retry showed the row appear correctly. Worth
  a future look if the owner reports the row taking a visibly long time to appear after
  "Share," but it did not reproduce the "does nothing at all, ever" symptom this task was
  scoped to — that was the `String`/`Number` compare above, and it's fixed.
- `cargo build --lib -p simple-screensharing-client`: passes (no Rust touched).
- `cargo test -p simple-screensharing-client --lib`: 160 passed, 0 failed, 15 ignored — no
  regressions.
- `node --check` on the touched file passes.

### Follow-up same day: live cards' "Stop"/"Share" were still small — the staged-vs-live size distinction removed entirely (2026-07-29)

Owner, from a screenshot of a running/live share's card: *"For a running stream, the 'Share'
and 'Stop' buttons are still too small."* Earlier the same day, only the **staged** card's
"Go live"/"Cancel" (`.dest-actions-staged .dest-action-btn`) and "+ Add audio"
(`.dest-audio-add-btn-staged`) were bumped to a measured 32px — the live card's equivalent
"routine" buttons (Transfer/Stop, sharing `.dest-action-btn`'s base rule; and the audio
picker's "Share" confirm, `.dest-audio-share-btn`) were deliberately left at the smaller
~22px size on the theory that a live card already has other "routine" actions to compare
against, so a small button there wouldn't read as under-weighted. The owner's report is
direct evidence that theory didn't hold in practice.

**Approach chosen: base-class bump, not a new `-live` modifier.** `.dest-action-btn` is used
at exactly four call sites — staged "Go live"/"Cancel" and live "Transfer"/"Stop"
(`components.js`'s `destinationCard`, ~line 1417-1444) — confirmed by grepping every use
before touching anything. `.dest-audio-share-btn` is used at exactly one call site (the
audio picker's "Share" button, `addAudioRow`, staged and live alike), with the `-staged`
modifier conditionally added only for staged cards. Since the owner now wants **all** of
these at the same size, there is no longer a live-only "small/routine" tier left to
preserve, so the cleanest fix was to bump the base rules themselves to the values the
`-staged` modifiers used to carry, and delete those modifiers as redundant rather than add
a parallel `-live` variant next to them:

```css
.dest-action-btn {
  padding: 6px 22px;
  height: auto;
  font-size: 14px;
  font-weight: 700;
}
.dest-audio-share-btn {
  padding: 6px 22px;
  height: auto;
  font-size: 14px;
  font-weight: 700;
}
```

`.dest-actions-staged .dest-action-btn` and `.dest-audio-share-btn-staged` were removed from
`stream-control.css`; `components.js`'s conditional `+ " dest-audio-share-btn-staged"` on the
Share button's `className` was removed too (now just `"dest-audio-share-btn"` unconditionally
— nothing in the DOM changes between staged and live for this button any more). `.dest-actions-
staged` itself is kept as a class on the staged row (in case future staged-only layout rules
want it) but no longer changes any button's size.

**"+ Add audio" (the dashed affordance, not the Share confirm) was deliberately left alone.**
The owner's report named "Share" and "Stop" specifically, not this button, and it didn't look
visibly mismatched next to the now-bigger Stop in a real screenshot — it sits in its own row
(the audio row, next to "No audio attached"/the attached-audio state), not directly beside
Stop in the same row, so the size difference doesn't read as jarring the way Stop/Share
themselves did sitting directly in the button row. `.dest-audio-add-btn-staged` (the staged
card's already-bumped version, from the block above) is untouched.

**Superseded later the same day — see "Full sweep" below.** This "left alone" call turned out
to be about the wrong buttons: the owner's next screenshot was of the audio row's own
Mute/Stop (`.dest-audio-mute`/`.dest-audio-stop`), not this "+ Add audio" chip, and came with
an explicit "all of them" instruction that closed off further per-button judgment calls on
this tab.

**Measured (real Electron + real built sidecar + a real local relay server, Playwright
`_electron` driver, `getBoundingClientRect()` — not assumed from padding math, per this
session's own recurring lesson that font-size changes line-height non-obviously):**

| Button | Before | After |
| --- | --- | --- |
| Live "Stop" | 43.7×22 px | 78.7×32 px |
| Live "Share" (audio picker confirm) | 49.6×22 px | 86.2×32 px |
| Staged "Go live" (unaffected, sanity check) | 93.0×32 px | 93.0×32 px |
| Staged "Cancel" (unaffected, sanity check) | 93.9×32 px | 93.9×32 px |
| Staged "+ Add audio" (unaffected, sanity check) | 107.5×32 px | 107.5×32 px |

"Transfer" shares `.dest-action-btn` with "Stop" (same call-site class), so it grows
identically — not a separate decision, per the class audit above; not separately measured
since a second joined room is needed to render it at all and the class is proven identical.

**Verified — real app, not mock/browser:** `xrandr --listmonitors` checked first (DP-1/2/3
free, HDMI-A-1 untouched — window placed at DP-1's `50,50` and bounds confirmed in-range).
Built this worktree's own sidecar (`cargo build -p simple-screensharing-client-tauri`),
spawned a real local relay (`simple-screensharing-server`'s prebuilt binary, headless via the
same `setsid script -qc ...` wrapper the Rust `end_to_end_test.rs` harness uses, an isolated
`XDG_CONFIG_HOME` scratch dir so it never touched the shared `rooms.toml`), and drove the real
Electron app with Playwright's `_electron` driver: joined a real room over that relay
(`join_room` invoked directly, bypassing the join form), staged a real capture via "Share
another screen" (real `xdg-desktop-portal` consent resolved non-interactively, as it did
earlier this session), selected the room, clicked "Go live" to get a genuinely `live`-state
destination card, then measured Stop/Share both **before** this change (`git stash` to the
pre-edit tree) and **after** (`git stash pop`) in separate runs against the same real flow —
table above. Screenshots of both card types confirmed visually: the live card's Stop/Share
now read at the same weight as the staged card's Go live/Cancel/+ Add audio, and the staged
card itself is pixel-identical to before (no regression from removing its now-redundant
`-staged` modifier rules). `node --check` on both touched files passes.

### Full sweep: the audio row's own Mute/Stop, and an exhaustive audit of every button on the tab (2026-07-29)

Owner, from a screenshot of the audio row next to "Floorp": *"Mute" and "Stop" are visibly
too small next to 'Floorp' (a real audio source). Make sure that all of the buttons on this
page have the same height as the ones in 'CREATE / JOIN A ROOM' (32px tall, to be very clear
about this!) Make sure to apply this to all of them (including for the Stream 'Go live' and
'Stop').* Explicit "all of them" — no further per-button judgment calls, full sweep of every
button-like element reachable from the Stream Control tab's render tree
(`stream-control.js`/`components.js`).

**The actual bug: `.dest-audio-mute`/`.dest-audio-stop` were never touched by the two earlier
2026-07-29 passes above** — both of those bumped `.dest-action-btn` (Go live/Cancel/Transfer/
Stop) and `.dest-audio-share-btn` (the audio picker's "Share" confirm) to 32px, but the
*attached-audio row's* own Mute/Stop buttons (rendered by `audioAttachedRow` in
`components.js`, ~line 1487) still carried their original small sizing (`padding: 3px 0`/
`3px 8px`, `font-size: 11px`) — exactly what the owner's screenshot showed, ~22px tall next to
32px-tall siblings.

**Fix — same recipe as `.dest-action-btn`/`.dest-audio-share-btn`:**

```css
.dest-audio-mute {
  flex: 0 0 auto;
  width: 76px;       /* was 64px — widened to fit "Unmute" at the larger font */
  padding: 6px 0;     /* was 3px 0 */
  height: auto;
  font-size: 14px;    /* was 11px */
  text-align: center;
}
.dest-audio-stop {
  flex: 0 0 auto;
  padding: 6px 22px;  /* was 3px 8px */
  height: auto;
  font-size: 14px;    /* was 11px */
}
```

**Exhaustive audit of every button-like element on the tab** (every `primaryButton`/
`secondaryButton`/`dangerButton`/raw `<button>` reachable from `stream-control.js`'s render
tree, cross-checked against `stream-control.css`):

| Class | Where | Before | After | Verdict |
| --- | --- | --- | --- | --- |
| `.dest-go-live-btn` (`.dest-action-btn`) | staged card "Go live" | 32px | 32px | already correct |
| `.dest-action-btn` (danger) | staged card "Cancel" | 32px | 32px | already correct |
| `.dest-action-btn` (danger) | live card "Stop" | 32px | 32px | already correct |
| `.dest-action-btn` (secondary) | live card "Transfer" | — | — | same call-site class as Stop; not separately re-rendered this pass (needs a 2nd joined room), proven identical by class audit, not re-measured |
| `.dest-audio-share-btn` | audio picker "Share" confirm (staged + live) | 32px | 32px | already correct |
| `.dest-audio-add-btn-staged` | staged card "+ Add audio" | 32px | 32px | already correct |
| `.dest-audio-mute` | attached-audio row "Mute"/"Unmute" | **22px** | **32px** | **fixed this pass** |
| `.dest-audio-stop` | attached-audio row "Stop" | **22px** | **32px** | **fixed this pass** |
| `.mode-toggle-btn` | BANDWIDTH card's Absolute/Per-Stream toggle; the Tune/Quality Level/Rate Control pill switches | 25px | 25px (unchanged) | **flagged, not changed** — see below |
| `.sc-ghost-row` | "Share another screen" ghost row | 48px | 48px (unchanged) | **flagged, not changed** — see below |
| `.dest-chip-add-btn` (base, no `-staged`) | live card's "+ Also send to…" fan-out chip; live card's non-staged "+ Add audio" | ~26px (per earlier session note) | unchanged | **flagged, not changed** — see below |

**Two classes deliberately left alone — judgment calls, not oversights:**

- **`.mode-toggle-btn`** (`.mode-toggle`/`.mode-toggle-btn` in `shell.css`) is a segmented
  pill *switch* (Absolute Maximum / Per-Stream Maximum; Low-Latency / Balanced / Quality;
  Tune/Quality Level/Rate Control), not a momentary-action button — it reads and behaves like
  a tab strip (compare `.add-room-tabs`, the Join/Create tabs on the Create/Join dialog, which
  share this same base class and are also not 32px). Forcing it to 32px would make the
  BANDWIDTH card's mode toggle taller than its own label row and inconsistent with every other
  pill switch in the app (language switch, encoder preset picker) that this same class backs
  outside the Stream Control tab too. Left at its current 25px.
- **`.sc-ghost-row`** ("Share another screen") is a deliberate 48px "headline invitation" —
  documented in `stream-control.css` as intentionally *larger*, not smaller, than a routine
  button: a full-width dashed-accent CTA row, not a button chip. Shrinking it to 32px would
  make the tab's primary entry point read as a routine action instead of the headline one.
  Left at its current 48px.
- **`.dest-chip-add-btn`** (base rule, backing the fan-out "+ Also send to…" chip and the
  live card's non-staged "+ Add audio") is the same dashed/chip "ghost add" visual family as
  `shell.css`'s `.ghost-add-room-row` used elsewhere in the app for the identical affordance
  ("add another X" as a muted secondary action, not a primary confirm) — changing it here
  alone would make this tab's chip style diverge from that established cross-app pattern.
  Not re-measured this pass (neither renders without extra setup a full sweep didn't need:
  fan-out needs a second federated room, and the live "+ Add audio" chip is superseded the
  instant audio is attached, at which point Mute/Stop render instead) — flagged here instead
  of silently left alone, per the instruction to call out rather than assume.

**Verified — real app, not mock/browser.** `xrandr --listmonitors` checked first (DP-1/2/3
free 0–5759px, HDMI-A-1 untouched from 5760px on). Built sidecar reused from this session
(`target/debug/simple-screensharing-client-tauri`, pointed at via `SIDECAR_BIN`). Playwright
`_electron` driver against this worktree's real `electron/main.js`, window forced to DP-1
(`50,50`) and bounds confirmed in-range before interacting. Drove the real flow: "Share
another screen" → staged card → opened the audio picker → selected the real running
**Floorp** browser (already producing real system audio, confirmed via `pactl list
sink-inputs` showing an active `application.name = "Floorp"` sink input — no synthetic test
tone spawned) → "Share" → real Mute/Stop rendered on the staged card, then "Go live" → same
real Mute/Stop measured again on the live card. `getBoundingClientRect()` on every button
class above, both before and after the CSS edit, in two separate real-app runs — not assumed
from padding math (this project's own recurring lesson: font-size changes line-height in ways
padding numbers alone don't predict). Screenshots confirm visually: Mute/Stop now read at the
same weight as Go live/Cancel/Stop/Share on both card types, with a real green audio-level
meter animating next to "Floorp". `node --check` on `components.js` (untouched — this pass
was CSS-only) and `stream-control.js` (untouched) both pass.

---

### Per-option level meters get a loading state; the "still-inconsistent bar width" report's real root cause measured fresh (2026-07-29)

Earlier the same day, the audio source dropdown's per-option level meters (`.dd-option-meter`/
`.dd-option-meter-bar`, `components.js`'s `DD_OPTION_METER_BARS`/`paintDdOptionMeter`, backed
by real per-candidate `AudioLevelTapHandle` taps via `start_audio_level_tap`/
`poll_audio_level_tap`) shipped, and a follow-up ("Tighten the lit VU-meter bar's glow")
copied `.dest-audio-meter-bar-lit`'s already-confirmed `box-shadow: 0 0 1px` fix onto this
meter's `.dd-option-meter-bar-lit`. The owner reported the bars still visibly differed in
width after that fix. Two asks this pass: add a loading state for the gap before the first
real level arrives, and actually root-cause the width report instead of assuming the same fix
applied elsewhere.

**Loading state.** Every option's meter now starts in a `.dd-option-meter-loading` state
(`components.js`'s `renderOptions`) — a CSS pulse (`dropdown.css`'s `dd-option-meter-pulse`
keyframes, bars staggered via `animation-delay` on `nth-child` so it ripples rather than
blinking in lockstep) instead of the plain dim rest state, covering the gap between "dropdown
just opened, tap just started" and "first real reading arrives." `setOptionLevel` clears the
class the instant real data lands for that option, handing off to the existing
`paintDdOptionMeter` height/opacity painting.

**Root cause, measured fresh rather than assumed:** real Electron + real sidecar + a real
local relay (spawned the same `setsid script -qc ...` way `end_to_end_test.rs` does, isolated
`XDG_CONFIG_HOME`), Playwright `_electron` driver, real system audio (Floorp actually
producing sound). `getBoundingClientRect()` on every `.dd-option-meter-bar`, lit and unlit
alike, across several captured frames: **every bar measured exactly 5px wide in the DOM,
lit or unlit, with or without the `box-shadow`.** There was never a real layout-width
difference. Zoomed real screenshots (same crop/scale, lit vs. unlit) showed the actual cause:
a lit bar's `box-shadow` glow spills a soft anti-aliased fringe past its otherwise-sharp box
edge. At `.dest-audio-meter-bar`'s larger scale (~26px meter / 5 bars) that same `0 0 1px`
fringe is a small fraction of the bar and reads as fine; at this meter's much smaller scale
(18px / 3 bars ≈ 5px/bar) the identical absolute fringe width is proportionally far more of
the bar, so it visibly widened the lit bars despite zero DOM difference — confirming the
task's "irradiation/perceptual illusion, not geometry" hypothesis. Fix: removed the
`box-shadow` from `.dd-option-meter-bar-lit` entirely (rather than shrinking it further) —
full opacity plus the saturated `--success` color still read clearly as "lit" without any
glow, and a before/after zoomed screenshot comparison confirmed the fringe is gone and lit/
unlit bars now show pixel-crisp, equal-width edges. `.dest-audio-meter-bar-lit` was left
untouched — its glow was already confirmed correct at its own scale; this was never actually
the same bug in two places, despite looking identical in the CSS diff.

**Verified — real app, not mock/browser.** `xrandr --listmonitors` checked first (DP-1/2/3
free, HDMI-A-1 untouched — window placed at DP-1's `50,50`, bounds confirmed in-range). Built
this worktree's sidecar, spawned a real local relay on an isolated port/`XDG_CONFIG_HOME`
scratch dir, created and joined a real room via the real `create_room` command, staged a real
capture via "Share another screen" (real portal consent resolved non-interactively), selected
the room, "Go live," then "+ Add audio" to open the real level-metered dropdown against a
real audio source (Floorp, already producing sound — a synthetic `audiotestsrc` test tone was
also spawned as an extra candidate row, capped to 5% real routed volume per this session's
audio-testing policy, verified via `pactl list sink-inputs` before capping and cleaned up
after). Confirmed: the loading pulse shows on every option immediately on open (`.dd-option-
meter-loading` present on all rendered meters right after the dropdown opens) and clears the
instant each option's first real reading lands. `getBoundingClientRect()` measurements (lit
and unlit, before and after the CSS fix) and zoomed real screenshots both confirm the bars now
read as equal width. `cargo test --lib -p simple-screensharing-client`: 163 passed, 0 failed,
19 ignored — no regressions. `node --check` on `components.js` passes.

---

## Deliberately not built here

The audio-as-attachment model (audio is still its own capture and its own stream id on the
wire — only its *presentation to a viewer* is now bundled with video, see above);
the View tab's tiled grid and tiered restart placeholders; per-destination floor/ceiling
*per* destination (one global floor is exposed, and every destination shares the project's
40 Mbit/s ceiling); an idle-audio reservation in `AllocInput::audio_reserved_kbps` (left
explicit at 0 until audio becomes an attachment with a known constant rate).
