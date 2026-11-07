# Keyframes and bandwidth

Keyframes bigger than the uplink, the keyframe pacing hold, and the two bandwidth modes with their allocator.

Part of the Stream Control reference, hub: `superdoc/features/stream-control.md`. The text moved here verbatim from that page: "above", "below" and "this page" in it may point at a section that now lives in a sibling page of this folder (the hub lists them).

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

Measured on the user's own line (this machine is the client: public IP `<client public IP>`, RTT to the relay 19 ms; the real values are in `.claude/local/network-measurements.md` in the main checkout, if present):

| probe | result |
|---|---|
| 230 x 1200 B ICMP in 12 ms (a keyframe at 12x) to a public DNS resolver (`<probe IP>`) | all 230 back, first 12.8 ms, **last 61 ms: 37 Mbit/s** |
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
| playout buffer on the viewer | unchanged | hidden | | added latency of about the stall (~70 ms at Gaming on this line, ~180 ms at 1000 ms), dynamic; **built in row 136**, measured in `features/video/playout-buffer.md` |

A faster uplink fixes it too: at 100 Mbit/s the same keyframe costs ~20 ms plus decode.

**Side finding, not fixed:** in the rig, AV1 with 8 s keyframes died at the first squeezed forced IDR (`the last keyframes were big; squeezing`,
23 ms later `pipewiresrc0: Internal data stream error ... reason error (-5)`, then a capture restart every few seconds), and `av1 60 quality` ran
at 69 frames in 10 s. `vaav1enc` also fails with -5 on its very first frame of `videotestsrc pattern=snow` while `vah264enc` takes it,
so an AV1 frame too big for the encoder looks like the trigger; a `bitrate` write on easy content survives. None of the user's logs has `capture source lost`.

**`video timing` log lines (permanent, info, one per 10 s and stream, near-zero cost).** Sender (`SendTiming`,
`capture.rs`): the settings in use, keyframe interval, sizes of keyframes / the 3 frames behind them / other deltas,
`pts_gap_ms`, `enc_gap_ms`, `enc_late_ms` (pulled minus capture PTS above the window's floor: an IDR's extra encode time),
`pump_hold_ms`, `quinn_queue_max_kB`, path stats. Viewer: `features/video/decode-and-display.md` "Timing report". Every figure is
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
renders that as the `ui.stream.bw_over` string (`ui/stream_settings.rs`) with
the actual numbers — *"Over budget by X kbps: the guaranteed floors need Y kbps but the
maximum is Z kbps. No stream is starved below its floor."*

---
