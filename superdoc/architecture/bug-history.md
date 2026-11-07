# Bug History: Found by End-to-End and Manual Testing

Split out of `implementation-notes.md` on 2026-11-07. Bugs whose root cause explains code that still exists (capture stop, frame pacing, audio start / jitter / late-join) and the QC passes that cleared two areas. The Tauri-era QC entry and the removed `--tui` front-end notes were deleted (git history: `git log -- superdoc/architecture/implementation-notes.md`).

## Bugs found via end-to-end testing

- **`CaptureHandle::stop()` used to hang forever.** It called
  `pipeline.set_state(gst::State::Null)` directly. `AppSinkStream` (what the capture pump
  loop awaits to know the pipeline is done) only ever yields its terminal `None` once the
  appsink's `is_eos()` flag is set by a real EOS event actually flowing through the pipeline
  — jumping straight to `Null` tears the elements down without ever sending that event, so
  the stream sits `Pending` forever and the pump task never completes. No unit test exercised
  a live `.stop()` call before the real end-to-end integration test
  (`client/src/end_to_end_test.rs`) awaited one on a real running
  pipeline for the first time. Fixed by sending a real EOS event first and *then* driving the
  pipeline to `Null` once the pump task observes completion. See
  `client/src/capture.rs`, `CaptureHandle::stop`.

## Bugs found via manual testing (2026-07-25)

Three bugs the user found by hand and filed in a repo-root `issues.md` (gone since; its three entries are covered here). Issue #2 (high
refresh-rate sharers slowing everyone else down) turned out to be the lock-contention
redesign covered in "Performance fixes" below; the other two are covered here, plus one
extra bug a QC pass turned up along the way.

- **Frame pacing ignored the capture-time PTS — issue #1.** Symptom: frames displayed the
  instant they finished decoding rather than at a consistent, PTS-derived cadence, so
  playback looked jittery/uneven even when the network itself was fine. Root cause: the
  8-byte authenticated PTS (see the protocol-refinements section above) was already
  threaded end-to-end through decrypt → decode, but the GStreamer appsink ran with
  `sync=false`, so `decode.rs`'s video `new_sample` callback pushed each decoded frame to
  the sink immediately — the PTS was decrypted and available but never consulted, i.e.
  pure "push as available." Fix: `decode.rs` gained `PacingState`
  (`client/src/decode.rs:352`) and a pure `PacingState::next_delay(pts_us,
  now)` function (`decode.rs:368`); the video `new_sample` callback (around `decode.rs:612-618`)
  now sleeps the computed delay before handing the frame to the sink, and re-bases —
  skipping the sleep and resetting its baseline — whenever it's more than 300ms behind, to
  bound worst-case added latency and self-heal after a stall or stream restart. Also
  bounded the previously-unbounded video ingest channel to a small fixed capacity with
  drop-newest-on-full (`StreamDecoder::feed`, `decode.rs:469`), matching the module's
  existing drop-and-continue policy elsewhere.

- **Sustained high sender fps vs. lower viewer decode/display capacity caused slow drift,
  not just jitter — follow-on to the frame-pacing fix above.** Symptom: when a sender
  captures/encodes at a higher framerate than a viewer's pipeline can sustain (e.g. a
  240fps sender against a viewer that can only decode/display 120fps), the just-fixed
  re-base-on-stall logic alone wasn't enough — playback slowly drifted out of sync with
  real elapsed time over time instead of staying locked to it. Root cause: `PacingState`'s
  re-base bounds worst-case *latency* but says nothing about *throughput* — every decoded
  frame still got displayed, just progressively later than the last, re-basing every
  ~300ms of accumulated lag forever, which reads as slow drift rather than a stall. Fix:
  `PacingState::next_delay` became `PacingState::decide`
  (`client/src/decode.rs:368`), returning a `PacingDecision` with a
  third outcome (`Skip`) alongside the existing "sleep then display"/"display now" ones.
  It tracks how many "badly behind" re-base events have fired *consecutively* (no
  on-time/ahead frame resetting the streak in between); the first one still displays
  (matching the pre-existing behavior, so an isolated stall/PTS-discontinuity doesn't lose
  a frame), but a *second* consecutive one — meaning the previous re-base didn't fix
  it — is skipped (dropped, never written to the display sink) instead, and skipping
  continues for as long as the pattern keeps recurring
  (`PACING_SKIP_AFTER_CONSECUTIVE_REBASES`, `decode.rs:279`). The video `new_sample`
  callback (`decode.rs:~689`) simply doesn't write the frame into `LatestFrameSlot` when
  told `Skip`. This hysteresis (skip only on the *second* consecutive miss, not the first)
  exists so a single momentary hiccup — a GC pause, a scheduler blip — still displays its
  frame rather than dropping one it didn't need to; only a *sustained* rate mismatch
  actually thins the displayed framerate. Extended `tests::pacing_*` with cases covering a
  burst of consecutive badly-behind frames (`pacing_skips_after_consecutive_rebases`,
  `pacing_stops_skipping_once_back_on_pace`,
  `pacing_skips_some_frames_of_a_sustained_backlog_burst`).

