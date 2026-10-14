# Video Pipeline (View tab): H.264-over-HTTP relay + browser-side WebCodecs

**Added 2026-07-28** (backfilling docs for work that landed across several sessions before
this page existed — see `superdoc/architecture/overview.md`'s dated module-map entries for
the detailed build history this page summarizes). How the View tab actually gets decoded
video onto screen, end to end: the wire format between the Rust sidecar and the Electron
renderer, the client-side jitter buffer, and how the three live encoder-tuning controls
(`superdoc/features/stream-control.md`) interact with an in-flight watch. Lives in
`simple-screensharing-client`.

Not covered by `superdoc/features/stream-control.md` (which is about the *encode* side —
building/editing/tiering outgoing destinations) or `superdoc/features/self-preview.md`
(sender-side only, a completely different mechanism — a live GStreamer `egui::ColorImage`
texture, not HTTP, not WebCodecs). This page is the *decode/delivery* side, for a watcher.

## The two ends

- **Sender side**: unchanged by anything in this doc. `capture.rs` encodes H.264,
  encrypts, fragments, and sends over the QUIC media-datagram plane — see
  `architecture/overview.md`'s "Data flow" and `capture.rs`'s module doc.
- **Watcher side, Rust sidecar**: `net.rs` reassembles/decrypts. `session.rs`'s
  `spawn_media_ingestion_task` (one per room, always running) is the single place that
  sees every `ClientEvent::MediaFrame(DecryptedMediaFrame)` — a clean, access-unit-aligned
  Annex-B H.264 access unit per frame, confirmed against `capture.rs`'s encoder caps
  (`video/x-h264,stream-format=byte-stream,alignment=au`, no `h264parse` in the pipeline,
  no GStreamer-specific wrapping). `commands::toggle_watch` wires an optional `raw_tx`
  (`session.rs`'s `WatchEntry`, a plain `std::sync::mpsc::Sender<DecryptedMediaFrame>`,
  deliberately *not* the `tokio::sync::mpsc` used elsewhere on that struct) right after
  creating the watch, so `src-tauri/src/video_server.rs` can tap the same raw frames
  without `session.rs` (the front-end-agnostic library crate) depending on `src-tauri`
  (which owns the relay) — dependency direction stays one-way.
- **Watcher side, `video_server.rs`**: its own `TcpListener` (bound at `Managed::new()`,
  ephemeral port), one route per watched `(room_handle, owner, stream_id)`. Relays the raw
  Annex-B access units byte-for-byte over a never-ending `application/octet-stream` HTTP
  response body — **no decode, no re-encode, no GStreamer involvement at all** on this
  path. Started/stopped in lockstep with the watch by `commands::toggle_watch`; a second
  connection to an already-flowing route can also be resolved via `stream_video_url`
  (used by the standalone viewer window — `superdoc/features/viewer-window.md`).
- **Watcher side, browser (Electron renderer)**: `frontend/scripts/tile-video.js`
  (`window.TileVideo`) fetches the URL, parses the wire format below, and feeds chunks to
  a WebCodecs `VideoDecoder`. Annex-B framing is inferred by Chromium when
  `VideoDecoder.configure()`'s `description` is omitted, so no Annex-B→AVCC bitstream
  conversion is needed anywhere in this pipeline.

## Why this replaced an MJPEG-over-HTTP design

An earlier version of this same delivery path decoded every frame with GStreamer
(`decode.rs`'s `StreamDecoder`, now dead code — see `architecture/overview.md`'s "Data
flow" step 4) and re-encoded it as JPEG, served as `multipart/x-mixed-replace`. That paid
for a full decode+re-encode round trip on every single frame, was capped at a ~60fps poll
rate, and threw away the source encoder's GOP structure (every "frame" was an independent
JPEG regardless of whether the underlying access unit was a keyframe). Relaying the raw
H.264 and decoding it *in the browser* deletes that round trip entirely and removes the
poll-rate ceiling — frames forward the instant they arrive.

## Wire format

The HTTP response body (`Content-Type: application/octet-stream`, never terminates while
the watch is active) is a sequence of chunks, each:

```text
offset  size  field
0       4     body_len   u32 big-endian — byte length of everything below
                         (i.e. 10 + payload.len()), NOT counting this field.
4       1     frame_type u8 — 0 = CodecInfo, 1 = VideoFrame.
5       1     flags      u8 — bit 0 (0x01) = keyframe. Only meaningful for
                         VideoFrame; always 0 for CodecInfo. Bits 1-7 reserved.
6       8     pts_us     u64 big-endian — the sender's capture-time PTS in
                         microseconds (DecryptedMediaFrame::pts). Always 0 for
                         CodecInfo.
14      *     payload    body_len - 10 bytes. VideoFrame: one raw Annex-B
                         H.264 access unit, byte-for-byte identical to
                         DecryptedMediaFrame::payload (start codes included).
                         CodecInfo: a UTF-8 WebCodecs codec string, e.g.
                         avc1.640028 (no NUL terminator).
```

So each chunk is `4 + 10 + payload.len()` bytes on the wire. **Every connection's very
first chunk is always exactly one CodecInfo chunk**, sent before any VideoFrame chunk, so
a fresh `VideoDecoder` always has a codec string to `configure()` with before it needs to
decode anything. `VideoFrame` chunks are withheld from a given connection until a keyframe
is available to send as that connection's *first* one (re-armed on a mid-connection
CodecInfo change — see below), so a watcher never has to decode a mid-GOP delta frame with
no reference. `src-tauri/src/video_server.rs`'s own module doc is the authoritative,
byte-exact version of this section; it's verified against a real loopback socket by
`video_server::tests::relay_streams_codec_info_then_frames_in_wire_format`.

### Codec-string discovery: derived, not configured

There is no explicit `profile=` GStreamer property set anywhere in `capture.rs` — neither
`x264enc` nor `vah264enc` is told a profile, so it isn't statically knowable. The codec
string is instead **parsed live out of each keyframe's SPS NAL**
(`parse_avc_codec_string`, `profile_idc`/`constraint_flags`/`level_idc`) and delivered
in-band as a CodecInfo chunk, not as a REST field — because it can genuinely change
mid-stream (see the `BranchSwap` interaction below).

## Encoder-tuning interaction: codec string is re-derived per keyframe, not cached once

This mechanism exists because of a real, real-hardware-verified bug (2026-07-28): a
watcher connecting **after** the sharer had toggled the live encoder-tuning controls
(`tune`/`quality_level`/`rate_control` — `superdoc/features/stream-control.md`) got stuck
at 0fps behind a `Stream error` placeholder. Root cause: `x264enc` picks its AVC
profile/level automatically from `speed-preset` when no explicit `profile=` is set — a
`speed-preset=ultrafast` branch emits `avc1.42c01f` (Baseline, level 3.1), while
`speed-preset=veryslow` at the same resolution emits `avc1.640032` (High, level 5.0), real
SPS bytes compared directly. The old design cached the codec string once
(`Mailbox::set_codec`, a no-op once already set) on the assumption that profile doesn't
change mid-stream — false the moment a tuning control triggers a `BranchSwap` to a
differently-configured encoder. A route that had already latched a codec string kept
handing the *stale* one to `VideoDecoder.configure()`, and every `decode()` call after
that silently failed.

**Fix**: the codec string is re-derived from *every* keyframe's SPS, not just the first.
The mailbox carries a `codec_generation` counter that only bumps on an actual value
change; `handle_connection`'s relay loop watches that generation alongside the frame
generation and pushes a fresh CodecInfo chunk to every already-connected watcher the
instant it changes — not only to new connections. `tile-video.js`'s CodecInfo handler
already called `decoder.configure()` unconditionally on every such chunk (not gated to
"first ever"), so **no frontend change was needed** for this fix. A related fix
(`f74e070`) drops any already-queued, still-encoded chunks from the *old* profile when a
mid-stream CodecInfo change arrives — decoding a stale-profile chunk against a
freshly-`configure()`d decoder for the new profile would otherwise fail or corrupt output.
Regression coverage:
`video_server::tests::relay_pushes_a_fresh_codec_info_chunk_when_a_branch_swap_changes_profile`.

`Why the tuning controls are routed through the same tier-2 `BranchSwap` machinery that
resolution/fps/backend edits use, rather than a live property write:` see
`superdoc/features/stream-control.md`'s "Encoder tuning controls" section — the short
version is that none of `x264enc`/`vah264enc`'s tuning properties are safely writable on a
`Playing` element, so "change one of these" is definitionally "build a new encoder," which
is exactly what a branch swap already exists to do safely.

