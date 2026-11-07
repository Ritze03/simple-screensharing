# Video Pipeline: native decode and GPU display

> **Status (2026-11-07).** The client is one native egui binary. This page is the hub; the detail lives in
> `superdoc/features/video/`: **"Native decode"** and **"Native display"** (`decode-and-display.md`, access units
> to NV12 frames and to the screen via wgpu, plus the "Timing report"), **"Playout buffer"** (`playout-buffer.md`),
> **"Viewer recovery"** (`viewer-recovery.md`, L1/L2/L3) and **"Selective retransmission"** (`selective-retransmission.md`,
> NACK, viewer side). Here stay a short **"Capture side"** pointer list and **"Audio follows video"** (current). The
> **History** section at the end condenses the removed Electron-era display path (HTTP relay in `video_server.rs`, WebCodecs in
> the Electron renderer, `tile-video.js` jitter buffer); the long form is in git, `git log -- superdoc/features/video-pipeline.md`.
> The browser client (`docs/`) has its own WebCodecs pipeline (current), described in `superdoc/features/website.md`.

Where each side lives:

- **Encode / capture / fan-out:** `superdoc/features/stream-control.md` (see "Capture side" below).
- **Decode + display for a watcher:** `superdoc/features/video/decode-and-display.md`, `client/src/decode.rs` + `decode/`.
- **Delay before the decoder:** `superdoc/features/video/playout-buffer.md`.
- **A stream that stops decoding:** `superdoc/features/video/viewer-recovery.md`; lost fragments: `superdoc/features/video/selective-retransmission.md`.
- **Pop-out windows:** `superdoc/features/viewer-window.md`.
- **Wire format between peers:** `superdoc/architecture/protocol-spec.md`.

## Capture side (summary; detail lives in `stream-control.md`)

The decoder only sees what the capture pipeline produces. Everything below is in `stream-control.md`:
"Capture/encode performance" (#39: 120-240 fps of unique frames, `min-buffers`, `RateCap`, GPU/System capture domain),
"Idle monitors" and "Silence watchdog" (#32), "Live-verified: rotated outputs (#35)" and "AV1 branch deaths read as a lost portal
source" (#137: the coded buffer, the 4x keyframe-pacing cap that keeps keyframes from overflowing the uplink and forcing an
L1 recovery, see `video/viewer-recovery.md`). Two facts a decode-side reader meets that are not on that page:

- **Orientation (#35).** `pipewiresrc` turns `SPA_META_VideoTransform` into an `image-orientation` tag and the pipeline follows
  it (`video-direction=auto`). On the System domain the stage is spliced in lazily because `videoflip` costs about 0.8 ms per
  1080p frame even when idle. There is no transform table of our own (`capture.rs` `orientation_fragment`).
- **Caps query.** The tee answers caps queries with BGRx at any size, because otherwise `vapostproc` shrank the capture to 320x180.

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
  1/16, ease step 20 ms), capped by the user's `max_jitter_buffer_ms` (default 2000, 0-5000). The
  browser viewer keeps this design in `docs/js/core/jitter.js`. The native viewer has the same shape since
  row 136 with a different target (peak, not EWMA): "Playout buffer" in `video/playout-buffer.md`.
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
