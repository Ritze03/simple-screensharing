# Audio sharing: A/V sync (viewer-side audio delay)

Audio capture/encode/playback is described where it grew up: `features/stream-control.md`
("Sharing audio ...", "The 2026-07-28 audio bugs"), `features/video-pipeline.md` ("Audio follows
video") and the module docs of `client/src/audio.rs` and `client/src/decode.rs`. This page covers
**keeping the audio aligned with the picture** (task #91, 2026-11-01).

## 2026-11-01: audio is delayed to match the video, dynamically

User request: *"Make sure that the audio is delayed, to stay in sync with the video (dynamic as
well)."* Context: the user keeps x264's "Best quality" tune, whose lookahead adds about 0.7 s
(medium, `rc-lookahead` 40) to about 1 s (veryslow, 60 frames at 60 fps) of video latency; GPU
encode adds little. Audio had no delay, so a viewer heard sound up to a second before the
matching picture.

### What was wrong

1. **No comparable timestamps.** Both media kinds carry an 8-byte capture `pts` inside the AEAD
   (`protocol-spec.md` 6.4), but on a native sender they counted from different zeros: video from
   its destination's creation (`capture.rs` `DestinationRuntime::pts_epoch`), audio from the audio
   pipeline's own running time. A viewer could not tell which packet belongs to which frame.
   (The browser sender already stamped both from one `MediaClock`.)
2. **Independent playout.** Native: the video appsink is `sync=false` (frames show when decoded,
   no pacing); audio played at `now + 100 ms` from its own re-based timeline. Browser: video and
   audio each anchored themselves on their own first packet. Nothing connected the two.

### Design

**One sender clock.** Every wire PTS a process sends is *capture time* in microseconds since one
process-wide epoch, `decode::sender_clock_epoch()` (`decode/avsync.rs`). Video: that epoch is now
the `pts_epoch` of every `DestinationRuntime` (one line in `capture.rs::spawn_destination`).
Audio: `audio.rs` maps its pipeline running time onto the same axis with an offset read at
pipeline start (`sender_clock_offset_us`, `audio_wire_pts_us`). No wire-format change: still a
`u64` of microseconds; only the meaning is now "same clock for all streams of one sender".
Spec note: `protocol-spec.md` 6.4.

**Viewer: match latencies, not clocks.** The viewer does not know the sender clock's offset to
its own and does not need it. Per stream it tracks a *latency floor*, the windowed minimum of
`arrival - pts` (3 s window). The unknown offset is identical in both floors:

```
delta       = floor(video) - floor(audio)           // how much later than audio the picture arrives
extra delay = clamp(delta - AUDIO_LEAD (100 ms), 0, 2 s)
```

The video floor includes the encoder lookahead because `pts` is the *capture* time, not the
encode-output time. That is the only way the viewer can see a sender-side delay.

**Native** (`decode/avsync.rs`, wired in `decode.rs`): `StreamDecoder::feed` and
`AudioDecoder::feed` find each other through `AvSync::for_peer(conn_id)`, a registry of `Weak`
handles keyed by the sender's `ConnId`; `session.rs` needed no change. `AudioDecoder::feed`
sends the current target with each packet; the audio thread smooths it with a `DelayController`
and applies it in `AudioTimeline`: the delay is part of the lead (`AUDIO_TARGET_LEAD_US + delay`),
and a change of the applied delay re-bases the timeline once.

**Browser** (`docs/js/core/avsync.js`, wired in `video-rx.js`, `audio.js`, `room-client.js`):
same maths. The video side reports when its jitter buffer will release each frame
(`JitterBuffer.lastDecodeAtMs`), so the video's own jitter buffering is matched too. The audio
receiver re-anchors its WebAudio schedule when the applied delay changes; on a decrease it stops
queued sources that would overlap.

### Behaviour, in one table

| Situation | Audio delay |
| --- | --- |
| Only audio watched (Listen, or no video decoder for that sender) | none (0) |
| Video + audio, healthy | `delta - 100 ms`, in `[0, 2 s]` |
| Video latency rises (settings change, encoder rebuild, lookahead) | follows within about 3 s (window), one step |
| Video latency falls | follows at once (a minimum), one step |
| Estimate wobbles by < 60 ms | ignored (deadband) |
| Two steps close together | rate-limited to one per 500 ms |
| Video stalls or the screen is idle (no frames) | **last delay is held**: no information is not a reason to move audio |
| Stalled-capture keepalive frames (the sender re-sends a cached IDR, `stalled` flag) | ignored for the floor: they carry no encoder latency |
| Video watch ends | back to 0 |
| `delta` outside [-1 s, +3 s] | treated as "not one clock" (older sender), no delay |
| Latency above 2 s | capped at 2 s: audio then leads the picture |

### Why

- **Why delay audio instead of speeding video up?** Video is already shown as soon as it is
  decoded (latency is the project's goal); the only knob that does not cost latency elsewhere is
  holding the cheap stream, audio, back. The price is that audio latency equals video latency.
- **Why a minimum filter?** The floor is "the path with nothing queued"; jitter only adds. A mean
  or median would wobble with network noise and turn into audible re-timing.
- **Why steps with a deadband instead of a smooth slew?** `opusdec` (a `GstAudioDecoder`) ignores
  input timestamp shifts below its 40 ms tolerance anyway, so sub-40 ms nudges would do nothing,
  and a continuous time-stretch needs an extra element (`pitch`/`scaletempo`) that is not
  guaranteed to be installed. A re-time is rare (a settings change), so one clean step (a short
  silence when the delay grows, a short skip when it shrinks) beats constant processing.
- **Why a process-wide epoch and not the destination epoch?** Audio and video are separate
  captures with separate destinations; the only thing they share is the process.
- **Why match the first 100 ms lead?** The audio path's own playout lead (jitter headroom) is
  already a delay audio has and video does not; adding the full `delta` on top would overshoot.

### Known limits and risks

- **Real audio playback was not tested** (hard project rule: no audio tests, never create a
  PipeWire audio stream, never touch the user's Spotify; see
  `claude-instructions/testing-and-verification.md`). The estimator, controller and timeline are
  covered by unit tests with synthetic timestamps (`avsync::tests`, `decode::tests::timeline_*`,
  `audio::tests::audio_wire_pts_*`, `docs/js/core/avsync.test.mjs`). How the real sink behaves at
  a re-time step (silence on increase, drop on decrease) is reasoned from GStreamer's
  `audiobasesink`/`basesink` behaviour, not measured.
- **Startup step.** Audio starts with delay 0 and steps once the first video samples exist
  (typically within a frame of the first audio packet, since the video's pre-keyframe frames
  count). If the first audio packet beats the first video frame, the first second can contain one
  re-time. Closing this fully needs `session.rs` to tell the audio decoder "this is paired with
  video" at creation.
- **Older senders** (before this change) stamp audio on its own epoch. The plausibility window
  rejects most of that, but if audio was started up to a second *before* the share the delay can
  be off by up to that much. A sender that signals "shared clock" (an announce flag) would remove
  the guess; not added, it would touch the protocol crate.
- **Video decode and sink latency are assumed equal** (VA decode is a few ms; the audio sink adds
  its own buffer, not queried). Expect a residual of a few tens of ms.
- **Video floor uses arrival, not presentation, on native** (the renderer pulls frames its own
  way). Browser uses the jitter buffer's release time.
- **Capturing sources that stamp unusual PTS** are unaffected: only `buffer pts + offset` reaches
  the wire, as before.
