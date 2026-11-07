# Capture stop and restart

Stop is instant; a lost capture source is restarted; sender keyframe redundancy, big-keyframe pacing and the output-mode-change cases found while doing it.

Part of the Stream Control reference, hub: `superdoc/features/stream-control.md`. The text moved here verbatim from that page: "above", "below" and "this page" in it may point at a section that now lives in a sibling page of this folder (the hub lists them).

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
RECOVERING. `ui/SHARE-FLOW.md`.

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

**Field case.** Sharer on a GNOME desktop with a recent AMD GPU (radeonsi), `vah264enc`, 1080p, 120 fps, 8300 kbps,
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

**Encoder side: what radeonsi honors (measured on the test machine's AMD GPU, Mesa 26.2, GStreamer
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
has no equivalent of the `bitrate` write): `features/website/nack-and-big-keyframes.md` "Big keyframes in the browser sharer".

### Audio watch teardown no longer holds the video lock (#97, latent)

`RoomSession::teardown_audio_watch` dropped the `AudioWatch` (it joins the audio thread) while
the `audio_watches` mutex was held; the video ingestion task locks the same mutex per frame, so
a slow audio sink teardown stalled video. The watch is now taken out under the lock and dropped
after it.

---
