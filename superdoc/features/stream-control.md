# Stream Control (desktop client)

> **Status (2026-10-14).** The client is one native egui binary and this page is its
> **backend** reference: `client/src/capture.rs` (with its inline `destination` and `branch`
> modules), `session.rs`, `bandwidth.rs`, `presets.rs`, `rooms.rs`. It covers "The shape of a
> capture", staging and "Go live", fan-out, transfer, the three tiers, "Capture/encode
> performance" (#39, with the rotated-output check from #35), "Idle monitors" and the silence
> watchdog (#32), resuming, the encoder-tuning backend mapping, "Video codec selection", the
> bandwidth modes, and the audio-share backend. The UI lives in `client/src/ui/stream_settings.rs`,
> `ui/action_bar.rs`, `ui/room.rs`, `ui/modals.rs` (see `superdoc/ui/APP-SHELL.md`).
> The removed Electron frontend's per-widget write-ups (edit panel, START A STREAM form, tab
> layout, sidebar tree, audio-card button audits) were dropped from this page; the *why* that
> outlived them is in "History: the Electron UI" at the end, the full text in version control
> (`git log -- superdoc/features/stream-control.md`).
> The old always-on 320x180 "self-preview" branch is gone; since #78 a valve-gated live preview branch
> feeds the sharer's own tile while the window is focused (`superdoc/features/self-preview.md`, and "Own-tile
> live preview" below).

**Added 2026-07-26.** The feature set built in the second stage of the original three-stage UI
plan (`superdoc/ui/APP-SHELL.md`): starting and editing outgoing streams, **multi-destination
fan-out**, **transfer** between rooms, and the two **bandwidth modes** with
`bandwidth::allocate` wired into live encoders. The plan it implemented is
`superdoc/brainstorm/ui-redesign-multiroom-streaming.md` (final plan items 4, 5 and 7).

---

## The shape of a capture, after this stage

One capture is **one screen grab, one GStreamer pipeline, N destinations**:

