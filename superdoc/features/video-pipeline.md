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
  reset the viewer to 100 %.
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
  browser viewer keeps this design in `docs/js/core/jitter.js`.
- **Sustained sender-faster-than-decoder overload** drifted without bound because the
  browser decoder was still fed every chunk. A frame cannot be skipped singly (delta frames
  reference earlier ones, so a skipped P-frame raises a fatal decoder error); the safe drop is a
  contiguous run of delta chunks up to the next queued keyframe, triggered when the decoder's queue
  exceeds a small threshold (3). Relevant to any WebCodecs path.
- **A "Stream error" placeholder that never cleared** was a shared-module bug (it needed a
  hide on the next decoded frame), and a later recurrence was CSS, not logic.
- **Verification lesson:** many of the old entries were verified by driving the real shell via
  Playwright `_electron` with synthetic input. Today use the `run` skill and `drive.sh`
  (`client/.claude/skills/run/SKILL.md`, `client/scripts/`).
