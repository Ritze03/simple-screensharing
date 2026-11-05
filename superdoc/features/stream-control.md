# Stream Control (desktop client)

> **Status (2026-10-14; the webcam section, "The lagspike on every keyframe", "AV1 branch deaths read as a lost portal source" and "Settings changes on a live share, and the coded-buffer deaths that were left" added 2026-11-04).** The client is one native egui binary and this page is its
> **backend** reference: `client/src/capture.rs` (with its inline `destination` and `branch`
> modules), `session.rs`, `bandwidth.rs`, `presets.rs`, `rooms.rs`. It covers "The shape of a
> capture", staging and "Go live", fan-out, transfer, the three tiers, "Capture/encode
> performance" (#39, with the rotated-output check from #35), "Idle monitors" and the silence
> watchdog (#32), resuming, the encoder-tuning backend mapping, "Video codec selection", the
> bandwidth modes, and the audio-share backend. The UI lives in `client/src/ui/stream_settings.rs`,
> `ui/share.rs`, `ui/stage/bar.rs`, `ui/room.rs`, `ui/modals.rs` (see `superdoc/ui/APP-SHELL.md`).
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

## The source changes size: a window resized, a mode change, a game changing resolution (2026-11-05, row 141)

**Brief.** While a share is live the source can change size. Before this, nothing noticed: `source_caps()` cached the first size for
good and the only way out was the dead-source route below, which waited for the silence watchdog (3 s without an encoded frame, 2 s
more) and then restarted: about 6 s frozen.

**Measured in the rig (stock xdpw 0.8.4, `swaymsg output HEADLESS-1 resolution ...`, `client/scripts/hs-resize.sh`).** The portal
cannot follow a size change in place. Three outcomes were seen, run to run, at every codec: (a) `xdpw.log` `ext: frame capture failed:
buffer constraint mismatch`, then `out of buffers` / `unable to export buffer` for ever; `pipewiresrc` renegotiates and pushes the new
size as a caps event, but delivers 0 fresh buffers (a bare `pipewiresrc ! queue ! fakesink` probe does the same, so it is not our
pipeline); (b) the same without any caps event; (c) the node is destroyed (`stream error: target not found`). A burst of 16 buffers
sometimes arrives seconds later and nothing after it. So an in-place fix (rewriting caps, swapping branches) cannot help against this
portal, and the restart (new portal session, restore token, ~0.5 s) stays the answer for it. Window capture was not testable: the rig's
xdpw shares outputs only (`chooser_type=none`).

**What changed.**
- `SizeWatch` (a probe on the tee's sink pad, `capture.rs`): the live frame size from the caps events, when it last changed, buffers
  since, and the newest buffer's time. `source_caps()` stays the size the destinations were built for (the basis `rescale_for_new_source`
  and `edited_params` work from); the live size is separate.
- `CaptureHandle::poll_resize` (called per frame from `AppState::poll_capture_recovery_at`, step 0):
  1. the size must stay put for `RESIZE_SETTLE` (600 ms), so a drag with many sizes per second acts once;
  2. settled and **fresh** buffers arrived (`RESIZE_ALIVE_FRAMES` = 3 distinct `GstBuffer`s, because `pipewiresrc`'s keepalive resends the
     held buffer and must not read as alive): every destination gets `rescale_for_new_source(params, old, new)` through
     `change_destination`, the ordinary tier-2 branch swap (build alongside, first IDR, flip). The new size becomes `source_caps()`.
     Viewers keep the stream id; the decoder reconfigures on the new parameter sets and the stage re-lays out from the decoded size
     (the relay's announced `width`/`height` are not updated, nothing reads them once a frame decoded);
  3. settled and no fresh buffer: the capture is marked lost at once (`SOURCE_RESIZED_TEXT`) and the session restarts it, instead of
     waiting for the 5 s watchdog;
  4. no buffer at the tee for `SOURCE_GAP_LOST` (2 s) on a keepalive source: lost too, for outcome (b), which has no caps event.
  The pumps' watchdog holds off `SourceDead` while the size is still changing (`CaptureErrors::size.resizing`).
- `finish_recovery`: a restart that comes out at another size does not walk the restart backoff (`recovery_history`). A drag with a portal
  that cannot follow it restarts once per settled size, each time at position 0 (before: +2 s, +5 s, ... after the second resize).
- `rescale_for_new_source` moved to `capture::destination` (re-exported by `session`); the per-mille share is rounded and 99.5 % counts as
  the whole source (truncation drifted 1920x1080 to 1916x1076 over three restarts).

**Why:** a branch has fixed encode caps; scaling into them keeps the stream alive but distorts the aspect, and an encoder cannot take new
parameters mid-stream, so the branch swap is the one honest way to a new size. It is debounced because every swap is an encoder build.
The restart is kept, not replaced, because the measured portal needs it.

**Measured (rig, release, 60 fps, stock xdpw, viewer = client 2):**

| Scenario | Before | After |
| --- | --- | --- |
| one mode change 1920x1080 -> 1280x720, H.264 / H.265 | source declared lost 5.7 s later, ~6.3 s frozen | lost 1.1 s after the change, recovered 0.5 s later: ~1.6 s, viewer at the new size and aspect |
| burst of 14 sizes in 1.2 s, ending 1600x900 | (not run; schedule +2 s, +5 s each) | one or two restarts at position 0, picture back ~2.7-3 s after the first change, correct size |
| size change with no caps event (outcome b) | ~5.7 s | 2 s gap rule + 0.5 s restart: ~2.6 s |
| live source that survives (test source 1280x720 -> five sizes in 500 ms -> 640x480) | not handled | one swap, viewer decodes 640x480, no gap over 3 s (`end_to_end_test::a_source_that_changes_size_keeps_the_viewer_decoding_at_the_new_aspect`) |

**Known ceilings (`ponytail:` in code).** A destination whose swap is already in flight when the size settles answers `Err` and keeps
the old size until the next change. The 2 s gap rule restarts a capture that stalls 2 s for any reason (the watchdog's 5 s before).
One H.265 run hit `ring vcn_unified_1 timeout` (only that ring was reset, `Ring reset succeeded`, the client process died with `amdgpu: The
CS has cancelled because the context is lost`): the encoder was busy while the old capture was torn down. Not reproduced in two more runs.

**For a direct `pipewiresrc` of a `Video/Source` node (gamescope, row 142):** such a node renegotiates in place, so it takes path 2 (swap),
not the restart; `SizeWatch` hangs on the tee and works unchanged for any source. Frames there must be fresh buffers, not keepalive
resends, or `poll_resize` reads it as dead.

---

## Settings changes on a live share, and the coded-buffer deaths that were left (2026-11-04, task #139)

**Brief.** Row 137 stopped a dead encode branch from reading as a lost portal source, but (1) nothing had tested what the user does
all day, changing Preset / bitrate / FPS / Keyframe on a live share (their log: gop 250/1000, kbps 10000/25700, Quality/Performance, 60/120
fps within minutes, on AV1), and (2) 1080p 60 fps Quality still rebuilt the branch every GOP and then restarted the capture.

**The rig can now drive live changes.** `hs-drive.sh live fps=60 gop=1000 preset=quality quality=performance kbps=10000 codec=h265`
opens the live modal and sets the named values through the real controls; `hs-live-cycle.sh` runs a cycle of those over minutes and counts
sender and viewer log lines (`client/scripts/README.md`). Every change goes through `CaptureHandle::change_destination`, so it takes
the tier `classify_change` gives it: FPS, Keyframe, Encoding, Preset, Codec = one branch swap each, Bitrate = one `bitrate` property
write. A rise of the FPS over what the source runs at is a tier-3 `set_source_framerate`, which here only raises a number: the capture
restarts the old build showed came from encoder deaths, and **no run of the new build had one**.

**Before** (`9343855`, the build of row 137; same rig, `hs-live-cycle.sh`, AV1 120 fps Gaming 25 700 kbps, a detailed desktop with a
moving window, the default 12-step cycle: Quality 10 000 kbps / Gaming 25 700 / 60 fps + 1000 ms / 120 + 250 / 10 000 / 25 700 /
Performance / Quality / 1000 ms / 250 ms / 240 fps / 120 fps, one visit of the modal every ~8 s). Counted from before the share starts:
see the table. The failures were the ones the brief named:

- **`av1_reset_off` outlived the settings.** It is learned for one frame rate / keyframe interval / quality (60 fps Quality: the post-restart
  IDR fills the 972 kB buffer; 120 fps Gaming: it fits), and the pump kept it across the swap, so after Quality -> Gaming the pump never
  restarted the rate control again and the branch died on the first oversized keyframe. `IdrControl::params_changed` now forgets the learned
  state (and a pending restart) after a user swap and after a `bitrate` write. `av1_coded_buffer_bytes` itself was already right: it is
  computed from `params.framerate` at every keyframe, so it follows a change of FPS.
- **A `bitrate` write on `vaav1enc` forces an IDR** (the row 137 observation) and `DestCmd::SetBitrate` did nothing about its size. It now
  marks the next keyframe as "after a restart", so one that fills the buffer is learned like the pump's own restarts.
- **A restart that does not help ran forever.** With the keyframes held near the limit by something else (a `min-qp` floor: 758 kB of 972 kB
  at every keyframe) the 75 % trigger fired every 2 s and each restart cost an extra ~760 kB IDR (50 restarts in 40 s). A restart whose
  keyframe is still >= 80 % of the one that triggered it is now learned off (`av1_reset_trigger`).
- **The swap path with `async=false` worked**: 0 `produced no IDR within 5s` in every run of the new build; the old build had 1 in the
  90 s run (the 4th rebuild of a death loop).

**1080p 60 fps Quality, detailed still content: what was tried, one fresh rig per row** (60 fps, Quality, Keyframe 1000 ms,
25 700 kbps, 40 s, a static desktop with one moving window; `SSS_VAENC_EXTRA` appends encoder properties; "dead" = branch flow errors):

| encoder property (added to the pipeline) | content | largest keyframe | result |
|---|---|---|---|
| none | 7-gray 3x3 blocks ("noise") | 1130 kB | dead branch every GOP |
| none | photographic (smooth colour + grain) | 935 kB | 19 branch deaths, 3 capture restarts |
| `mbbrc=0`, `rate-control=cbr`, `target-usage=1`, `target-percentage=50`, `hierarchical-level=1 gf-group-size=1 ref-frames=1` | photographic | 935 kB | no change (19-20 deaths) |
| `allow-intrabc=true` | noise | 1130 kB | no change |
| **`palette-mode=true`** | noise | **129 kB** (deltas 17-40 -> 4-5 kB) | no deaths |
| `palette-mode=true` (+ intrabc) | photographic | 935 kB | no change |
| `min-qp=100 max-qp=200` | noise | 760 kB | no deaths (restart loop, above) |
| **`min-qp=70` / `90` / `110`** | photographic | **414 / 305 / 162 kB** | no deaths |

So two levers work and they are different content: **palette mode** is what flat-colour screen content (a code editor, a terminal, a
UI) needs and costs nothing elsewhere (identical keyframe sizes and encode times on photographic content, `enc_gap` / `enc_late` equal);
**`min-qp`** caps what the rate control may spend on a keyframe at any content, at the price of a quality ceiling (still 4-6 kB deltas
on a still picture: it stops "spending the surplus" on static detail). What does *not* help is every rate-control knob; `cpb-size` is
discarded for AV1 (GStreamer raises a value under half the bitrate to 2 s of bitrate), and a larger buffer by lying about the frame
rate is no option at 60 fps: 972 kB (level <= 4.1, ratio 4) is already the largest `vaav1enc` ever allots a 1080p frame.
*Why the earlier "min-qp 60/120 made no difference" differs from this table: unknown (single runs; most likely the property did not
reach the encoder).*

**What the code does now (`capture.rs`):**

1. **`palette-mode=true` for AV1 on Mesa AMD** (`launch_fragment`, gated like `vaapi_target_usage` by `va_driver_is_mesa_amd()`: it is a
   sequence-level tool flag and only this driver was measured).
2. **First AV1 branch death in 30 s: a quality floor** (`AV1_QP_FLOOR` = `min-qp` 70, `AV1_FLOOR_FAILURES` 1): the dead branch is rebuilt
   with it, the log says `rebuilding it with a quality floor (min-qp=70) so its keyframes fit the encoder's N kB coded buffer`. *Why on
   the first death and not never:* a floor for everyone costs sharpness on every still screen for a problem that only some content has;
   *why on the first and not the second:* a rebuilt branch without it dies at its next big keyframe, and every death is a ~1 s freeze
   (150 s cycle: 12 deaths when the floor came at the second death and was forgotten at every settings change, 1 death in 200 s
   when it comes at the first and is kept; the floor is process-wide because the keyframes are big because of
   the shared screen, and it is **kept across settings changes** and cleared by a new share: forgetting it at each change meant two deaths
   to relearn it at every change to 60 fps).
3. **Third VA branch death in 30 s: another codec** (`fallback_codec`, `CODEC_FALLBACK_FAILURES` 3, below `MAX_BRANCH_FAILURES` 4):
   AV1 -> H.265 -> H.264, only codecs this machine can encode, rebuilt as the same `SwapMode::Failed` swap with the new codec and the
   log `the AV1 encode branch failed 3 times in 30s ...; moving this destination to H265 for the rest of the share (change its
   settings to try AV1 again)`. The viewers sniff the codec from the bitstream and the swap sends a keyframe, as for a user's codec
   change. *Why this exists although AV1 now rarely gets there:* the brief's "never leave it cycling". **H.265 is not a safe way out**
   (`Calculate codedbuf size` 1 053 240 B at 1080p 120 fps, 1 575 480 B at 60 fps: level-limited too; the keyframes of 120 fps Quality
   reached 760 kB-1 MB: 29 deaths and 5 capture restarts in 50 s on the pre-#139 build), so it falls on to H.264, whose coded buffer
   never failed in any rig run (keyframes up to 1.3 MB at 120 fps). The settings UI still shows the user's codec; a settings change
   re-applies it.

**After (same scenarios, same rig, sender = the new build; counts are for the whole run including the first seconds):**

| scenario (live-change cycle, sender build) | changes | capture restarts (portal) | branch deaths | rate-control restarts | quality floors | codec fallbacks | no IDR in 5 s | viewer `never completed` |
|---|---|---|---|---|---|---|---|---|
| AV1 photographic 120 fps Gaming, 200 s: `9343855` | 24 | **6** | **30** | 21 | - | - | 0 | 0 |
| same, #139 | 24 | **0** | **1** | 3 | 1 | 0 | 0 | 0 |
| AV1 photographic, cycle of Quality/Gaming/60 fps/120 fps/Encoding, starts 60 fps Quality, 150 s: `9343855` | 15 | **6** | **30** | 23 | - | - | 0 | 0 |
| same, #139 | 15 | **0** | **1** | 2 | 1 | 0 | 0 | 0 |
| AV1 7-gray desktop 120 fps Gaming, 150 s: `9343855` | 18 | **4** | **26** | 20 | - | - | 1 | 0 |
| same, #139 | 18 | **0** | **0** | 0 | 0 | 0 | 0 | 0 |
| H.265 photographic 120 fps Gaming, 150 s: `9343855` | 18 | 0 | 2 | 0 | - | - | 0 | 0 |
| same, #139 | 18 | 0 | 2 | 0 | 0 | 0 | 0 | 0 |
| H.264 photographic 120 fps Gaming, 150 s: `9343855` | 18 | 0 | 0 | 0 | - | - | 0 | 0 |
| same, #139 | 18 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |

(The default cycle is 12 steps, one modal visit every ~8 s: Quality + 10 000 kbps, Gaming + 25 700, 60 fps + 1000 ms, 120 fps + 250 ms,
bitrate 10 000, bitrate 25 700, Performance, Quality, 1000 ms, 250 ms, 240 fps, 120 fps. A "portal restart" is the
`capture source lost` -> `restart attempt` path the user saw as *Recovering*; "branch deaths" are `returned a flow error` lines.
`never completed` is 0 in every run; the final viewer screenshots of the two AV1 photographic runs (checked by eye) show the live
picture, 121 and 60 fps in the tile.) Earlier runs of the
intermediate builds: a 300 s cycle with the floor forgotten at each change had 0 deaths in its counted part, so the first-death
counting window matters: the cycle script now counts from before the share starts. The static 60 fps Quality case on its own: 19 deaths and
3 capture restarts in 40 s before, 1 death and 0 restarts after (keyframes 414 kB).

**Not proven.** (1) The viewers were the rig's native client only; a browser viewer of the codec that a fallback switches to
was not exercised. (2) Palette mode and the `min-qp` floor were measured on this rig's synthetic desktops (7-gray blocks, smooth colour
with grain), not on the user's real screen; their log (`key_kB` 130-700 at 60 fps) suggests less extreme content, so the floor may
never trigger for them. (3) The H.265 path was cycled once (2 branch deaths in 150 s, below the fallback threshold, no restarts); the
H.265 -> H.264 step ran only as a unit-tested ladder, not live. (4) Everything was on the dev machine's RX 7900 XTX / Mesa 26.2.5 /
GStreamer 1.28.8; other drivers may size their coded buffer differently. (5) Bitrate writes were cycled 10 000 <-> 25 700 kbps only.

---

## AV1 branch deaths read as a lost portal source: the coded buffer, the branch latch, the 4x pace cap (2026-11-04, task #137)

**User report (real build `3a204da`, RX 7900 XTX, CachyOS, sharing and watching on one machine through the public relay):**
"The short freezes are gone, but it just randomly drops out and cancels the connection and switches to recovering."

**What the real log showed.** Five capture deaths in 90 s (`capture pipeline error from ...GstPipeWireSrc:pipewiresrc0: Internal
data stream error ... streaming stopped, reason error (-5)`, also from `portal_queue`), each followed by `capture source lost`, a
portal restart with schedule position 0..3 (delays of 0, 2, 5, 10 s: the visible *Recovering*), `encode branch removal: EOS was
not accepted` and two 1 s timeouts. Every one came 10-15 ms after a *natural keyframe time* of an AV1 120 fps / 25.7 Mbit/s /
Keyframe 250 ms stream, right after a run of keyframes that had grown from ~250 to 570 kB.

**Cause (reproduced in the rig, `GST_DEBUG=2` on the sharer; the element is named by evidence).** `vaav1enc` fails to map its
VA coded buffer for a frame that is bigger than the buffer:

```
vadisplay vasurfaceimage.c:309:va_map_buffer: vaMapBuffer: operation failed
vaav1enc gstvaav1enc.c:4122:gst_va_av1_enc_prepare_output:<branch_encoder_0> Failed to create output buffer
vabaseenc gstvabaseenc.c:557:_push_buffer_to_downstream:<branch_encoder_0> Failed to prepare output
basesrc gstbasesrc.c:3187:gst_base_src_loop:<pipewiresrc0> error: streaming stopped, reason error (-5)
```

GStreamer sizes that buffer as `width x height x 15 / 8 / MinCompressionRatio` of the AV1 *level* it derives from the picture size
and the display rate (`gstvaav1enc.c`, `_av1_calculate_coded_size`; log line `Calculate codedbuf size: 648000` at 1080p 120 fps,
level 5.0). The ratio rises with the level, so the buffer *shrinks* as the frame rate rises: 1080p up to ~68 fps 972 kB, 70-120 fps
648 kB, 144-240 fps 486 kB (`av1_coded_buffer_bytes`, unit test). The VA rate control on a static screen makes each keyframe bigger
than the last (rig: 253, 256, 288, 344, 382, 411, 435, 486, 540, 627 kB) until one does not fit. The failure is a plain
`GST_FLOW_ERROR` returned by the encoder's chain function **without a bus message**, and it travels *upstream*: the branch's leaky
queue, the shared `tee` (a flow combiner: an ERROR from one branch is fatal for the tee), then `pipewiresrc`, which stops its task.
The bus therefore only ever carried the source's and `portal_queue`'s messages, and `is_source_side_error` (anything not under
`:branch_`) read them as a dead portal source. So an encoder problem cost a whole capture restart. `-5` means "something downstream
returned an error"; it was never the portal.

*Why this is not the pacing:* the crash comes at keyframe time because that is when the encoder is handed a big frame; the multiplier
climb is a separate thing (below). Deaths also happen with the pace pinned at 2.5x and at 60 fps.

**Fix 1: contain a branch failure (`capture.rs`, `branch::BranchFailure`, `branch_entry_probe`).** The probe on the branch's
entry pad (it already counted offered frames) now looks at the entry queue's src pad `last_flow_result()` before every buffer. On an
`Error`/`NotNegotiated` it finds the culprit (`failing_element`: the peer after the last pad in the chain that holds the error, i.e.
the element whose own src pad is fine), logs
`encode branch BranchId(0): branch_encoder_0 (vaav1enc) returned a flow error ...`, latches it and `Drop`s every later buffer, so the
tee never sees the error and the source and every other branch keep running. The destination pump has an arm on the latch and
rebuilds only that branch at once (`SwapMode::Failed`: the recovery swap, old branch removed with `RemoveBounds::DEAD`, no drain wait for
a queue that can never pass an EOS). More than `MAX_BRANCH_FAILURES` (4) in 30 s ends as before: the capture is declared lost (a
restart; a share that cannot restart fails). Test:
`a_failing_branch_is_cut_off_and_named_while_the_source_and_the_other_branch_keep_running` (`identity error-after` stands in for the encoder:
the failing element is named, the other tee branch keeps getting frames, the only bus error is the failing element's).

**Fix 2: the encode branch's appsink is `async=false`.** The first version of fix 1 rebuilt a branch and the *second* rebuild never
produced a frame (`encode branch produced no IDR within 5s`, the x264 fallback too, then a declared lost source anyway). The stats tracer
showed the tee still at 240 fps and the new branch's queue passing exactly one buffer: the encoder's first IDR reached the appsink
(`gst_app_sink_preroll: setting preroll buffer`) and the streaming thread then sat in the preroll wait. The live-preview branch hit the
same thing and has `async=false` for it (see `live_branch`); the encode branch did not. With it every rebuilt branch gets frames (rig: 25
rebuilds in 90 s with the encoder dying every 2.5 s, none failed to start).

**Fix 3: keep AV1 keyframes under the buffer (`IdrControl`).** Containing a failure costs one rebuild (~0.3 s freeze and a keyframe
burst), so the pump avoids it where it can:

- *Restart the rate control before the limit.* A natural AV1 keyframe of >= 75 % of `av1_coded_buffer_bytes` (and at most every 2 s)
  gets a `bitrate` write of `kbps - 1` / `kbps` alternately. Any write restarts the VA rate control and makes the encoder code an IDR at
  once, and the keyframes start small again (~330-390 kB at 1080p 120 fps, 25.7 Mbit/s): one extra ~330 kB keyframe every ~2.5 s instead of
  a dead branch. Log: `AV1 keyframe of 491 kB is 75% of the encoder's 648 kB coded buffer; restarting its rate control ...`. *Why
  alternate:* a write of the same value may be a no-op. *Why only above 75 %:* the write itself forces an IDR, so a write at every
  keyframe sends every frame as an IDR (4762 keyframes in 40 s in the experiment that showed it).
- *Learned off when it does not fit.* The keyframe after a restart is measured; one >= 90 % of the buffer (or a branch death while the
  restart is pending) turns the restart off for the destination (`av1_reset_off`). Measured: at 1080p **60 fps Quality** the post-write
  IDR overflowed the 972 kB buffer (the branch died 34 ms after the write, every time), at 120 fps Gaming it is ~51 % of 648 kB.
- *No squeeze for AV1.* `IdrControl::force` squeezes the bitrate before a forced IDR when the last natural keyframes were big (#102,
  measured for `vah264enc`: the IDR after a write is 5-10x smaller). For AV1 that write is what killed the branch at 60 fps Quality (row
  133's "-5 after a squeezed IDR"), so it is skipped; the +300 ms / +1 s repeat IDRs after a rebuild are skipped too (`big()` is always
  true for a VA AV1 branch): at 60 fps Quality the repeat IDR, coded right behind the first, overflowed the buffer and killed every rebuilt
  branch in turn.

**Same rig scenario, before and after** (AV1 120 fps Gaming, Keyframe 250 ms, 25 700 kbps, headless rig, direct relay):

| build | capture deaths (portal restarts) in 90 s | branch failures | viewer `keyframe never completed` |
|---|---|---|---|
| `3a204da` | 12 (12) | not counted | 0 |
| fix 1 only (60 s) | 3 restarts (4 failures in 30 s gave up) | 20 | 1 |
| fixes 1-3 | **0 (0)** | **0** | 0 |

**Not fixed by this change, said plainly (2026-11-04, #137): superseded by the section above (task #139).** At **1080p 60 fps Quality
(1000 ms GOP)** on this rig's static detailed desktop the encoder filled the whole 972 kB buffer with the second keyframe at any bitrate
and nothing tried then stopped it (`cpb-size` discarded for AV1, `mbbrc`, `rate-control=cbr`, single runs). #139 found what does work
(`palette-mode`, a `min-qp` floor, and the codec fallback), and that two statements made here were wrong: `min-qp` *does* shrink AV1
keyframes (70 / 90 / 110 gave 414 / 305 / 162 kB instead of 935 kB; why the earlier "60/120, no difference" runs saw nothing is
unknown, most likely the property did not reach the encoder), and H.265's buffer is level-limited too (it is **not** a safe way out).

**The multiplier climb and the viewer's lost keyframes (a second problem, separate cause).** `PacePolicy` grows the keyframe pacing
factor x1.5 per clean keyframe (>= 100 datagrams) up to its `MAX` and halves it on a retransmit burst; `MAX` was 12. On the user's
37-38 Mbit/s line a 465 kB keyframe at 12x leaves in 12 ms (about 310 Mbit/s) and overflows the modem queue. The policy can only learn
that limit by losing a keyframe, and after every 10 s hold it climbs back and loses the next one. Rig, `udpproxy` 38 Mbit/s up / 150 ms
queue / 0.1 % loss, same build, AV1 120 fps 25.7 Mbit/s, ~500 kB keyframes, 70 s, only the pace differing:

| pace | viewer `a keyframe never completed` |
|---|---|
| pinned 2.5x | 0 |
| pinned 4x | 1 |
| pinned 12x | **27** |
| adaptive up to 12x | 9 (8 of them within a keyframe of a 6x or 12x burst) |
| adaptive up to 4x (this change) | 0 and 1 (two runs) |

`PacePolicy::MAX` is now **4.0** (row 133 had already measured "anything above about 3.7x buys nothing" on this line). A 500 kB keyframe
at 25.7 Mbit/s takes ~39 ms instead of 12 ms to leave the pump; the playout buffer (row 136, `video-pipeline.md` "Playout buffer") absorbs
that, which is what made the 12x window worth having in row 129. In the user's log the single viewer episode (14:02:47, 107/331
fragments) came right after the factor had been halved to 6x and then 2.5x by two resume requests, with 330-700 kB keyframes: so the 12x
climb is not the only way to lose a keyframe on that line, but it was the one that made it happen again and again in the rig.

**Logs for next time.** The failing element is in the log now (`... branch_encoder_N (vaav1enc) returned a flow error`), so a real log
settles it: that line with no `capture source lost` after it means the containment worked; a `capture source lost` with no branch line
before it is a real source problem. For the coded buffer itself run the sharer with `GST_DEBUG=2,vaav1enc:4` (`Calculate codedbuf size`,
`Use level`). The bench scripts (`crash.sh`, `pxrun.sh`, `summ.sh`, `corr.py`: one fresh rig per case) are in the worker's
`.tl-scratch/bench/`.

---

## Keyframe stutter on the real machine: the keyframe is bigger than the uplink (2026-11-04, row 133)

**User report (2026-11-04, verbatim), after row 129 below:** "the stutter on keyframes is still there." Then: "It is only like
a short stutter (maybe 100ms), but it perfectly lines up with the keyframe interval setting. No matter the framerate/preset."
and "Moving the mouse stutters, moving windows stutters and playing back video stutters". And: "The only time where i cant
observe any stutter is at 240fps, but that may be due to the encoder only receiving 180fps?"

**Cause: a keyframe takes its size divided by the sender's uplink to get through, and every later frame waits behind it.**
The viewer cannot show the keyframe before its last fragment, so the picture stops for that long and the waiting frames then
arrive in a clump. Row 129 shortened the time the *pump* spends on a keyframe to ~14 ms, which only moved the queue from the
pump into the modem: the user's line takes about 37 Mbit/s up, 12x of 10 Mbit/s is 120 Mbit/s, and anything above about 3.7x
buys nothing there. The loopback rig has no such limit, so it could not show this.

Measured on the user's own line (this machine is the client: public IP 92.117.193.182, RTT to the relay 19 ms):

| probe | result |
|---|---|
| 230 x 1200 B ICMP in 12 ms (a keyframe at 12x) to `1.1.1.1` | all 230 back, first 12.8 ms, **last 61 ms: 37 Mbit/s** |
| the same to the relay's public IP | first 19.6 ms, last 58-59 ms, 38 Mbit/s (only ~200 replies: its router stops answering, the app sees no such loss) |
| 24 MB over ssh to / from the relay's host | 31 Mbit/s up, 52 Mbit/s down (with ssh's start-up) |

So 230 kB takes ~50 ms and the 438-datagram keyframes of their 1000 ms sessions ~95 ms, which is the "maybe 100ms". There is
no loss to see: the queue absorbs the burst, which is why NACK stayed quiet (13 repairs in 703 keyframes).

**Reproduced** in the rig behind `udpproxy` set to the same path (38 Mbit/s up, 150 ms queue, 10 ms each way), a detailed desktop
with a detailed 480x360 window moving in circles (`pat.py`, source 240 fps, 10 Mbit/s). Gap = `shown_gap_ms[key` median:

| codec, fps, keyframe | keyframe kB | `span_ms` key | gap at the keyframe | frame period |
|---|---|---|---|---|
| no bottleneck, AV1 120, 250 ms | 257 | 15 | 27 | 8.3 |
| 20 ms RTT only, AV1 120, 250 ms | 256 | 15 | 27 | 8.3 |
| AV1 60, 250 ms | 275 | 61 | 83 | 16.7 |
| AV1 120, 250 ms | 259 | 57 | 69 | 8.3 |
| H.264 120, 250 ms | 271 | 59 | 68 | 8.3 |
| H.264 165, 250 ms | 241 | 53 | 61 | 6.1 |
| H.264 240, 250 ms | 241 | 53 | 57 | 4.2 |
| AV1 240, 250 ms, source 176 fps (the user's "180") | 230 | 51 | 60 | 5.7 |
| AV1 120, 1000 ms | 607 | 134 | ~140 (one window) | 8.3 |
| H.264 60, 1000 ms | 708 | 157 | 181 | 16.7 |

AV1 165 and AV1 240 from a 240 fps source produced no report behind the bottleneck (the share did not run; see the side finding). The gap follows the keyframe's size at every frame rate, codec and preset that ran, as the user said. The frames behind it then arrive
2.3 ms apart (`shown_gap_ms after`). Sender side in all of them: `pump_hold_ms` 14-17, `quinn_queue_max_kB` 33-59, `enc_late_ms` key
about 1: encoder and pump are not it.

**Why 240 fps looks clean: not explained.** The numbers say it stalls the same (AV1 240 at 176 fps: 60 ms, H.264 240: 57 ms), and
the user's own 240 session sent the same 230-datagram keyframes (4583 frames, 76 keyframes: 60 frames each, 176 fps). `RateCap` and
the tee are not involved: nothing differs at the keyframe. A guess, not a measurement: at 240 fps the catch-up after the stall shows
about twice as many intermediate pictures, so it reads as a quick speed-up, not a jump.

**Ruled out, measured:** encoder skipping or starving frames after an IDR (`motion.py`: a marker in the picture tracked through
vaav1enc + vaav1dec at 120 fps stays within 5 ms of where it should be across every keyframe, no frozen run; deltas behind the IDR 1.5-2 kB, as
elsewhere); IDR encode (4.0 ms against 3.4) and decode (8.7 against 3.3); `vaav1enc` reordering (none); quinn: at 20 ms RTT `cwnd_kB` is 0.5-36 MB and it
holds nothing back (on loopback it is ~37 kB, harmless at RTT 0); loss and the NACK hold (`held=0`); forced IDRs: the interval is `key-int-max` *frames*
(relay: 29.4-30 frames per keyframe while the rate moved 61-95 fps), which is why it read 0.3-0.75 s at damage-driven rates.

**What can be done, each measured on that path unless said. Nothing is changed yet: every line costs something, it is the user's choice.**

| option | keyframe | stall | how often | cost |
|---|---|---|---|---|
| today, Gaming 10 Mbit/s | 259 kB | 69 ms | 4/s | |
| bitrate 5 Mbit/s | 135 kB | 39 ms | 4/s | half the picture quality budget |
| Keyframe 1000 ms | 600-700 kB | 140-180 ms | 1/s | a longer stall: the size grows with the interval |
| Keyframe 8 s (H.264, temporary override, 0.1 % loss) | 0.9-1.5 MB | 291 ms | every 8 s | much longer stall; fall-back to 2.5x makes it worse |
| squeeze every IDR (#102's `bitrate` write) | 27-57 kB (#102) | about 10-20 ms, estimated | 4/s | -2 dB static, -4..-6 dB detail on each keyframe: a visible pulse; radeonsi quirk, not measured here |
| playout buffer on the viewer | unchanged | hidden | | added latency of about the stall (~70 ms at Gaming on this line, ~180 ms at 1000 ms), dynamic; **built in row 136**, measured in `video-pipeline.md` "Playout buffer" |

A faster uplink fixes it too: at 100 Mbit/s the same keyframe costs ~20 ms plus decode.

**Side finding, not fixed:** in the rig, AV1 with 8 s keyframes died at the first squeezed forced IDR (`the last keyframes were big; squeezing`,
23 ms later `pipewiresrc0: Internal data stream error ... reason error (-5)`, then a capture restart every few seconds), and `av1 60 quality` ran
at 69 frames in 10 s. `vaav1enc` also fails with -5 on its very first frame of `videotestsrc pattern=snow` while `vah264enc` takes it,
so an AV1 frame too big for the encoder looks like the trigger; a `bitrate` write on easy content survives. None of the user's logs has `capture source lost`.

**`video timing` log lines (permanent, info, one per 10 s and stream, near-zero cost).** Sender (`SendTiming`,
`capture.rs`): the settings in use, keyframe interval, sizes of keyframes / the 3 frames behind them / other deltas,
`pts_gap_ms`, `enc_gap_ms`, `enc_late_ms` (pulled minus capture PTS above the window's floor: an IDR's extra encode time),
`pump_hold_ms`, `quinn_queue_max_kB`, path stats. Viewer: `features/video-pipeline.md` "Timing report". Every figure is
`median/p99/max`, split `key` / `after` (3 frames behind a keyframe) / `other`. `pts_unix_floor_us` on both sides: with both clients on one
machine their difference is the shortest capture-to-complete time.

*Why:* "the stutter on keyframes is still there": only the user's machine can show which stage hitches, and their
uploaded log comes back with no effort from them.

The user's sessions up to 12:10 UTC ran `8d923739b`, older than these lines, so none has reached the relay yet (the upload's filter keeps
`simple_screensharing_client` at INFO, which covers them). On a build with them, `span_ms[key=` is the proof: about 50 ms on their line.

## The lagspike on every keyframe: the pacing hold, deadline pacing and an adaptive rate (2026-11-04, row 129)

*(Row 133 above corrects this: on a real line the keyframe's time is set by the uplink, not the pump. The conclusion below holds for the loopback rig only; the 12x rate does nothing past the uplink's speed.)*

**User report (2026-11-04, verbatim):** "On every keyframe, there is a lagspike in the screenshare." Their uploaded
client log had 1846 `sending a keyframe of ~300-360 datagrams (~300-360 kB), paced over ~80-100 ms` lines in one session
(Quality, one keyframe per second, about 10-11 Mbit/s; local machine, both clients through the relay).

**Cause (measured; it is the keyframe pacing of #97/#102, made 2-4x more frequent by the D8 intervals).** The pump
spreads a big keyframe over `bytes / (2.5 x bitrate)` and does nothing else meanwhile: the next frames wait in the
encoder's appsink. The viewer cannot show the keyframe's picture before its last fragment arrives, so on every keyframe
the picture freezes for the whole pacing window and then jumps (the queued frames arrive back to back; the latest-only
presenter shows the newest). No other stage contributes.

Method: the rig, a trace line per frame and stage (encoder output, pump pull, send done, viewer complete frame, decoder
output, upload to the GPU; wall clock µs, joined by the wire PTS; temporary patch, not committed), H.264 1080p VA,
10 000 kbps, a full-screen static detailed texture plus a moving ball (keyframes 370 kB at 60 fps / 158-253 kB at 120 fps:
the VA rate control gives an IDR a generous budget whatever the content, so these are the user's size or a bit more; the
deltas are small), `hs-rig.sh`'s viewer on a 60 Hz virtual output (so the *decoder-output* gaps are the proof, not the
present gaps, which are quantised to 16.7 ms). "Gap" = the largest interval between decoded frames within -50 ... +350 ms
of a keyframe's encoder output.

| build, settings | keyframe | pump hold (send) | viewer gap at the keyframe (median / max) | how often |
|---|---|---|---|---|
| master `77e2c9c`, Quality 60 fps, 2000 ms | 372 kB | 163 ms | **180 / 186 ms** | every 2.1 s (8 % of the time frozen) |
| master, Gaming 120 fps, 750 ms | 253 kB | 110 ms | **119 / 139 ms** | every 0.77 s (15 %) |
| head before, Quality 60 fps, 1000 ms | 371 kB | **173 ms** (planned 119) | **197 / 208 ms** | every 1.0 s (20 %) |
| head before, Gaming 120 fps, 250 ms | 158 kB | 69 ms | **77 / 85 ms** | 4 per second (31 %) |
| head before, Text 15 fps, 250 ms | 349 kB | 101 ms (deadline pacing only) | 155 / 172 ms (period 67) | 4 per second |
| **head after**, Quality 60 fps, 1000 ms | 378 kB | **23 ms** | **38 / 51 ms** (period 16.7) | every 1.0 s |
| **head after**, Gaming 120 fps, 250 ms | 158 kB | **9 ms** | **18 / 27 ms** (period 8.3) | 4 per second |
| **head after**, Text 15 fps, 250 ms | 349 kB | 21 ms | 83 / 98 ms (period 67) | 4 per second |

So the spike's **size is the same on master** (a keyframe's pacing window, 110-180 ms): the D8 intervals only made it
2-4x as frequent, plus the real pacing was ~45 % longer than planned (below). Stages for the "head before" Quality row
(medians): encoder output gaps 16.6 ms (p99 24: no encoder stall), pump wait behind the previous frame 0.1 ms, **send
173 ms**, relay + loopback 1.1 ms, decode of the IDR 13 ms (a delta 4 ms), upload 1.7 ms. The loopback cannot show a
real network or a loaded relay host.

**What changed (`client/src/capture.rs`, one accessor in `client/src/net.rs`).**

1. **Deadline pacing.** `PreparedMedia::send` slept `PACE_STEP` = 2 ms per chunk; every `sleep` overshoots ~0.9 ms, so a
   119 ms plan took 173 ms. It now sleeps until `start + i x 2 ms`. Alone: Quality 60 fps 197 -> 119 ms.
2. **An adaptive rate, `destination::PacePolicy`.** The window is the freeze, so a faster rate is the lever, but a
   fixed faster rate is wrong on a tight link (table below). The factor (x the destination's bitrate) starts at #102's 2.5,
   grows x1.5 per clean keyframe of >= 100 datagrams up to 12 (**capped at 4 since 2026-11-04, task #137**, see "AV1 branch deaths read as a lost portal source"), and halves (not below 2.5) and holds 10 s when (a) the relay
   forwarded at least `max(4, 1 % of the keyframe's datagrams)` retransmit requests for what we sent since the last
   keyframe (`SendRing::resent`: our burst overflowed a queue, or the path loses), or (b) a viewer that was not just joining
   asked for a keyframe (`DestCmd::KeyframeRequest`, > 2 s after a subscriber-count change). It only moves while the relay
   speaks NACK (`ScreenshareClient::nack_active()`, new in `net.rs`, the HelloAck flag): without NACK a lost fragment is
   never repaired and nothing would tell us, so it stays at 2.5. The 60 ms floor of the keyframe window (#86) scales with
   the factor (`60 ms x 2.5 / factor`). `SSS_PACE_MULT=<f>` pins a fixed factor (A/B, field rollback). Log: the existing
   `sending a keyframe ...` line now says `paced over ~N ms at F x the bitrate`; changes are `debug`.
   Ramp on a clean path: 2.5, 3.75, 5.6, 8.4, 12 over five big keyframes (5 s at 1 s intervals, 1.3 s at 250 ms).

*Why a rate and not "pace less"/"don't wait":* the keyframe's picture exists only when its last fragment is there; sending
the following deltas meanwhile does not help (the viewer must hold them, and the reassembler would deliver them out of order
otherwise); a smaller keyframe means a lower-quality IDR (#102's squeeze: -2 dB on a static picture, -4..-6 dB on detail,
every 250 ms: a visible pulse, worst for Text); the keyframe interval is the user's table (D8). Intra refresh is not
available (`vah264enc` has no such property). So the time on the wire is the only thing left, and it is bounded by the
path.

**Lossy paths** (rig, relay behind an ordered UDP impairment proxy with a tail-drop queue and random loss, only datagrams
>= 600 B impaired, 60 fps 10 000 kbps H.264 Quality; "drops" = the proxy queue's; one run per row unless said, runs vary):

| path | pinned 2.5x (#102) | pinned 8x | adaptive (this change) |
|---|---|---|---|
| clean | gap 119 | - | gap 38; 0 never completed in 36 keyframes |
| sharer link 20 Mbit/s, 50 ms queue | 1505 drops, 1 keyframe never completed | 0 drops, 0 abandoned (bottleneck-bound: gap 162) | not run |
| sharer link 14 Mbit/s, 50 ms queue | 326 / 372 drops, 2 / 2 frames abandoned | **7338** drops, **111** abandoned, 4 never completed | 393 drops, 5 abandoned; another run 3029 drops, 29 abandoned (the factor never left 2.5: that run was just worse, runs vary 10x here) |
| 0.4 % random loss, both legs, 60 s | 600 fragments asked / 597 recovered, 0 never completed | - | stays at 2.5x (about 5 resent per second trips the threshold): 588 / 587, 0 never completed, gap 120 |
| 0.1 % both legs + 15 ms each way, 150 s | gap 99 (median), 381 / 380 asked/recovered, 0 never completed, 0 abandoned | - | gap **67**, 520 / 494, 0 never completed, 1 abandoned |

A 12x pinned run with no loss and 15 ms delay: gap 35, 0 steady-state keyframe losses in 53 keyframes. In the 60 s runs
of the 0.1 % case one keyframe arrived with 212/379 fragments (a long contiguous hole at 12x, repaired by an L1 request
in 138 ms): the cost of the faster burst is a rare burst loss, which the policy answers by halving. A viewer that
subscribes *during* a keyframe sees `a keyframe never completed (92/380 fragments arrived)` at the join (seen in both
fast and slow runs; harmless, L1 follows): it is not counted above.

**What the user should see on their machine:** on a clean path the freeze at each keyframe drops from ~100-200 ms to one
or two frame periods within a few seconds of going live (60 fps: 38 ms; 120 fps Gaming: 18 ms). Their own log shows a
NACK repair of 1-54 fragments every ~10-20 s: each such event backs the rate off for at least 10 s, so expect the old
hitch to come back for a short while after a loss event and then go away again. `SSS_PACE_MULT=2.5` restores the old
behaviour exactly (apart from deadline pacing).

**Not covered:** a real internet path (burst loss, a loaded relay host: the relay's UDP receive buffer is the kernel
default, ~100 packets, and a 12x burst is ~15 000 datagrams/s), H.265 and AV1 were only checked for "it ramps and plays"
(AV1: 461 kB IDR paced in 30 ms at 12x), only the sharer's leg was impaired with a queue (the viewer's leg only with
random loss), several viewers of one stream, the browser sharer (`docs/js/core`, still a fixed 2.5x pacing).

---

## Webcam capture (2026-11-04, task I8, plan decisions D7, D8, D10)

A native webcam share is **its own capture**, not a mode of the screen share: a second `LocalCapture` next to the
screen (or alone, a screen share is not needed), with its own destinations and its own bar button. Code: the inline
`capture::webcam` module (listing, modes, the source fragment, pure and tested), `CaptureSource::Webcam`,
`capture::start_webcam_sharing`; `AppState::start_webcam` / `stop_webcam` / `set_webcam_settings` /
`refresh_webcams` and `AppSettings::webcam` (`rooms::WebcamSettings`) on the session side; the UI is
`ui/webcam_settings.rs`, `share::webcam_menu`, `stage/pip.rs` (`superdoc/ui/APP-SHELL.md` "Webcam and webcam in picture").

- **On the wire a webcam is an ordinary `StreamKind::Video` stream whose label starts with `Webcam`** (D7;
  `webcam::LABEL`, `webcam::is_webcam_label`, `LocalCapture::is_webcam`, `LocalCapture::wire_label`). *Why a label and
  not a new kind:* a new `StreamKind` would break every peer that does not know it (the wire is unchanged); a label
  does not: an older client shows the webcam as one more stream called "Webcam" (its picture, Watch button, volume
  row and all). The one rule "label starts with `Webcam`" is what the stage, the audio rules and the PIP read, and the
  browser client uses the same name. `spawn_destination` takes the label from the capture's source
  (`CaptureSource::wire_label`), so a destination added later (fan-out, a reconnect) announces it too.
- **Linux only, `v4l2src`, System domain** (D10). `CaptureSource::Webcam(Spec)` and `start_webcam_sharing` are
  `#[cfg(target_os = "linux")]`, as the Windows-only arms next to them are `#[cfg(windows)]`; the rest of
  `capture::webcam` (modes, keys, `source_fragment`) compiles everywhere, `list_cameras()` is empty and
  `unavailable()` says `Platform` off Linux, which disables the bar's button with a tooltip. The pipeline is
  `v4l2src device=".." ! image/jpeg,WxH,framerate=N/D ! jpegdec` (MJPEG modes) or `v4l2src ! video/x-raw,format=F,WxH,framerate=N/D`
  (raw), then the usual System-domain `videoconvert ! BGRx ! tee`. *Why no GPU domain:* a camera frame is small,
  `CaptureDomain::for_source` only takes the GPU path for the portal, and nothing is gained by a VA upload of a
  720p picture that a decode already put in system memory. A missing `v4l2src` plugin (`Unavailable::NoPlugin`)
  disables the button; it never crashes.
- **Modes come from the device** (`webcam::list_cameras`, blocking: run it off the UI thread, `AppState::refresh_webcams`
  does). GStreamer's `DeviceMonitor` with class `Video/Source`, so no new crate. **Two providers show up depending on
  the machine and the code reads both:** GStreamer's own `v4l2deviceprovider` (the rig: its private PipeWire has no
  v4l2 monitor) says `device.path` and **merges structures** (`width=2304, height={ 1536, 1296 }`); PipeWire's
  provider (a desktop with PipeWire's v4l2 monitor, the host: the monitor then shows *only* that one) says
  `api.v4l2.path` and lists one structure per size with a rate list. `modes_of` expands every list field
  (`list_of`: one value or a `gst::List`); a first version read only plain values and silently lost 114 of the C920's
  217 modes under the native provider (found in the rig: the modal said 103 modes, `v4l2-ctl` says 217). Skipped:
  `memory:DMABuf` variants (the same modes again), compressed H.264 (the encoder needs raw frames), a node that has
  no picture mode (the C920's `/dev/video1` is its metadata node: it does not list), a device already listed by the
  other provider. Mode order (`sort_modes`): the modes a person picks first (>= 15 fps) biggest picture first, motion-JPEG
  before raw at one size, faster first; the **slow ones (< 15 fps) after all the others** (a C920 raw 2304x1536 runs at
  2 fps and headed a 217-row list). *Why MJPEG first:* the camera's own compression is what makes its high modes fast
  (a C920 delivers raw 1080p at 5 fps, MJPEG at 30). Default mode (`default_mode`): the biggest picture up to 1280x720
  at >= 15 fps, MJPEG first, a rate up to 30 (60 fps of a face is waste): MJPG 1280x720@30 on the C920.
  **Checked against the device 2026-11-04:** the C920 lists 217 modes (MJPG 17 sizes x 7 rates = 119, YUYV 98) under both
  providers, identical to `v4l2-ctl --list-formats-ext -d /dev/video0` minus its H.264 formats (a one-off comparison, not kept as a script).
- **The saved choice** (`AppSettings::webcam`: camera name, mode as `MJPG 1280x720@30`, codec, bitrate, `#[serde(default)]`,
  old files load) is strings, so a camera that is unplugged or a mode it no longer offers falls back to the first camera
  and its default mode (`webcam::resolve`) **without overwriting the saved names**. A new camera resets the mode.
- **Encoder (D8): fixed** `Performance`, `TuneMode::LowLatency`, keyframe 2000 ms (`EncodeChoice::webcam`, shared with
  the screen's `EncodeChoice::screen` through `start_sharing_with_source`); only the codec and the bitrate (default
  2500 kbps, 200-12000) are the user's. The destination's `params.framerate` is the mode's rate, so the encoder never
  asks the camera for more than it gives (`RateCap` is a cap, never a floor, as for screens).
- **No audio rides with a webcam.** `RoomSession::sync_audio_watches` skips a webcam's watch when it picks the sender's
  audio to follow (`is_webcam_stream_in`), the tile has no volume row ("No audio in this stream"), and
  `owner_has_video` ignores webcams, so a sender with only a webcam and an audio share has that audio as an audio-only
  share with its own Listen button. *Why:* the sound belongs to the screen; a viewer who only watches the camera
  would otherwise get the desktop audio switched on with no control for it on the picture they watch.
- **One webcam at a time** (a camera opens once): `start_webcam` into another room stops the running one first and
  starts when no webcam capture is left (`webcam_start_after_stop`, `poll_webcam_start` runs in `poll_capture_states`),
  because the old pipeline still holds the device while it is `Stopping`. The same path restarts a webcam after a
  camera or mode change in the modal (a different capture); a codec or bitrate change is a live `edit_destination`.
  A room's reconnect keeps the capture and re-adds its destination like a screen's (`SuspendedShare::Video`).
- **A camera that goes away ends the share, with a toast; no restart loop** (`poll_capture_recovery_at` -> `end_lost_webcam`):
  `v4l2src` reports a source-side bus error (`CaptureErrors::mark_lost`), and a webcam is stopped like a user stop
  (destination released at once, the room told) with `errors.webcam_lost`. *Why not the screen's restart schedule
  (#99):* a portal comes back by itself within seconds and a restart can be silent; an unplugged camera does not, and
  nothing here could pick another camera for the user. `CaptureErrors::silence_is_loss` stays false for a webcam
  (only the portal's source resends frames on a static screen; a camera that is quiet for a while is not dead).
  Tested with a `videotestsrc` capture labelled `Webcam` and `inject_source_lost`
  (`session::ui_ops_tests::a_webcam_that_is_lost_ends_the_share_and_a_screen_is_restarted`); the real unplug was not done
  (the host's C920 stays plugged in).
- **Everything screen-specific ignores a webcam:** `share_state`, `Cx::live_rooms/my_phase/live_captures/on_air_rooms`
  skip `is_webcam()` captures, so the Share screen button, "Live in N rooms" and "Add X here too" never see it; the
  webcam has `Cx::webcam_share(room)`, and the sidebar's Outgoing popover lists it with a camera icon.
- **Rig hook `SSS_FAKE_CAMERA=1`** (off unless set, like `SSS_UI_FAKE_AUDIO`; `CLIENT_ENV_1="SSS_FAKE_CAMERA=1"` for
  `hs-rig.sh up`): `list_cameras` returns one "Test camera" (MJPG 1280x720@30, MJPG 640x360@30, YUYV 640x480@15, device
  `fake:ball`) and its source fragment is `videotestsrc is-live=true pattern=ball ! video/x-raw,..`, so the PIP checks have a picture that
  moves. The host's real C920 has its lens blocked: a black, noisy picture is the right result for it
  (device, modes and the stream path), and a camera without the hook lists the real device.
- **Packaging:** `video4linux2` (v4l2src; also the device monitor's native provider) is in `gst-plugins.txt` and
  `v4l2src` in `gst-elements.txt` (not rebuilt: the AppImage needs a container build); the Flatpak has `--device=all`
  (camera nodes); `check-deps.sh` warns about a missing `v4l2src` / `jpegdec` (webcam only, not fatal).

---

## Presets: Text, Quality, Gaming; the keyframe interval is saved (2026-11-04, plan decision D8)

The four presets (Low-Latency, Balanced, Quality, Gaming) and the Advanced tuning rows are gone. The
user's table (`client/src/presets.rs`, `EncoderPreset`):

| preset | FPS (set once) | encoding (`QualityLevel`) | keyframe interval | tune (hidden) |
| --- | --- | --- | --- | --- |
| Text | 15 | Quality | 250 ms | best quality |
| Quality | 60 | Quality | 1000 ms | best quality |
| Gaming | 120 | Performance | 250 ms | low latency |

*Why (user's table, plan decision D8):* three presets people can tell apart replace four that were
partly twins; only Performance and Quality survive as "encoding" modes (the VA `target-usage` mapping
below is unchanged, Quality = 4 on Mesa AMD, Performance = 2; x264 Quality is `medium`, see the `Why:` below). Text is for
documents and code: sharp at a low frame rate, a keyframe is cheap at 15 fps so 250 ms costs little.

- **A preset sets the FPS once.** `apply_preset_to_start_stream` / `DestEdit::apply_preset` write the
  preset's frame rate, quality level, tune and keyframe interval; the FPS dropdown afterwards changes
  only the rate. "Which preset is lit" is derived from **quality level + keyframe interval** only
  (`EncoderPreset::detect`), so a hand-changed FPS keeps the highlight, and a stale stored tune does not
  unlight it (the tune only matters on x264 and is no longer shown). A combination that matches no preset
  (an older settings file) lights no button; the Video card's caption always reads the real values
  (`60 FPS · Quality encoding · Keyframe 1000 ms`) and never says "custom".
- **The keyframe interval is saved** in `settings.toml` (`start_stream_defaults.gop_millis`, written at
  "Go live" with the rest) and restored by `apply_start_stream_defaults`. It used to be reset to the
  default on every start because only Gaming moved it; now every preset sets one, so forgetting it would
  show a preset's name with another preset's interval. `session::save_start_stream_defaults` now takes the
  whole `StartStreamDefaults`.
- **Old files keep loading.** `StartStreamDefaults.quality_level` loads `"balanced"` (the retired middle
  level) as `quality` (`deserialize_with`); `tune = "low_latency"` is kept as stored (hidden, only x264
  reads it); a missing `gop_millis` becomes 1000 ms (`DEFAULT_GOP_MILLIS`, the Quality preset's), a missing
  `quality_level` Quality. An old `performance` stays Performance at 1000 ms, which is no preset until the
  user picks Gaming (the old Gaming's 750 ms was never saved, and old Low-Latency and Gaming are
  indistinguishable in the file, so they cannot both map to Quality as the old preset names would
  suggest). Stored values are not rewritten until the next save. `QualityLevel::Balanced` is removed from the
  enum: `quality_level = "balanced"` is a `serde(alias)` of `Quality`, so nothing can produce it any more
  (a file written by this build never contains it; an old one is not rewritten until its next save).
  `EncoderPreset::from_value("balanced" | "low_latency")` is Quality, for symmetry with the browser, which
  stores preset names (`docs/js/core/video-tx.js` `ENCODER_PRESETS`).
- **Fresh app default = the Quality preset's fields** (`AppState::default`, `StartStreamDefaults::default`)
  at the old 30 fps / 8000 kbps; 30 fps with Quality encoding and 1000 ms still lights "Quality" (the FPS is
  not part of the match). `capture::DEFAULT_GOP_MILLIS` is 1000 now (was the protocol spec's 2 s; the spec
  figure is only a self-heal ceiling, a shorter interval stays conformant, it costs bitrate and buys a
  faster join/recovery). The webcam (task I8) is fixed to Performance and 2000 ms (`EncodeChoice::webcam`, see
  "Webcam capture" above).
- **Encoding and Keyframe are editable again (2026-11-04):** the modal shows Encoding (quality level) and Keyframe (`gop_millis`) as editable fields next to the Preset; reasoning in `ui/APP-SHELL.md` "Video card: Encoding and Keyframe".
- **Removed:** `EncoderProfile` (and `EncoderBackend::tune_applies`, `quality_effect`,
  `visible_quality_levels`, `display_quality_level`, `EncoderPreset::visible_for/detect_for`): the twin hiding
  they drove (#95) is moot with three presets that are distinct on every backend. The "hide a setting that
  does nothing" rule is kept by deleting the Tune, Quality-level and Keyframe rows rather than hiding them.
- **UI** (`ui/stream_settings.rs`, replica `superdoc/ui/replica/index.html`): right column **Compression**
  (Codec, Bitrate) then **Video** (FPS dropdown, Preset, caption). The Source card's **Change** button is
  there staged and live. While live it ends the stream first (`Discard` + `StageShare`: there is no source
  swap under a running encoder; it also stops the capture's other rooms), and the modal reopens staged.
- **Keyframe interval at 250 ms: nothing broke.** `key-int-max` = `capture::gop_frames` (rounded frames:
  Text is 4 frames = 267 ms at 15 fps). The sender-redundancy bursts (#97: +300 ms / +1 s repeats, one per
  `KEYFRAME_BURST_MIN_GAP` = 2 s) only follow forced IDRs and are harmless next to a natural IDR every
  250 ms; `IdrControl`'s "big" test is by size (> 1/4 s of bitrate), independent of the interval, and a
  natural IDR that large at 250 ms would eat the whole interval's budget, which the VBR does not produce on
  these contents. Not stress-tested with a big-IDR scene at 250 ms (the rig's vkcube IDRs are 30-60 kB).
  **It was not harmless for the picture:** with a big-IDR scene every keyframe is a pacing freeze, 4 per second at
  250 ms; see "The lagspike on every keyframe" at the top of this page (row 129), which fixes it.
- **Measured** (headless rig, release build, H.264 VA, 1080p vkcube, 8000 kbps, GStreamer tracer on the
  encoder output pad, `DELTA_UNIT` clear = keyframe; the client log's `sending a keyframe` line only covers
  keyframes of 24+ datagrams, vkcube's are smaller at 60/120 fps, so it cannot be used for this):

  | preset | encoder output | keyframes | median gap (p10 / p90) |
  | --- | --- | --- | --- |
  | Text | 15.0 fps | 282 in ~75 s | 267 ms (263 / 271) |
  | Quality | 60.0 fps | 27 in ~24 s | 1001 ms (697 / 1003; the 697 is the transition) |
  | Gaming | 119.7 fps | 241 in ~60 s | 250 ms (246 / 254) |

  The native viewer decoded Text (sidebar "16 fps", 5.6 Mbps) and Gaming ("121 fps"). `settings.toml` after
  "Go live" with Text: `framerate = 15`, `tune = "best_quality"`, `quality_level = "quality"`,
  `gop_millis = 250`; the next staged card opened with Text / 250 ms / 15 fps. Presets were switched live
  Text -> Quality -> Gaming (a branch rebuild each, the stream kept running).
- **Why x264 Quality is `medium`, not `veryslow` (2026-11-04):** Quality became the default (and is the Text
  preset), and software x264 must not get heavier than the old default, Balanced = `medium`. Windows encodes
  with `x264enc` only (`features/windows.md`), so `veryslow` at 1080p would have been the default there.
  `capture::EncoderBackend::launch_fragment` maps `Performance` to `ultrafast` and `Quality` to `medium`
  (pinned by `quality_level_maps_to_speed_preset_and_target_usage`). VA is unchanged (Quality = 4 on Mesa AMD,
  1 elsewhere). The `windows.md` row that measured x264 at `veryslow` (~line 210) is historic and was left as
  written. Not measured on x264 since the change (it is the same `speed-preset` the old default used).

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
   see "Encoder limits made visible" below. Balanced was `target-usage=4` then, the same
   half-speed AV1 band; #92 (2026-11-01) moved it to 3 on Mesa AMD.

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

**Start-up flap, the pause that closes it (#112, 2026-11-03).** *(Correction, #122 below: the pause closes the crash but leaves xdpw stuck in about half of the starts; the closing sentence about 100 ms is wrong, and `repair_portal_start` is the rest of the fix.)* The residual crash was bigger than the #59 note says: measured in the headless rig on 2026-11-03 (stock xdpw 0.8.4, pipewire 1.6.10, machine load 3-5 from other workers, every start = Share click through "capture ready", then Discard), 13-20 % of starts crashed xdpw at every commit from #52 (`a5433cf` 2/20 + 4/25 + 6/25, `3a55552` 2/20 + 3/25 + 5/25, `5412dda` 3/20, `facc373` 4/20 + 3/25 + 2/25, `HEAD` 6/20 + 4/20 + 1/25 + 7/25; `4abf65a`, just before the queue, 4/5). So it was not a regression of the stop rewrite, capture-source recovery, keyframe pacing or NACK; the queue still helps (about 80 % to about 15 %), but not enough. Mechanism (read from `gstpipewiresrc.c`): the READY->PAUSED change leaves the stream active, `set_active(false)` follows once the element has pushed its caps, and the PAUSED->PLAYING change calls `set_active(true)`; with one `set_state(Playing)` the pause and the resume are ~100 us apart. xdpw asks the compositor for a frame on the first `streaming`, stops asking after the `paused`, and creates another frame object on the resume; if the first frame has not been delivered yet that is the protocol error. **Fix:** `capture::play_and_wait` takes a portal pipeline to PAUSED first and sleeps `PORTAL_START_SETTLE` (30 ms) before PLAYING, so the compositor finishes the outstanding frame (4-17 ms for 240-60 Hz outputs). Measured as an A/B inside one rig (the delay read from a file per start, values interleaved, so load drift cancels): 240 Hz output 0 ms 11/39 crashes (28 %), 10 ms 0/30, 30 ms 0/30; 60 Hz output 0 ms 7/40, 10 ms 1/40, 30 ms 0/40, 60 ms 0/40. Final binary, 60 starts mixed H.264/H.265/AV1 at 60/120 fps: 0 crashes. A 100 ms pause still gave a healthy 120 fps H.265 share (Decoded 120 fps / shown 120 fps, 0 dropped), so the "100 ms leaves xdpw out of buffers" observation from #52 did not reproduce; 30 ms is the smallest value with margin. *Contradiction with the #52/#59 notes above:* they say a settle delay did not help (#59) and that 100 ms broke xdpw (#52). Their sample sizes were a handful of starts against a 1-in-15 base rate, which cannot show an effect either way; the interleaved A/B above can (p < 0.001), and the delay there is a pause between PAUSED and PLAYING. Cost: 30 ms on every portal capture start. The rig now emulates dbus activation for xdpw (`client/scripts/README.md`), so even a crash that still happens is retried by the client's single retry (#59) instead of killing the rig.

**Start-up flap, the stuck stream and its repair (#122, 2026-11-03).** #112's 30 ms pause fixed the crash and made the other half of the flap worse: with the stock xdpw 0.8.4 in the headless rig, **a share on the #112 build got no frame at all in about half of the starts** (task #120 saw 2 of 2: `xdpw.log` ~16k lines of `pipewire: unable to export buffer, dropping frame` / `out of buffers`, client `the capture source has produced no frame in 3.01s`, share stays STARTING). Measured with a "Share, Go live, wait for the first keyframe" loop (`Go live` is the first thing that needs a frame, a staged capture alone looks healthy), same rig, settle values interleaved, the pause read from a file per start: **0 ms** (= the pre-#112 behaviour) crashed xdpw in 20-50 % of starts (the rig's bus restart plus #59's one retry rescued about half; two crashes in a row ended in a `share error`), **10 ms 5/10 and 30 ms 4-8/10 and 100 ms 8/10 stuck**, no crash at any value >= 10 ms. The stuck share is *not* a load flake and not rig-only (the bus activation is irrelevant; a fresh rig also hit it on the first start).

Mechanism (read from xdpw 0.8.4 `pipewire_screencast.c` + `ext_image_copy.c`, pipewire 1.6 `stream.c`, and a `GST_DEBUG=pipewiresrc:5` + xdpw `-l TRACE` log of a stuck start): xdpw is the *driver* of its stream. On `streaming` it dequeues a buffer and requests a frame; on `paused` it queues the (half-written) buffer back; frames then run only through `frame ready -> enqueue -> trigger_process -> on_process -> dequeue -> request frame`. Whether xdpw is told `paused` at all is a race (the pause usually reaches it when it is held long enough: invisible in about half of the starts at 30 ms, in 70 % at 0 ms, only 20 % at 100 ms). When it is told: with a short pause the resume dequeues a second buffer and asks for a second frame while the first is outstanding -> the protocol error (crash, #112); with the 30 ms pause the first frame completes during it, xdpw's enqueue then logs `no buffer to queue` / `not streaming`, and after the resume the stream is dead: the consumer receives the **first buffer flagged `SPA_CHUNK_FLAG_CORRUPTED`** (the buffer queued at `paused`), a second one, and then nothing, while xdpw dequeues all 16 buffers, `out of buffers` forever (graph cycles keep running, the consumer's `pipewiresrc` just waits for buffers; no thread of ours is blocked). `pw-dump` of the stuck state: xdpw node, our node and the link all `running`/`active`. **A stream that was never told `paused` is fine.**

**Fix:** `capture::repair_portal_start`, called by `play_and_wait` for portal pipelines right after `PLAYING`. A one-shot buffer probe on `pipewiresrc`'s src pad (`watch_first_buffer`) records whether the first buffer carries `GST_BUFFER_FLAG_CORRUPTED`; the first buffer arrives 2-10 ms after `PLAYING` (`PORTAL_FIRST_BUFFER_WAIT` = 200 ms bounds the wait on a portal that sends nothing before the first screen change). If it is corrupted the pipeline goes `PAUSED` -> `PORTAL_START_SETTLE` -> `PLAYING` once more, up to `PORTAL_FLAP_REPAIRS` = 3 times, then fails as a `StartFailed` (new portal session, #59). *Why this and not something else:* the corrupted first buffer is a perfect marker (first run with the probe, 12 starts at 30 ms: 7 corrupted = the 7 that never got a frame, 5 clean = the 5 that did); frame *absence* is no marker because an idle monitor delivers nothing either. The second pause/resume starts from a quiet xdpw (no frame outstanding, no half-written buffer), and unsticks it every time (every repair in the experiment runs, always with the first re-flap; a 30 ms and a 100 ms hold behaved the same, 14/14 OK each, against 2/14 OK without the repair in the same interleaved run). A shorter first pause does not help (0 ms crashes xdpw, which also takes down other apps' screencasts on a real desktop), and a longer one makes the stuck case more common; the flap cannot be removed from the client side: `pipewiresrc` connects active and deactivates itself in `READY->PAUSED` (no property, no `PW_STREAM_FLAG_INACTIVE`), and the stream is `streaming` for xdpw before any element of ours can deactivate it. **Measured on the final binary** (same rig, stock xdpw, load 3-5): 40 of 40 starts reached the first keyframe, 30 of them needed the repair (always exactly one), 0 xdpw crashes; a full share + watch ran at "Decoded 120 fps / shown 120 fps, H.265 via vah265dec, 0 dropped". Cost: the repair adds ~110 ms to the 75 % (this rig, under load) of starts that need it; the first-buffer wait adds ~5 ms to the others. The #112 sentence "a 100 ms pause still gave a healthy share, so the #52 observation did not reproduce" was wrong: it only looked at starts that happened not to hit the stuck state. The #52 "100 ms leaves xdpw out of buffers" note was right.

## `target-usage` is a bitfield on Mesa AMD; Balanced = 3, Quality = 4 (2026-11-01, #92)

**2026-11-01 (#95): the UI hides what the mapping makes a twin, per running backend.** *(Superseded 2026-11-04: `EncoderProfile` and the twin hiding were deleted when the presets became Text/Quality/Gaming, see "Presets: Text, Quality, Gaming".)* On Mesa AMD,
Performance (2) and Balanced (3) are the same encode (table below), so the stream-settings modal no longer
offers a Balanced quality level there, and Low-Latency (Performance, 2000 ms) is the same encode as Balanced
(Tune is dropped on VA) so that preset is hidden too (user rule: "If a setting doesn't do anything, hide it").
`capture::EncoderProfile` derives this from `vaapi_target_usage` (`quality_effect`: Mesa AMD folds
{2,3}/{4,5}/{6,7}; Intel and x264 have no twins), so changing the mapping changes what is hidden with no UI edit.
Stored values are never rewritten. Also new here: `EncoderBackend::tune_applies` (moved from `presets`),
`EncodeBranch::backend()` (the backend a branch was *actually* built with, after a per-branch fallback to x264)
and `DestinationRuntime::actual_backend()` (recorded when a branch becomes authoritative; `params.backend` stays
the preferred one so the next swap tries VA first). `supported_codecs(Vaapi)` now probes for `vah265enc` /
`vaav1enc` instead of listing all three. UI side and rationale: `ui/APP-SHELL.md` (2026-11-01, #95).

**2026-11-01 (#94): Performance is 7 on every non-Mesa-AMD driver.** Spec-order drivers
(Intel iHD) follow the VA order, 1 = best quality ... 7 = fastest, so `vaapi_target_usage`
now gives them Performance 7 (Balanced 4 and Quality 1 unchanged). Mesa AMD keeps 2 / 3 / 4.
*Why:* Performance = 2 was chosen in #46 because 7 was the slowest band on radeonsi; since #92
radeonsi has its own mapping, so the 2 only ever protected a driver it no longer applies to.
**Untested on Intel hardware** (none available, and the headless rig is AMD, so the changed
branch cannot be exercised there); the value rests on the VA spec. Pinned by the
`vaapi_target_usage_per_driver` unit test. The #46 note below keeps its history.

User report: Balanced is very slow with AV1 on the RX 7900 XTX, and AV1 shares lag when the
GPU is fully loaded. Balanced was `target-usage=4`, the half-speed AV1 band of the #46
table below. Cause and fix, from the Mesa 26.2.5 and GStreamer 1.28.8 sources (the installed
versions) and measurements. Mapping lives in `capture::vaapi_target_usage`, driver detection in
`capture::va_driver_is_mesa_amd`.

### How `target-usage` reaches the hardware

1. GStreamer's `vah264enc`/`vah265enc`/`vaav1enc` send `target-usage` unchanged as
   `VAEncMiscParameterBufferQualityLevel.quality_level` (`gstvabaseenc.c:1175-1186`, in
   gst-plugins-bad `sys/va`; nothing is sent for 0, the property is 1-7). They first clamp it to
   the driver's `VAConfigAttribEncQualityRange` (`gstvaav1enc.c:2613-2624`); radeonsi reports
   32 (`si_mm_screen.c:139`, `PIPE_VIDEO_CAP_ENC_QUALITY_LEVEL`), and Mesa's own comment
   (`config.c:341-347`) says the range "isn't strictly faster when higher value used".
2. Mesa's VA frontend treats the value as a bitfield, not an ordinal.
   `vlVaQualityBits` (`src/gallium/frontends/va/va_private.h:445-456`):
   `valid_setting:1` (bit 0), `preset_mode:2` (bits 1-2), `pre_encode_mode:1` (bit 3),
   `vbaq_mode:1` (bit 4). `vlVaHandleVAEncMiscParameterTypeQualityLevel` (`encode.c:32-55`):
   0 = speed preset, no pre-encode, no VBAQ; **1 = special case: balance preset + pre-encode +
   VBAQ** ("suggested setting"); anything else = the bits read as they are (the valid bit is
   not even checked). Presets (`va_private.h:74-77`): speed 0, balance 1, quality 2, high
   quality 3; the radeonsi side (`radeon_vcn_enc.c:15-60`) clamps high quality to quality below
   VCN 4.0, and takes pre-encode only on the first frame and as 4x downscale.
3. With 1..7 only bits 0-2 exist, so:

| target-usage | 1 | 2, 3 | 4, 5 | 6, 7 |
| --- | --- | --- | --- | --- |
| preset | balance | balance | quality | high quality |
| pre-encode | **on** | off | off | off |
| VBAQ | **on** | off | off | off |

   My first hypothesis (4 = bit 2 = pre-encode) was wrong: the valid bit comes first, so the
   4-5 band is the *quality preset*, and 6-7 is *high quality*. Pre-encode and VBAQ (bits 3-4,
   values >= 8) are reachable only through 1. The speed preset (0) is not reachable at all
   from GStreamer. 2 and 3 produce bit-identical streams (same byte counts below).

### Measurements (2026-11-01)

`client/scripts/enc-bench.sh` with the new `SRC=` mode (1080p NV12 file, 360 frames of a 60 fps
screen recording played 3 times, `SRC_REPEAT=3`), `rate-control=vbr bitrate=8000`, median of 3
rounds, fps of raw encoder throughput. **The machine was busy** (load average 55-60 from other
workers' builds, GPU 23 % busy from the desktop), which compresses H.265 to ~240 and AV1's fast
band to ~300; the bands, not the absolute values, are the result. The old idle-machine
`videotestsrc` numbers are kept in the #46 table below. (`videotestsrc` itself caps at ~180
fps on a loaded box, which is why the `SRC=` mode exists.)

| target-usage | 1 | 2 | 3 | 4 | 5 | 6 | 7 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `vah264enc` | 161 | 191 | 182 | 128 | 126 | 127 | 129 |
| `vah265enc` | 238 | 265 | 267 | 229 | 240 | 239 | 242 |
| `vaav1enc` | 268 | 297 | 305 | 152 | 152 | 95 | 96 |

Quality, same clip, encoded to a file and compared with ffmpeg (`psnr`, `libvmaf`), VMAF /
PSNR dB, actual bitrate in brackets. At 8 Mbit/s the clip is easy (6.3 Mbit/s used) and all
levels are within 0.1-0.5 dB; at 3 Mbit/s the rate control binds:

| 3 Mbit/s | H.264 | H.265 | AV1 |
| --- | --- | --- | --- |
| tu 1 | 77.2 / 38.2 (2465 kbps) | 88.5 / 41.1 (2371) | 89.9 / 41.3 (2399) |
| tu 2 (= 3) | 85.7 / 39.8 (2452) | 91.0 / 41.7 (2419) | 90.2 / 41.4 (2412) |
| tu 4 (= 5) | 85.7 / 39.8 (2454) | 91.1 / 41.7 (2424) | 90.8 / 41.7 (2397) |
| tu 6 (= 7) | 85.7 / 39.8 (2454) | 91.4 / 41.8 (2430) | 90.8 / 41.7 (2402) |

(8 Mbit/s AV1: VMAF 94.47 / 94.49 / 94.83 / 94.94 for tu 1 / 2 / 4 / 6.)
Single clip of game footage, objective metrics only; VBAQ is a perceptual tool that PSNR and
VMAF may undervalue.

Real app (headless rig, release build, one live AV1 share at 240 fps, 1080p vkcube, 8 Mbit/s,
sender stats tracer, 30 s window, machine load average ~25; the rig ran with
`HS_XDPW=patched` because the stock xdpw died on the start-up flap under that load, #59, on
the first try): encoder output fps

| preset (level, target-usage) | capture | into encoder | encoder output |
| --- | --- | --- | --- |
| Gaming (Performance, 2) | 238.3 | 236.0 | 232.6 |
| Balanced (3) | 238.9 | 238.9 | 235.3 |
| Quality (4) | 237.9 | 184.2 | 184.2 (encoder-limited) |

Balanced (3) holds the full 240; with the old Balanced value 4 it would have sat at the
Quality row. The log line `VA encoder kernel driver: amdgpu` confirms the detection.

### Decision

- **Performance 2, Balanced 3** on Mesa AMD: both are the balance preset. The encodes are
  identical, because nothing reachable sits between the balance and quality presets (the
  only other fast option, 1, is *worse*, below). Balanced's job here is to stay in the fast
  band (AV1 ~300 fps instead of ~150, H.264 ~185 instead of ~127), which is what the
  full-GPU-load lag needs; the cost is -0.0 to -0.6 VMAF.
- **Quality 4** (quality preset), not 1 and not 6. **Why not 1 (the old value):** pre-encode +
  VBAQ was slower than 2 on H.264 and *lower* in PSNR and VMAF on every codec at 3 Mbit/s
  (H.264 VMAF 77.2 vs 85.7), so "Quality" was the worst-looking level on this driver. **Why not
  6:** over 4 it adds nothing for H.264, +0.1 dB for H.265 and +0.01 dB for AV1 while AV1
  falls from ~152 to ~95 fps, below 120 fps streams.
- Other drivers (anything not on the `amdgpu` kernel driver, or undetectable) keep
  Performance 2 / Balanced 4 / Quality 1. Performance 2 there is unmeasured (an Intel box was
  not available; the spec's fastest would be 7).
- **Detection:** the kernel driver behind `vah264enc`'s own `device-path` property
  (`/dev/dri/renderD128` -> `/sys/class/drm/renderD128/device/driver` -> `amdgpu`), read once
  (`OnceLock`) and logged as `VA encoder kernel driver: amdgpu`. **Why not the VA vendor
  string:** GStreamer has no accessor for it from Rust and libva would be a new dependency.
- The `QualityLevel` UI/saved values are unchanged; only the `target-usage` they map to moves.

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
(1) were unchanged then (moved on Mesa AMD by #92, section above, which also explains the
bands). **Why Performance is 2 (2026-11-01 update, #94):** that holds for Mesa AMD only; other
drivers get the spec's fastest, 7, since #92 gave radeonsi its own mapping (untested on Intel). **Why 2 and not 1:** 1 is slower and its rate control overshoots (H.264
wrote 65 % more bytes than at 2 for the same bitrate). Quality cost of 2 is noise (H.265
PSNR 40.83 dB at 2, 40.84 at 4, 40.90 at 7 on a `testsrc2` clip). Trade-off: a driver that
follows the VA order (Intel iHD) gets a slower encode for Performance; the hint below still
tells the user when that caps the framerate. Full table and reasoning:
`EncoderBackend::launch_fragment` in `client/src/capture.rs`.

Real app, vkcube on DP-1 (280 Hz), 240 fps target, sender stats tracer: AV1 Gaming 107.8 fps
encoder output before, 240.1 after; H.264 Gaming 239.9 after.

### The encoder-limit hint (removed)

Every 500 ms the pump compared the encoder's output fps with `min(frames offered to the branch, target fps)` and the UI
showed "Encoding N of M fps". **Removed 2026-11-04 (user, row 135):** "framerate is dynamic, so remove the warning. It
always pops up, when nothing is moving." The UI went first, the measurement (`EncoderLimit`) with row 133. The
offered-frames probe on the branch stays: `stall_verdict` uses it to tell a dead source from a wedged encoder.
The older stream-settings banner ("Your encoder reaches about N fps", "Use H.265") went on 2026-10-15 (user, #87: "it's just annoying").

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

## Stop is instant; a lost capture source is restarted (2026-11-02, #97 #98 #99)

Three user reports from real use over the internet: (1) after Stop the share "stays open" and
a new one cannot be started for ~15 s, (2) screen shares "stall out of nowhere" and have to be
restarted by hand, (3) the viewer hangs after fps/codec changes. Investigated in the rig; this
is what changed on the sharer side (the viewer side is #101).

### Stop (#98)

**What the user saw:** `share_state()` (ui/mod.rs) said Live for any capture that still had a
destination in the room, and `stop_share_one` only swapped the status to `Stopping`; the
capture was reaped when `CaptureHandle::stop()` finished. So the whole teardown, and any hang
in it, showed as LIVE / "Stop sharing" / no "Share screen" button.

**Now**, in order of what the user and the viewers feel:

1. `AppState::stop_share_one` releases the capture's destinations **at the click**
   (`release_stopping_destinations`: `forget_destination_map` + clearing the map, and ending a
   pending recovery). The `LocalCapture` stays in `Stopping` only to own the stop's result.
   `share_state` also never reports a `Stopping` capture as a staged share. "Share screen" is
   back at once and a new share started now is independent of the old teardown.
2. The destination pump's `Stop` arm calls `client.stop_stream()` **before** removing the
   branch (viewers drop the tile in milliseconds, not after the 1-10 s the EOS drain and a
   hardware encoder's `Null` take).
3. `CaptureHandle::stop`: destinations stop **concurrently** under one 3 s deadline
   (`DESTINATIONS_STOP_DEADLINE`), `DestinationHandle::stop` waits 4 s instead of 7 s + 7 s, the
   branch removal uses `RemoveBounds::FAST` (EOS drain 1 s, teardown 3 s; swaps keep 5 s / 10 s),
   the pipeline EOS wait is 1 s (it was 5 s), the pipeline's `Null` runs on a blocking thread
   (3 s bound) concurrently with the preview-task join and the portal session close (3 s each).
   `Drop` skips `set_state(Null)` after a `stop()` so a stuck teardown cannot block an async worker.
4. The live and room-preview appsinks are `wait-on-eos=false`: an unpulled sample (the UI stops
   pulling when the capture leaves Active) used to hold the pipeline EOS back for the full 5 s.
5. The `ApplyParams` branch swap runs as its own task (`PendingSwap`) joined from the pump's
   `select!`, so `Stop` is served immediately during a settings change. Other commands that
   arrive mid-swap are deferred, not dropped; the swap's branches are cleaned up by a background
   task after a Stop.
6. Info-level timings: `capture stopped in N ms: destinations, EOS, pipeline Null, preview,
   portal close`, `encode branch removed: drain N ms, teardown N ms`, `stream N stopped: relay
   notified in N ms, branch removed N ms later`. A real slow stop now says which step hung.

`Why the bounds are what they are:` the old worst case was 7 s reply + 7 s join per destination,
in sequence, plus 5 s EOS, plus unbounded `Null`/preview/portal (a hung `vaDestroyContext`
reproduced 10 s branch teardown + 5 s EOS ≈ 15 s). The new worst case is ~3 s destinations +
1 s EOS + 3 s steps, and none of it is visible to the user or the viewers any more.

### A lost capture source is restarted (#99)

**The reproduced "stall out of nowhere":** kill xdpw mid-share, or `swaymsg output HEADLESS-1
mode 1280x720` / `transform 90` while sharing. `pipewiresrc` reports `stream error: target not
found`; the old `capture_loop` logged the first bus error, stopped reading, and the destination
sat `Stalled` forever re-sending its cached IDR. Real-world triggers: portal restart, monitor
hotplug or mode change on Hyprland.

**Detection.** `capture_loop` now reads the bus until it ends. A bus error whose element is not
inside an encode branch (`/branch_N` bin, see `is_source_side_error`) or whose text is
`SOURCE_GONE_TEXT` marks the source lost (`CaptureErrors::lost`, read through
`CaptureHandle::source_lost()`). `Why branch errors don't:` one encoder failing is that
destination's problem (below), not a reason to restart the screen grab. The silence watchdog
adds a second path: a `Stalled` destination's pump compares frames *offered to the branch* with
frames *encoded* (`destination::stall_verdict`): offered > 0 and no output for 6 s is an
**encoder wedge**, offered == 0 for 5 s is a **dead source** (`mark_lost`), but only for a source
that keeps resending on a static screen (`CaptureErrors::silence_is_loss`: the portal's
`pipewiresrc keepalive-time`); a source that is silent on a static screen by design (Windows)
never restarts the capture from silence alone. The bus path is faster (immediate). Measured in the
rig: `output transform 90` / `mode 1280x720` gave no bus error at all, only silence, so the silence
path is the one that matters there; killing xdpw gave `target not found` on the bus at once.

**Encoder wedge (per destination, in the pump).** Rebuild the branch with identical params via
`swap_to_new_branch_with(.., SwapMode::Recovery)`: the destination stays `Stalled` (keepalive
keeps the viewer's stream alive; `begin_rebuild` still refuses from `Stalled`), the old, stuck
branch is removed in the background, the new one may fall back VA -> x264 (`add_branch_live`).
Rebuilds at +0, +5 s, +15 s after the stall passes 6 s (`wedge_rebuild_backoff`); after
`MAX_WEDGE_REBUILDS` = 3 and 10 s more, the destination is `Failed`. The VA-to-x264 fallback no
longer waits for the stuck dud branch to be removed (that was 5 s drain + up to 10 s teardown
before the software branch even started): `add_branch_live_waiting` removes it in a spawned task.
Verified with an `LD_PRELOAD` fault that hangs `vaEndPicture` on encode contexts only
(`vaCreateConfig` entrypoint tracking, so the shared pre-tee `vapostproc` keeps working; a hang
on every VA call kills the capture and shows up as a dead source instead): picture back about 14 s
after the hang (3 s silence + 3 s stalled + 5 s for the VA dud to miss its IDR + the x264 branch).

**Dead source (per capture, in the session): Option 1.** (A size change reaches this route faster since 2026-11-05: see "The source changes size" at the top.) `AppState::poll_capture_recovery`
(once per frame) sees `source_lost()` and starts a `CaptureRecovery` (`AppState::recoveries`):
`begin_stall` (every destination `Stalled`, cached IDR keepalive; the old pipeline stays in
place so viewers keep their frozen tile), then restart attempts on the schedule
`CAPTURE_RESTART_DELAYS` = +0, +2, +5, +10, +30 s. An attempt is `capture::start_capture_restoring`
(the portal restore token of the dead capture, `PersistMode::ExplicitlyRevoked`: portals that
persist - xdph, GNOME, KDE - re-grant silently; stock xdpw ignores it, a picker may appear).
When the new capture is up (`finish_recovery`): the old capture is stopped **in a spawned task**
(it may hang; nothing waits for it), `ShareStatus::Active` points at the new handle (same
`CaptureSourceId`), and every destination is re-added with its own params (rescaled to the new
source by `rescale_for_new_source`: same share of the source, so a rotated or resized monitor
keeps its aspect) as a *new* stream. The viewers see `StopStream` for the old stream id and a new
announce with the same owner/kind/label, which their wanted-stream logic re-watches (30 s
window, `RESUME_AFTER_STOP`). Destinations requested while recovering (a reconnected room,
"Go live") are parked in `CaptureRecovery::deferred`.
After the last attempt: every destination `Failed("capture lost: ...")`, its stream is stopped,
the user gets a share error, and `retry_destination` starts a fresh recovery
(`recovery_gave_up` stops the per-frame loop from restarting it by itself). **The egui UI has no
Retry button** (`retry_destination` is lib-only): the user stops sharing and shares again. A
button would need a `UiAction` arm in `ui/apply.rs`. A **dismissed
picker ends the recovery at once** (`SHARE_START_CANCELLED`): a picker is never looped blindly.
A capture that dies again < 30 s after a recovery starts one schedule position later
(`RECOVERY_RESET_AFTER`, `CaptureRecovery::start_pos`): a flapping source is slowed down, while a
recovery that needed five tries is a long outage, not a flap, and does not count against the next.
Measured in the rig (`output transform 90`, 60 fps H.265): the source is declared lost 5 s after
the change, the restart takes ~0.1 s, and the viewer's picture moves again ~0.7 s later (wanted-stream
re-watch worked every time), ~6 s of freeze in total. Killing xdpw: attempts at +0/+2/+5/+10/+30 s
while the portal is down, recovered on the 5th when xdpw was started again (47 s).

`Why Option 1 and not an in-place source restart (Option 2):` the pipeline, the tee and each
destination's `StreamId` would survive and viewers would not even re-watch, but swapping the
source element of a live tee without renegotiation hangs is the hard part (the measured
"rewriting a pre-tee capsfilter stalls the capture dead", see "Tier 3 turned out to need no
pipeline surgery"). Option 1 is exactly what the user does by hand, with the proven start path.
Option 2 is a later improvement.

**UI (sharer).** The pill reads *Recovering* (`PillKind::Progress`, `ui.pill.recovering`) and
the action bar title *Screen capture lost - Restarting the screen capture* (since I3 the same lines are the recovering pill and note in the Share menu, `share::share_menu`); the tooltip has the
attempt number, the countdown to the next attempt and the last error. The room tile chip reads
RECOVERING. `ui/APP-SHELL.md`.

### Sender keyframe redundancy (#97)

After a branch swap (settings change or recovery rebuild) and when a destination starts, the
pump sends the forced IDR plus two more at +300 ms and +1 s
(`destination::keyframe_repeat_schedule`). The same two repeats follow a viewer-asked IDR (a
relay-forwarded `KeyframeRequest`, a resume from pause): a viewer asking means it lost one.
Rate cap: one such burst per `KEYFRAME_BURST_MIN_GAP` = 2 s per destination
(`schedule_requested_repeats`); a request inside it is served by the burst under way. **Keyframe pacing (the rate is adaptive since row 129, "The lagspike on every keyframe"):** a keyframe of 24+ datagrams is no longer sent back to back: the pump
spreads it over min(60 ms, 4 frame periods) in chunks every 2 ms (`pace_plan`; **since #102 by rate,
see below**,
`PreparedMedia::send`; delta frames and small keyframes still go out at once, the stall keepalive
and a branch's proving IDR are unpaced). *Why:* #86's lab: quinn lets a ~256-packet burst out at
once and a shallow bottleneck queue (50 ms at 8 Mbit/s) drops the tail of every IDR sent that way,
and an IDR with a missing fragment is not decodable; spreading it over ~60 ms took that link from
0 to 58 decodable fps (BBR). The cost is that the pump (which also serves `Stop`) is held for up to
60 ms per keyframe, and the following frames queue behind it in the encoder's appsink (8 buffers).
Only checked for "still decodes" in the rig (no lossy link); the loss benefit is #86's measurement.
Not done: sending the *same* IDR's datagrams several times back to back. It would need the viewer's
reassembler (`fragment.rs`) to drop duplicate fragments of an already completed `frame_id`
(the repeats above are separate frames with fresh nonces, so they need nothing there); a repeated
fragment must also not be re-encrypted (a nonce is never reused), only re-sent. On a lossy link the one forced IDR is often lost and
the viewer waited for the next GOP boundary (2 s) with a frozen picture.

### A second output mode change that left the share stuck (2026-11-04, #115)

**Seen (I5):** live share, captured output 1920x1080 -> 1280x1024 (recovered) -> 1920x1080 (stuck: viewer at ~2-5 fps,
squashed picture, L1 recovery looping). **Reproduced** in the headless rig on stock xdpw 0.8.4 (`swaymsg output
HEADLESS-1 mode ...`), 1 of ~20 mode changes (3 + 8 fresh-rig two-change runs + a 10-change loop); the other changes all
recovered. Stock xdpw has three behaviours on a mode change of the captured output, none of them ours:
1. it **crashes** (`ext_image_copy_capture_session_v1#18: error 1: session already has a frame object`, `xdpw-crashed.log`),
   `pipewiresrc` reports `stream error: target not found`, the session restarts the capture (~0.5 s); about 40 % of changes;
2. it **survives but its stream goes silent** (nothing offered to the branch): the 5 s silence path declares the source
   lost and the capture is restarted (`the capture source stopped delivering frames`, 2-3 s); about 55 %;
3. it **survives and renegotiates** (`wlroots: num_modififiers` x4, `paused`, `streaming`) and then logs
   `ext: frame capture failed: buffer constraint mismatch` / `ext: started frame without buffer` (or, later,
   `out of buffers` / `unable to export buffer, dropping frame`): the stream is dead but `pipewiresrc`'s
   `keepalive-time` keeps resending its last buffer. The tracer shows `capture 29.9 fps` = `into encode branch 29.9 fps`
   and no encoder output; the branch is *offered* frames, so `stall_verdict` calls it an **encoder wedge**, rebuilds the
   branch 3 times (`the source delivers frames but the encoder emits none`) and ends `the encoder stayed silent through
   3 rebuilds; giving up` with the destination `Failed`. Caught once (stock, second change after an xdpw restart).
   **Why the encoder emits nothing from the keepalive buffers is not known** (could not catch the state again to look).

**Patched xdpw** (`HS_XDPW=patched`, wlr-screencopy): 2 of 2 changes recovered through the silence path
(`the capture source stopped delivering frames`, source lost after ~5 s, new portal session, 0.5-2.9 s), no crash. One
sequence only, so "the wlr-screencopy build does not show behaviour 3" is weak evidence.

**Fix (client, small):** `destination::give_up_restarts_capture`: when the wedge verdict gives up (3 failed rebuilds, ~35 s)
and the source is a portal (`silence_is_loss`) not yet declared lost, the pump declares the *source* lost
(`treating the capture source as dead`) instead of failing the destination, so the session's restart schedule runs.
Restarting the capture is the path that recovered behaviours 1 and 2 every time. *Why not detect keepalive-only
frames earlier:* the branch cannot tell a resent buffer from a real one (that is the #39 finding), and the cause of
the silent encoder is unknown, so the late, bounded escalation is the safe version; a real encoder wedge pays up to the same
restart, bounded by `CAPTURE_RESTART_DELAYS`. **Not verified in the rig:** behaviour 3 could not be provoked on demand, so
the new branch was only unit-tested and regression-run (mode changes still recover).
Related, not a bug: several mode changes within ~2 min walk down `CAPTURE_RESTART_DELAYS` (`RECOVERY_RESET_AFTER`, "a flapping
source is slowed down") and end in `giving up after 5 restart attempt(s)`; a 13 s loop of changes did that after 5.

Hyprland (xdph) was not tested (no xdph in the rig; `xdph-emu` only emulates its format offer). The escalation applies to
it as well, but whether xdph ever shows behaviour 3 is unknown. The percentages above are from about 20 changes.

### Big keyframes: rate pacing, a squeezed IDR, no repeats (2026-11-03, #102)

**Field case.** Sharer on Fedora + GNOME, RX 9070 XT (radeonsi), `vah264enc`, 1080p, 120 fps, 8300 kbps,
H.264, Quality, 40 Mbit/s uplink; native viewer at 16-30 ms RTT. On big screen movement the viewer sat on
"Recovering..." for 4.8 s, 30.6 s and 100 s. Its log: `a keyframe never completed (981/984 fragments
arrived)` 295 times (IDRs of 400-985 datagrams, 0.5-1.2 MB, ~1 s of bitrate; a calm screen's IDR is 40 kB),
`recovery L1` every ~1.5 s; relay -> viewer loss ~0.03 %, no relay eviction warnings, so the loss is on
the sharer side.

**Root cause (confirmed).** A frame is lost if *any* fragment is, so the survival chance falls with the
fragment count: at 0.1 % loss only ~37 % of 984-fragment frames arrive, at 0.3 % ~5 % (and the relay ->
viewer leg alone, 0.03 %, costs another ~26 %). Three things made it a loop instead of a hiccup:
(1) the VA encoder, whose VBR does not honor its own limits (below), emits an IDR of ~1 s of bitrate whenever
the picture is detailed and moving; (2) `pace_plan` spread any keyframe over at most 60 ms (33 ms at
120 fps), i.e. 130-280 Mbit/s for such an IDR into a ~40 Mbit/s uplink, and a 1.18 MB IDR does not even fit
quinn's 1 MiB datagram send buffer (which silently evicts the *oldest* queued datagrams, the front of the
IDR); (3) every failed IDR made the viewer ask again (L1, Unsubscribe+Subscribe), and `schedule_requested_repeats`
answered each ask with the IDR plus two repeats at +300 ms / +1 s, so ~3 MB of IDRs per request, each
as unlikely to arrive whole as the last.

**Encoder side: what radeonsi honors (measured on the dev machine's RX 7900 XTX, Mesa 26.2, GStreamer
1.28.8, the client's exact `vah264enc` config, 1080p120, 8300 kbps).** Content: a 1920x1080 colored-noise
texture scrolling 3 px/frame (P frames predict it, an IDR cannot: big IDR, tiny P, like game/video
content) and a calm-then-moving clip (120 static frames, then 120 noisy ones).

| setting | IDR sizes (kB) | verdict |
|---|---|---|
| VBR as shipped (`cpb-size` 3112) | 1004, 662, 236, 183 (scroll); 620 at the calm -> moving cut | no cap; the first IDR of a stream and the first frames after a calm stretch are the worst (the rate control has banked its budget) |
| VBR, `cpb-size` 800 / 400 + `target-percentage` 70 | byte-identical / 295-620 | `cpb-size` is ignored under VBR |
| VBR, `min-qp` 18-26 | 620 -> 421 (needs 30, which would soften every static screen) | not usable |
| **CBR** | 54-240 (scroll), max delta frame 19 kB instead of 1.9 MB | caps frame size, but pads a static screen to ~4 Mbit/s (424 kbps under VBR): the "wastes bandwidth" the owner rejected in #88 |
| VBR + any write of `bitrate` right before the forced IDR | 27-57 (even `bitrate * 0.99`), PSNR of that IDR -4..-6 dB on noisy detail, -2 dB on a static picture, no refinement within 12 frames on noise | what #102 uses |

A write of the `bitrate` property restarts radeonsi's rate control (the encoder also emits one more IDR two
frames later), and the IDR coded right after is 5-10x smaller. It is a driver quirk, not a documented
contract; the squeeze is only armed on `EncoderBackend::Vaapi` and only when needed (below), and measured on
radeonsi only (Intel iHD untested).

**What changed (`client/src/capture.rs`).**

- **`pace_plan` by rate.** A keyframe's window is `max(old window, bytes / (2.5 x configured bitrate))`, cap
  `PACE_MAX_RATE_WINDOW` = 600 ms (1.18 MB at 8.3 Mbit/s: ~390 ms as chunked, 455 ms exactly). A *delta* frame is paced only when that
  time exceeds two frame periods (> 5x an average frame; the 1.9 MB delta after a calm screen starts moving),
  so ordinary deltas still go out at once. *Why:* the pump is held while pacing (the raw queue drops the
  oldest raw frames meanwhile and `Stop` waits behind it, up to 600 ms), which is a price worth paying: a
  screen share tolerates a keyframe that arrives 0.4 s late, not one that never arrives. The log line
  `sending a keyframe of N datagrams (~X kB), paced over ~Y ms` (info, only >= 24 datagrams) is the field
  diagnostic that was missing.
- **`IdrControl`.** Natural keyframes (the encoder's own GOP ones) that exceed 1/4 s of bitrate (259 kB at
  8.3 Mbit/s) mark the content *big* for 10 s. While big, every forced IDR (viewer request, resume, swap,
  stalled-branch) is preceded by a write of 90 % of the real bitrate, written back when that keyframe leaves
  the encoder (the write-back's own IDR two frames later is a free repeat, and is ignored for the "big"
  decision for 1 s). A calm screen (IDR < 1/4 s) is never squeezed, so it keeps its quality. Log:
  `the last keyframes were big; squeezing the encoder's bitrate for the forced one`.
- **No explicit repeats while big.** The +300 ms / +1 s repeat IDRs (#97) are skipped (they tripled the burst);
  small keyframes keep them (#101's numbers: a ~100-datagram IDR is lost often enough on a lossy path).
- Not changed: the client's 1 MiB quinn datagram send buffer. With rate pacing the backlog stays below it
  (the buffer drains at the congestion controller's rate; a 455 ms window at 2.5x bitrate adds < 0.7 MB);
  the rig's IDRs (<= 700 kB) never came near it, the field's 1.18 MB would have.

**Rig results** (headless rig, UDP impairment proxy between the sharer and the relay, H.264
120 fps 8300 kbps Quality, the scrolling-noise pattern, random loss on the sharer -> relay leg, 100 s per run,
"frozen" = the `grim` crop of the viewer tile did not change for 0.8 s; baseline = this commit without the
change except for the log line):

| sharer -> relay loss | build | `never completed` | L1 | frozen total / longest | keyframe size kB (median / max) | datagrams through the proxy (100 s) |
|---|---|---|---|---|---|---|
| 0.4 % | before | 80 | 47 | 39.0 s / 5.5 s | 275 / 709 | 109.8k |
| 0.4 % | after (A) | 44 | 48 | 4.1 s / 1.7 s | 102 / 608 | 79.0k |
| 0.4 % | after | 39 | 44 | 6.8 s / 2.3 s | 102 / 667 | 77.0k |
| 0.4 % | after | 46 | 45 | 9.1 s / 3.6 s | 102 / 455 | 77.2k |
| 0.2 % | before | 43 | 33 | 8.0 s / 1.7 s | 276 / 667 | 109.2k |
| 0.2 % | after (A) | 26 | 33 | 3.8 s / 1.4 s | 135 / 668 | 87.4k |
| 0.2 % | after | 22 | 29 | 0.0 s / 0.0 s | 179 / 455 | 83.5k |

(A) = the first variant, where only the *last* natural keyframe decided "big" (no 10 s memory): with
natural IDRs of 100-600 kB it switched the squeeze off right when a request needed it. One run per row;
the loss is random, so single runs vary by a few seconds. Recovery times (`recovered, keyframe ... arrived N
ms after the problem began`): 0.4 % median 1.2 s / max 6.8 s before, median 1.1-1.2 s / max 3.3-5.1 s after.

A 40 Mbit/s / 20 Mbit/s tail-drop uplink (50-60 ms queue) showed no loss at all before or after: quinn's
BBR already spreads the IDR at its bandwidth estimate, so the field loss is random loss times fragment
count, not a bottleneck queue. The remaining `never completed` are the *natural* (unsqueezed) GOP IDRs,
up to 600 kB here; the viewer recovers from them in ~1.2 s with a squeezed IDR. Making the natural ones
small too would mean taking over the GOP (`key-int-max` large, the pump forcing every IDR through
`IdrControl`); not done. The rig's IDRs (275 kB median) are smaller than the field's (1.18 MB), so the
absolute numbers understate the field problem; the 30-100 s recoveries were not reproduced.

**Browser sharer** (`docs/js/core`): ported in #103 (rate pacing only; no repeats exist there, and WebCodecs
has no equivalent of the `bitrate` write): `features/website.md` "Big keyframes in the browser sharer".

### Audio watch teardown no longer holds the video lock (#97, latent)

`RoomSession::teardown_audio_watch` dropped the `AudioWatch` (it joins the audio thread) while
the `audio_watches` mutex was held; the video ingestion task locks the same mutex per frame, so
a slow audio sink teardown stalled video. The watch is now taken out under the lock and dropped
after it.

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

**Superseded in the UI 2026-11-04** (Tune, Quality-level and Keyframe rows and the four old presets are gone, see "Presets: Text, Quality, Gaming"); the backend mapping below is still how the fields are encoded.

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
`medium`/`4` on both backends; since #92 the VA half compares against `vaapi_target_usage`, 3 on Mesa AMD, and `vaapi_target_usage_per_driver` pins the table) and `capture::destination::tests::tuning_knob_changes_are_tier_two`.

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

**Since 2026-11-04 the Balanced level is gone and x264 `Quality` is `medium` (see "Presets: Text, Quality, Gaming"); the table below is the older mapping.**

| Control | `x264enc` (Software) | `vah264enc` (Vaapi) |
| --- | --- | --- |
| Tune | `tune=zerolatency` (LowLatency) / unset (**BestQuality, default** — x264enc has no explicit "best quality" value, only the *absence* of the low-latency one) | **No equivalent control.** The field is still accepted and stored, but has no effect while the branch is VA-API, so the stream-settings modal does not show the Tune row there (#90, `ui/APP-SHELL.md`) |
| Quality Level | `speed-preset`: `ultrafast` (Performance) / `medium` (**Balanced, default** — x264's own upstream default preset) / `veryslow` (Quality) | `target-usage`, VA spec order (Intel): `7` (Performance; `2` between #46 and #94, 2026-11-01; untested on Intel) / `4` (**Balanced, default**) / `1` (Quality). **Mesa AMD (radeonsi), since #92:** `2` / `3` (**Balanced, default**) / `4` — `target-usage` is a bitfield there, see "`target-usage` is a bitfield on Mesa AMD" |
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
First-run fallback (since 2026-11-04) is the Quality preset: 30 fps / 8000 kbps / `best_quality` /
`quality` / 1000 ms keyframe interval (saved too now) / H.264. The `[start_stream_defaults]` block is written to `settings.toml`
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

> **Superseded in part 2026-11-04 (task I7, plan D9; `features/audio-sharing.md`):** audio is no longer an attachment on a screen share. The audio button of the
> control bar is independent of the screen: no "+ Add audio on a card", no "paired with video" invariant (`start_audio_share(room, None, ..)` is what the UI
> always passes; `for_capture: Some(..)` survives for a share paired with a screen, whose Stop then stops the audio too). Stopping the screen leaves the audio
> running. A viewer sees an audio-only sender as a **Listen** tile (`Watch` is for video; a sender with a screen still gets its audio with the watch,
> "Audio follows video"). Everything below is the 2026-07-28 reasoning, kept where it still explains the pipeline.

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

> **Note 2026-11-04 (I7):** the UI presentation described here (an extra row on the share) is gone; the backend facts (meter, mute, boost, stop) hold. Boost is set from the audio settings menu now (not Settings > Shared audio), the "Pairing" bullet is only the paired case (`for_capture: Some`), and "No stranded audio" is moot: an audio share is listed in Outgoing on its own.

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
- **Boost (#114):** `volume name=gain` + `rglimiter name=limiter` in F32 *before* the mute and the meter
  (`audio::GAIN_STAGE`), set live by `AudioCaptureHandle::set_gain` from the audio settings menu (Settings > Shared audio until 2026-11-04); an
  `audioconvert` after the meter takes it back to S16 for `opusenc`. `features/audio-sharing.md` (2026-11-03,
  task #114): why sender-side, why the limiter is bypassed at 100 %. The calibration was removed and the boost capped at 200 % in #119; viewers level loudness themselves (`leveller.rs`).
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
- **The choice is a name; same-name streams are mixed (#105, 2026-11-03).** `list_audio_sources`
  returns one entry per display name (first node, used for the meter); `start_audio_sharing` links
  *every* `Stream/Output/Audio` node with that name into the one loopback input and
  `audio::link_poll_loop` re-checks every second (links new matches, forgets vanished ones; pure
  logic in `link_diff`). One loopback, one pipeline, one encoder: nothing restarts. Zero matching
  streams = a silent share (the capture only ever reads the loopback output, so never the
  microphone). The choice is not persisted anywhere (`AppState::selected_audio_source` is
  in-memory), so there is no settings back-compat. Details and *Why*: `features/audio-sharing.md`.
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
