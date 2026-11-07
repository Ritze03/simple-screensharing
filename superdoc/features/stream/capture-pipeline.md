# The capture pipeline

One screen grab, one GStreamer pipeline, N destinations: the pipeline shape, the own-tile live preview and the 120-240 fps performance work.

Part of the Stream Control reference, hub: `superdoc/features/stream-control.md`. The text moved here verbatim from that page: "above", "below" and "this page" in it may point at a section that now lives in a sibling page of this folder (the hub lists them).

## The shape of a capture, after this stage

One capture is **one screen grab, one GStreamer pipeline, N destinations**:

```text
GPU domain (Linux portal + VA stack, the normal case; 2026-10-14, #39):
pipewiresrc(DMA-BUF) ! vapostproc(video-direction=auto) ! VAMemory NV12 ! tee
    ├─ queue ! videorate(1fps)  ! vapostproc ! BGRx ! videoconvert ! RGB ! appsink   (JPEG thumbnails, fixed)
    ├─ valve[RateCap 30fps; closed unless the window is focused] ! queue(leaky,1) ! vapostproc ! capsfilter(BGRA, fitted to <=1920x1080) ! appsink(async=false)   (own-tile live preview, #78; `capture::live_branch`)
    ├─ queue ! valve[RateCap] ! vapostproc ! VAMemory NV12 ! capssetter(fps) ! va*enc ! appsink   ← destination A (room 1)
    └─ queue ! valve[RateCap] ! vapostproc ! VAMemory NV12 ! capssetter(fps) ! va*enc ! appsink   ← destination B (room 2)

System domain (Windows, x264enc machines, tests):
<platform source> ! videoconvert ! capsfilter(format=BGRx) ! tee
    ├─ queue ! videorate(1fps)  ! RGB ! appsink   (JPEG thumbnails, fixed)
    ├─ valve[RateCap 30fps; closed unless the window is focused] ! queue(leaky,1) ! videoscale ! videoconvert ! capsfilter(BGRA, fitted to <=1920x1080) ! appsink(async=false)   (own-tile live preview, #78; `capture::live_branch`)
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
(`capture.rs` `live_branch`, `LivePreview`, `CaptureHandle::set_live_preview`), <= 1920x1080 at <= 30 fps BGRA (was 640x360 until 2026-11-04, task #132; the new size and its cost table are in `self-preview.md`),
open only while the egui window is focused and the own tile was painted within 0.6 s; closed it is a pad call
and an unref per frame. The encode branches are not touched.

**Measured cost at the old 640x360 size (history; 1080p numbers: `self-preview.md`) (2026-10-15, headless rig, stock xdpw, GPU domain, H.265 240 fps balanced, `vkcube` at 240 Hz; the
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

## Capture/encode performance: 120-240 fps of *unique* frames (2026-10-14, #39)

**Requirement (owner):** "at least 120 FPS sharing works or even 240 FPS". The viewer's
decoder and presenter already reached 240; the *unique* frames (real screen content) did
not. Measured on this machine (an AMD GPU, VA-API; capture output at about 280 Hz running
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
   see "Encoder limits made visible" below. Balanced was `target-usage=4` then, the same
   half-speed AV1 band; #92 (2026-11-01) moved it to 3 on Mesa AMD.

### Result (release build, `session.sh`, 30 s windows, receiver on a second output)

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

The second test output is rotated 180 degrees in the compositor. The patched portal **does** send the transform:
`portal-probe.py` prints `image-orientation=rotate-180` from `pipewiresrc`. In the GPU domain
the source `vapostproc video-direction=auto` follows that tag (it is the orientation
stage, with no extra pass). Sharing the rotated output for real, the viewer shows the desktop the way the
screen is laid out (sharer window left, viewer window right, title bars on top);
sharing the unrotated output is unchanged (upright cube). The System domain keeps #35's lazily spliced
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

**Start-up flap, the pause that closes it (#112, 2026-11-03).** *(Correction, #122 below: the pause closes the crash but leaves xdpw stuck in about half of the starts; the closing sentence about 100 ms is wrong, and `repair_portal_start` is the rest of the fix.)* The residual crash was bigger than the #59 note says: measured in the headless rig on 2026-11-03 (stock xdpw 0.8.4, pipewire 1.6.10, machine load 3-5 from other workers, every start = Share click through "capture ready", then Discard), 13-20 % of starts crashed xdpw at every commit from #52 (`a5433cf` 2/20 + 4/25 + 6/25, `3a55552` 2/20 + 3/25 + 5/25, `5412dda` 3/20, `facc373` 4/20 + 3/25 + 2/25, `HEAD` 6/20 + 4/20 + 1/25 + 7/25; `4abf65a`, just before the queue, 4/5). So it was not a regression of the stop rewrite, capture-source recovery, keyframe pacing or NACK; the queue still helps (about 80 % to about 15 %), but not enough. Mechanism (read from `gstpipewiresrc.c`): the READY->PAUSED change leaves the stream active, `set_active(false)` follows once the element has pushed its caps, and the PAUSED->PLAYING change calls `set_active(true)`; with one `set_state(Playing)` the pause and the resume are ~100 us apart. xdpw asks the compositor for a frame on the first `streaming`, stops asking after the `paused`, and creates another frame object on the resume; if the first frame has not been delivered yet that is the protocol error. **Fix:** `capture::play_and_wait` takes a portal pipeline to PAUSED first and sleeps `PORTAL_START_SETTLE` (30 ms) before PLAYING, so the compositor finishes the outstanding frame (4-17 ms for 240-60 Hz outputs). Measured as an A/B inside one rig (the delay read from a file per start, values interleaved, so load drift cancels): 240 Hz output 0 ms 11/39 crashes (28 %), 10 ms 0/30, 30 ms 0/30; 60 Hz output 0 ms 7/40, 10 ms 1/40, 30 ms 0/40, 60 ms 0/40. Final binary, 60 starts mixed H.264/H.265/AV1 at 60/120 fps: 0 crashes. A 100 ms pause still gave a healthy 120 fps H.265 share (Decoded 120 fps / shown 120 fps, 0 dropped), so the "100 ms leaves xdpw out of buffers" observation from #52 did not reproduce; 30 ms is the smallest value with margin. *Contradiction with the #52/#59 notes above:* they say a settle delay did not help (#59) and that 100 ms broke xdpw (#52). Their sample sizes were a handful of starts against a 1-in-15 base rate, which cannot show an effect either way; the interleaved A/B above can (p < 0.001), and the delay there is a pause between PAUSED and PLAYING. Cost: 30 ms on every portal capture start. The rig now emulates dbus activation for xdpw (`client/scripts/README.md`), so even a crash that still happens is retried by the client's single retry (#59) instead of killing the rig.

**Start-up flap, the stuck stream and its repair (#122, 2026-11-03).** #112's 30 ms pause fixed the crash and made the other half of the flap worse: with the stock xdpw 0.8.4 in the headless rig, **a share on the #112 build got no frame at all in about half of the starts** (task #120 saw 2 of 2: `xdpw.log` ~16k lines of `pipewire: unable to export buffer, dropping frame` / `out of buffers`, client `the capture source has produced no frame in 3.01s`, share stays STARTING). Measured with a "Share, Go live, wait for the first keyframe" loop (`Go live` is the first thing that needs a frame, a staged capture alone looks healthy), same rig, settle values interleaved, the pause read from a file per start: **0 ms** (= the pre-#112 behaviour) crashed xdpw in 20-50 % of starts (the rig's bus restart plus #59's one retry rescued about half; two crashes in a row ended in a `share error`), **10 ms 5/10 and 30 ms 4-8/10 and 100 ms 8/10 stuck**, no crash at any value >= 10 ms. The stuck share is *not* a load flake and not rig-only (the bus activation is irrelevant; a fresh rig also hit it on the first start).

Mechanism (read from xdpw 0.8.4 `pipewire_screencast.c` + `ext_image_copy.c`, pipewire 1.6 `stream.c`, and a `GST_DEBUG=pipewiresrc:5` + xdpw `-l TRACE` log of a stuck start): xdpw is the *driver* of its stream. On `streaming` it dequeues a buffer and requests a frame; on `paused` it queues the (half-written) buffer back; frames then run only through `frame ready -> enqueue -> trigger_process -> on_process -> dequeue -> request frame`. Whether xdpw is told `paused` at all is a race (the pause usually reaches it when it is held long enough: invisible in about half of the starts at 30 ms, in 70 % at 0 ms, only 20 % at 100 ms). When it is told: with a short pause the resume dequeues a second buffer and asks for a second frame while the first is outstanding -> the protocol error (crash, #112); with the 30 ms pause the first frame completes during it, xdpw's enqueue then logs `no buffer to queue` / `not streaming`, and after the resume the stream is dead: the consumer receives the **first buffer flagged `SPA_CHUNK_FLAG_CORRUPTED`** (the buffer queued at `paused`), a second one, and then nothing, while xdpw dequeues all 16 buffers, `out of buffers` forever (graph cycles keep running, the consumer's `pipewiresrc` just waits for buffers; no thread of ours is blocked). `pw-dump` of the stuck state: xdpw node, our node and the link all `running`/`active`. **A stream that was never told `paused` is fine.**

**Fix:** `capture::repair_portal_start`, called by `play_and_wait` for portal pipelines right after `PLAYING`. A one-shot buffer probe on `pipewiresrc`'s src pad (`watch_first_buffer`) records whether the first buffer carries `GST_BUFFER_FLAG_CORRUPTED`; the first buffer arrives 2-10 ms after `PLAYING` (`PORTAL_FIRST_BUFFER_WAIT` = 200 ms bounds the wait on a portal that sends nothing before the first screen change). If it is corrupted the pipeline goes `PAUSED` -> `PORTAL_START_SETTLE` -> `PLAYING` once more, up to `PORTAL_FLAP_REPAIRS` = 3 times, then fails as a `StartFailed` (new portal session, #59). *Why this and not something else:* the corrupted first buffer is a perfect marker (first run with the probe, 12 starts at 30 ms: 7 corrupted = the 7 that never got a frame, 5 clean = the 5 that did); frame *absence* is no marker because an idle monitor delivers nothing either. The second pause/resume starts from a quiet xdpw (no frame outstanding, no half-written buffer), and unsticks it every time (every repair in the experiment runs, always with the first re-flap; a 30 ms and a 100 ms hold behaved the same, 14/14 OK each, against 2/14 OK without the repair in the same interleaved run). A shorter first pause does not help (0 ms crashes xdpw, which also takes down other apps' screencasts on a real desktop), and a longer one makes the stuck case more common; the flap cannot be removed from the client side: `pipewiresrc` connects active and deactivates itself in `READY->PAUSED` (no property, no `PW_STREAM_FLAG_INACTIVE`), and the stream is `streaming` for xdpw before any element of ours can deactivate it. **Measured on the final binary** (same rig, stock xdpw, load 3-5): 40 of 40 starts reached the first keyframe, 30 of them needed the repair (always exactly one), 0 xdpw crashes; a full share + watch ran at "Decoded 120 fps / shown 120 fps, H.265 via vah265dec, 0 dropped". Cost: the repair adds ~110 ms to the 75 % (this rig, under load) of starts that need it; the first-buffer wait adds ~5 ms to the others. The #112 sentence "a 100 ms pause still gave a healthy share, so the #52 observation did not reproduce" was wrong: it only looked at starts that happened not to hit the stuck state. The #52 "100 ms leaves xdpw out of buffers" note was right.
