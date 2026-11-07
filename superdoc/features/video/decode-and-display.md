# Video: native decode and GPU display

What the native viewer does with an access unit: decode it to NV12 (VA-API, D3D11 or software), report its timing, and put it on screen with wgpu. Part of the video pipeline hub, `superdoc/features/video-pipeline.md`.

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
  they come out. Since row 136 (2026-11-04) the viewer paces *before* the decoder instead: the
  **playout buffer** below holds encoded access units for a measured delay. `PacingState` stays
  unwired as a `pub` building block.

**A keyframe is a freeze, not a latency step (row 129, 2026-11-04).** The viewer cannot show a keyframe's picture before its last fragment arrives and the presenter is latest-only, so the sharer's keyframe pacing window shows up 1:1 as a freeze of the picture (decoder-output gap = pump hold + ~13 ms IDR decode; relay, upload and the rest measured at 1-2 ms on loopback). On a real line the sender's uplink sets that window, not the pump (row 133): `features/stream/keyframes-and-bandwidth.md` "Keyframe stutter on the real machine".

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
`Why the announced size is needed:` `vaav1enc` on AMD VCN 4 hardware encodes
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

**User report (live, 2026-11-03):** a viewer said a remote sharer's screenshare was "constantly going
faster and slower again", a *smooth* slowdown and speedup; the sharer shared no audio.

**Mechanism (code):** the native viewer had no playout pacing when this was written (frames were shown the moment
they were decoded; `PacingState` is not wired; the two Settings sliders for it were read by nothing and removed,
#125, 2026-11-04; the playout buffer of row 136 and the sliders came back later that day, see "Playout buffer" in `playout-buffer.md`). So when the decoder
is slower than the stream nothing is dropped at first: access units queue in the pipeline
thread's backlog (`decode/video.rs` `run`, up to `BACKLOG_MAX_AUS` = 64, 0.7 s at 90 fps), the
picture runs late more and more (slow motion at decoder speed / stream rate), and when the decoder
recovers the backlog plays out at full decoder speed (fast-forward). At the cap `trim_backlog`
jumps to the newest keyframe or drops the oldest deltas.

**Measured on the reporting viewer's machine** (read-only, from `/proc` of the running client, which logs to
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
The leading suspect is the video engine, shared with the viewer's own share: the saved defaults are
AV1, Quality, 120 fps (the running share's settings and the remote sharer's codec and resolution were not read),
and on VCN 4.0 hardware the kernel moves every AV1 encode and decode job to instance 0
(`drivers/gpu/drm/amd/amdgpu/vcn_v4_0.c`, `vcn_v4_0_limit_sched`; H.264/H.265/VP9 may use either).
Quality is AV1's half-speed band (~152 fps raw, `features/stream/vaapi-target-usage.md` "`target-usage` is a bitfield
on Mesa AMD"), so 120 fps would be ~80 % of that instance.

- For: the encoder thread was ~95 % in GPU waits in the same stretches, and slow stretches happened
  with no other process on the video engine.
- Against: `vcn_busy_percent` was often only 55-60 % in slow stretches (whether that averages both
  instances is unknown); a test AV1 decode next to an in-process 120 fps AV1 encode still reached
  ~228 fps (1080p; ~192 fps at 4K), far above 90; and a rig share of AV1 240 fps Quality on the same
  GPU from 23:19:46 did **not** slow the user's decoder (it held ~88-90 fps).
- Confound: the 23:16:40-23:19:34 stretch overlapped this investigation's own release builds, and
  one 44 fps window at ~23:12 followed its GPU load tests, so neither is a clean natural sample.

Ruled out by the code (at the time; the playout buffer arrived in row 136): a playout or catch-up controller and clock drift (the native viewer had
neither), A/V alignment (the sharer shared no audio). NACK hold-and-release and recovery loops were **not**
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

## Native display (`client/src/decode/render.rs` + `render.wgsl`)

**Added 2026-10-14 (I9).** Replaces "HTTP relay + WebCodecs in a `<canvas>`" (see History). The
module doc comment in `render.rs` is the authority; the points below are the ones an agent needs
before touching it. The UI side (`VideoViews`, tiles, hover chrome) is in `superdoc/ui/STAGE.md` "Video".

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
- **Measured:** 240 fps presented on a ~280 Hz output, 1080p. Pop-outs hold 240/240 in Mailbox mode,
  which is why Mailbox is the default; see `superdoc/features/presentation-modes.md`.