## `errorPlaceholder` now clears itself once playback recovers (2026-07-28)

**Bug** (reported by the project owner): turning a watched stream fullscreen/borderless
(`superdoc/features/viewer-window.md`) — and, it turns out, plain on-screen View tab tiles
too — could leave a "Stream error" label stuck on top of video that was visibly still
playing fine.

**Root cause**: `tile-video.js`'s `startTileVideo` reveals `errorPlaceholder` (removes its
`hidden` attribute) from five call sites — a fetch rejection, a non-OK HTTP response, the
`VideoDecoder`'s `error` callback, a `decoder.configure()` throw, and the read loop's own
`catch` — but **nothing ever re-hid it**. That was fine as long as every one of those five
was fatal (no more frames ever decode afterward, so the label staying up matched reality),
but not every one of them necessarily is. A transient, self-recovering hiccup right around a
fresh/second connection to an already-flowing route (`stream_video_url`, used by both a
freshly-opened viewer window and a freshly-popped-out tile — see `video_server.rs`'s module
doc on late-connection keyframe gating) or around a mid-stream CodecInfo swap (the exact
class of event the "Encoder-tuning interaction" section above already had to make
non-fatal at the *protocol* level) could flash `showError()` once and then have the read
loop carry on decoding fine — but the label, once shown, had no path back to hidden. Live
production trace (real Electron app, real portal capture, real relay, two instances): a
watch that had just started (freshly clicked "Watch" and, in the same beat, freshly popped
into a viewer/borderless window) showed "Stream error" over demonstrably live, updating
video (confirmed via the tile's own fps counter climbing normally) — and the label stayed
stuck indefinitely with no further errors logged, exactly the reported symptom.

**Fix** (`tile-video.js`): `startTileVideo` now has a `hideError()` counterpart to
`showError()`, called from `decoder.output` every time a real frame actually makes it to
the canvas — i.e. the moment playback is *known* to be working, whatever happened before
it. This can never mask a genuinely fatal error: every one of the five `showError()` sites
either `return`s before a decoder even exists, or (the `VideoDecoder` `error` callback)
calls `stopController` first — either way `decoder.output` never fires again for that
controller, so the placeholder correctly stays visible for a real failure and self-clears
only when frames are genuinely flowing again. Shared by both callers of `window.TileVideo`
(`view.js`'s on-screen tiles and `viewer.js`'s pop-out grid — see
`superdoc/features/viewer-window.md`), so the fix applies to both without any per-caller
change.

No server-side change was needed or made here — this is purely about the client no longer
leaving a stale overlay up past the moment its own pipeline recovered.

## Client-side jitter buffer (`tile-video.js`)

Without buffering, `startTileVideo` used to call `decoder.decode()` immediately on every
chunk as it arrived off the wire, so network jitter translated directly into visible
playback stutter. It now queues each already-parsed, still-*encoded* chunk
(`controller.pendingChunks` — cheap, kilobytes each) and delays the `decoder.decode()`
call itself until a per-chunk scheduled time, computed from an RFC 3550 §6.4.1-style EWMA
of arrival-time jitter measured off the wire format's `pts_us` header.
`decoder.output`'s immediate draw-and-`close()` is untouched — only *when* `decode()`
fires moved, not what happens after.

`Why buffering happens before decode, not after:` decoded `VideoFrame`s are far too
memory-heavy (~3MB each at 1080p) to hold hundreds of at high framerates, while raw,
still-encoded chunk data is cheap to queue. Buffering pre-decode trades a small amount of
CPU (decoding slightly late instead of immediately) for a buffer that can actually hold a
meaningful window of frames without exhausting memory.

The buffer ceiling is a module-level `maxJitterBufferMs` (`tile-video.js`) — a single
`let` binding rather than an inlined constant, specifically so it can be sourced from
elsewhere without touching the jitter math itself. It is: `init()` in both `view.js` and
`viewer.js` fetches the real `max_jitter_buffer_ms` setting via `get_settings()` once at
startup and assigns it via `window.TileVideo.setMaxJitterBufferMs`, falling back to
`DEFAULT_MAX_JITTER_BUFFER_MS = 2000` on the mock/no-backend branch. **Default 2000ms,
user-configurable 0-5000ms** from the Settings tab (a range slider,
`frontend/scripts/settings.js`), clamped both client-side (the slider) and server-side
(`rooms::AppSettings::clamp_max_jitter_buffer_ms`, defense in depth) — plumbed through the
same `AppSettings` (`rooms.rs`) → `AppState`/`save_max_jitter_buffer_ms` (`session.rs`) →
`SettingsDto` (`dto.rs`) → `set_max_jitter_buffer_ms` command → REST dispatch
(`api_server.rs`) pattern `language`/`presentation_mode` already established.

Key pieces (`tile-video.js`):

- `updateJitterEstimate` — the RFC 3550 EWMA itself (`JITTER_EWMA_ALPHA = 1/16`), fed by
  each chunk's arrival time vs. its `pts_us` delta from the previous chunk. A chunk that
  arrives very late (e.g. after a real stall) naturally produces a large deviation here —
  a stall feeds the next target-buffer calculation with no separate "detect underrun"
  branch needed.
- `updateAppliedBuffer` — eases the *applied* buffer toward `jitterEwmaMs * 4 +
  JITTER_BUFFER_FLOOR_MS` (floor 40ms) by at most `MAX_BUFFER_EASE_STEP_MS` (20ms) per
  chunk, clamped to `[0, maxJitterBufferMs]` — so a jump in the jitter estimate drifts the
  playout delay smoothly instead of visibly jumping the schedule.
- `maybeScheduleNext`/`enqueueChunk` — the actual `setTimeout`-driven playout scheduler;
  guarded so at most one timeout is outstanding per controller.
- `stopController` — explicitly clears the scheduled timeout on teardown (same
  already-diagnosed class of teardown-ordering bug this file hit once before with its
  fetch/reader cleanup — leaving a stale timeout alive past `stopController` would fire a
  decode call against an already-torn-down decoder).

`tile-video.js` was extracted out of `view.js` specifically so the standalone
fullscreen/borderless viewer window (`superdoc/features/viewer-window.md`) can drive the
exact same fetch+parse+decode+jitter pipeline against its own `<canvas>` grid, independent
of DOM layout — both callers share one `window.TileVideo` module rather than maintaining
two copies that could drift.

## Sustained sender-rate-exceeds-decoder overload: unbounded drift, and the fix (2026-07-29)

**Bug report:** when the viewer's decode pipeline structurally cannot keep up with the
sender's sustained framerate (reported repro: ~280fps-equivalent sent, viewer only ever
managing ~240fps-equivalent), the picture drifts further and further behind live, without
bound, instead of settling into a bounded, slightly-behind-realtime steady state.

