# Go live and destinations

Staging a capture, attaching destinations, fan-out, transfer, live edits, resuming and retrying, plus the test coverage of those paths.

Part of the Stream Control reference, hub: `superdoc/features/stream-control.md`. The text moved here verbatim from that page: "above", "below" and "this page" in it may point at a section that now lives in a sibling page of this folder (the hub lists them).

## "Go live": staging a capture with zero destinations, then attaching the first one (2026-07-28)

**Owner's request:** pressing "Share another screen" opens the stream settings with the real
capture already running but **nothing sent**; a "Go live" button then actually goes live, and the
settings used at that moment are saved as the defaults for next time. (The dead-weight preview
thumbnail on the card was removed in the same request: it never showed the actual stream.)

**Why no pipeline restructuring was needed:** the fan-out primitive
(`CaptureHandle::spawn_add_destination`) already adds a branch to a *running* tee, and a capture
pipeline is fully `Playing` with real negotiated caps before its first destination exists. So the
split was only in the entry points:

- `capture::start_capture(framerate) -> CaptureHandle` is the staged half: source acquired
  (portal prompt), pipeline up, bus pump running, `destinations` empty.
  `start_sharing`/`start_sharing_from_test_source` call a shared helper and then attach their
  first destination as before (the one-call path, still used by `AppState::start_video_share`).
- `AppState::stage_video_share() -> CaptureSourceId` (`session.rs`) allocates the id and inserts a
  `LocalCapture` with an empty `destinations` map. No room, no `stream_id`, no `AnnounceStream`.
- `AppState::go_live(capture_id, room_handle)` is `add_destination`'s fan-out primitive used for
  the first branch. It raises the source-framerate floor if needed
  (`CaptureHandle::set_source_framerate`), builds `VideoParams` from the *current* process-wide
  framerate/bitrate/tune/quality/codec, and calls `add_destination_with_params` with
  `PendingRole::GoingLive` (identical to `FanOut`, a separate variant only so the UI says "Going
  live" rather than "Also sending here" while it waits for the first IDR).
- **Room-preview thumbnail:** `preview::spawn_preview_task` needs a client and `stream_id`, which a
  staged capture lacks, so `CaptureHandle` holds the preview appsink unconsumed
  (`max-buffers=1 drop=true`, so no backpressure) until the first destination is registered;
  `ensure_preview_task` then starts it once, and only a capture's first-ever destination gets the
  periodic thumbnail.
- "Cancel" on a staged capture is just `stop_share` on the capture id.

**Two regressions found right after, both from `go_live`/`add_destination` building `VideoParams`
by hand (2026-07-28):**

1. *Resolution 0x0 after going live.* The placeholder `width: 0, height: 0` was never resolved for a
   destination attached via `add_destination_with_params` (fan-out, transfer and `go_live` all use
   it). Fix: with no sibling destination to copy, read `handle.source_caps()` off the capture.
2. *GPU (VAAPI) encoding silently disabled.* They used `EncoderBackend::default()` (static
   `Software`) instead of `capture::detect_encoder_backend()`, and nothing downstream re-derives the
   backend (`branch::add_branch_live` uses `VideoParams.backend` as given), so every "Go live"
   encoded on the CPU. Fix: call `detect_encoder_backend()` (now `pub(crate)`). `start_video_share`'s
   own default is harmless because `start_sharing_with_source` builds its own params.
   Coverage: `end_to_end_test::go_live_attaches_the_first_destination_with_real_resolution_and_backend`
   (`#[ignore]`d, real relay + test-source capture; it failed before the fix with `left: (0, 0),
   right: (640, 480)`).

**Defaults are saved once, at "Go live"**, not on every control change (the owner's explicit ask);
see "Start-stream defaults" below. A UI that renders a live card must seed its displayed tuning from
what it just staged: the destination row carries no tune/quality fields, so the
staged values are the only copy (the former Electron UI once showed "Balanced" on a card that really ran
the staged "Low Latency" because it skipped this seed; the encoder was never affected).

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
| `BranchSwap` | resolution / fps (≤ source rate) / encoder backend / tune / quality level / `gop_millis` / codec | `branch::swap_to_new_branch`: build the replacement alongside the original, wait for its first IDR, flip, drop the old one. A forced keyframe follows the flip (the readiness IDR was consumed proving the branch). | nothing (row shows "Applying…") |
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

## Retrying a failed destination

A `Failed` destination's Retry calls `AppState::retry_destination` (`session.rs`), not
`stop_destination` + `start_destination`.

**Why:** that pairing used to be the implementation and had two real bugs. It lost the failed
destination's last-configured bitrate/tune/quality (a new destination copies params
from a *sibling* on the capture, and the common case has none, so it silently reset to defaults).
Worse, `stop_destination` tears down the *whole capture* once its destination count hits one, so
`start_destination` bailed silently and retry failed outright for the single-destination case.
When the *capture* is what died (the recovery schedule ran out, see "A lost capture source is
restarted"), Retry starts a fresh recovery of the capture instead. `retry_destination` reads the failed destination's params first, removes just that branch without
touching `ShareStatus`, and re-adds a replacement in the same room with the same params.
Coverage: `end_to_end_test::retry_destination_preserves_prior_encoder_tuning` (real relay + real
capture).

The destination list is deliberately **cross-room**: a destination is bandwidth the user is paying
for right now, and hiding it behind a room selection would make fan-out invisible. Each row folds
the capture's start/stop state, the pending transfer/fan-out role and the live `DestinationState`
(`Starting` / `Live` / `Rebuilding` as "Applying" / `Stalled` as "Waiting for capture" / `Failed`)
into one status. A destination's live bitrate rides the per-tick stats payload
(`Destination::live_params().bitrate_kbps`; formerly `DestinationStatsDto`): the allocator writes it as a tier-1 property write that
emits no state-change event, so a UI that only refreshes on state changes shows a stale rate under
Absolute Maximum. That bug once looked like a backend fault when the allocator was fine (checked
with `capture::branch::tests::setting_one_branchs_bitrate_writes_through_to_that_encoder_only`).

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
  "Burst and wire-clock bugs found in owner testing" above.
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

**Why every settings control reads one local edit buffer (carried over from the removed Electron UI):** several bugs (slider snapping back, "Balanced" shown after Go live, stale bitrate readout, a volume slider that disagreed between windows) were the same shape: a control rebuilt from a server value that had no field for it, or a live change that fires no event. The egui `DestEdit` (`session.rs`) is the local buffer.
