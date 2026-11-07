# Encoder failures and freezes

Settings changes on a live share, coded-buffer and AV1 branch deaths, and the burst / wire-clock / viewer freezes found in owner testing.

Part of the Stream Control reference, hub: `superdoc/features/stream-control.md`. The text moved here verbatim from that page: "above", "below" and "this page" in it may point at a section that now lives in a sibling page of this folder (the hub lists them).

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
H.265 -> H.264 step ran only as a unit-tested ladder, not live. (4) Everything was on the dev machine's AMD GPU / Mesa 26.2.5 /
GStreamer 1.28.8; other drivers may size their coded buffer differently. (5) Bitrate writes were cycled 10 000 <-> 25 700 kbps only.

---

## AV1 branch deaths read as a lost portal source: the coded buffer, the branch latch, the 4x pace cap (2026-11-04, task #137)

**User report (real build `3a204da`, an AMD GPU, sharing and watching on one machine through the public relay):**
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
at 25.7 Mbit/s takes ~39 ms instead of 12 ms to leave the pump; the playout buffer (row 136, `features/video/playout-buffer.md`) absorbs
that, which is what made the 12x window worth having in row 129. In the user's log the single viewer episode (14:02:47, 107/331
fragments) came right after the factor had been halved to 6x and then 2.5x by two resume requests, with 330-700 kB keyframes: so the 12x
climb is not the only way to lose a keyframe on that line, but it was the one that made it happen again and again in the rig.

**Logs for next time.** The failing element is in the log now (`... branch_encoder_N (vaav1enc) returned a flow error`), so a real log
settles it: that line with no `capture source lost` after it means the containment worked; a `capture source lost` with no branch line
before it is a real source problem. For the coded buffer itself run the sharer with `GST_DEBUG=2,vaav1enc:4` (`Calculate codedbuf size`,
`Use level`). The bench scripts (`crash.sh`, `pxrun.sh`, `summ.sh`, `corr.py`: one fresh rig per case) are in the worker's
`.tl-scratch/bench/`.

---

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