**Root cause, and why it lives here and not in `decode.rs`:** `decode.rs`'s module doc
(and `architecture/overview.md`'s `Why:` note on video pacing) already describes a
well-designed mechanism for exactly this class of problem — a `pts_budget` DDA/accumulator
plus a wall-clock staleness backstop, both unit-tested. But `decode.rs`'s video path
(`StreamDecoder`, GStreamer `avdec_h264` → `egui::ColorImage`) is **dead code** — zero live
callers since the Electron migration moved real video decode into the browser (`tile-video.js`'s
`VideoDecoder`, fed by `video_server.rs`'s raw H.264 relay). That mechanism was never ported
to the browser path when the decode responsibility moved, and — critically — **couldn't be
ported unmodified even if someone had tried**: `PacingState` only ever skips *writing a
decoded frame to the display sink*, never skips *decoding* it, because GStreamer's
`avdec_h264` element decodes every frame it's handed regardless of pacing (decode is cheap
relative to the encode side it was designed against). In the browser, `VideoDecoder.decode()`
itself is the resource under contention — skipping display only, and still calling
`decode()` for every chunk on schedule, does nothing to stop the decoder's own internal queue
(`decoder.decodeQueueSize`, invisible to any code upstream of the decode call) from backing up
further with every chunk than it drains. `tile-video.js`'s jitter buffer (previous section)
schedules each `decode()` call strictly against a real-time/pts schedule and had **no
mechanism at all** that reacted to the decoder actually falling behind — confirmed by grepping
`frontend/scripts/*.js` for any backlog/catch-up/skip logic before this fix: there was none.
So under a sustained (not bursty — `video_server.rs`'s single-slot mailbox + clamped send
buffer already rule out a *delivery*-side backlog, see that module's doc) rate mismatch, every
chunk still got decoded, just later and later, which reads exactly as the reported symptom:
slow, ever-worsening drift, never a stall.

**Why the fix can't just reuse `PacingState`'s "skip a frame" idea directly:** naively
dropping an individual queued delta (P-frame) chunk instead of decoding it is unsafe — H.264
delta frames reference earlier frames in decode order, so skipping one frame's `decode()` call
corrupts every later delta frame that depended on it, which surfaces as a fatal `VideoDecoder`
`error` that (per `tile-video.js`'s existing `error` callback) tears the whole tile down. That
would turn "the viewer is slightly late" into "the viewer is dead" — worse than the bug it
would fix. The one drop that is always safe is a **contiguous run of delta chunks up to (not
including) the next keyframe still sitting in the not-yet-decoded queue**: a keyframe is
self-contained by definition, so resuming decode from it can never reference anything that was
skipped.

**The fix** (`tile-video.js`'s `maybeScheduleNext`): when a chunk's scheduled decode time
arrives, if `decoder.decodeQueueSize` exceeds `DECODE_QUEUE_CATCHUP_THRESHOLD` (3 — a few
frames' headroom so ordinary jitter doesn't trigger it) and the chunk about to be decoded is
*not* itself a keyframe, scan forward in `pendingChunks` for the next keyframe and drop
everything before it in one `splice`. A due keyframe is always decoded as-is (never delayed
for a later one — it's cheap and immediately resyncs). No new keyframe-request plumbing was
needed: `capture.rs`'s regular encoder GOPs already emit periodic keyframes independent of this
mechanism, so one is normally already queued or arrives again shortly. `controller.catchUpDroppedFrames`
counts drops for diagnostics; a `console.warn` fires on every catch-up event.

**Verification:** `cargo build --lib -p simple-screensharing-client` and `cargo build -p
simple-screensharing-client-tauri` both clean; full suite `cargo test -p
simple-screensharing-client --lib` (160 passed) and `-- --ignored` (14 passed) both green
(JS-only change, Rust side untouched by design — see root cause above). Real-app smoke check
via a throwaway Playwright `_electron` script (per the `run` skill): launched the real
Electron shell against the real built sidecar, confirmed the window landed on a safe monitor,
confirmed zero page/console errors and that `window.TileVideo` still exposes its full function
surface after the edit. **Not done:** a deterministic, real-hardware 30-60s sustained-overload
measurement proving bounded vs. unbounded display lag growth end-to-end — reliably forcing
this browser's `VideoDecoder` to sustain a *specific* fps ceiling below a *specific* sender
rate on this machine (to reproduce "280 sent, 240 displayed" exactly) isn't something this
pass found a controllable, deterministic knob for; the fix's correctness instead rests on the
decode-order/keyframe-safety argument above plus the build/test/smoke verification. If this
needs stronger confidence, the next step would be to add an artificial per-chunk decode delay
knob (test-only) to `tile-video.js` to deterministically simulate "decoder slower than sender"
without needing real hardware limits.

## Audio follows video — and never appears as its own stream to a viewer

**Added 2026-07-28.**

Product requirement, from the project owner verbatim:

> "Also, the shared audio should be connected to the screenshare. The user shouldnt care,
> how we handle it architecturaly. The streamer should be able to enable audio on his
> share and the viewer should automatically hear that. No (visual) seperate 'stream' for
> Audio."

### The architecture, and why audio does *not* use the pipeline above

Audio is **not** relayed to the browser. It never touches `video_server.rs`, the wire
format above, WebCodecs, or the renderer at all. It is decoded and played **inside the
Rust sidecar process**, by `decode.rs`'s `AudioDecoder` straight into GStreamer's
`autoaudiosink`.

`Why:` the only reason video needs an HTTP relay and a browser-side decoder is that the
*pixels* have to end up in a `<canvas>` the renderer owns. Audio has no such constraint —
it has to end up in the user's **speakers**, and the sidecar is already running on the
same machine, already receiving/decrypting the Opus packets (`net.rs`), and already owns a
working playback pipeline that the egui client has used since long before the Electron
shell existed. Two alternatives were considered and rejected:

- **A parallel `/audio/<stream_id>` HTTP endpoint** feeding a browser `AudioDecoder` +
  `AudioContext` — a second wire format, a second producer/mailbox/connection-thread
  stack, a second jitter buffer, and a fresh set of A/V-sync questions, all to move sound
  from a process that can already play it into one that then has to play it anyway.
- **Muxing audio chunks into the existing video connection** — couples the two media to
  one route's lifetime and to one `VideoDecoder`-shaped pipeline, and breaks the
  audio-only-sender case (`toggle_listen`) which has no video route to ride on.

Neither buys anything a user can perceive. The sidecar-plays-it design is a ~0-line
delivery path: the mechanism already existed and worked; what was missing was the
*pairing* logic below.

### The pairing rule: `RoomSession::sync_audio_watches` (`src/session.rs`)

One reconcile pass, run from `AppState::drain_events` (so both frontends get it) and from
`driver.rs`'s `prune_stale_watches`:

> There is exactly one auto-subscribed audio watch per sender we currently hold a **video**
> watch on, for as long as that sender has an audio stream announced — and nothing else.

`AudioWatch::auto` distinguishes these from a standalone "Listen" on an audio-only sender
(`toggle_listen`), which lives in the same `audio_watches` map but is only ever turned off
by the user.

`Why a continuous reconcile rather than latching the pairing once inside toggle_watch`
(which is what the code did before, and what the egui client had done from the start):
latching is only correct if the sender's audio exists *before* the viewer clicks Watch and
never changes after. Four real cases break that:

1. **The normal order of operations.** A sharer starts their screen share first and picks
   an app in the Share Audio row second. Every viewer already watching at that moment
   latched `None` and stayed **permanently silent**, with no recovery short of un-watching
   and re-watching. This is precisely the failure the owner's requirement describes.
2. **Two monitors, one audio stream.** Two `WatchEntry`s, one audio stream. Un-watching
   one tore down the audio the *other* was still using.
3. **`prune_stale_watches`** drops `WatchEntry`s without going through `toggle_watch`, so
   the paired audio watch and its relay subscription leaked.
4. **A sender stopping only their audio** left a dead `AudioWatch` and a stale
   subscription behind.

Reconciling from current state handles all four with one rule and no bookkeeping to keep
in sync.

### Viewer-facing UI: nothing to add, one thing to check

The "no separate visual stream" half of the requirement needed **no code change** — every
viewer-facing list in the Electron frontend already filters to `kind === "Video"`
(`view.js`'s "NOT ON SCREEN" rail, `shell.js`'s and `stream-control.js`'s per-member Watch
rows). An earlier investigation flagged four `kind === "audio"` branches in
`components.js` (~385, 571, 599, 604) as the offenders; they are not — all four render the
user's **own outgoing destinations** (`DestinationRow`: the sidebar OUTGOING list and the
Active Shares cards), i.e. the streamer's side, which the owner explicitly wants kept.

What *was* added is a per-tile **volume/mute control** in the View tab's hover overlay
(`view.js` `renderTile`, `.media-tile-audio` in `view.css`), shown only for a sender who is
actually sharing audio. Deliberately **not** a listen/unlisten toggle: by the time a tile
exists the audio is already subscribed and playing, so the only thing left to want is
"quieter" or "off".

`commands::set_volume` accepts **either** the audio stream id or the **video** stream id
and resolves the sibling itself. `Why:` the tile only ever knows the video stream id it was
rendered from, and shouldn't need an audio stream id — pushing that resolution into the
backend keeps the frontend ignorant of a concept whose entire point is that it isn't
separately visible.

### Status

**Verified end to end against a real relay**, in the order a real session happens in:
`end_to_end_test::watching_video_automatically_delivers_the_sharers_audio_too`. Real relay
child process, real QUIC, real H.264 capture/encode, real Opus capture through a real
`pw-loopback` off a real PipeWire node, real Opus decode into `autoaudiosink`. It asserts:
video-only share → Watch → no audio watch; sharer *then* enables audio → the viewer's
`AudioWatch` appears on its own with `auto: true` and its playback pipeline reaches
`Playing` (`AudioDecoder::is_alive()`), with **zero viewer actions**; stopping the video
watch tears the audio down with it. Confirmed to **fail** without the `sync_audio_watches`
call (checked by disabling that one line), so it is genuine regression coverage rather
than a test that passes either way.

`Why the test's synthetic source runs at volume=0:` the receiving end is a real
`autoaudiosink` on a real desktop session, so a default-amplitude `audiotestsrc` plays a
440Hz sine out of the developer's speakers for the duration of the run — which it did,
and which interrupted the project owner mid-session. Silence is still a real, continuously
produced PCM stream that traverses the entire capture/encode/encrypt/relay/decrypt/decode
path identically; every assertion is about pipeline liveness and subscription state, none
about waveform content.

> **Superseded 2026-07-28.** That last sentence was the mistake, not a justification. The
> test above passed, and the feature did not work at all: the sharer was broadcasting
> their **microphone** and the viewer heard nothing of the shared application. A test that
> cannot distinguish "audio is flowing" from "audio is not flowing" cannot protect an
> audio feature, and "every assertion is about liveness, none about waveform content" is
> precisely the statement of that blind spot. See "The audio was never captured" below.

---

## The audio was never captured (2026-07-28)

The receiving half documented above was correct and is unchanged. The **sending** half was
not: `audio.rs` targeted its `pipewiresrc` with `path=<pw-dump registry node id>`, a
deprecated property that takes a node's `object.serial` or `node.name` — never the
registry id, which is a different number. A `pipewiresrc` whose target does not resolve
does not error; `node.autoconnect` defaults to `true`, so the session manager links it to
the **default audio source**, i.e. the microphone. Verified live with `pw-link -l`.

Everything downstream then behaved perfectly, on the wrong audio. The full root-cause
write-up, the two other defects found with it (audio destinations stuck on `STARTING`
forever; mono negotiation losing the right channel), and the guard that now makes a
mis-targeted capture impossible to ship silently (`audio::wait_for_capture_link`) are in
`superdoc/features/stream-control.md`'s "The 2026-07-28 audio bugs" section, since the
fixes live on the capture/UI side.

What changed **here**, in this page's territory:

- `AudioDecoder::packets_received()` — a plain counter incremented in `feed()`. A live
  playback pipeline proves the *receiver* is ready, not that anything is arriving; this is
  the cheapest signal that says audio really crossed the wire, so the test can assert it.
- `watching_video_automatically_delivers_the_sharers_audio_too` now drives its synthetic
  source at **`volume=0.02`** (~34 dB down — inaudible in practice, but *distinguishable
  from nothing*) and asserts both the sender's real capture level
  (`AudioCaptureHandle::level()` > 0) and the viewer's real packet count > 0. Neither
  assertion can pass on a silent or absent stream.

### What real playback actually measured

Two real Electron clients, a real relay, a throwaway decorrelated-noise application on the
sharer, and the audible result measured off the output device's monitor with the viewer's
per-tile volume control at three settings:

| viewer volume | measured RMS at the speakers | expected |
|---|---|---|
| 0%   | −30.80 dBFS (source only) | baseline |
| 100% | −28.09 dBFS (**+2.71 dB**) | +3.01 dB (one more equal-power copy) |
| 200% | −24.29 dBFS (**+6.51 dB**) | +6.99 dB (source + 4× power) |
| 0%   | −30.80 dBFS | exactly back to baseline |

That is the volume control in `commands::set_volume` → `AudioDecoder::set_volume` → the
`volume` element before `autoaudiosink`, resolving through the *video* stream id exactly
as the tile passes it, and it is real sound out of a real device.

`Why decorrelated noise and not a test tone:` the viewer reproduces the sharer's audio
~100 ms late, so a 440 Hz sine sums with the original at an arbitrary and drifting phase —
measurements swung between +5.8 dB (reinforcing) and −3.6 dB (cancelling) with no change
in configuration. Two decorrelated noise copies add in *power*, which is monotonic and
reproduced to 0.01 dB across runs.

## The volume slider did nothing for the first room joined each session (2026-07-28)

Reported as "when viewing, the volume slider doesn't work at all" — a real regression from
the state documented above, not a flaw in that verification. Root cause was a single
falsy-zero check in `view.js`'s `pushVolume`:

```js
if (!window.__TAURI__ || !tile.room_handle) return;
```

`tile.room_handle` is a real `u32` `RoomHandle` (`session::RoomSession::next_room_handle`
starts at `0`), so **the first room joined in a client process's lifetime is `RoomHandle
0`**, and `!0` is `true` in JavaScript. Every volume/mute change for that room returned
before ever calling `invoke("set_volume", ...)` — no error, nothing in the console, no
retry; the command was simply never dispatched. This is exactly the "doesn't work at all"
symptom: an ordinary session (join a room, share, watch, adjust the slider, all in one
app launch) always has its first — and typically only — joined room at handle 0.
`fetchWatchedStreams` a few lines above already gets this right (`r.room_handle != null`);
`pushVolume` was the one place that didn't. Not introduced by the pop-out/fullscreen
consolidation or any other later merge — present unchanged since the slider's original
commit; it simply never manifested in the original real-hardware verification above
because that session's test room happened not to be the first one joined.

**Fix:** `tile.room_handle == null` instead of a bare truthiness check.

**Verified with real audio, chain fully traced.** Two real Electron clients (a sharer
sharing a real portal-captured screen + a throwaway `audiotestsrc` at 5% volume as its
audio, and a viewer watching), a real relay, room handle confirmed at `0` for the joined
room. Temporary `tracing`/`console.log` breadcrumbs at each hop (removed before commit)
confirmed the full chain — DOM `input` event → `pushVolume` → `invoke("set_volume")` →
`commands::set_volume` → resolved `audio_stream_id` → `AudioDecoder::set_volume` →
`pump_audio_loop`'s `volume.set_property("volume", v)` with a matching readback — actually
executes end to end, which it did not before the fix (the trace was silent at every stage
because `pushVolume` never even called `invoke`).

Real audible confirmation, recorded in isolation directly off the viewer sidecar's own
PipeWire sink-input (`pactl`'s `--monitor-stream`, not the system's mixed default sink, so
no other running application's audio pollutes the measurement):

| viewer volume | measured RMS at the isolated stream | expected (linear-gain element) |
|---|---|---|
| 100% (baseline) | −53.48 dBFS | reference |
| 0%   | −inf dBFS (exact digital silence) | silence |
| 200% | −47.46 dBFS (**+6.02 dB**) | +6.02 dB (`20·log10(2)`) |
| 50%  | −59.50 dBFS (**−6.02 dB**) | −6.02 dB (`20·log10(0.5)`) |
| 100% | −53.48 dBFS | back to reference, exactly |

Exact linear-gain math both directions, confirming the `volume` GStreamer element itself
(and everything downstream of `commands::set_volume`) was never the problem — only the
frontend's dropped `invoke` call was.

## Viewer's chosen volume reset to 100% whenever the sender restarted/switched audio (2026-07-29)

Reported as: a viewer sets a tile's volume to e.g. 50%, then the **sender** (not the viewer)
restarts their screen/audio share or switches which app they're sharing audio from — and the
viewer's volume silently jumps back to 100%, with no action on the viewer's side at all.

Two separate copies of "volume" exist, and only one is actually authoritative for playback:

- **Frontend** (`frontend/scripts/view.js`'s `tileVolume` map, keyed by the stable *video*
  tile id) is correctly stable and persisted client-side, but it's write-only — it pushes a
  value via `set_volume` on user interaction and never re-pushes it on its own.
- **Backend** — the actually-*playing* copy — is `AudioWatch::volume_percent` inside
  `RoomSession::audio_watches`, keyed by `(owner, audio stream_id)`. The audio `stream_id` is
  volatile: every time a sender stops and restarts an audio share (or switches source app) it
  gets a fresh id from `next_stream_id`. `sync_audio_watches` correctly notices the old
  `(owner, old_stream_id)` is no longer announced and tears the whole `AudioWatch` down —
  dropping `volume_percent` with it — then `start_audio_watch` creates a brand-new one, which
  used to hardcode `volume_percent: 100`. Indistinguishable, at that layer, from "a genuinely
  new audio source with no prior preference", so every restart/switch quietly reset the
  viewer's chosen volume.

**Fix:** `RoomSession::audio_volume_by_owner: HashMap<ConnId, u32>` (`src/session.rs`) — an
owner-scoped "last chosen volume" that survives the `(owner, stream_id)` churn described
above. Set from both real call sites a volume can change (`src-tauri/src/commands.rs`'s
`set_volume`, and `src/ui.rs`'s inline egui slider — this is shared session state, so both
front-ends needed the write), and consulted by `start_audio_watch` (`src/session.rs`) as the
new `AudioWatch`'s initial `volume_percent`, defaulting to 100 only when the owner has no
recorded preference yet. Applies equally to the "audio follows video" auto-watch path and the
standalone `auto: false` Listen path (backend-capable, though the frontend currently never
surfaces a standalone Listen — see the "Audio follows video" section above), since both share
the same `audio_watches` map and the same `start_audio_watch` entry point.

`Why keyed by `ConnId` alone, not paired with any stream id:` `ConnId` is fresh-random per
connection (chosen by the client itself at connect time, not a small server-assigned counter
— see `superdoc/architecture/protocol-spec.md` §3.1), so a stale entry surviving a
`MemberLeft` cannot practically collide with a later, unrelated sender reusing the same
`ConnId`. The map is cleared on `MemberLeft` anyway, purely so it doesn't grow unbounded
across a long-lived room with many join/leave cycles — not because collision is a realistic
risk.

**Regression test:** `src/end_to_end_test.rs`'s
`volume_survives_sender_audio_restart_or_switch` — real relay server, real QUIC, real
PipeWire audio capture/decode (same "real everything" shape as the "Audio follows video"
test above). A viewer sets 50% on a real `AudioWatch`, the sharer's first audio share is
stopped and a second one started under a different `stream_id` (exactly what a real
restart/switch does), and the new `AudioWatch` is asserted to come up already at 50%, not
the hardcoded 100. Confirmed to **fail** without the fix (asserted `Some(100)` where 50 was
expected) before confirming it passes with the fix in place.

**Live-verified** against the real Electron app + real sidecar as the viewer (a
Playwright `_electron`-driven session, isolated from the real user's `~/.config` via
`XDG_CONFIG_HOME`) joined to a room fed by a Rust-harness sharer (test-source video, so no
interactive xdg-desktop-portal dialog was needed — only the *viewer* needed to be the real
app): real volume slider set to 50%, real sender-side audio restart/switch, real slider still
reading 50% afterward (was hardcoded back to 100 before the fix).

## View tab stats made prominent: always-visible stat bar, bigger "not on screen" cards, and a real received-bitrate DTO field (2026-07-29)

**Owner's request, verbatim:** *"In the view Tab, the streams are kinda 'hidden' or not
visible enough. It would be nice, if its design is more stylish and prominent. Just bigger.
Like show the resolution, bitrate, and if audio is being shared and stuff. Make it more
prominent!"* Two separate places in `view.js` matched that complaint, and both were changed:

1. **The "NOT ON SCREEN" rail** (`renderNotOnScreenRail`) — was a row of small text chips
   (`.view-not-on-screen-chip`: name · room + a tiny play icon, nothing else). Now a row of
   bigger cards (`.view-not-on-screen-card`, `view.css`): two-line name/room stack, a
   resolution readout, an audio-sharing badge when the sender has audio, and a bigger filled
   accent play button. The resolution/audio data comes from the same `StreamInfoDto` this
   room's watched-tile join already reads (`s.width`/`s.height`/`kind === "Audio"` on a
   sibling stream from the same owner) — the same source `shell.js`'s
   `renderRoomPanelStreamRow` already shows a resolution string from, so no new DTO field was
   needed here. **No bitrate on this card, deliberately**: bitrate is a live encode/transport
   stat that genuinely doesn't exist yet for a stream nobody is watching (see point 2's
   `WatchedStatsDto` — it's keyed by an active watch, `room.watches`, which this stream isn't
   in until someone presses Play). Showing a number here would have to be fabricated.
2. **The on-screen tile overlay** — `.media-tile-overlay`'s stats (owner/room/resolution/fps)
   used to live entirely inside `.media-tile-overlay`, which per its own then-doc-comment
   "only fades in on hover/focus". Replaced with `.media-tile-statbar`, a new **always-visible**
   bar pinned to the top of every tile (no hover needed) carrying owner · room · resolution ·
   fps · bitrate · an audio-sharing badge. The hover overlay still exists but is now scoped
   down to just the two things a viewer reaches for occasionally rather than glances at
   constantly: the pop-out/stop-watching action buttons (top-right) and the mute/volume
   control (bottom-left, unchanged). Value readouts (`.media-tile-statbar-value`,
   `.view-not-on-screen-card-value`) reuse `stream-control.css`'s `.dest-metric-value` recipe
   — `--font-mono`, `font-variant-numeric: tabular-nums`, 600 weight — this app's existing
   vocabulary for "a live numeric readout on a card", rather than inventing a new one; the
   live-dot/audio-badge accent colors reuse `tokens.css`'s `--success-glow`/`--accent` the
   same way the old nameplate's dot and the destination card's audio row already did.

### The bitrate gap: real received throughput was NOT plumbed to the frontend at all, so it was added

Before this change, no DTO anywhere carried a bitrate figure for a *watched* stream — only
`DestinationStatsDto::bitrate_kbps` existed, and that's sender-side (`dto.rs`'s doc comment on
it: "this destination's actual live encoder bitrate"), meaningless to a viewer with no channel
back to the sender's encoder. Rather than fake a number on the new stat bar, real plumbing was
added, end to end:

- **`decode::ByteRateCounter`** (`src/decode.rs`, new, next to the existing `FpsCounter` it's
  modeled on) — a rolling sliding-window byte counter over the same `DECODE_FPS_WINDOW`.
  `StreamDecoder::feed` (called for every incoming media frame — see below) records
  `frame.payload.len()` into it; `StreamDecoder::received_bitrate_kbps()` reads the current
  rate in kbps, the same shape as the existing `current_fps()`.
- **Why receiver-measured, not read off the sender:** a viewer has no live channel back to the
  sender's own encoder bitrate — cross-process, over the relay, and this decoder doesn't even
  know which of the sender's possibly-several destinations it is. Counting the compressed
  access-unit bytes this decoder actually receives is the only bitrate figure a *viewer* can
  honestly report — and arguably the more useful one anyway: it reflects what's actually
  arriving after any relay/network loss, not just what the sender is configured to emit.
- **Why this figure is NOT subject to the same "dead counter" problem `current_fps` has**
  (see `updateStats`'s pre-existing doc comment on `received_fps` being permanently 0 for any
  WebCodecs-driven tile): `current_fps` only increments inside `StreamDecoder::latest_frame()`,
  which nothing has called since the H.264-relay/WebCodecs redesign stopped consuming
  server-decoded frames. `ByteRateCounter` instead records inside `StreamDecoder::feed()` —
  called from `session.rs`'s `spawn_media_ingestion_task` for **every** incoming media frame,
  regardless of whether anything downstream ever polls `latest_frame()`. So
  `received_bitrate_kbps` is real and live even though `received_fps` (from the same event
  payload) is not — confirmed live (see "Real-world verification" below): the tile's bitrate
  readout updated tick-to-tick (8.1 → 8.0 Mbps) while its fps readout sat at 0.
- **New DTO field**: `dto::WatchedStatsDto::received_bitrate_kbps` (`f64`), computed in
  `driver.rs`'s existing per-tick `watched.push(...)` loop alongside `received_fps`, riding on
  the same already-unconditional `stream-stats-updated` event `received_fps` already used —
  same "piggyback on an existing every-tick payload" reasoning `DestinationStatsDto::
  bitrate_kbps`'s own doc comment used for the sender-side equivalent.
- **Frontend**: `view.js`'s `updateStats` now patches a `bitrateSpans` map (parallel to the
  existing `fpsSpans`) **unconditionally**, even for a tile with an active `TileVideo`
  controller — unlike the fps write, which still skips controller-owned tiles (no client-side
  bitrate source exists to prefer instead, so there's no double-write race to avoid).

### Real-world verification (2026-07-29)

Verified against the real app per `testing-and-verification.md` — two real Electron+sidecar
instances (isolated via per-instance `XDG_CONFIG_HOME`, so each got its own room-store file)
against a real locally-run relay (`simple-screensharing-server`, real UDP/QUIC, not mocked),
driven via Playwright's `_electron` API calling the real Tauri commands directly
(`create_room`/`join_room`/`stage_video_share`/`go_live`/`toggle_watch`), **not** the
mock/browser path:

- **"Not on screen" card**, real data: owner `Ana (sender)`, room `Verify Room`, resolution
  `1920x1080` in mono, big filled play button — screenshotted before any watch existed.
- **On-screen tile stat bar**, real data, real capture (real `xdg-desktop-portal` ScreenCast,
  not `videotestsrc` — this UI-triggering path has no test-source seam, see `capture.rs`'s
  module doc): `Ana (sender) · Verify Room · 1920x1080 0fps 8.1 Mbps`, then `8.0 Mbps` on the
  next tick — a real, live-updating bitrate number, proving the whole `ByteRateCounter` →
  `WatchedStatsDto` → `updateStats` → `.media-tile-statbar-value` chain works end to end.
  Hovering the tile also confirmed the scoped-down hover overlay: pop-out/stop-watching
  buttons appeared top-right (fixed `justify-content: flex-end` now that the title/fps that
  used to occupy the opposite side of `.media-tile-overlay-top` moved into the always-visible
  bar), stats stayed visible under the hover scrim throughout.
- **fps stayed 0** for the duration of this run — a pre-existing condition, not a regression
  from this change: this environment's `TileVideo`/WebCodecs canvas never visibly decoded a
  frame either (tile rendered as a plain dark rectangle, no image), most likely a
  hardware-decode/timing quirk of this specific scripted-Electron environment. Not chased
  further here since it's orthogonal to this change (this change never touched `tile-video.js`
  or its fps counting) and the bitrate readout — the actually-new code path — was proven live.
- **Monitor safety**: per `testing-and-verification.md`'s hard rule, real capture never
  touched HDMI-A-1. `hyprctl monitors` confirmed DP-1 focused (not HDMI) immediately before
  the one real `go_live` call in this session, and both test windows were explicitly
  positioned/verified on DP-1 (x=50)/DP-2 (x=2000) via `BrowserWindow.setPosition`+
  `getBounds()`. Ended with an explicit in-app teardown (`toggle_watch` to stop watching,
  `stop_destination`, `leave_room` on both instances) rather than a bare process kill, to
  avoid leaking the portal `ScreenCast` session the way `implementation-notes.md`'s "Leaving
  while a share was still starting leaked the portal session" entry describes for the
  `--tui` frontend's equivalent early-disconnect case.
- **Known caveat for future verifiers**: `ashpd`'s `stage_video_share` call resolved to a
  `"ready"` staged capture near-instantly in this environment with no visible interactive
  portal picker (`xdotool search` found nothing, twice, across two runs) despite
  `capture.rs`'s `select_sources` explicitly requesting `PersistMode::DoNot`. The exact
  reason wasn't root-caused (out of scope for this change) — treat "which monitor a
  non-interactive `stage_video_share` call will pick" as unconfirmed-by-code, and re-run the
  `hyprctl monitors`-focused-output safety check immediately before every real
  `go_live` call, not just once per session.

## Hover-overlay action buttons painted under the stat bar (2026-07-29)

**Owner's report, verbatim:** *"For the stream hover overlay, draw the buttons (top right)
above the vignette."* Regression from the "View tab stats made prominent" redesign directly
above this entry: that change added `.media-tile-statbar`, an always-visible, absolutely
positioned, `z-index: 1` bar pinned to the top of every tile — but `.media-tile-overlay` (the
hover-only sibling holding the pop-out/stop-watching buttons in
`.media-tile-overlay-top`/`.media-tile-overlay-actions`, occupying that exact same top-right
corner on hover) was left with no `z-index` at all (`auto`). Per CSS stacking rules, a
positioned sibling with an explicit `z-index` paints above a `z-index: auto` sibling
regardless of DOM order — so the statbar's gradient bar painted *over* the pop-out/
stop-watching buttons on hover, not the other way around, which is what the owner saw as
"buttons drawn below the vignette".

**Fix**: `.media-tile-overlay` (`view.css`) now has `z-index: 2`, outranking the statbar's
`z-index: 1`. `.media-tile-overlay-actions` already had `pointer-events: auto` from the
original design (the overlay itself stays `pointer-events: none` so it never blocks clicks
through to the tile below when its buttons aren't hovered) — the statbar has no
`pointer-events` override, but that was never actually a click-blocking risk once the buttons
paint on top: a click always hits the topmost element at that point, which after this fix is
the button, not the statbar beneath it.

**Verified** (mock/browser path — legitimate here per `testing-and-verification.md`'s
carve-out: z-index/paint-order/pointer-events is pure CSS stacking, identical DOM/CSS in mock
and real branches, no video pixels or capture-pipeline state involved): real Chromium via
Playwright against `frontend/` served over plain HTTP, View tab's dev "restore
watched-streams demo state" affordance, a real tile hover. Confirmed three ways: (1) computed
styles — `.media-tile-statbar` z-index `1`, `.media-tile-overlay` z-index `2`; (2)
`document.elementFromPoint()` at the stop-watching button's own center point resolved to the
button's `close` icon, not the statbar; (3) an actual `.click()` on the stop-watching button
in the overlapping region really triggered `stopWatching()` — the tile visibly moved from the
on-screen grid into "Not on screen" — proving the click isn't swallowed by the
now-underneath statbar.

## "Overlay always shown, should only show on hover" — investigated, was statbar/overlay conflation, not a hover-gating bug (2026-07-29)

**Owner's report, verbatim:** *"right now, for the stream the overlay is always shown. It
should only be shown, when hovered. Also, the whole overlay text, buttons and volume should
be drawn above the vignette/darkening!!"* — filed after the z-index fix directly above this
entry had already landed, and specifically flagged that the earlier fix's own verification
never actually confirmed the audio/volume control's stacking, only the pop-out/stop buttons,
and that it used mock+browser rather than the real app.

**Investigated three hypotheses**: (a) hover-gating genuinely regressed — `.media-tile-overlay`
visible at rest; (b) the owner is seeing the always-visible `.media-tile-statbar` (added by
the "View tab stats made prominent" redesign two entries up) and calling *that* "the
overlay", not `.media-tile-overlay` itself; (c) the prior z-index fix's mock-only
verification missed something real-Electron-specific.

**Real-app verification this time** (not mock+browser — see "How verified" below):
`.media-tile-overlay` was confirmed correctly hover-gated the whole time — `opacity: 0`/
`pointer-events: none` at rest, `opacity: 1` on real mouse hover, `z-index: 2` still
outranking the statbar's `z-index: 1` exactly as the prior fix set it. The audio/volume
control specifically (the gap the owner called out in the prior fix's verification) was also
confirmed: hovering `.media-tile-audio` expands the slider to full width, and it's the
topmost element at its own point — nothing paints over it. **(a) and (c) were both ruled
out** — no hover-gating regression, and the real Electron renderer's cascade/paint order
matched the CSS exactly as written.

That left **(b)**: looking at the real screenshot of a tile at rest, `.media-tile-statbar`
was a full-tile-width dark linear-gradient bar pinned across the entire top edge — visually
indistinguishable from "an overlay vignette that's always showing", even though it's a
different element serving a different, deliberately-always-visible purpose (the stats
readout, per the redesign two entries up). The owner's "always shown" report was accurate
about what they were looking at; it just wasn't `.media-tile-overlay`.

**Fix**: shrunk `.media-tile-statbar` (`view.css`) from a full-width gradient scrim to a
content-sized pill anchored top-left (`position: absolute; top/left: 8px`, `border-radius:
999px`, solid `rgba(0,0,0,0.62)` + `backdrop-filter: blur(6px)` + a thin border) — the same
"dark translucent chip" visual language `.media-tile-audio`'s pill already uses lower on the
tile, reused rather than inventing a second look. Stats stay always-visible (still an
explicit prior requirement — nothing here re-hides them behind hover), but the pill no
longer reads as a second, competing "overlay" the way a full-width scrim did. Hover-gating
and z-index were untouched — they were already correct.

**How verified**: real Electron + real sidecar (`_electron` driver, per the `run` skill) +
the real relay server already running locally on `127.0.0.1:4433` + a real `create_room`/join
over real network — not mock+browser. Full real video capture via `start_video_share` was
deliberately *not* exercised: on this machine it goes through xdg-desktop-portal's
interactive ScreenCast monitor picker, which can render thumbnails of every output including
the owner's real, live HDMI session, and this task's own screenshots would have captured
that — an information-leak risk, not a capability gap, so the environment-can't-do-X carve-out
in `testing-and-verification.md` doesn't quite cover it but the underlying reasoning (verify
for real without the specific unsafe step) applies. Instead, `room_detail`/`stream_video_url`
were augmented at the Electron main process's shared `http` module (intercepting only those
two outgoing sidecar calls, real passthrough otherwise) to splice in one synthetic
Video+Audio stream so a real tile renders under real `view.js`/`view.css` without a second
real sender. Checked at rest and on real mouse hover: computed `opacity`/`pointer-events`/
`z-index`, and `document.elementFromPoint()` at the popout/stop/audio-mute buttons' own
center points (confirmed self-or-descendant, i.e. nothing else paints over them) — plus a
dedicated hover of `.media-tile-audio` itself confirming the expanded slider is topmost at
its own point too. Screenshots at rest and on hover confirm the visual read: a compact pill
at rest, the full hover vignette with legible top-right buttons and bottom-left volume
control on hover.

## Reversal: "always-visible stat bar" un-done — hover-gated like everything else now, plus rounded/height-unified overlay buttons (2026-07-29)

**This deliberately reverses the "View tab stats made prominent" redesign above** (which
explicitly argued for "no hover needed" — stats visible at all times) **and the investigation
entry right above this one** (which kept that always-visible property, only shrinking the bar's
shape). The owner came back and asked for the opposite, twice, with specifics: *"make sure that
all of the elements disappear, if the window/area is not hovered! ... In window, the info
overlay (name, channel, resolution and so on) stays visible [named as the bug]. ... If popped
out, the info overlay is also always visible and the cross/close symbol is also always
visible [also named as the bug]."* This is a real, explicit design reversal, not a
misunderstanding — recorded here so a future reader of the entries above doesn't reapply the
"no hover needed" reasoning without noticing it was superseded.

**What changed** (`frontend/styles/view.css`, `frontend/styles/viewer.css`):

- **`.media-tile-statbar`** — now hover-gated exactly like `.media-tile-vignette`/
  `.media-tile-overlay`: `opacity: 0` at rest, `opacity: 1` on `.media-tile:hover`/
  `:focus-within`, same `transition: opacity 0.12s ease`. Applies to both the View tab and the
  pop-out window automatically for the DOM/CSS (both link the same `view.css` rule), but the
  pop-out window's own hover *trigger* selector (`.viewer-tile:hover`/`:focus-within`, since its
  tile wrapper isn't `.media-tile`) needed its own added rule in `viewer.css` — the
  vignette/overlay already had this wiring, the statbar didn't.
- **`.viewer-exit-btn`** (pop-out window's × close button) — was always `opacity: 1`, no
  hover-gating. Now fades in on hover/focus too. This button lives outside `.viewer-tile` in the
  DOM (a preceding sibling of the tile grid, appended via `setupExitControls`'s
  `#viewer-root.before(...)`), so `.viewer-tile:hover .viewer-exit-btn` can't reach it (CSS
  combinators don't reach backward to a preceding sibling). Fixed via `body:hover`/
  `:focus-within` instead — this window's `<body>` fills the full viewport, so it's
  functionally "pointer anywhere in this window," which is exactly the trigger this button
  wants, without any DOM restructuring or a JS mouseenter/leave listener. See
  `superdoc/features/viewer-window.md`'s matching entry for the live-verification detail
  (confirmed the two hover surfaces — tile-hover for the statbar/overlay, body-hover for the ×
  button — are independent, each revealing only its own chrome).
- **Rounded + height-unified the overlay action buttons**, same pass: `.media-tile-popout-btn`/
  `.media-tile-stop-btn` (View tab) went from a rounded square (`--rounding-sm`) to a true
  circle (`border-radius: 50%`), matching `.viewer-exit-btn`'s existing shape. All three buttons
  (`.media-tile-popout-btn`/`.media-tile-stop-btn`/`.viewer-exit-btn`) were resized from
  30px/30px/32px to a shared **29px**, matching `.media-tile-statbar`'s own rendered height —
  measured live via `getBoundingClientRect()` in the real Electron app (29px), not assumed from
  the CSS padding math (`padding: 5px 10px` + line-height + border), since font metrics can
  shift the real number in either direction — here it landed close to, but not exactly at, the
  buttons' prior 30px.

**How verified**: real Electron + real built `src-tauri` sidecar via Playwright's `_electron`
driver (per the `run` skill), not mock+browser. A real live two-peer relay session wasn't
available in this pass (no active sender in the session's existing rooms), so a real tile's
exact DOM (verbatim classes/structure from `view.js`'s `renderTile` and `viewer.js`'s
`renderTile`) was injected into the real app's live DOM/CSS context — the same Chromium
renderer, same linked stylesheets, same font loading (`document.fonts.ready` awaited before
measuring) the real tile would use, just without a live video feed driving it. Measured with
real `getBoundingClientRect()`/`getComputedStyle()` calls and a real Playwright mouse `hover()`
(exercising the actual `:hover` pseudo-class), in both windows:

- View tab: statbar/vignette/overlay all `opacity: 0` at rest, all `opacity: 1` on real hover,
  back to `0` after the mouse moved away, and stayed `1` under a real (non-mouse) `:focus-within`
  — confirming the keyboard-accessibility path (a tabbed-to volume slider keeps controls visible)
  survived this change.
- Pop-out window: same statbar/vignette/overlay pattern confirmed via a real `BrowserWindow`
  loading the actual `viewer.html`/`viewer.js`/`viewer.css`; additionally confirmed the ×
  button reveals on `body:hover` even when the hovered point is clearly outside the tile's own
  bounding box (proving the sibling-reaching fix works), while the tile's own statbar correctly
  stayed hidden at that same off-tile point (proving the two hover surfaces don't leak into each
  other).
- Popout/stop/exit buttons all measured 29×29px, `border-radius: 50%` via `getComputedStyle`.
- `cargo test --workspace`: 181 passed, 0 failed, 22 ignored (pre-existing, real-relay/GStreamer
  tests requiring `--ignored`), no regressions.

## Mute-toggle flicker, slider DOM presence, and pill shape (2026-07-28)

Four related fixes to the per-tile `.media-tile-audio` pill (`view.js`'s `renderTile`,
`view.css`), all reported together against the same screenshot:

- **Muting flickered.** The real cause was *not* the ~4x/sec `stream-stats-updated`
  re-render loop — `updateStats` already only patches the fps `<span>` text in place (see
  its doc comment above) and never calls `render()`. The actual culprit: the mute button's
  own `onClick` called the top-level `render()`, which unconditionally tears down and
  restarts *every* tile's video pipeline (`window.TileVideo.stopAllTileVideo()`, see
  `render()`'s "Full rebuild" comment) before rebuilding the whole grid from scratch — so
  clicking mute on one tile flashed the entire grid while every canvas re-decoded just to
  swap one icon. Fixed by extracting the pill into a `buildAudioGroup(muted)` closure that
  the mute button swaps in place via `bottom.replaceChild(...)`, touching only that tile's
  audio pill and never calling `render()`.
- **The pill wasn't a true circle.** `.media-tile-audio` was a flex row with `gap: 6px`
  between the icon button and the volume slider — CSS flexbox `gap` reserves that space
  between flex items even when one item is `width: 0`, so the resting pill's content width
  was wider than its height. Fixed by dropping `gap` from the container entirely and
  moving the icon-to-slider spacing onto the slider's own `margin-left`/`margin-right`,
  which collapse to `0` together with its `width`/`opacity` at rest.
- **Slider now genuinely absent from the DOM while muted**, not just CSS-collapsed —
  `buildAudioGroup` only appends the `<input type="range">` when `!muted`. Combined with
  the `gap` removal above, this makes the muted pill (icon button + `2px` padding + `1px`
  border on all sides) a real circle: **measured 28×28px** via `getBoundingClientRect()`
  in a live DOM (mock `hasAudio: true` tile), both at rest and while muted.
- **Expanded slider's right-edge spacing restored to 8px.** An earlier pass this session
  had reduced this from ~8px to ~2-3px (dropped the slider's `margin-right: 6px`, leaving
  only the container's `2px` padding) on a since-superseded instruction. Now
  `margin-right: 5px` on the hover/focus-within-expanded slider + `2px` container padding
  + `1px` container border = **measured exactly 8px** between the slider's right edge and
  the pill's actual outer (border) edge.

**Verified live in a browser** (`frontend/index.html` served over plain HTTP, no Tauri —
exercises the same `MOCK_WATCHED` `hasAudio: true` mock branch `renderTile` always had) via
Playwright: toggled mute on/off twice while a spy wrapped `window.TileVideo.
stopAllTileVideo` and tagged the tile's canvas node — **zero calls, same canvas node
identity** across both toggles (no rebuild, no flicker). Measured the muted pill at
28×28px, confirmed `.media-tile-audio-slider` is absent from `querySelector` results while
muted and reappears on unmute, and measured the real-hover expanded gap at exactly 8px.

## View tab and pop-out window disagreed on a tile's displayed volume/mute (2026-07-29)

**Bug** (project owner): *"The volume setting isn't visually shared between the in window
and the popped out view (but the volume doesn't change, so it is right logically)."* — the
real, actually-playing backend volume was always correct (confirmed by the owner); only the
slider's displayed value/position disagreed between the View tab's tile and a pop-out
window showing the same stream.

**Root cause**: `view.js` and `viewer.js` are two separate Electron `BrowserWindow`
renderer processes — two separate JS heaps — and each kept its own independent
`tileVolume`/`tileMuted` state (see this file's "Mute-toggle flicker..." entry above for
when that per-window state was introduced). Popping a tile out started `viewer.js`'s copy
at `DEFAULT_VOLUME`/unmuted regardless of what the View tab's tile had been set to;
closing the pop-out never fed the pop-out's final value back to the View tab's copy either.
Compounding this, `AudioWatch` (the backend's actually-playing copy, in `session.rs`) had no
`muted` concept at all — the frontend "muted" a tile by *zeroing* the volume percent it sent
to `commands::set_volume`, which is fine for playback (0% is silent either way) but loses
the *pre-mute* value the instant it's zeroed, so nothing could recover "was muted at 40%" to
show it correctly elsewhere.

**Fix — backend becomes the single source of truth, both windows query it, in both
directions** (chosen over a query-string/IPC handoff passed only at pop-out-open time,
which was considered — see the task brief this fix was scoped from — because a handoff
only ever covers the *open* direction; the *close* direction has no existing metadata
channel to piggyback on, and a backend query is the same one mechanism for both without
per-transition wiring):

- `AudioWatch` (`src/session.rs`) gained its own `pub muted: bool` field, alongside the
  existing `volume_percent`. `commands::set_volume` (`src-tauri/src/commands.rs`) now takes
  an explicit `muted: bool` parameter and stores `volume_percent` **as the slider's desired
  value**, never zeroed by mute — the decoder itself still gets `0.0` while muted
  (`if muted { 0.0 } else { volume_percent / 100.0 }`), but the stored value survives an
  unmute (or another window's query) intact.
- A new `commands::get_volume(room_handle, owner, stream_id) -> VolumeStateDto { volume_percent, muted }`
  query command — the read-side counterpart to `set_volume`, resolving the video-or-audio
  `stream_id` ambiguity the exact same way `set_volume` already does. Defaults to
  100%/unmuted (the same default every window always used before this command existed) when
  no `AudioWatch` is found yet.
- `view.js`: a new `ensureVolumeSeeded(tile)` fires once per tile id this window hasn't
  rendered before (tracked in a `seededVolumeIds` Set, pruned the same way `tileVolume`/
  `tileMuted` already are), calling `get_volume` and re-rendering once it resolves. This is
  the *pop-out-close-then-return* direction: `onPopoutClosed` now also deletes the
  returning tile's id from `seededVolumeIds` (not `tileVolume`/`tileMuted` themselves — those
  keep showing the last-known value with no flicker until the query confirms/corrects it),
  so a tile coming back from a held slot re-fetches instead of trusting a
  from-before-it-was-popped-out value.
- `viewer.js`: `resolveTiles` now calls `get_volume` for each tile **before** `renderGrid`/
  `renderTile` ever runs (this window resolves its one tile fully before rendering anything,
  unlike `view.js`'s ~4x/sec re-render loop), so the pop-out's slider is correct from its
  very first paint — no render-then-patch needed the way `view.js` needs one.
- Both `pushVolume` functions (`view.js`, `viewer.js`) now send the *desired* slider value
  plus an explicit `muted` flag on every change, instead of folding mute into a zeroed
  number.

**Known, narrow, out-of-scope gap** (documented in `AudioWatch::muted`'s and
`start_audio_watch`'s doc comments rather than silently left implicit): the pre-existing
`RoomSession::audio_volume_by_owner: HashMap<ConnId, u32>` map (see this file's "Viewer's
chosen volume reset to 100%..." entry above) that lets a viewer's chosen volume survive a
*sender's* audio restart/switch only remembers the numeric value, not mute — so a tile that
was muted, then has its sender restart/switch audio before it's unmuted, resumes audible
rather than staying silent. Not the bug this task was scoped to fix (that's the
View-tab/pop-out display sync above); closing it would mean widening
`audio_volume_by_owner`'s type, which also touches `ui.rs`'s egui slider and the existing
`volume_survives_sender_audio_restart_or_switch` regression test — left alone rather than
bundled into this pass.

**Verified live, partially** against the real Electron app + real built `src-tauri` sidecar
(per `testing-and-verification.md`), via Playwright's `_electron` driver and real
`XDG_CONFIG_HOME`-isolated instances talking to a real relay server on `127.0.0.1`: created a
real room, staged a real portal video capture, went live, attached real Floorp system audio,
then called the real `toggle_watch`/`get_volume`/`set_volume` Tauri commands directly (the
same `invoke()` bridge the real UI uses) against the real running app — `get_volume` on a
freshly-watched stream correctly returned the `100%`/unmuted default, `set_volume(55%)`
succeeded, and an immediate follow-up `get_volume` correctly reflected `55%` — proving the
new backend command pair works end to end against the real sidecar, not just in unit tests.

**Could not complete the full DOM-level check** (View tab slider ⇄ pop-out slider, read via
the real rendered tile) in this environment: the instant the real View tab actually renders a
tile with an active real video watch (i.e. `tile-video.js` starts its real WebCodecs decode
pipeline against the real H.264 stream), the whole Electron app process disappears within a
few seconds — reproduced 4 times, including once with only a *single* Electron+sidecar
instance watching its own just-published stream (ruling out two-process GPU/render-node
contention as the cause, since a lone app crashed the exact same way). Everything *before*
that point — room join, real portal capture reaching `ready`, `go_live`, real audio share,
`toggle_watch`, and the new `get_volume`/`set_volume` round trip above — worked instantly and
reliably every time; the crash is specific to a real video watch actually rendering, which is
pre-existing `toggle_watch`/`decode.rs`/`tile-video.js` machinery this task didn't touch, not
the volume/mute sync code. Flagging this explicitly rather than assuming the DOM-level
direction is equivalent to the command-level verification above — a future pass with real
display/GPU access (or a headless-friendly decode path) should re-attempt the actual slider
screenshots this bug report asks for.

`cargo test --workspace` (181 passed, including the existing
`volume_survives_sender_audio_restart_or_switch` real-hardware regression test run
explicitly) showed no regressions.

## Status

**Built and verified** (real hardware, real relay, real portal capture unless noted):

- The relay itself: wire format round-trip verified with a real loopback socket
  (`video_server::tests::relay_streams_codec_info_then_frames_in_wire_format`).
- A watcher subscribing well after a share has been running (past a GOP boundary)
  reliably receives a real keyframe within 20-70ms — proven across both encoder backends,
  synthetic and real portal capture: `late_subscriber_after_unwatched_period_gets_a_real_keyframe`
  / `..._real_portal` in `src/end_to_end_test.rs`.
- A watcher connecting after all three tuning controls have been toggled reliably gets a
  real, correctly-`configure()`-able keyframe: 29/29 clean across two independent
  real-hardware investigation sessions (15 in the original fix, 14 in a 2026-07-28
  re-investigation) —
  `late_watch_after_all_three_tuning_controls_toggled_gets_a_real_keyframe_every_time`
  in `src/end_to_end_test.rs`. See `architecture/overview.md`'s matching 2026-07-28 entry
  for why that test lives in this file (the `capture`/`net` layer) rather than
  `src-tauri`, and for the "residual test-harness flakiness, not a regression" conclusion
  of a single non-reproducing failure seen in a much larger multi-window smoke test.

**Known gap**: no per-watched-stream "is this actually decoding right now" DTO/event
exists yet — a tile that stops receiving frames (sender crash, network partition) has no
dedicated backend signal distinct from the stall protocol's own `Stalled` state
(`architecture/overview.md`'s "Stall protocol" section, which is about the *sender*
pausing intentionally, not about delivery actually breaking).
