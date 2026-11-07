# Video: viewer recovery

How the native viewer notices a stream that stopped decoding well and escalates (L1 keyframe request, L2 decoder reset, L3 reconnect). Part of the video pipeline hub, `superdoc/features/video-pipeline.md`.

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

- `fragment::Reassembler` (`ingest(now, header, payload) -> Vec<Completed>` and `poll`) reports the incomplete frames it
  evicted or aged out (`FrameLoss {frames, keyframe}`, riding on `Completed.loss`), delivered with the *next* frame of that
  stream as `DecryptedMediaFrame::loss`. (`insert_with_loss` is a test-only helper.) Only *partially received* frames show up: a frame whose every fragment was
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
  The sharer now squeezes and rate-paces such IDRs and skips their repeats: `features/stream/capture-restart.md`
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