```text
GPU domain (Linux portal + VA stack, the normal case; 2026-10-14, #39):
pipewiresrc(DMA-BUF) ! vapostproc(video-direction=auto) ! VAMemory NV12 ! tee
    ├─ queue ! videorate(1fps)  ! vapostproc ! BGRx ! videoconvert ! RGB ! appsink   (JPEG thumbnails, fixed)
    ├─ valve[RateCap 30fps; closed unless the window is focused] ! queue ! vapostproc(<=640x360) ! RGBA ! appsink(async=false)   (own-tile live preview, #78)
    ├─ queue ! valve[RateCap] ! vapostproc ! VAMemory NV12 ! capssetter(fps) ! va*enc ! appsink   ← destination A (room 1)
    └─ queue ! valve[RateCap] ! vapostproc ! VAMemory NV12 ! capssetter(fps) ! va*enc ! appsink   ← destination B (room 2)

System domain (Windows, x264enc machines, tests):
<platform source> ! videoconvert ! capsfilter(format=BGRx) ! tee
    ├─ queue ! videorate(1fps)  ! RGB ! appsink   (JPEG thumbnails, fixed)
    ├─ valve[RateCap 30fps; closed unless the window is focused] ! queue ! videoscale ! videoconvert ! RGBA(<=640x360) ! appsink(async=false)   (own-tile live preview, #78)
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

## Own-tile live preview (task #78, 2026-10-15)

*Why (user request 2026-10-15):* "For the sharer, it should show a stream preview, while he is tabbed into
the program. Otherwise, its fine to hide it." Design, code map and the appsink `async=false` trap:
`superdoc/features/self-preview.md`. In short: one valve-gated tee branch per capture
(`capture.rs` `live_branch`, `LivePreview`, `CaptureHandle::set_live_preview`), <= 640x360 at <= 30 fps RGBA,
open only while the egui window is focused and the own tile was painted within 0.6 s; closed it is a pad call
and an unref per frame. The encode branches are not touched.

**Measured cost (2026-10-15, headless rig, stock xdpw, GPU domain, H.265 240 fps balanced, `vkcube` at 240 Hz; the
sharer window focused or not by `swaymsg '[pid=..] focus'` with a held `wtype` giving the seat a keyboard, see below):**

| state | viewer tooltip, 3 samples (decoded / shown fps, dropped) | sharer process CPU (`top`, 3 s) | live branch sink counter |
| --- | --- | --- | --- |
| sharer unfocused (valve closed) | 234/233, 240/240, 234/234; 0 dropped | 9 % | frozen (126 -> 126 over 12.7 s) |
| sharer focused (preview on) | 236/235, 234/233, 234/233; 0 dropped | 14 % | 30 fps (331 frames in 11.0 s) |
| unfocused again | 233/232, 238/238, 234/233; 0 dropped | 9 % | frozen (457 -> 457 over 10.1 s) |

The preview does not move the share's numbers (about 234 fps H.265 at 240 in all three states); it costs about 5
points of one core while it runs (30 fps scale + download + RGBA upload + 30 fps egui repaint), nothing when closed.
Refocus to a picture on screen: under 0.3 s (the screenshot 0.3 s after `focus` already shows the preview). Screenshots:
the #78 worker's scratchpad (`t78_B_sharer_1.png` / `_2.png` one second apart show the cube moved, `t78_C_...` the
placeholder).

**Three traps found building it (covered by `capture::branch::tests::live_*` where a test can see them):**
- *A sink behind a closed valve stalls the whole capture* unless it is `async=false` (preroll), see
  `self-preview.md`.
- *`videorate drop-only` does not cap a variable-rate portal source:* with the portal's `framerate=0/1` the branch
  delivered 240 fps through `videorate ! framerate=30/1` (counter at the sink). The live branch therefore uses a
  `branch::RateCap` probe on its valve instead (30.1 fps measured). The fixed 1 fps room-preview branch
  (`preview_sink`) was suspected of the same (#78's worker) and checked in #79, see below: it is **not**
  affected.
- *`vapostproc ! video/x-raw,format=RGBA` writes A,B,G,R bytes on this stack* (solid-colour test: RGBA gave
  `ff 35 2c f9`, BGRA the correct `35 2c f9 ff`): the whole preview was red. The branch asks for BGRA and
  `sample_to_live_frame` swizzles to RGBA.

### Room-preview branch rate check (task #79, 2026-10-15): already ~1 fps, unchanged

Question (from #78): does the fixed `queue ! videorate drop-only ! framerate=1/1 ! ... ! preview_sink` branch run at
the full source rate, as the live branch's `videorate` did? **No.** Measured in the rig (GPU domain, stock xdpw,
sharer 240 fps H.265, viewer decoding 234 fps / shown 233, 0 dropped), GStreamer stats tracer on the sharer:

| pad | buffers/s |
| --- | --- |
| `queue0:src` (the branch's queue, i.e. what reaches its `videorate`) | 239.2 |
| `videorate0:src` | **1.0** |
| `vapostproc0:src` (the GPU download, BGRx 8100 KiB) | **1.0** |
| `videoconvert1:src` (RGB) | **1.0** |

So the expensive download/convert runs once a second; the 239 fps only pays a `videorate` pad call plus a
discarded buffer. Sharer process CPU in that state: 5-9.5 % of one core (`top`, with the tracer on).
System domain: `capture::branch::tests::room_preview_branch_stays_near_one_fps_on_a_variable_rate_source` feeds a
240 fps source relabelled `framerate=0/1` (what the portal advertises) and counts 1-8 buffers at `preview_sink`
in 4 s (passes), so no `RateCap` was added to either domain.

`Why the live branch needed RateCap but this one does not:` not pinned down (untested guess: the 1/1 caps behind this
branch's `videorate` renegotiate differently from the live branch's late-set RGBA capsfilter). If this branch is ever
restructured, re-run the check (pad probe on `preview_sink`'s sink pad, or the tracer's `videorate*:src` row).


**Testing note (rig):** a headless sway seat without a keyboard never sends `wl_keyboard.enter`, so egui never sees
focus (`viewport().focused` stays false, preview never opens). Give the seat a keyboard for the whole run with a held
`hs-rig.sh run -- wtype -P Shift_L -s <ms> -p Shift_L` (record its PID, stop only that), then move focus with
`swaymsg '[pid=<client pid>] focus'`.


---

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

A screenshare start can hang ~30 s ("GPU capture path failed to start ... Element failed to
change its state", then the CPU fallback hangs on the same source) when another native
PipeWire stream is stuck `suspended` with an `active` link, e.g. a live
`audiotestsrc ! pipewiresink` that never starts. PipeWire 1.6.9 then never negotiates any new
link (no `check_states` for the screencast link) and `pipewiresrc` gives up with
not-negotiated after 30 s. A cleanly corked/paused stream (PulseAudio cork, native
`set_active(false)`) does not trigger it. Diagnose with `pw-top`: the stuck stream sits at `S`
instead of `I`/`R`. Found by task #48; not fixable from the client.

**Stock xdg-desktop-portal-wlr 0.8.4 (ext-image-copy-capture) and the start-up stream flap (#52).** `pipewiresrc` always takes its stream streaming -> paused -> streaming at start: the `READY->PAUSED` change leaves the stream active so it can link and negotiate, then deactivates it until `PLAYING` (`gstpipewiresrc.c`). The element only does the deactivation after it has pushed the negotiated caps downstream, which behind the GPU chain (`vapostproc`, tee, preview `vapostproc`) takes ~1.3 ms, long enough for the portal to have asked the compositor for its first frame. Stock xdpw creates a frame object on every `streaming` without checking for an outstanding one, so the resume creates a second: `ext_image_copy_capture_session_v1#N: error 1: session already has a frame object`, xdpw exits, the node is gone, the share fails (GPU path `StateChangeError`, CPU fallback `target not found`). The wlr-screencopy backend (the user's patched build) tolerates it. **Fix:** `capture::decoupled_source` puts a `queue max-size-buffers=2` directly behind `pipewiresrc`. The queue takes the caps event on its own thread, so `start()` follows `streaming` by ~100 us and the pause and resume land before the portal has requested anything; xdpw never sees the flap. `portal-probe.py` never flapped because its tail is cheap; with the client's heavy tail and no queue it flaps and kills xdpw. Before the fix 5/6 stock runs crashed; after, 4/4 single shares and 3 consecutive shares were clean at 236-238 fps (vah265dec). **Why not a settle delay:** pausing 5 or 20 ms worked, 100 ms left xdpw in a permanent "out of buffers" state, 0 crashed: it depends on how long the compositor takes to finish the first frame. **Why not retries:** xdpw dies. `capture started in the Gpu|System domain` is now logged at every capture start. The headless rig now runs stock xdpw by default (`client/scripts/README.md`).

