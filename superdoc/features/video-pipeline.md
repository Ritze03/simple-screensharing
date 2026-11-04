# Video Pipeline: native decode and GPU display

> **Status (2026-10-14).** The client is one native egui binary. This page covers
> **"Native decode"** (access units to NV12 frames), **"Native display"** (NV12 frames to the
> screen via wgpu), a short **"Capture side"** pointer list, and **"Audio follows video"**
> (current). The **History** section at the end condenses the removed Electron-era display path
> (HTTP relay in `video_server.rs`, WebCodecs in the Electron renderer, `tile-video.js` jitter
> buffer); the long form is in git, `git log -- superdoc/features/video-pipeline.md`. The browser
> client (`docs/`) has its own WebCodecs pipeline (current), described in
> `superdoc/features/website.md`.

Where each side lives:

- **Encode / capture / fan-out:** `superdoc/features/stream-control.md` (see "Capture side" below).
- **Decode + display for a watcher:** this page, `client/src/decode.rs` + `decode/`.
- **Pop-out windows:** `superdoc/features/viewer-window.md`.
- **Wire format between peers:** `superdoc/architecture/protocol-spec.md`.

## Native decode (`client/src/decode.rs` + `decode/{codec,video,frame}.rs`)

**Added 2026-10-14 (I3).** Replaces the old `StreamDecoder` that could only do software
`avdec_h264` into an `egui::ColorImage`. Why it was rewritten: the project requirement is
>= 120 fps end to end (target 240), and the sender already encoded H.265 and AV1 while the Rust
decoder could only do software H.264.

**Input:** `StreamDecoder::feed(&DecryptedMediaFrame)` with the access unit `net.rs` already
reassembled and decrypted (no re-decryption here).

**Codec sniffing (`codec::sniff`).** The wire carries no codec tag (decision D5, `protocol-spec.md`
§6.4). The decoder drops access units until a keyframe AU that carries parameter sets, then
looks at the bytes: the first byte separates Annex-B (H.264/H.265, start codes) from an AV1 OBU
stream, and H.264 vs H.265 parameter-set NAL headers are disjoint. A later keyframe that
sniffs as a different codec rebuilds the pipeline. `Why not a tag:` it would change the protocol
and the relay, and a tag goes stale when a sender edits a destination to another codec; the
bitstream cannot.

**Pipeline** (one decoder thread owns it, `decode/video.rs`):

```text
HW: appsrc ! {h264,h265,av1}parse ! va{h264,h265,av1}dec ! video/x-raw,format=NV12 ! appsink(sync=false)
SW: appsrc ! {h264,h265,av1}parse ! {avdec_h264,avdec_h265,av1dec} ! videoconvert ! video/x-raw,format=NV12 ! appsink(sync=false)
```

- **Software fallback** (dav1d is not installed, hence `av1dec`) kicks in on element
  build/link failure, a bus error, or no decoded frame within 2 s of the first push
  (`HW_PROBATION`). It **replays the pushed access units** into the new pipeline so the stream
  does not wait for the next keyframe, and the choice is **remembered per codec** so a rebuild
  does not retry the broken hardware path every time.
- **No leaky queue; the decoder thread owns its backlog.** Above 64 AUs (`BACKLOG_MAX_AUS`) the
  backlog jumps to the newest keyframe. `Why:` the old leaky `queue` dropped the *oldest* AU,
  which on a freshly built pipeline is the keyframe it needs, so the decode never started while
  VA was still initialising. The thread can see keyframe boundaries; a queue cannot.
- **NV12 out, no CPU colour conversion on the HW path:** VA decoders produce NV12 and the GPU
  renderer converts YUV to RGB in a shader. The old `videoconvert ! RGBA` stage capped
  throughput far below the 120-240 fps target.
- **No pacing in the decoder.** The appsink is `sync=false` and frames are published the moment
  they come out; pacing belongs where the display refresh is known (the presenter).
  `PacingState` stays as a `pub` building block for that.

**A keyframe is a freeze, not a latency step (row 129, 2026-11-04).** The viewer cannot show a keyframe's picture before its last fragment arrives and the presenter is latest-only, so the sharer's keyframe pacing window shows up 1:1 as a freeze of the picture (decoder-output gap = pump hold + ~13 ms IDR decode; relay, upload and the rest measured at 1-2 ms on loopback). Numbers and the fix (adaptive pacing rate): `features/stream-control.md` "The lagspike on every keyframe".

**Output:** zero-copy `Arc<DecodedFrame>` (Y and UV slices of the mapped buffer, strides,
`pts_us`, `YuvMatrix`, `full_range`) published into a latest-only slot. Consumers hold a
cloneable `FrameSource`: poll `frame_counter()` (one atomic load), fetch with
`latest_if_newer(last_seen)`, call `note_presented()` when a frame is actually shown, read
`stats()`. Ingestion never waits for a consumer and never triggers a UI repaint.

**Stats.** *Decoded* fps is counted by a pad probe on the decoder output; *presented* fps is
counted by `note_presented()`. They are separate numbers. The old `current_fps` only counted
inside `latest_frame()`, so with nothing polling it the counter read 0 (see the 2026-07-29
bitrate section below). `StreamDecoder::decoded_fps()` / `presented_fps()` are the new
accessors; `current_fps()` is kept for the old UI. `received_bitrate_kbps()` is unchanged: it
counts compressed bytes in `feed()`.

