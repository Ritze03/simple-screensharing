# Video: playout buffer

The viewer-side delay that holds still-encoded access units before the decoder so a clump of late frames reaches the screen evenly. Part of the video pipeline hub, `superdoc/features/video-pipeline.md`.

## Playout buffer (2026-11-04, row 136)

`client/src/decode/playout.rs` (pure, `now` injected, 11 unit tests) used by `decode/video.rs` `run`. **What:** access units
are held *encoded* (kB each, never decoded frames) for a measured per-stream delay before they join the decoder backlog; the
loop wakes for the head's due time through `recv_timeout` (a real wake-up, no polling; `VideoView`'s waker still drives the
repaint). The delay shrinks again by playing up to 5-10 % faster than live. Every number is the browser's
(`docs/js/core/jitter.js`) except what the delay aims at.

```text
x = arrival - pts;  baseline = min x (creeps up 1 ms/s: clock skew);  late = x - baseline   (sender clock offset cancels)
peak = max late over the last 8 s;  target = 1.15 * peak, clamped [0, max_jitter_buffer_ms]
applied: up by at most 20 ms per frame; down by at most catch_up_percent of wall time
release = pts + baseline + applied   (never before the previous release); a frame never waits more than max since arrival
```

*Arrival* is when `feed` got the complete (NACK-repaired) frame. Rules, each with a test: first frame at once (`applied`
starts at 0); nothing is ever dropped by the buffer; a **still screen** adds nothing (no frames, no measurement; the next
frame's `arrival - pts` is the baseline again); `max = 0` is today's behaviour exactly; the maximum is a hard cap
(`min(release, arrival + max)`, read at every poll, so lowering the slider releases held frames at once); a PTS restart
(5 s of lateness) releases what is held and measures again. `PipelineMsg::Reset` (L2 recovery) clears it. The two sliders
(Settings > Playback) call `decode::playout::set_limits`, read by every stream on its next frame.

**Why held encoded, before the decoder (the browser's way):** 1080p NV12 frames are ~3 MB, a 250 ms buffer at 240 fps would
be 180 MB; access units are tens of kB, and the existing `trim_backlog` still guards the decoder side.

**Why the peak, not the browser's `ewma * 4 + 40 ms`:** the EWMA averages `|jitter|`, and a keyframe stall is one big sample
among hundreds of normal ones: at 60 fps with a 180 ms stall once a second the average deviation is about 3 ms and the
formula asks for about 50 ms, so most of the stall still reaches the screen (a unit test uses exactly this case). The
buffer has to cover the worst recent lateness. The 8 s window outlasts a 2 s keyframe interval; after the stalls stop the
target falls and the catch-up speed sets how fast. The `+ 40 ms` floor is gone: a clean path keeps about what its own
keyframes need (23 ms in the rig with no bottleneck).

**Why (user, 2026-11-04):** "On 5 mbit/s it seems a little bit shorter. But if building the playout buffer fixes it, add it.
Make it dynamic, like it was before with an automatic catchup speed. Also add the settings back to the Settings menu."
Cause of the stutter: row 133 (`stream-control.md`): a keyframe needs size / sender uplink to arrive, later frames queue
behind it and arrive in a clump.

**A/V sync follows.** `StreamDecoder::feed` tells `AvSync` the video arrived at `now + playout delay`
(`Shared::playout_us`, set by the pipeline thread), so the video latency floor rises with the buffer and the audio is
delayed by the same amount (`decode/avsync.rs`; test `the_playout_buffer_delay_reaches_the_audio_delay`). Not verified
with real audio (no audio on the host).

**The `video timing` line** gained `playout_ms[median/p99/max]` (the delay at each release). `decode_ms` now includes the
time a frame was held. `late_ms` is against the window's minimum, so it shows evenness, not the added latency (that is `playout_ms`).

**Measured (row 136, rig, release, relay behind the row-133 proxy: 38 Mbit/s up, 150 ms queue, 10 ms each way, 10 Mbit/s
stream, a detailed window moving in circles; "off" = `max_jitter_buffer_ms = 0`; viewer `video timing`, last 10 s window,
median/p99 of the gap between shown frames at a keyframe, and the added delay):**

| case | keyframe shown gap off | on | other frames' gap on | playout delay |
|---|---|---|---|---|
| AV1 120 fps Gaming (key 250) | 68.8 / 73.0 ms | 11.1 / 18.8 | 8.3 / 12.8 | 70.6 / 74.3 ms |
| H.264 60 fps Gaming | 78.0 / 88.2 | 20.4 / 26.4 | 16.6 / 21.2 | 81.2 |
| H.264 60 fps Quality (key 1000) | 165.6 / 171.0 | 31.6 / 40.8 | 16.8 / 29.7 | 192.0 / 199.5 |
| AV1 120 fps Quality (key 1000) | 500.9 / 999.5 | 516.7 / 1001.1 | - | 87 / 154 |
| AV1 120 Gaming, clean path (no bottleneck) | - | 10.7 / 15.6 | 8.3 / 12.7 | 22.6 / 23.0 |
| AV1 120 Gaming, 1 % loss up + 0.5 % down | 90.5-108 / 154-215 | 10.7 / 88 (first window), then 11-15 / 16-32 | 8.3 / 12.5 | 226-260 |

The AV1 Quality row is not a buffer matter: a 605 kB keyframe overflows the proxy's 150 ms queue (`a keyframe never
completed (261/521 fragments)`), viewer recovery L1 asks for a new one (recovered after 673 ms off / 690 ms on): same
with and without the buffer. A *still screen* (the pattern frozen with SIGSTOP; the sharer kept sending keyframes at about
30 fps) left the delay at 73-87 ms, no growth. Under loss the buffer grows to cover NACK repair (a repaired keyframe
arrives about 250 ms late): the stutter is gone, the price is that delay; the user can cap it with the slider.