- **The consecutive-rebase skip trigger above never actually engaged for the scenario it
  was built for — found by QC, follow-on to the two pacing fixes above.** Symptom: fed
  2000 synthetic frames at a realistic 240fps-sender/120fps-viewer differential
  (`dt_pts≈4.17ms`, `dt_wall≈8.33ms`), `PacingState::decide` produced `skipped == 0` —
  the exact bug the previous fix was meant to close. Root cause: "consecutive" counted
  *individually* over-threshold single steps, but a gradual per-frame differential never
  crosses 300ms in one step — it takes ~72 frames of ~4ms drift each to accumulate past
  the threshold, and every one of those 71 intermediate "slightly behind" frames reset the
  streak to 0, so it never reached 2. A secondary bug in the same design: on a genuinely
  one-step stall exceeding 300ms, the "badly behind" condition held on every following
  frame too (no on-time/ahead frame occurs under a sustained bottleneck to reset it), so
  once triggered it skipped essentially everything with no recovery — not a proportional
  rate. Fix: replaced the `consecutive_behind` counter entirely with a small accumulator,
  `PacingState::pts_budget` (`decode.rs`, `PacingState` struct) — every call adds this
  frame's own `dt_pts` to the budget and every *displayed* frame spends `dt_wall` back out
  of it (skip leaves it untouched, carrying the deficit into the next frame's accounting
  rather than resetting to zero). This is the standard accumulator/DDA technique for
  frame-rate down-conversion: it has the right asymptotic behavior for free — a sustained
  `dt_wall/dt_pts` mismatch converges to skipping almost exactly `1 - dt_pts/dt_wall` of
  frames (verified empirically at ~50% for the 240-vs-120 case and ~66.7% for a
  300-vs-100 case, each within a generous but bug-catching tolerance band, not hardcoded),
  and it self-corrects within a frame or two once the mismatch resolves, since nothing
  about it depends on how long the mismatch has been going on. The old 300ms
  `PACING_MAX_BEHIND` tolerance-before-rebase concept no longer applies (the budget
  mechanism doesn't need a discrete "how far behind" gate at all); it's replaced by
  `PACING_HARD_STALL_GAP` (2s, chosen to be well outside any realistic sustained mismatch
  and in the same ballpark as the stream's own worst-case 2s GOP length), which now
  guards only genuinely pathological single-step gaps (system suspend/resume, a debugger
  pause) — those still hard-reset the budget and display immediately rather than spending
  many skipped frames paying a multi-second backlog down. Rewrote `tests::pacing_*`
  accordingly, most notably `pacing_settles_near_theoretical_skip_ratio_for_240fps_over_120fps_viewer`
  (QC's exact repro, now asserting `skipped` lands in a 40-60% band rather than 0) and
  `pacing_settles_near_theoretical_skip_ratio_for_300fps_over_100fps_viewer` (a second,
  differently-proportioned ratio, so the result isn't an artifact tuned to the 2x case);
  also `pacing_skips_isolated_moderate_stall_then_recovers` and
  `pacing_hard_resets_after_pathological_stall_gap` for the two ends of the "isolated
  hiccup vs. pathological stall" spectrum this design distinguishes.

- **Audio silently failed to start most of the time — issue #3, part 1.** Symptom: sharing
  audio frequently produced a share error instead of working audio. Root cause (the share used a `pw-loopback` then; since #121 it is a `pw-cli` null sink, `audio.rs`): PipeWire
announces a loopback node and its ports as separate registry events, a beat apart;
  `audio.rs`'s setup polled only for the *node* before proceeding, so
  `start_audio_sharing` (`client/src/audio.rs:429`) intermittently raced
  ahead of the port registration and `bail!`ed because the fresh loopback's ports didn't
  exist yet. Fix: merged the node-wait and port-wait into one combined poll,
  `wait_for_loopback_ready` (`audio.rs:312`), that only returns once both the node and its
  ports exist.

- **Audio stutter, two independent causes — issue #3, part 2.** Symptom: audio that did
  play was stuttery/glitchy. Root cause (a): `opusdec`'s `plc` (packet-loss-concealment)
  property defaults to `false` (confirmed via `gst-inspect-1.0 opusdec`), so even a
  correctly-signalled gap was silently dropped instead of concealed. Root cause (b):
  `decode.rs`'s `AudioDecoder` had no jitter/reorder buffer at all — packets went straight
  to `opusdec` in raw arrival order with no gap signalling, even though `fragment.rs`'s
  `Reassembler` can legitimately emit frames out of `frame_id` order under UDP racing. Fix:
  a new `AudioJitterBuffer` (`decode.rs:885`), owned solely by the pipeline thread and
  keyed on `pts_us` — deliberately *not* the wire `frame_id`, which is actually the
  connection's shared nonce counter across video/audio/preview (see the protocol
  refinements above) and therefore useless for audio-only gap detection — holds packets up
  to ~40ms to reorder them and detect genuine gaps, pushing a GAP-flagged buffer to
  trigger Opus PLC on a detected loss. `plc` is now explicitly enabled where the `opusdec`
  element is built (`decode.rs:1242`, `build`/`~1260`). The buffer is depth-capped at 8
  packets (`decode.rs:785`) so a sustained gap doesn't grow latency unboundedly.

- **Late-joining viewers heard no audio at all — found during QC of the stutter fix, not
  in the original issues.** Symptom: a viewer who subscribed more than a few seconds after
  the sharer started sharing got video but no audio whatsoever, even though the audio
  stream was otherwise healthy. Root cause: the receiver's `autoaudiosink` runs `sync=true`
  against a base-time fixed at subscribe-time, while the sender's PTS is the sender's own
  capture-pipeline running time (time since *that sharer* started sharing) — for a
  late-joining viewer the gap between those two clocks becomes a large scheduled-playout
  delay, and the sink stalls waiting for buffers whose PTS puts them deep in its past.
  Confirmed live against a real music player's audio stream: 6 of 299 buffers reached the sink
  before the fix, 296 of 299 after. Fix: a new `AudioTimeline` (`decode.rs:1462`) re-bases
  incoming sender PTS onto the receiver pipeline's own running time (100ms initial lead),
  using the same self-healing re-base-on-discontinuity approach as `PacingState` on the
  video side. Note: the video path was never exposed to this specific footgun because it
  runs `sync=false` and paces frames manually instead of relying on GStreamer's own
  PTS-based sink sync (see the frame-pacing fix above); audio still relies on
  `autoaudiosink`'s built-in sync, so it needed its own PTS re-basing layer instead.

## QC findings

- **`Drop` backstops for both capture handles.** `CaptureHandle` and `AudioCaptureHandle`
  each gained a synchronous `Drop` impl (found necessary via a dedicated resource-cleanup QC
  pass, not the end-to-end test above) that force-stops the GStreamer pipeline — and, for
  audio, also kills the orphaned `pw-cli` helper process that owns the share sink (formerly `pw-loopback`; `std::process::Child`'s own `Drop` does *not* kill the OS process) — for the case where `.stop()` is
  never awaited at all: a crash, a panic, or an abrupt disconnect path. This is explicitly a
  backstop, not a replacement for graceful teardown — on Linux the portal session's async
  D-Bus close still needs `.stop()` to actually run for the OS recording indicator to clear
  promptly. See `client/src/capture.rs` (`impl Drop for
  CaptureHandle`) and `src/audio.rs` (`impl Drop for AudioCaptureHandle`).
- **Nonce/AEAD implementation verified correct by a dedicated security QC pass.** Confirmed:
  a single per-connection nonce counter (`Arc<AtomicU64>`) is genuinely shared across that
  connection's video, audio, and preview senders (no per-stream counters that could collide);
  counter overflow is detected and closes the connection rather than wrapping back to a
  reused nonce (spec §5.2's "MUST close" rule); the relay atomically rejects a colliding
  `client_id`/`ConnId` at `Join` rather than allowing a race; and the room key is never
  written to a log line anywhere in either codebase.