**Visible size, not coded size (task #40, 2026-10-14).** `DecodedFrame::width/height` are the
visible picture. Order of preference (`frame::visible_rect`): the decoder's `GstVideoCropMeta`
if the buffer has one; else the size the sender announced in `StreamInfo` (set with
`StreamDecoder::set_display_size`, which `RoomSession::toggle_watch` calls), when the frame is
less than 64 px larger in each direction. The planes start at the crop origin, so `y()`/`uv()`
and the strides stay valid and the GPU renderer samples only the visible area.
`Why the announced size is needed:` `vaav1enc` on this machine (AMD VCN4, RX 7900 XTX) encodes
1920x1080 as 1920x1082: the AV1 sequence header says `max_frame_height = 1082`, the frame header
uses it and has `render_and_frame_size_different = 0`, so `vaav1enc`'s own caps say 1080 but the
decoder (`vaav1dec`) outputs 1920x1082 with no crop meta. Nothing in the bitstream names the
two padding lines (a known VCN4 hardware limitation: AV1 block alignment 64x16; also Mesa issue
9185). 720p, 1088 and 1440p come out right; `vah264dec`/`vah265dec` already give 1080.
Decoded padding lines are black. Not fixable at the encoder from `capture.rs` short of
rewriting the OBU headers.

**Legacy adapter.** `latest_frame() -> egui::ColorImage` still exists (CPU NV12 to RGBA at poll
time) from before the GPU renderer; the UI no longer uses it (display goes through `VideoView`).

**Benchmark** (`client/examples/decode_bench.rs`, 1080p, decode throughput):

| codec | HW (fps) | SW (fps) |
|-------|---------:|---------:|
| H.264 | 541      | 250      |
| H.265 | 488      | 166      |
| AV1   | 463      | 110      |

Every codec clears the 240 fps target in hardware; in software only H.264 does, which is why
the hardware path is first and the fallback is a safety net, not a plan.

### Decoder falling behind: slow motion, then fast-forward (2026-11-03, #133)

**User report (live, 2026-11-03):** "ale is streaming and his screenshare is constantly going
faster and slower again", a *smooth* slowdown and speedup, ale shares no audio.

**Mechanism (code):** the native viewer has no playout pacing (above: frames are shown the moment
they are decoded; `PacingState` is not wired; the two Settings sliders for it, `max_jitter_buffer_ms` /
`catch_up_speed_percent`, were read by nothing native and were removed, #125, 2026-11-04). So when the decoder
is slower than the stream nothing is dropped at first: access units queue in the pipeline
thread's backlog (`decode/video.rs` `run`, up to `BACKLOG_MAX_AUS` = 64, 0.7 s at 90 fps), the
picture runs late more and more (slow motion at decoder speed / stream rate), and when the decoder
recovers the backlog plays out at full decoder speed (fast-forward). At the cap `trim_backlog`
jumps to the newest keyframe or drops the oldest deltas.

**Measured on the user's machine** (read-only, from `/proc` of the running client, which logs to
`/dev/null`; build `cd91ebc`, so no log was available). Decoder output is a proxy: wakeups of the
`video-repaint` thread, one per decoded frame.

- Output alternated between ~90 fps and **35-45 fps** in stretches of 1-15 s and more, most of
  23:01-23:05 and again 23:16:40-23:19:34, rarely in between. Leaving a slow stretch showed
  110-190 fps for up to a second, the backlog draining.
- In slow stretches the decoder's streaming thread was ~90 % in `drm_syncobj_array_wait_timeout`
  (GPU fence waits), ~8 % runnable, and the pipeline thread spun on a full `appsrc`. So it was
  waiting on the GPU, not short of CPU. The bytes received stayed near 820 kB/s (1 s resolution,
  all room traffic); the arriving *frame* rate was not measured.
- A few windows at 73-77 fps had the decoder idle ~60 % of the time: there less was arriving, which
  this mechanism does not explain.

**Cause of the slow stretches: not established.** Why a fence wait took ~22 ms per frame is open.
The leading suspect is the video engine, shared with the user's own share: the saved defaults are
AV1, Quality, 120 fps (the running share's settings and ale's codec and resolution were not read),
and on Navi31 (VCN 4.0) the kernel moves every AV1 encode and decode job to instance 0
(`drivers/gpu/drm/amd/amdgpu/vcn_v4_0.c`, `vcn_v4_0_limit_sched`; H.264/H.265/VP9 may use either).
Quality is AV1's half-speed band (~152 fps raw, `stream-control.md` "`target-usage` is a bitfield
on Mesa AMD"), so 120 fps would be ~80 % of that instance.

- For: the encoder thread was ~95 % in GPU waits in the same stretches, and slow stretches happened
  with no other process on the video engine.
- Against: `vcn_busy_percent` was often only 55-60 % in slow stretches (whether that averages both
  instances is unknown); a test AV1 decode next to an in-process 120 fps AV1 encode still reached
  ~228 fps (1080p; ~192 fps at 4K), far above 90; and a rig share of AV1 240 fps Quality on the same
  GPU from 23:19:46 did **not** slow the user's decoder (it held ~88-90 fps).
- Confound: the 23:16:40-23:19:34 stretch overlapped this investigation's own release builds, and
  one 44 fps window at ~23:12 followed its GPU load tests, so neither is a clean natural sample.

Ruled out by the code: a playout or catch-up controller and clock drift (the native viewer has
neither), A/V alignment (ale shares no audio). NACK hold-and-release and recovery loops were **not**
ruled out by measurement, the client's log being unavailable.

**What changed (#133): it is now in the log.** `decode/video.rs` `LagWatch` watches the backlog
(PTS span of the queued access units) and logs a WARN when the decoder was >= 250 ms behind for
>= 0.5 s or had to drop access units, at most once per 30 s, folding episodes in between, and
reports an episode still running after 5 s: `video decoder fell behind the stream N time(s) since
the last report (longest X s): up to Y ms late, Z access units dropped, decoder vaav1dec`.
Exercised in the rig with the viewer forced to software AV1 (`LIBVA_DRIVER_NAME` pointed at a
missing driver, private `GST_REGISTRY`): one line at start-up, then `19 time(s) ... 188 access
units dropped, decoder av1dec` 30 s later. That proves the line, not the hardware case.
`Why a log line and not a policy change:` the user's own log showed nothing of this, so the evidence
existed only while someone watched `/proc`. A decoder that cannot keep up can only choose between
slow motion (now), freezing until the next keyframe, or corrupt pictures; none is a fix, and the
cause has to be known first. The line deliberately names no cause.

**Next test (untested suggestion):** during a slow stretch stop the own share, or switch it to
Balanced or H.265, and see whether the viewer recovers. That separates the video-engine suspect from
the rest.

### Timing report (2026-11-04, row 133)

`decode/timing.rs`: one `Timeline` per stream records each frame at reassembly (`fragment::FrameArrival`: first fragment,
complete, released), at the decoder thread, at the decoder's output and when it is uploaded for a repaint
(`FrameSource::note_shown`), matched by PTS, one mutex per stage. Every 10 s two info lines `video timing stream N: ...`:

- `span_ms` first to last fragment; `net_late_ms` arrival against the sender's clock, above the window's floor; `arrive_gap_ms`;
  `queue_ms` from release to the decoder thread; `decode_ms`; `out_gap_ms`; `shown_gap_ms`.
- **`late_ms`, the stutter number:** shown time minus PTS above the window's floor. `Why:` a gap between shown frames proves
  nothing on a damage-driven source; a frame late against the sender's clock is a hitch. `late_frames key=N other=M` counts clearly
  late frames at keyframes and elsewhere, which says at once whether the hitches belong to keyframes.
- `held`, `held_worst_ms`, `repaired` (NACK), `undecoded`, `unshown` (replaced before a repaint took it).
- `worst late_ms=X:` the frames around the worst moment, `pts`, `K` for a keyframe, size and `complete/decoded/shown` in ms.

"Shown" is the upload in the paint callback, not the moment of scan-out: time waiting for the swapchain shows up in the next
frame's figure, the compositor's own delay does not.

## Viewer recovery (`client/src/decode/recovery.rs`, 2026-11-02, #101)

**The problem.** The wire is lossy and has no retransmission, so every codec/fps transition and every
stretch of packet loss needs one *intact keyframe*, and until now the native viewer could not ask for
one. After a codec switch whose IDR was lost it fed the new codec's deltas into the old decoder (`RXSTAT
fed=36 kf=0 decoded=0` for 8 s); after ordinary loss it showed a corrupt picture until the sender's next
scheduled IDR (2 s GOP); a dead sharer capture was invisible (it resends the cached IDR at 2 fps, so the
tile stayed "alive"); a black-holed path froze the tile until the QUIC idle timeout. User report
(2026-11-02, real use over the internet): "switching frame rates, audio or codec can still make the share
hang for the viewer, who has to close it and view it again" and "screen shares just stall out of nowhere".
The browser already recovered (`docs/js/core/video-rx.js` `markLoss`/`needKeyframe`,
`room-client.js` `RESUBSCRIBE_MIN_INTERVAL_MS`); this is the native counterpart, tuned differently where
the rig showed the browser's rule hurts.

**The only primitive** a viewer has is Unsubscribe + Subscribe: the relay sends `KeyframeRequest` to the
owner only for a *newly added* subscriber (spec §9), a plain re-Subscribe does nothing. Measured: first
picture ~1.1-1.4 s after the request. It makes the sharer's branch see `SubscriberCount` 0 then 1 when
the viewer is the only one (a valve pause/resume), and one IDR goes to everybody watching.

**Pieces.**

- `fragment::Reassembler::insert_with_loss` reports the incomplete frames it evicted or aged out
  (`FrameLoss {frames, keyframe}`), delivered with the *next* frame of that stream as
  `DecryptedMediaFrame::loss`. Only *partially received* frames show up: a frame whose every fragment was
  lost leaves no trace (the `frame_id` counter is shared by all streams of the connection and the previews,
  so gaps in it are not evidence), the same blind spot as the browser.
- `decode/recovery.rs` is a pure state machine (`Recovery`) with injected `Instant`s, owned by
  `StreamDecoder` (`feed` -> `on_frame`/`on_fed`/`on_ingest_drop`, `tick(now)` -> `Option<RecoveryAction>`,
  `view_state(now)`). Unit tests: `decode/recovery_tests.rs`.
- Driver: `session::run_recovery_tick`, called every 100 ms from `spawn_media_ingestion_task`; it turns
  `RequestKeyframe`/`ResetDecoder` into `unsubscribe`+`subscribe` (skipping the subscribe if the user stopped
  watching meanwhile) and `Reconnect` into a synthetic `ClientEvent::ControlClosed`, which the existing
  `drain_events` -> `suspend_room` -> `restore_after_reconnect` path handles like a dropped connection.
  `SSS_DISABLE_VIEWER_RECOVERY=1` turns the driver off (A/B measurements).
- `PipelineMsg::Reset` / `Worker::reset` (`decode/video.rs`): tear the pipeline down; the next keyframe
  builds a fresh one.

**Levels and thresholds (each with its why; constants are in `recovery.rs`).**

| Level | Trigger | Action |
|---|---|---|
| L1 | a keyframe arrived incomplete or was dropped at the ingest channel; a burst of >= 3 incomplete delta frames within 1 s, early enough in the GOP for the request to beat the scheduled IDR; an arrival gap >= 0.5 s on a flowing stream (>= 1 s: always); fed-but-nothing-decoded >= 1 s; the decoder thread wants a keyframe | Unsubscribe+Subscribe |
| L2 | two silent L1 requests in one episode, or fed-but-nothing-decoded >= 3 s although keyframes arrive | reset the decoder pipeline (once per episode, >= 5 s apart) + L1 request |
| L3 | a stream that was flowing (>= 10 frames, mean gap < 0.5 s, not flagged stalled) delivers nothing for 8 s *and* no other watched stream of the room did | reconnect the room now |

- **A requested IDR can itself be the problem** (field case 2026-11-03, #102): a VA encoder under motion emits
  IDRs of ~1 s of bitrate (~1000 fragments, almost never intact), each failure triggers the next request.
  The sharer now squeezes and rate-paces such IDRs and skips their repeats: `features/stream-control.md`
  "Big keyframes".
- **Min 1.5 s between requests** (browser parity): a request cannot be answered faster than ~1.2 s, so
  asking sooner only multiplies keyframes. Silent failures back off 1.5 / 3 / 6 / 10 s so a dead sharer or
  path is not hammered. A request answered by an *incomplete* IDR is proof of life and restarts the
  backoff (a ~100-fragment IDR is intact only ~37 % of the time at 1 % loss).
- **Delta loss asks only for a burst (>= 3 incomplete frames within 1 s) and only if the request beats the scheduled IDR**: a single lost delta is a local artifact and asking for each one only multiplied keyframes in the A/B runs; a request costs ~1.3 s, the observed keyframe
  interval is ~2 s, so a delta lost 1.5 s into the GOP is healed sooner by waiting. These asks use a
  backoff that is *not* reset by each recovery (only by 10 s without evidence): the first rig run, which
  asked at the maximum rate, became a permanent keyframe storm on a link that keeps losing frames (the
  requested IDRs, several times a delta's size, are what gets lost next).
- **No holding of the last frame on loss evidence alone** (differs from the browser). Deltas are dropped
  and the last good picture stays only while the decoder has no valid state: before the first keyframe,
  after a reset, after the pipeline thread reported a failure or saw the other bitstream family (Annex-B vs
  OBU, i.e. a codec switch to/from AV1 whose IDR was lost: `codec::family_matches`), and while fed AUs produce
  nothing. The first design gated on every lost IDR and the rig froze for 5-47 s per transition, because an
  intact IDR can take several 1.5 s attempts; a moving, slightly corrupt picture that heals is the better
  failure. H.264 vs H.265 deltas cannot be told apart, so that switch relies on the fed-but-nothing-decoded rule
  (HW decoders produce nothing from the wrong codec).
- **Reset waits for the keyframe to rebuild** (`Worker::reset`): the hardware probation (`HW_PROBATION`, 2 s)
  runs from pipeline creation, so building at reset time demoted a healthy VA decoder to software for "no
  frame" while the keyframe was still in flight. A HW decoder is demoted only when it was reset after it
  *had* been fed a keyframe and produced nothing across two resets.
- **L3 is conservative on purpose**: a reconnect also rebuilds our own shares in that room, so it needs a
  stream that was demonstrably flowing (a sparse damage-driven stream, a stalled sharer, a fresh watch never
  count) and silence of every watched stream, not just one. A decoder that stays stuck after L2 does *not*
  escalate to a reconnect (a deliberate deviation from "L2 failed 10 s -> L3": reconnecting cannot fix a local
  decoder and would interrupt the user's own shares); it repeats L1/L2 at the 10 s cap.
- **Idle sharers**: the 8 s silence rule assumes a sharer sends >= 2 fps (Linux capture `keepalive-time`,
  browser keepalive, the stalled 2 fps resend). A Windows `d3d11screencapturesrc` share of a static desktop
  is the one case that was not verified to heartbeat.

**UI** (`ui/room.rs`, `ui/popout.rs`): `StreamDecoder::view_state` is `Healthy`, `Recovering` (an episode older
than 1.5 s, or 3 s of silence on a flowing stream) or `SharerStalled` (the advisory media-header flag, only
displayed, never acted on). The tile shows an always-visible centred chip ("Recovering…" / "Sharer's capture
stalled", amber dot) and the hover statbar's dot turns amber; the pop-out shows the same chip and wording.
Keys `video.recovering`, `video.sharer_stalled`.

**Rig results** (headless rig, a UDP impairment proxy between each client and the relay, H.265 60 fps 6 Mbit to
start, +20 ms each way, 12 random live fps/codec edits 12 s apart; "freeze" = the `grim` crop of the viewer tile
did not change for 0.8 s, so it counts corrupt-but-moving as moving; A/B = the same binary with and without
`SSS_DISABLE_VIEWER_RECOVERY=1`, same seed; evidence in the task scratchpad `evidence-101/`):

| link / transport | recovery | total freeze | freezes | longest | transitions >= 4 s | L1 requests |
|---|---|---|---|---|---|---|
| 1 % path loss, Cubic (before #86/#100) | on | 62.6 s | 21 | 7.7 s | 4 of 12 | 85 (max-rate asking) |
| same | off | 43.2 s | 18 | 5.7 s | 3 of 12 | - |
| 0.5 % path loss, Cubic | on / off | 35.6 s / 34.2 s | 20 / 16 | 3.9 s / 3.8 s | 0 / 0 | 38 |
| 1 % path loss, BBR + IDR repeats/pacing (#86/#100) | on | 18.2 s | 10 | 1.9 s | 0 of 12 | 23 |
| same | off | 27.3 s | 14 | 3.4 s | 0 of 12 | - |
| 0.5 % path loss, BBR + IDR repeats/pacing | on / off | 24.8 s / 30.4 s | 14 / 18 | 2.4 s / 2.2 s | 0 / 0 | 22 |

What the numbers say: (1) **under Cubic recovery changed nothing measurable** (and the first, gating design was
much worse, see above), because the keyframes it asked for were themselves being destroyed: the new log line
`a keyframe never completed (N/M fragments arrived)` showed IDRs of only 6-19 fragments missing 15-30 % of their
fragments (random loss would miss <0.2): the congestion controller, reacting to random loss, paced/queued the IDR
burst and the datagram send buffer dropped part of it. (2) **With BBR and the IDR repeats the remaining lost IDRs
miss exactly one fragment** (`17/18, 7/8, 10/11, 25/26 ...`, i.e. plain random loss), a request is usually answered
(median ~1.2 s, 1.5 s minimum between requests, measured min gap 1.5 s), and recovery now helps (-33 % total
freeze at 1 %, longest 3.4 -> 1.9 s; at 0.5 % -17 %, within noise of one run each). (3) Clean link, 5 min H.265
144 fps 6 Mbit: **0** requests, 0 incomplete keyframes, 0 freezes. (4) 5 s blackout of the viewer's leg: the picture
is back ~2.0 s after the path returns with or without recovery (that is QUIC probe backoff, a transport matter).
(5) 10 s black hole: L3 fired **8.05 s** after the path died (`recovery L3`), the room reconnected and the tile
was moving 1.4 s after the path returned; without L3 the viewer waits for the QUIC idle timeout (20 s after #86).
(6) The sharer's branch sees the unsubscribe+subscribe as `0 subscribers` / `has a subscriber again` 60 µs apart:
no measurable stall; with a second viewer the count never reaches 0 (relay `Subscribe`: `newly_added` still
sends the `KeyframeRequest`), and the requested IDR is delivered to everybody watching.

## Selective retransmission (NACK, #104, 2026-11-03)

**The problem it fixes.** Under #101 a frame missing any fragment is thrown away and the only cure is
an L1 keyframe request (~1.2-1.5 s, and the requested IDR is itself ~1000 fragments that die the same
way). Field case 2026-11-02: 1080p120 8.3 Mbit/s H.264 on AMD, IDRs of ~1 MB, at 0.1 % loss an IDR arrives
whole 37 % of the time, `a keyframe never completed (981/984 fragments arrived)` 295 times, recoveries of
30-100 s. #102 made IDRs smaller and rarer; the encoder's natural GOP IDRs are still big. Paths are short
(16-30 ms), so re-sending the three missing fragments beats a new keyframe by an order of magnitude.

**Design, with the why of every choice: `architecture/protocol-spec.md` section 6.6.** In one paragraph:
NACKs are < 19 byte datagrams (every older relay, client and browser drops them as undersized, so
nothing can break; a control-stream variant would have killed old connections), a Hello/HelloAck
datagram handshake with the relay switches the viewer's NACK mode on (so a new client on an old relay
behaves exactly as before), the relay answers from a 2 s / 8 MiB per-stream cache of the datagrams it
forwarded and forwards only what it lacks to the sender, whose 2 s ring re-sends it with the retransmit flag
(media `flags` bit2; the relay delivers those only to the viewers that asked). Retransmits are the original
datagrams, byte for byte: nothing is re-encrypted, no nonce is used twice. Audio is not repaired.

**Viewer side (`fragment::Reassembler`, NACK mode; driven by `net.rs::datagram_reader_loop`).**

- *Detect*: a missing fragment with >= 2 later fragments of its frame behind it, or any missing fragment
  once the frame has been quiet 20 ms (a lost tail). *Ask*: one Request per contiguous run; a fragment is
  asked again after `R = clamp(2 x rtt + 60 ms, 100, 300 ms)`, at most 3 times; a frame with > 128 suspect
  fragments is abandoned without asking (congestion, not loss).
- *Order*: while a frame is being repaired, later *complete* frames of that stream are held and released in
  order once it completes or is abandoned. Delivering N+1..N+5 and then N would corrupt the picture
  (and a late IDR after its deltas is worse than a short delay). The cost is up to ~4R (440 ms at 25 ms RTT)
  of added latency on the stream, only around a frame that cannot be repaired; a repaired frame costs
  about one RTT.
- *Give up* (rounds exhausted, `Miss`, 1 s hard limit): the frame is discarded and reported as `FrameLoss`
  exactly as before, so the L1 recovery above takes over unchanged. The two compose: NACK makes the
  `FrameLoss` evidence rare, L1 stays the safety net.
- *Duplicates* of delivered/abandoned frames are ignored (last 256 frame ids per stream), so a retransmit
  that races the original cannot open a ghost frame (an old reassembler would report that as loss).
- `SSS_DISABLE_NACK=1` (native) turns the whole thing off for that client: no Hello, no NACKs, no re-sends
  (A/B measurements, field rollback). The relay's own switch is `SSSHARE_NACK=0`.
- Logs: viewer and sharer log `NACK activity in the last 5 s requests=.. fragments_requested=..
  fragments_recovered=.. frames_recovered=.. frames_abandoned=.. fragments_resent_for_others=..`, the relay
  `NACK activity in the last 10 s requests=.. from_cache=.. forwarded_to_owner=.. miss_replies=..
  owner_resends_relayed=..`.

**Rig results** (headless rig, UDP impairment proxies: random loss on **both** legs at once, sharer -> relay
and relay -> viewer; H.264 1080p120 8300 kbps Quality, a detailed scrolling pattern (IDRs 100-500 kB, median
~300 kB = 100-500 fragments), 100 s per cell; NACK off = the same binaries with `SSS_DISABLE_NACK=1` on both
clients; evidence `evidence/` in the task scratchpad):

| random loss, both legs | NACK | `never completed` | L1 requests | frozen total (longest) | fragments asked / recovered | repaired from relay cache / from sharer |
|---|---|---|---|---|---|---|
| 0.2 % | off | 38 | 42 | 1.4 s (1.4 s) | - | - |
| 0.2 % | **on** | **0** | **0** | **0 s** | 335 / 335 | 161 / 174 |
| 0.4 % | off | 31 | 37 | 4.0 s (1.7 s) | - | - |
| 0.4 % | **on** | **0** | **0** | **0 s** | 759 / 755 | 391 / 367 |
| 1 % | off | 57 | 55 | 19.7 s (5.3 s) | - | - |
| 1 % | **on** | **0** | **0** | **0 s** | 1818 / 1792 | 892 / 915 |

(Frozen = the viewer tile's pixels unchanged for 0.8 s. `frames_abandoned` was 0 in every "on" cell: every frame
that lost fragments was repaired. A first "on" run with a 60 ms floor on the retry interval had one
incident at 0.4 % (2 L1, 3.9 s frozen): a keyframe repair that had to cross the sharer's link at ~90 ms RTT
was abandoned; the floor is now 100 ms and the rerun was clean. After merging master a 60 s run at 1 % was clean
too: 0 / 0 / 0 s, 1185 asked, 1165 recovered.)

Extra bandwidth is the retransmitted fragments: at 1 % loss ~1800 fragments in 100 s = ~1.8 MB = ~0.15
Mbit/s (~2 % of the 7 Mbit/s stream, i.e. about the loss rate), plus 18-byte requests (~1800, negligible).
Of the repairs about half came from the relay cache (relay -> viewer loss) and half from the sender
(sharer -> relay loss), as expected from loss on both legs.

**Mixed versions** ("old" = the pre-#104 binaries built from `facc373`; 60 s, 1 % loss on both legs; frozen total,
`never completed`):

| viewer | relay | sharer | result |
|---|---|---|---|
| old | new | new | unchanged from old/old: 10.5 s frozen, 30 never completed; relay saw 0 requests, sent 0 duplicates |
| new | old | new | unchanged: no HelloAck, no NACK traffic at all, 24.7 s frozen, 40 never completed (the loss run varies) |
| new | new | old | relay cache repaired the viewer leg (347 fragments from cache), sharer-leg losses answered `Miss` at once (330) and fell back to L1: 1.8 s frozen, 25 never completed (vs ~10 s for old/old) |
| old | old | old | baseline: 10.7 s frozen, 34 never completed |
| **browser** new | old (`facc373`) | browser new | #111, 0.3 % both legs, 30 s, ~1 MB IDRs: no HelloAck (`nackStats().active` false on both), 0 requests, **0 fps / 246 loss events: identical to `?nack=0`** (a browser viewer never recovers at this loss without NACK); nothing got worse |
| **browser** new viewer | new | browser sharer with `?nack=0` (never says Hello) | #111, 0.5 % sharer leg: the relay answers `Miss` at once (219 fragments asked, 201 frames abandoned, 0 repaired), L1 as before, 0 fps / 190 loss events: same as NACK off |
| **browser** viewer with `?nack=0` | new | browser sharer new | #111, 0.5 % viewer leg: never says Hello, so never gets a retransmit or a duplicate: 0 fps / 199 loss events, same as NACK off |
| browser viewer new | new | native sharer (new) | #111 rig, 1 % on the browser's leg: 60 fps, 0 loss events (off: 2.7 fps, 306 loss events); relay cache repairs |
| native viewer (new) | new | browser sharer new | #111 rig, 0.5 % on the browser's leg: 0 `never completed`, ~300 fragments repaired by the browser's ring; with the browser sharer on `?nack=0`: 24 `never completed`, 23 L1 recoveries, "Recovering..." at 0 fps |

The old peers need no change and see no NACK traffic (undersized datagrams are dropped), which is the point
of the datagram design.

**The browser** (#111, 2026-11-03) now speaks it too, both as viewer and as sharer: `features/website.md` "Selective
retransmission in the browser" (design differences are mechanical: no `connection.rtt` in Chromium 153, so a fixed 150 ms
RTT; re-sends bypass the #103 pacing loop) with its own loss tables and the mixed rows above. Without NACK a browser
viewer decoded **0 fps** at 0.1 % random loss with ~1 MB IDRs (no keyframe ever completes); with it 49.4 fps and 0 loss events up
to 1 %.

**Not verified**: a real internet path
(burst loss, reordering beyond 2 fragments, RTT jitter), more than one viewer of the same stream (unit tests
only), audio.

## Native display (`client/src/decode/render.rs` + `render.wgsl`)

**Added 2026-10-14 (I9).** Replaces "HTTP relay + WebCodecs in a `<canvas>`" (see History). The
module doc comment in `render.rs` is the authority; the points below are the ones an agent needs
before touching it. The UI side (`VideoViews`, tiles, hover chrome) is in `superdoc/ui/APP-SHELL.md`.

- **NV12 stays NV12 until the shader.** `queue.write_texture` uploads straight from the decoder's
  mapped slices (`bytes_per_row = stride`): Y as `R8Unorm` (w x h), UV as `Rg8Unorm`
  (ceil(w/2) x ceil(h/2)). `render.wgsl` converts YUV to RGB; the matrix (BT.601 / 709 / 2020) and
  the limited/full range expansion are folded into three affine rows on the CPU
  (`yuv_to_rgb_rows`) and passed as a uniform, so the shader has no per-standard branches. Textures
  are recreated only on a size change.
- **Drawn with an egui paint callback** (`VideoCallback`), letterboxed (`Fit::Contain`) into the rect
  the caller gives `VideoView::show`/`paint`. `paint` resets the viewport to the full unclamped video
  rect and relies on egui's scissor for clipping, because egui clamps a callback's viewport to the
  screen and would otherwise squash a partially off-screen video.
- **Textures are per stream, shared across viewports.** One `VideoRenderer` lives in
  `callback_resources` (eframe has one wgpu device for every viewport); GPU state is keyed by the
  decoder's `FrameSource`. A stream shown in a tile *and* a pop-out is uploaded once per decoded
  frame, whichever viewport paints first. `Why:` half the upload bandwidth, and it gives the
  presenter rule for free.
- **Presenter rule:** `FrameSource::note_presented()` is called **once per frame**, when it is first
  uploaded for display, no matter how many views draw it. `Why:` no "designated presenter" to keep
  in sync as views come and go (closing a pop-out cannot stop the count), and "presented fps" then
  means distinct frames that reached a screen. A frame superseded before any repaint uploads it is
  not counted.
- **Repaint is frame-driven, not polled.** Each `VideoView` runs one waker thread (`run_waker`,
  thread name `video-repaint`) that sleeps on `FrameSource::wait_for_newer` and calls
  `ctx.request_repaint_after_for(1 us, viewport)` per new frame. `Why not request_repaint_of:`
  egui turns every zero-delay request into *two* passes "to let things settle", which at 240 fps
  doubled the paint rate (480/s measured); a non-zero delay skips that and still means "now". The
  waker exits when its view stops being shown (500 ms `HIDDEN_AFTER`) and respawns on the next
  visible show; screens never call `request_repaint()` for video.
- **Measured:** 240 fps presented on DP-1 (279.75 Hz), 1080p. Pop-outs hold 240/240 in Mailbox mode,
  which is why Mailbox is the default; see `superdoc/features/presentation-modes.md`.

## Capture side (summary; detail lives in `stream-control.md`)

The decoder in this page only sees what the capture pipeline produces. Three things changed that
a decode-side reader will meet:

- **Orientation (#35).** `pipewiresrc` turns `SPA_META_VideoTransform` into an `image-orientation`
  tag and the pipeline follows it (`video-direction=auto`), so a rotated monitor arrives upright. The
  stage is spliced in lazily on the System domain because `videoflip` costs about 0.8 ms per 1080p
  frame even when idle; in the GPU domain the source `vapostproc` is the stage. The tee answers caps
  queries with BGRx at any size, because otherwise `vapostproc` shrank the capture to 320x180. There is
  no transform table of our own. Live-verified: the portal sends rotate-180 for DP-2. See
  `capture.rs` `orientation_fragment` and `stream-control.md` "Live-verified: rotated outputs (#35)".
- **Performance (#39).** 120-240 fps of *unique* frames, PipeWire `min-buffers`, `RateCap`, and the
  GPU/System **capture domain**: `stream-control.md` "Capture/encode performance".
- **Idle monitors (#32).** The first branch waits for the source's first repaint; a silence watchdog
  moves a quiet destination to `Stalled(SourceLost)` and viewers are fed a cached IDR meanwhile:
  `stream-control.md` "Idle monitors" and "Silence watchdog".



## Audio follows video (current rule; the playback path is native)

*A/V sync (audio delayed to match video latency, 2026-11-01): `features/audio-sharing.md`.*

Product requirement, from the project owner: "the shared audio should be connected to the
screenshare ... The streamer should be able to enable audio on his share and the viewer should
automatically hear that. No (visual) separate 'stream' for Audio."

- **Audio is decoded and played in-process** (`decode.rs`'s `AudioDecoder` into GStreamer's
  `autoaudiosink`), never relayed to a display layer. *Why:* only pixels need a GPU/canvas
  destination; audio has to reach the speakers of the machine that already receives and decrypts
  the Opus packets. A parallel audio relay or muxing audio into the video route would add a second
  wire format and jitter buffer and break the audio-only sender case.
- **Pairing rule, `RoomSession::sync_audio_watches` (`client/src/session.rs`):** exactly one
  auto-subscribed audio watch per sender we hold a *video* watch on, for as long as that sender
  has an audio stream announced, and nothing else (`AudioWatch::auto`; a standalone "Listen" on an
  audio-only sender lives in the same map and is only ended by the user). *Why a continuous
  reconcile, not a pairing latched when Watch is clicked:* a sharer normally starts the screen
  first and adds audio second, so a latched pairing left every existing viewer permanently silent;
  it also tore down audio still used by a second monitor's watch, leaked on pruned watches, and
  left a dead watch when a sender stopped only audio.
- **No separate visual audio stream.** Viewer-facing lists filter to video; audio destinations
  appear only on the sharer's own outgoing side. The viewer's tile has a volume/mute control
  instead of a listen toggle (by the time a tile exists the audio is already playing).
- **Volume is backend state.** `AudioWatch` carries `volume_percent` and `muted`, resolved through
  the *video* stream id, so the tile and the pop-out show the same value. The last chosen volume is
  remembered per owner (`RoomSession::audio_volume_by_owner`, keyed by `ConnId` alone) because the
  audio `StreamId` changes every time a sender restarts or switches the shared app, which used to
  reset the viewer to 100 %. Mute is remembered the same way (`audio_muted_by_owner`, #118).
- **Tests must tell "flowing" from "not flowing".** The end-to-end audio test
  (`end_to_end_test::watching_video_automatically_delivers_the_sharers_audio_too`) drives a quiet
  (`volume=0.02`) synthetic source and asserts the sharer's capture level and the viewer's
  `AudioDecoder::packets_received()` are > 0. An earlier all-silence version passed while the
  sharer was broadcasting the microphone instead of the app (the capture-targeting bug is in
  `stream-control.md`, "The 2026-07-28 audio bugs"). Volume measurements need decorrelated noise,
  not a sine: the viewer's copy plays ~100 ms late, so a tone sums at a drifting phase.

## History: the Electron-era display path (removed)

Before the egui rework the watcher side was: `session.rs` tapped every `DecryptedMediaFrame`
into the sidecar's `video_server.rs` (a local HTTP relay, one route per watched stream, raw
Annex-B access units byte for byte, no decode), and the Electron renderer's `tile-video.js` fed
them to a WebCodecs `VideoDecoder`. All of that was deleted in the egui rework (I23). Lessons
that outlived the code:

- **Why raw H.264 relay + browser decode replaced MJPEG-over-HTTP:** the earlier path decoded
  every frame with GStreamer and re-encoded JPEG, which cost a full round trip per frame, capped
  at a ~60 fps poll, and discarded GOP structure. The egui client has no such cap
  (`ui::video` repaints at the decoder's real rate).
- **Wire format between sidecar and renderer** was `[u32 body_len][u8 type][u8 flags][u64 pts_us]
  [payload]` with a CodecInfo chunk (a WebCodecs codec string) first on every connection. The codec
  string was parsed live out of each keyframe's SPS, because it can change mid-stream when the
  encoder is re-tuned. The native equivalent is bitstream sniffing (`decode/codec.rs`,
  `superdoc/meta/TERMINOLOGY.md` "Sniffing"); the browser's is `docs/js/core/codec.js`.
- **Jitter buffer:** buffer *encoded* chunks and delay `decode()`, not decoded frames (a 1080p
  `VideoFrame` is ~3 MB). Playout delay eased toward `jitterEwma * 4 + 40 ms` (RFC 3550 EWMA, alpha
  1/16, ease step 20 ms), capped by the user's `max_jitter_buffer_ms` (default 2000, 0-5000; browser only, the native setting is gone, #125). The
  browser viewer keeps this design in `docs/js/core/jitter.js`.
- **Sustained sender-faster-than-decoder overload** drifted without bound because the
  browser decoder was still fed every chunk. A frame cannot be skipped singly (delta frames
  reference earlier ones, so a skipped P-frame raises a fatal decoder error); the safe drop is a
  contiguous run of delta chunks up to the next queued keyframe, triggered when the decoder's queue
  exceeds a small threshold (3). Relevant to any WebCodecs path.
- **A "Stream error" placeholder that never cleared** was a shared-module bug (it needed a
  hide on the next decoded frame), and a later recurrence was CSS, not logic.
- **Verification lesson:** many of the old entries were verified by driving the real shell via
  Playwright `_electron` with synthetic input. Today use the `run` skill and the headless rig (`hs-rig.sh`, `hs-drive.sh`)
  (`client/.claude/skills/run/SKILL.md`, `client/scripts/`).