**Start-up flap, residual risk and failure handling (#59).** The #52 queue narrows the window between `pipewiresrc`'s `streaming` and its pause (about 0.4 ms, `GST_DEBUG=pipewiresrc:6`) but cannot close it: the flap is inherent to `pipewiresrc` (READY to PAUSED leaves the stream active, `set_active(false)` follows, PLAYING re-activates). Under host CPU load the stock xdpw 0.8.4 still crashes on it (headless rig: 1 of 15 starts at 36-thread stress, about 2 to 3 of 7 in 6-thread bursts; QC: 4 of 25 at load >= 20, 0 of 12 below 10). A settle delay, a source-only start (`set_locked_state`, gap 550 us to 60 us, no measurable effect) and thread priorities did not help. **What happens now:** `start_pipeline_bounded` runs the start with the bus polled, so the first pipeline error ends the attempt (`stream error: target not found` arrives ~60 ms after the portal dies) and a start with no answer ends after 8 s (`START_TIMEOUT`; 5 s, `FALLBACK_START_TIMEOUT`, for the CPU fallback). Before, a dead node blocked `set_state` 30 s and the fallback another 30 s. *Why no CPU fallback on a dead node:* a fresh `pipewiresrc` on it only waits again, and on a live portal it would flap a second time; the fallback in `start_capture_with_source` now runs only for failures that are not `StartFailed` (`start_is_hopeless`: node gone, timeout). On failure the portal session is closed (`close_portal_quietly`, 3 s cap) and the error goes to `AppState::share_error` and the log (`share error: ...`, `session.rs`), which the UI toasts. *Why one retry on a new portal session (`start_capture`):* the portal service restarts xdpw on the next request, so a fresh session is cheap; a picker backend asks the user again, once; it never loops. Not addressed: an "out of buffers" stall where xdpw delivers one frame and then none.

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
tooltip (now always present, with the full status) adds "Encoding 172 of 240 fps".
Measured: H.264 @240 Balanced → hint at 171-173 fps; H.265 → viewer 240-241 fps, hint gone.

**Removed 2026-10-15 (user, #87): the stream-settings banner.** The Video card used to show an
Info banner under Framerate, "Your encoder reaches about N fps with H.264 on this machine.",
with a "Use H.265" button. The user: "it's just annoying". Both strings
(`ui.stream.encoder_limit`, `ui.stream.use_h265`) and the card code are gone; the measurement
(`EncoderLimit`) stays because the action bar's status colour and tooltip still use it, and
`ui.bar.encoder_limit_why` no longer points at Stream settings.

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

## Capture source errors lead the share error (2026-10-15, #76)

**Symptom (user, AppImage on Hyprland):** `Share error: failed to bring up the encode branch
for stream 0: software fallback encode branch produced no IDR either`, while the terminal
had the real cause: `capture pipeline error from .../GstPipeWireSrc:pipewiresrcN: stream
error: unhandled format`. The root cause of *that* error is packaging, see
`client/packaging/README.md` "the bundled `pipewiresrc` is a patched build".

**Why the message was wrong:** a source that dies right after its first frame(s) leaves the
encode branch without input, and `branch::add_branch_live_waiting` can only observe "no IDR
within 5 s" (twice: hardware, then the software retry). The capture pipeline's bus error was
consumed by `capture_loop` and only logged, so the user-facing error blamed the encoder.

**Fix (smallest one):** `capture_loop` now also records the *first* bus error in the
capture's `CaptureErrors` (`CaptureHandle::capture_errors`, shared with the loop through an
`Arc<Mutex<Option<String>>>`; later errors are consequences, so they are dropped).
`spawn_destination` passes a failed bring-up through `blame_the_source`: with a recorded
error the result reads `screen capture failed: pipewiresrc17: stream error: unhandled
format: failed to bring up the encode branch for stream 0: software fallback encode branch
produced no IDR either: encode branch produced no IDR within 5s` (the encoder's own chain
stays at the end for diagnosis); without one it is unchanged. Covered by
`capture::tests::a_source_error_on_the_bus_is_blamed_when_the_branch_fails`.

`Why post-hoc and not cancel the wait on a source error:` `add_branch_live_waiting` may be
mid-way through building a branch; dropping its future there could leave an orphan branch
on the tee (its failure arms do the cleanup explicitly). The 5 s IDR deadline bounds the
delay.

**Known gap:** a source that errors *before its very first frame* never reaches the IDR
stage: `wait_for_source_frame(Indefinite)` keeps waiting (the destination stays `Starting`)
and the recorded error is not surfaced until something fails. A source that dies that early
is normally caught by `start_pipeline_bounded` (its bus poll), so it has not been seen.

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
`tune`/`quality_level` params (snake_case strings, same convention as every
other `edit_destination` param) and as controls in the destination settings
(`client/src/ui/stream_settings.rs`).

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

### Presets (simple) over the raw tuning fields (advanced)

*Why two tiers:* the project requirement is "easy for regular users and powerful for advanced
users", and showing Tune/Quality/Rate Control jargon (Rate Control has since been removed, #88) on every card failed the first half. A
**preset** is a convenience over the same three raw fields, never a fourth field: picking one sets
`tune`/`quality_level`/`rate_control` together through the same `edit_destination` call a single
raw control uses. The table lives in `client/src/presets.rs` (ported from the former Electron
frontend's `ENCODER_PRESETS`; it later gained a fourth preset, Gaming, see "Rate control collapses
..." below):

| Preset | `tune` | `quality_level` | `rate_control` |
| --- | --- | --- | --- |
| Low-Latency | `low_latency` | `performance` | `constant` |
| Balanced (default) | `best_quality` | `balanced` | `constant` |
| Quality | `best_quality` | `quality` | `quality` |

The active preset is computed from the destination's current three fields (whichever preset
matches exactly); a hand-tuned combination selects none, an intentional unvalidated state. The raw
fields sit behind an "Advanced" disclosure that must keep its open state across re-renders.
Framerate is a regular-user control (always visible); the raw knobs are not.


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


### Burst and wire-clock bugs found in owner testing of the edit panel (2026-07-28)

The first hands-on test of the edit panel found four defects. Three were former-Electron-UI state bugs
(the panel only shown in one bandwidth mode, no framerate control on a running stream, and the
bitrate slider rebuilt from a stale DTO value). The lasting rules: the UI's local edit buffer is the
single source of truth for every control, and the allocator owns *bitrate only*, so the other
controls stay editable in both bandwidth modes. The fourth was a real backend bug:

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

**Superseded 2026-10-15 (#87): the "Efficient"/"Max Quality" names and the Low-Latency/Gaming = Cqp rows below were reversed/changed, see "Rate control renamed Dynamic / Static".**

The Advanced panel's raw `Rate Control` `pillSwitch` now shows two plain-language options,
"Efficient" (Cqp) and "Max Quality" (Qvbr), replacing the old three raw mode names. The four
`ENCODER_PRESETS` (now `client/src/presets.rs`) were re-assigned per-preset rather than blanket-defaulted:

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
`dto::StagedCaptureRow`/`edit_destination` → a new `set_gop_millis` command → the UI.
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
(short version: the wire string `"qvbr"` is both the UI's `rateControl` value in
~15 call sites and the on-disk persisted field in `AppSettings`, so renaming it would break
existing users' saved settings for a purely cosmetic gain; the user-facing "Max Quality" label is
unaffected either way).

**Verified live, real hardware, real `vah264enc`, same AMD RX 7900 XTX** — not the full two-party
relay+viewer path this time (a sender's own `DestinationStatsDto::bitrate_kbps` merely
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
- The app (then the Electron shell) still launched without console errors after this change;
  driving a full two-party watch session to read a viewer's real
  `received_bitrate_kbps` was not done in this pass — flagged above as the honest verification gap.

**Why these specific values**: `target-percentage=95` sits at the high end of the property's
50-100 range so the real encode target tracks close to the configured ceiling rather than the
default 66% (the original pop-in's diagnosed cause). `cpb-size`'s 3/8 (0.375x) fraction was chosen
as a value meaningfully tighter than the old 1x-bitrate buffer per the task's guidance
(0.25x-0.5x range) — not separately swept against other fractions in this pass; the measured
result above already shows a real improvement at this value, so no further tuning was done, but a
tighter (e.g. 0.25x) or looser (0.5x) value remains unexplored if the current setting turns out to
need further adjustment after real-world (non-scripted) use.

### Rate control renamed Dynamic / Static; Low-Latency and Gaming move to Dynamic (2026-10-15, #87)

**Superseded the same day (#88): the choice was removed, only Dynamic remains. Kept for the measurements (the rationale).**

**User report (2026-10-15):** "'Efficient' works flawlessly. 'Max quality' freezes on big movements ... Efficient uses like 6 Mbit/s with just my desktop showing, and Max quality like half a Mbit/s, but it should be the other way around, right?"

**The names were backwards, the modes were not switched.** `Cqp` (shown as "Max quality") holds the
*quantizer* (quality per frame) constant, so the bitrate follows the content: tiny on a still desktop, enormous
on motion, and the configured kbps is not a cap. `Qvbr` (shown as "Efficient", emits VA `rate-control=vbr`)
holds the *bitrate* near `VBR_TARGET_PERCENTAGE`% (95) of the configured value with the configured value as
a ceiling, and lets quality vary. "Max quality" sounded like "spends the most" but is the one that spends
least when nothing moves; "Efficient" sounded frugal but spends toward its budget.

**Measured** (`vah264enc`, 1080p60, 5 s, `b-frames=0 key-int-max=120 target-usage=2`, `videotestsrc` through
`vapostproc`, same element properties `launch_fragment` builds; 8000 kbps configured):

| content | `vbr` (Dynamic) | `cqp` (Static) |
| --- | --- | --- |
| `smpte` (static, detailed) | 7484 kbps | 35591 kbps |
| `ball` (static, simple) | 131 kbps | 84 kbps |
| `snow` (noise, worst-case motion) | 9340 kbps | 561864 kbps |

So CQP on busy content produces 4-70x the configured bitrate: a link that cannot carry it drops packets and
the stream freezes (the user's "freezes on big movements"). Dynamic stays at about the budget. On a real
1080p60 vkcube in the rig, Low-Latency + Dynamic at 8000 kbps measured 7.6 Mbps at the viewer (= 95 %),
and the same stream switched live to Static 4.8 Mbps (simple moving content, under the cap; the spikes
above need more complex content).

**Why Dynamic shows ~6 Mbit/s on a still desktop:** VA `vbr` does not undershoot much on detailed
content even when it is static: with `target-percentage=95` the rate control keeps spending toward the target
(it lowers QP on the P frames and the periodic IDRs are large), see the `smpte` row. Truly flat content
(`ball`) does stay tiny (131 kbps). Text and gradients on a desktop land in between. This is the
2026-07-30 tuning working as designed (a high target tracks the budget, which is what avoids the old
"pop-in" swings), not a bug, and was deliberately left alone here. x264 (software) `Qvbr` is plain
`bitrate=` (ABR-like single pass) and was not measured.

**What changed:** UI names Efficient -> **Dynamic** (`Qvbr`), Max quality -> **Static** (`Cqp`), de
"Dynamisch" / "Statisch", with a plain-words tooltip (`ui.stream.rate_control_tip`); i18n keys renamed
`rc_dynamic` / `rc_static`. Enum names and the persisted `"qvbr"`/`"cqp"` values are unchanged (saved
settings keep working). `presets.rs`: **Low-Latency and Gaming now use `Qvbr`** (Dynamic), so all four
presets are Dynamic; Low-Latency vs Gaming still differ by GOP only. A saved Low-Latency/Gaming setup with
`Cqp` now matches no preset (shown as hand-tuned, Static selected) until a preset is picked again.
The browser mirrors it: Low-Latency/Gaming use `bitrateMode: "variable"`, the read-only rate-control row
reads "Dynamic (variable bitrate)".

### Rate control is no longer configurable: always Dynamic (2026-10-15, #88)

**User request (2026-10-15):** "It's probably best if we just make Dynamic the default and remove the ability to
configure that. So the rate control is just set and it just works, since it behaves really good and the other one
doesn't behave at all."

`Why:` Static (CQP) froze the stream on motion because it ignores the configured bitrate (measurements in the #87
section above: 4-70x the configured kbps on busy content); Dynamic held the budget. With one mode that was never
the wrong choice, the setting was only a way to break a stream, so it was deleted rather than hidden (nothing in
the UI, presets or settings reads it any more).

**What is gone:** `capture::RateControlMode` (`Cqp`/`Qvbr`) and every `rate_control` field (`VideoParams`,
`EncodeSettings`, `EncoderTuning`, `DestEdit`, `NextStream`, `AppState`, `StartStreamDefaults`, the
`launch_fragment` argument); the Advanced "Rate control" row; i18n `ui.stream.rate_control`, `rate_control_tip`,
`rc_dynamic`, `rc_static`; the browser's read-only row, `bitrateMode` on the presets and the `bitrateMode` override
on `shareScreen` (the sender always uses `"variable"`). A preset is now `tune` + `quality_level` + GOP; Low-Latency
vs Gaming is still told apart by GOP alone.

**What is fixed:** VA encoders (`vah264enc`/`vah265enc`/`vaav1enc`) always get `rate-control=vbr
target-percentage=95 cpb-size=bitrate*3/8` (`VBR_TARGET_PERCENTAGE`, `VBR_CPB_SIZE_*` in `capture.rs`; `vbr`, not the
literal `qvbr`, because of the Mesa radeonsi ceiling bug documented above); x264enc keeps its plain `bitrate=`
(`pass` unset). The only backends are Software and Vaapi: there is no Windows/MF encoder branch in this crate.

**Old settings files:** `settings.toml` written while the choice existed has `rate_control = "cqp"` or `"qvbr"` in
`[start_stream_defaults]`. `StartStreamDefaults` has no `deny_unknown_fields`, so serde ignores the key and the file
loads as before (everything behaves as Dynamic). Test: `rooms::tests::start_stream_defaults_ignores_removed_rate_control_key`.
Nothing else persisted the value (destination params are runtime-only).

The tests that used `rate_control` as one of the toggled tuning controls now use `gop_millis` (also a `BranchSwap`):
`tuning_knob_changes_are_tier_two`, `late_watch_after_all_three_tuning_controls_toggled_...`.
`rate_control_mode_maps_per_backend` became `rate_control_is_always_dynamic_per_backend` (+ `..._on_h265_and_av1`).

### Interaction with viewers: codec-string re-derivation

Toggling any of these three controls on a *live* destination triggers a real `BranchSwap`
— a new encoder with (potentially) a different H.264 profile/level. `x264enc` in
particular picks its AVC profile/level automatically from `speed-preset` with no explicit
`profile=` set anywhere in this pipeline, so a Performance→Quality toggle can genuinely
change the bitstream's profile out from under an already-connected WebCodecs watcher. This
was a real, real-hardware-verified bug (a watcher connecting *after* all three controls had
been toggled got stuck at 0fps) — the fix lived entirely on the viewer's delivery side (the
codec string is re-derived from every keyframe's bitstream, never cached once). The native decoder
has the same property by construction: `decode/codec.rs` sniffs the codec from the bitstream
(`superdoc/meta/TERMINOLOGY.md` "Sniffing"). Regression coverage:
`end_to_end_test::late_watch_after_all_three_tuning_controls_toggled_gets_a_real_keyframe_every_time`.
Nothing about `capture.rs`'s own tuning-control mechanism needed to change for that fix.

---

## Video codec selection: H.264, H.265, AV1 (2026-07-30, backend/wire only — Stage 1 of 2)

**Scope of this entry:** this is the *backend + wire protocol* half of adding a codec
selector to the stream control UI (approved after a codec-comparison discussion this
session settled on offering all three codecs, not picking one). A separate Stage 2 built
the UI selector on top of the contract documented here (now the codec row in
`client/src/ui/stream_settings.rs`).

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

New/changed settings entry points (mirroring `set_quality_level`/`set_rate_control`; originally
Tauri commands, now `AppState` methods):

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

The UI calls `AppState::supported_codecs` (wraps `capture::
supported_codecs(capture::detect_encoder_backend())`, returning `["h264"]` on
`Software` or `["h264", "h265", "av1"]` on `Vaapi`) to grey out H.265/AV1 in the
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

The former sidecar's `video_server.rs` `CodecInfo` chunk (the in-band WebCodecs codec string
every watch connection gets first; removed in the egui rework, the native equivalent is
`decode/codec.rs` sniffing and the browser's is `docs/js/core/codec.js`) derived
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

No UI changes (that was Stage 2). No change to
`protocol.rs`'s `AnnounceStream`/`StreamInfo` (see the wire-chunk section above for why,
and the ambiguity that choice leaves open). No live per-codec VAAPI availability probing
beyond the existing `vah264enc`-only one.

---


## Start-stream defaults: process-wide settings applied before a capture starts

*Why this shape:* `edit_destination` needs an existing `destination_id`, and a stream that has not
started has none. Rather than invent a third settings mechanism, a not-yet-started stream uses the
existing pattern: **process-wide settings on `AppState`** (`framerate`, `bitrate_kbps`, `tune`,
`quality_level`, `rate_control`, `codec`, with setters that clamp), read by
`go_live`/`start_video_share` when they build the first destination's `VideoParams`.

**Persistence:** `rooms::StartStreamDefaults` (nested as `AppSettings::start_stream_defaults`) uses
the same local-TOML load-modify-save mechanism as `save_bandwidth_settings`
(`session::save_start_stream_defaults` never clobbers sibling settings). `TuneMode`/`QualityLevel`/
`RateControlMode`/`VideoCodec` derive `Serialize`/`Deserialize` with `snake_case`, so the stored
strings equal the wire strings; every new field is `#[serde(default)]` so an older `settings.toml`
still loads. *Why separate from the live `AppState` settings view:* the live `framerate`/
`bitrate_kbps` reset to defaults every launch ("every launch starts fresh"), which is a different
question from "what did the user last save"; tune/quality only exist per destination.
First-run fallback equals the backend `#[default]`s: 30 fps / 8000 kbps / `best_quality` /
`balanced` / `constant` / H.264. The `[start_stream_defaults]` block is written to `settings.toml`
in the app's config dir; check a persistence change with a scratch `XDG_CONFIG_HOME` and the `run`
skill's `drive.sh`.

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

### Overshoot

`allocate` never starves a destination below its floor and never silently exceeds the
user's total (plan doc, round-1 Q8): when the floors don't fit, everyone still gets their
floor and the excess is reported as `Allocation::overshoot_kbps`. The bandwidth card
renders that as a `theme::warning_banner` (a new tinted, `WARNING`-stroked primitive) with
the actual numbers — *"Over budget by X kbps: the guaranteed floors need Y kbps but the
maximum is Z kbps. No stream is starved below its floor."*

---

## Retrying a failed destination

A `Failed` destination's Retry calls `AppState::retry_destination` (`session.rs`), not
`stop_destination` + `start_destination`.

**Why:** that pairing used to be the implementation and had two real bugs. It lost the failed
destination's last-configured bitrate/tune/quality (a new destination copies params
from a *sibling* on the capture, and the common case has none, so it silently reset to defaults).
Worse, `stop_destination` tears down the *whole capture* once its destination count hits one, so
`start_destination` bailed silently and retry failed outright for the single-destination case.
`retry_destination` reads the failed destination's params first, removes just that branch without
touching `ShareStatus`, and re-adds a replacement in the same room with the same params.
Coverage: `end_to_end_test::retry_destination_preserves_prior_encoder_tuning` (real relay + real
capture).

The destination list is deliberately **cross-room**: a destination is bandwidth the user is paying
for right now, and hiding it behind a room selection would make fan-out invisible. Each row folds
the capture's start/stop state, the pending transfer/fan-out role and the live `DestinationState`
(`Starting` / `Live` / `Rebuilding` as "Applying" / `Stalled` as "Waiting for capture" / `Failed`)
into one status. A destination's live bitrate rides the per-tick stats payload
(`DestinationStatsDto::bitrate_kbps`): the allocator writes it as a tier-1 property write that
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
not a viewer-facing artifact. (A 2026-07-28 investigation initially misread some
UI branches as the viewer-facing "separate audio stream" to delete; they were the sharer's own
outgoing rows and were left alone.)

What changed is entirely on the **watcher's** side: a viewer never sees another person's
audio as a separate stream, row, or "Listen" action. Watching someone's video
automatically subscribes and plays their audio, and stops it again when the watch stops.
The full architecture, the rejected alternatives, and the four cases that broke the
previous "latch the pairing at watch time" approach are in
`superdoc/features/video-pipeline.md`'s "Audio follows video" section — including why
audio is played in-process rather than relayed to a display layer like video was.

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

## Audio is a row on the share, not a card of its own (backend facts)

**Owner's request (2026-07-28):** attach audio to the screen share instead of making it its own
card: a meter, the captured application's name, Mute and Stop on one extra row of the share.
(Audio is still its own `LocalCapture` and its own `StreamKind::Audio` stream id on the wire; only
its *presentation* is bundled.)

- **Meter:** a `level` element in the capture pipeline, folded into `audio::AudioLevel` (an
  `AtomicU32` of `f32::to_bits`: one writer, no history, lock-free for a GStreamer streaming
  thread) and surfaced as `DestinationStatsDto::audio_level` on the per-tick stats.
- **Mute:** `AudioCaptureHandle::set_muted`, a `volume name=mute` element placed *before* `level`,
  so muting flattens the meter too (the meter answers "is my audio going out", and a bouncing bar
  while muted would answer wrongly). *Why mute with `volume` and not by pausing the send:* a
  continuous Opus stream keeps the viewer's jitter buffer from seeing a mute as a multi-second
  dropout.
- **Stop:** plain `stop_destination(<audio destination id>)`; an audio capture always has exactly
  one destination, so it stops that capture only and the screen share is untouched.
- **Pairing:** `LocalCapture::audio_for_capture: Option<CaptureSourceId>`, set from
  `start_audio_share(room_handle, for_capture)` (exposed as `DestinationRow::paired_capture_id`).
  *Why an explicit link and not "same room":* one capture can fan out to several rooms and someone
  sharing two monitors into one room has two shares but possibly one audio share; "same room" would
  make both claim it and Stop on either would kill audio for both.
- **No stranded audio:** an audio destination with no matching video card (a share torn down under
  it) must still be listed and stoppable so it can never be stranded invisibly.
- **Source list freshness:** `audio::list_audio_sources` shells out to `pw-dump` on every call (no
  cache) and filters to real `PipeWire:Interface:Node` objects (PipeWire `Client` objects carry
  lookalike `media.class` props); the UI re-lists on open and while the picker stays open. Per-option
  level meters use real per-candidate taps (`start_audio_level_tap`/`poll_audio_level_tap`).
- **Real playback measurement** (viewer volume 0/100/200 % gave +0.0/+2.7/+6.5 dB at the speakers)
  is recorded with its decorrelated-noise rationale in `superdoc/features/video-pipeline.md`
  ("Audio follows video").

### Auto-Connect (join a saved room at startup)

`rooms::SavedRoom::auto_connect: bool` (`#[serde(default)]`, so older `rooms.toml` files load as
`false`), `AppState::set_room_auto_connect`, and `AppState::auto_connect_queue` drained one room at a
time by `poll_auto_connect_queue`. *Why a queue:* `pending_connect` is a single slot, so firing
several `start_connect`s at startup would orphan every one but the last. Nothing else makes a
saved room rejoin itself at launch. (A first misreading wired this checkbox to "auto-start audio
sharing"; the owner's correction: it means "connect to the room on startup".)

---

## Deliberately not built here

The audio-as-attachment model (audio is still its own capture and its own stream id on the
wire — only its *presentation to a viewer* is now bundled with video, see above);
the View tab's tiled grid and tiered restart placeholders; per-destination floor/ceiling
*per* destination (one global floor is exposed, and every destination shares the project's
40 Mbit/s ceiling); an idle-audio reservation in `AllocInput::audio_reserved_kbps` (left
explicit at 0 until audio becomes an attachment with a known constant rate).


## History: the Electron UI (removed)

The Stream Control tab used to be an Electron HTML page (`stream-control.js`, `components.js`,
`.media-tile-*` CSS) over the Rust sidecar's REST bridge (`src-tauri/src/commands.rs`, `dto.rs`,
`api_server.rs`), verified with Playwright's `_electron` driver. All of it was deleted in the egui
rework (I23). Roughly 1500 lines of per-widget write-ups were removed from this page; they are in
version control (`git log -- superdoc/features/stream-control.md`). The *why* that outlived them:

- **Every settings control must read from one local edit buffer.** Several bugs (slider snapping
  back, "Balanced" shown after Go live, stale bitrate readout, a volume slider that disagreed
  between windows) were the same shape: a control rebuilt from a server DTO that had no field for
  the value, or a live change that fires no event.
- **A `String`/`Number` mismatch in a dropdown compare silently broke "Share"** (the audio source id
  is a `u32`; the dropdown returned the raw number). Compare ids by type, not by `String()`.
- **Tests must separate "audio flowing" from "audio absent"** (see the audio bugs above).
- **Room-scoped settings row removal:** the tab's room picker plus members card duplicated the
  sidebar's room detail panel and was deleted rather than trimmed; the egui equivalent is the room
  settings modal in `ui/modals.rs`.
- **Verification then:** a real Electron shell + built sidecar + a real relay, sometimes with
  fabricated stream keys. **Today:** the `run` skill with `rig.sh`/`drive.sh`
  (`client/.claude/skills/run/SKILL.md`, `client/scripts/`).
