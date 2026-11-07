# Architecture Overview

One-paragraph orientation to this file's role: the first doc to read, links out to
sibling docs rather than duplicating them.

## Big picture

`simple-screensharing` (this directory) is **one monorepo** (GitHub repo
`Ritze03/simple-screensharing`, the rework landed on `master` 2026-10-15) holding the relay server, the
desktop client, the shared wire-protocol crate and this documentation. It used to be a docs-only
workspace over separate `simple-screensharing-server` / `simple-screensharing-client` repos;
those were imported with full history (old branches kept as `archive/client/*` and
`archive/server/*`; deleted from GitHub 2026-10-15, kept in a local git bundle outside the repo; where and how to restore: `.claude/local/reference-repos.md` in the main checkout). `Why one repo:` server, desktop client and the browser client must
change in lockstep.

| Path | Role |
| --- | --- |
| `server/` | Rust QUIC relay (`simple-screensharing-server`): broadcasts encrypted room traffic between clients, never decrypts it, plus a ratatui admin TUI. Also accepts browsers over WebTransport. |
| `client/` | Desktop client: one native egui app. The Rust library + bin crate (`simple-screensharing-client`, `client/src/`) holds capture, encode, encrypt, decode and session state in the lib (`session.rs`'s `AppState` is the model) and the egui UI in the bin (`client/src/ui/` screens, `client/src/widgets/` building blocks, `theme.rs` tokens). Video is decoded natively (VA-API with software fallback, `client/src/decode/`) and drawn by a wgpu renderer. Linux first; Windows builds (MSVC + GStreamer 1.26.9) and is verified headless Linux<->Windows, see `superdoc/features/windows.md`. Packaging: `client/packaging/`. |
| `protocol/` | Shared crate `simple-screensharing-protocol`: the single definition of wire types, header layouts and constants. `client/src/protocol.rs` and `server/src/protocol.rs` re-export it. Spec: `protocol-spec.md`. |
| `superdoc/` | This agent-facing knowledge base. |
| `docs/` | The browser client (static site served by GitHub Pages, WebTransport + WebCodecs): watch and share from a tab. `docs/js/core/` is the protocol and media core (`docs/js/core/README.md`), `docs/js/ui/` the UI (`docs/js/ui/README.md`: `app.js` render and modals, `stage.js` + `stage-state.js` + `stage-geom.js` (the stage, geometry a port of native `stage/geom.rs`), `chrome.js` (port of `stage/chrome.rs`), `bar.js`, `menus.js`, `actions.js`, `pip.js`, `capture.js`, `popout.js`, `sounds.js`, `media-event.js` (a watch's media event to its UI state, pure), `wakelock.js`, `dom.js` (layout primitives, `avatarEl`), `store.js`, `rooms.js`, `i18n.js`), `docs/dev/` the local test harness (`docs/dev/README.md`); map and rationale in `superdoc/features/website.md`. The relay accepts browsers over WebTransport on the same UDP port as native clients, with a pinned auto-rotating self-signed cert whose hash rides in the join link (`protocol-spec.md` §11). |

The root `Cargo.toml` is a workspace (resolver 3) with members `client`, `server` and `protocol`,
sharing one `Cargo.lock`. Root scripts: `build-appimage.sh` (Linux AppImage, see "Packaging"),
`launch-test.sh`, `launch-test-tmux.sh` (two-client test rig, see `client/scripts/README.md`). This page is the
up-to-date map of what is in the tree; the historical design record and the binding wire contract live in their own
pages (see "Where to look for X" below) and are **not** duplicated or rewritten here.

The Electron shell, the HTML frontend and the Tauri sidecar (`client/src-tauri`) were deleted in the rework
(task I23); the one place this page still describes them is the clearly labelled
"History: the Electron + Tauri-sidecar era" section.

## Process model

- **Relay server**: a single async process (tokio multi-thread runtime). The QUIC
  listener runs as a tokio task; the ratatui admin TUI runs on its own dedicated OS
  thread (not a tokio task), so crossterm's blocking terminal I/O never blocks the
  networking runtime — see `server/src/tui.rs`'s module doc comment.
  One tokio task per accepted QUIC connection ("connection actor") owns that
  connection's control bidi stream and `select!`s over control frames, media datagrams,
  and incoming preview uni-streams. Logs go to a daily-rolling file (`logs/`), never
  stdout/stderr, since the TUI owns the terminal — see `src/main.rs`.
- **Desktop client — one native process.** `main.rs` builds a tokio runtime and runs the
  eframe/egui window (wgpu backend); `--gallery` opens the widget gallery instead. The UI is
  a thin view over `session::AppState` (lib): every screen reads state through `Cx` and
  returns `UiAction`s that `ui/apply.rs` turns into calls on `AppState`, so no screen owns
  logic. `AppState` is polled once per frame from `App::update`; its async work (connect,
  media ingestion, reconnect) runs on the tokio runtime. `Why one process:` the earlier
  Electron + Rust-sidecar split put a loopback REST/SSE hop and a browser decoder between
  the pipeline and the screen; one binary lets the native decoder hand NV12 frames straight
  to the GPU renderer (`features/video-pipeline.md`) and removed a whole transport layer.
  Each active capture pipeline (`CaptureHandle` / `AudioCaptureHandle`) owns GStreamer
  element threads outside tokio, bridged into async Rust with `gstreamer-app`'s
  `AppSink::stream()` and `gstreamer`'s `Bus::stream()`. Video decode runs on its own thread
  per watched stream (`decode/video.rs`).
- **Browser client** (`docs/`): the same protocol in JS, no build step; see `features/website.md`.

## Packaging (Linux AppImage, Flatpak)

`./build-appimage.sh` at the repo root builds the client AppImage (and a relay tarball) inside
a Debian 13 container into `dist/`; it bundles GStreamer 1.26 and takes Mesa/libva/PipeWire/the
portal from the host. Files, supported hosts and the *why* (distros split GStreamer plugins
differently, so the client carries its own): `client/packaging/README.md`. `./build-flatpak.sh`
builds the client as a Flatpak bundle (`dist/simple-screensharing-client.flatpak`, runtime GStreamer,
manifest in `client/packaging/flatpak/`, section "Flatpak" of the same README). Windows (MSVC build, `client/packaging/windows/package.ps1` -> self-contained folder/zip):
`superdoc/features/windows.md`.

## Data flow

One sharer → relay → N viewers, all inside one room key's trust boundary (the relay
never has the room key and never decrypts anything):

1. A client connects over QUIC and sends `Join` (room id + a fresh-random 64-bit
   `ConnId`, doubling as its AEAD nonce prefix) on the control bidi stream; the relay
   replies with a presence snapshot (spec §4).
2. A sharer starts a capture (`capture.rs` for video, `audio.rs` for audio), which
   encodes, AEAD-encrypts (nonce = `ConnId ‖ counter`, AAD = `version ‖ stream_id`),
   fragments (`fragment.rs`), and sends media datagrams over the QUIC unreliable
   datagram plane (`net.rs`). It also feeds a low-rate preview branch
   (`preview.rs`) that sends periodic encrypted JPEG thumbnails over reliable
   unidirectional streams, broadcast to the whole room regardless of subscription.
3. The relay (`relay.rs`/`room.rs`) reads only cleartext header fields (`ConnId`,
   `stream_id`, frame/preview counters) and forwards the opaque ciphertext verbatim to
   subscribers of that stream — it never touches the encrypted payload.
4. A viewer's `net.rs` reassembles fragments and decrypts each access unit. **This is where
   video and audio diverge.** Audio access units go to `decode.rs`'s GStreamer `opusdec`
   pipeline for real playback (`autoaudiosink`). Video goes to the native decoder
   (`decode/video.rs`: codec sniffed from the bitstream, VA hardware decode with software
   fallback, NV12 `Arc<DecodedFrame>` out), and the wgpu renderer (`decode/render.rs`) draws
   it in the room view or a pop-out viewport (`ui/video.rs`, `ui/popout.rs`) — see
   `superdoc/features/video-pipeline.md`. The browser client does the same with WebCodecs.

Full byte-level detail (headers, nonce construction, message catalog) is the binding
spec, not this page — see `architecture/protocol-spec.md`. Where the real code
resolved something the spec/brainstorm left ambiguous, or turned out to work
differently once built, see `architecture/implementation-notes.md`.

## Module map

### Shared protocol crate — `protocol/src/lib.rs`

`simple-screensharing-protocol`: the single definition of the control-plane message enums
(postcard, frozen variant/field order), the fixed media and preview headers, `ConnId` /
`RoomId` / `StreamId`, limits (`MAX_NAME_LEN`, `MAX_ROOM_MEMBERS`, `MAX_MEDIA_DATAGRAM_LEN`),
the control-frame codec helpers and the cert-hash encoding, the NACK datagram layouts
(`NackRange`, `is_nack_datagram`) and `FragmentCache` (the bounded retransmit cache both relay and sender use; `protocol/src/cache.rs`), spec §6.6. `client/src/protocol.rs` and
`server/src/protocol.rs` re-export it. The `wire_bytes_unchanged` test pins the postcard bytes
of every enum variant against bytes captured from the pre-split code, so a refactor cannot
silently change the wire. There is no codec field: codec is sniffed from the bitstream
(`protocol-spec.md` §6.4).

### Server — `server/src/`

| Module | Role |
| --- | --- |
| `main.rs` | Entry point: CLI (`[PORT] [--cert FILE --key FILE] [--state-dir DIR] [--san NAME]... [--cert-validity-secs N] [--cert-rotate-before-secs N]`, see `--help`; the last two are test hooks that shorten the generated cert lifetime and rotation lead time), logging setup, starts the QUIC listener task and the TUI thread, on TUI quit or SIGINT/SIGTERM sets the TUI stop flag, runs `relay::shutdown` (close 0x200, ≤2 s), then `process::exit(0)` after the log guard drops (<0.1 s measured); a second signal forces exit(130). Why: the runtime used to wait forever on the `spawn_blocking` join of the TUI thread, which nothing told to stop. Check: `server/tests/exit_check.sh <bin> [port]`. |
| `protocol.rs` | Relay-only helpers over the shared `simple-screensharing-protocol` crate (`protocol/`, which owns the wire types, header layouts, transport constants and cert-hash encoding): the 64 KiB-capped control-frame codec and `clamp_utf8`. |
| `cert.rs` | The TLS certificate: ECDSA P-256 self-signed (13 days, persisted in `state/`, rotated when < 2 days left via `Endpoint::set_server_config`) or a provided CA chain (`--cert/--key`, reloaded on change; a failed reload keeps the old cert and is retried, `poll_provided`). Publishes the current hash/expiry for the TUI. See `protocol-spec.md` §11.3. |
| `transport.rs` | `TransportTuning`: the QUIC congestion controller (BBR), datagram send buffer, idle timeout (20 s) and keep-alive (5 s) of every relay connection, with `SSSHARE_CC` / `SSSHARE_DGRAM_BUF_KB` overrides; the native client mirrors it in `net.rs::build_client_endpoint`. Measurements and rationale: `superdoc/features/relay-transport.md`. `transport_lab.rs` (test-only, `#[ignore]`d) is the impairment lab that produced them. |
| `diag.rs` | Field diagnostics (#109): a 1 s sampler task prints one `diag stream` line per active stream and one `diag conn` line per busy connection every 60 s (and an `end=1` line when they end). The per-stream counters live in `StreamRoute::diag`, written by the media forwarder. Fields and reading guide: `superdoc/features/relay-transport.md` "Diagnostics log". |
| `client_logs.rs` | Storage of the opt-in client diagnostics uploads (#110): `logs/clients/<day>/<name>-<conn_id>.log`, name sanitising, control-character cleaning, 20 MiB / 500 MiB-per-day caps, 14-day retention (hourly prune), `SSSHARE_DIAG_UPLOAD=0` kill switch. `relay.rs` `handle_uni` dispatches the type-`0x02` uni stream to it after a `DiagHello`/`DiagAck`. `protocol-spec.md` §6.7, `features/relay-transport.md` "Client diagnostics upload". |
| `relay.rs` | Accept layer + relay logic. One UDP port, two ALPNs: `ssshare/1` (native, raw QUIC) and `h3` (browser, WebTransport on `/ssshare`), both turned into one `web_transport_quinn::Session` type so everything after `accept_session` is transport-agnostic. One connection-actor task per client plus one **media forwarder task** per client (`media_forward_task`) that owns that client's datagram fan-out, content-blind preview forwarding, keyframe-request debouncing. The actor only validates a datagram header and `try_send`s it to its forwarder over a bounded, drop-on-full queue, so fan-out never delays the actor's next read. **Viewer lists** (#116): a `ViewersHello` datagram marks the connection (`ConnState.viewers`), `RoomState::notify_viewers` sends the owner its stream's subscriber ConnIds after every subscriber-set change, `refresh_viewers` repeats non-empty lists every 2 s, `SSSHARE_VIEWERS=0` turns it off (`protocol-spec.md` §9.1, tests in `viewers_tests.rs`). **Emoji avatars** (#106): an `AVATAR_TAG_HELLO` datagram marks the connection (`ConnState.avatars`), `AVATAR_TAG_SET` stores a member's emoji (validated), the relay answers `AVATAR_TAG_ACK` and sends the whole `AVATAR_TAG_LIST` after every change, `refresh_avatars` repeats it every `AVATARS_REFRESH` (3 s), `SSSHARE_AVATARS=0` turns it off (`protocol-spec.md` §9.2). |
| `room.rs` | Room/connection/stream state (`Registry`): outer `RwLock` over the room map, per-room `Mutex` for **control-plane** mutations (join, announce, subscribe, leave); the **media data plane** deliberately avoids that mutex, reading a per-stream `StreamRoute` fan-out snapshot instead. Also the admin-log ring buffer (`EventLog`) the TUI reads. Read this module's docs before touching either — they carry the lock-ordering rules. |
| `tui.rs` | Ratatui admin TUI on its own OS thread: Overview (incl. native/browser connection counts and the TLS cert hash + expiry/rotation countdown) / Rooms / Room Detail tabs, reading the shared `Registry` via `blocking_read`/`blocking_lock`. |
| `nack_tests.rs` | Test-only. The relay's NACK behaviour against real quinn peers: Hello/HelloAck, a Request answered from the cache, a miss forwarded to the owner and its retransmit-flagged re-send routed only to the asker, `Miss` for an owner without Hello, the kill switch. |
| `avatars_tests.rs` | Test-only. The relay side of emoji avatars against real quinn peers: Hello/Ack, the whole list on every change, the periodic refresh, validation of what a member sets, and the old-peer behaviour (no Hello, or `SSSHARE_AVATARS=0`: nothing new is sent or kept). |
| `diag_upload_tests.rs` | Test-only. The relay side of the diagnostics upload against real quinn peers: ack, per-person file, refusals (no Hello, kill switch, oversize, wrong version), control characters, previews still fanning out through the new uni-stream dispatch. |
| `browser_smoke.rs`, `transport_tests.rs`, `tests/browser_smoke.html` | Test-only. `browser_smoke` drives **headless Chromium** (skips if none on `PATH`) through a real hash-pinned `new WebTransport(...)` join, datagrams and previews against an in-process relay with native peers; `transport_tests` covers ALPN dispatch and certificate rotation under live connections. |

### Client — `client/src/`

| Module | Role |
| --- | --- |
| `main.rs` | Entry point: CLI-flag parsing (`--gallery`, `--help`), tokio runtime setup, i18n init, then the eframe window (`ui::App`) or, with `--gallery`, the widget gallery. **The bin links the `simple-screensharing-client` lib** and declares only its own `ui`, `theme` and `widgets` modules; the lib modules (`audio`, `net`, `session`, ...) compile once. |
| `bandwidth.rs` | Pure `allocate()` bandwidth allocator: splits a total kbit/s budget across destinations by `width*height*fps` weight (largest-remainder rounding, floor/ceiling clamping with fixed-point redistribution, floor-overshoot-and-warn). No I/O, no wall-clock, no dependency on the room/capture/destination data model — plain data in (`AllocInput`), plain data out (`Allocation`). **Wired for real** by `session.rs`'s `BandwidthController` (Absolute-Maximum mode): an `AllocInput` is built from every active destination across every room, and each result is applied as a live encoder-bitrate write, behind a ~1s recompute rate limit and a 5%-or-100 kbit/s hysteresis threshold — both of which live *outside* this module so `allocate` stays pure. See `superdoc/features/stream-control.md`. |
| `protocol.rs` | Thin re-export of the shared `simple-screensharing-protocol` crate (`protocol/`, below). It used to hold an independently derived copy of the wire types; now the types cannot drift from the relay's. |
| `net.rs` | QUIC connect/join, control-message send/receive, AEAD encrypt/decrypt (`encrypt_frame`/`decrypt_frame`), the join-string format (`server|room-id|key[|certhash[|name]]`, see TERMINOLOGY "Join link"), and the `ClientEvent` channel everything else consumes. `build_client_endpoint()` builds the **one** local `quinn::Endpoint` a front-end owns; `ScreenshareClient::connect()` takes it by reference and opens a connection on it — see the "One endpoint, N connections" `Why:` in its module docs. `viewers_hello_loop` + the datagram loop turn the relay's viewer lists into `ClientEvent::Viewers` (#116, `protocol-spec.md` §9.1); `session.rs` keeps them in `RoomSession::viewers`, `ui/room.rs` `viewers_dropdown` shows them on the own card. |
| `logbuf.rs` | In-memory log ring (last 5,000 events / 2 MB; INFO+ own crates, WARN+ others; join-link keys redacted) as a tracing layer, plus `dump()` (header + lines) and `save_to_downloads()`, behind Settings > Diagnostics > Logs. Installed by `main.rs` `init_logging`. `ui/APP-SHELL.md`. |
| `diag_upload.rs` | Opt-in upload of the log ring to the relay (#110): `set_enabled` (the Settings checkbox, persisted as `send_diagnostics`), `UploadState` (per-connection cursor over the ring's sequence numbers, 30 s / 3 s-after-WARN cadence, 256 KiB batches), `upload_loop` (DiagHello until DiagAck, one uni stream per batch, waits for delivery). Spawned by `ScreenshareClient::connect`. `protocol-spec.md` §6.7. |
| `fragment.rs` | Pure, synchronous frame↔datagram fragmentation and reassembly (`fragment_frame` / `Reassembler`) — no I/O, unit-testable in isolation. `fragment_frame_with_flags` is the same thing with the header's whole `flags` byte supplied directly, used only by `capture.rs`'s stall protocol (which also raises `MEDIA_FLAG_STALLED`). The `Reassembler` also has the NACK mode (`set_nack`, `poll`: detects holes, builds Requests, holds later frames so they leave in order). |
| `nack.rs` | Native side of selective retransmission (#104): `SendRing` (the sender's last 2 s of video datagrams, answers relay-forwarded Requests with retransmit-flagged copies), `hello_loop` (capability probe), `enabled()` (`SSS_DISABLE_NACK=1`). The datagram loop in `net.rs` wires `Reassembler` and `SendRing` to the wire. `protocol-spec.md` §6.6. |
| `capture.rs` | Screen capture → H.264 / H.265 / AV1 encode (`features/stream-control.md` "Video codec selection") → encrypt → fragment → send. **The pipeline contains no fixed encoder branch**: it is `pipewiresrc(DMA-BUF) ! vapostproc ! VAMemory NV12 ! tee` (GPU domain; `source ! videoconvert ! capsfilter(one pinned raw format) ! tee` in the System domain, see `CaptureDomain`) plus the two fixed preview branches, and *every* outgoing destination — including the first — is a dynamic branch added to that tee (`spawn_destination`), with its own encoder, bitrate, pause valve, `stream_id`, room key and pump task (`destination_pump`). One capture therefore feeds N rooms as N independently encoded and encrypted streams. `CaptureHandle` owns the pipeline plus a `Vec<DestinationHandle>` and exposes `spawn_add_destination`/`spawn_remove_destination` (fan-out and transfer), `change_destination` (the single classified entry point for settings edits) and `set_source_framerate` (tier 3, which only records a raised rate — the real source is variable-rate and has no ceiling to restart for). See `superdoc/features/stream-control.md`. Linux: `ashpd` portal + PipeWire, behind `#[cfg(unix)]`. Windows: `d3d11screencapturesrc`, behind `#[cfg(windows)]`. Auto-selects VAAPI (`vah264enc`) hardware encoding with automatic `x264enc` software fallback. Also drives the 1 fps room-preview tee branch and the valve-gated own-tile live preview branch (#78): `superdoc/features/self-preview.md`. **`capture::branch` submodule**: adds/removes/swaps *dynamic* encode branches on the live pipeline's shared `tee` at runtime (`add_branch` sync; `remove_branch` async — block tee src pad, inject EOS, drain appsink, then unlink/remove/null/release; `swap_branch` = make-before-break, never touches the old branch). These are the GStreamer mechanics behind multi-destination fan-out, per-destination live settings changes, and stream transfer. Stress-tested (50 add/remove cycles, sibling-branch and fixed-branch isolation). Four invariants are load-bearing, all documented in-code: every branch needs `queue leaky=downstream` (a non-leaky queue blocks upstream through the shared tee and stalls *every* branch — the original "freezes solid under sustained motion" bug); `remove_branch`'s pad probe must return `PadProbeReturn::Drop`, not `Remove` (letting the blocked buffer reach an already-EOS'd branch returns `GST_FLOW_EOS` upstream, which makes the capture source shut down and push a real EOS through the shared tee, killing every branch and poisoning every future request pad via sticky-event forwarding); each branch has **exactly one** `AppSinkStream` for its whole life (`EncodeBranch::stream`, created lazily by `next_sample`, reused by `remove_branch` — see `session-model.md` "Per-destination lifecycle"); and adding a branch to the live tee must **not** let a `RECONFIGURE` event reach upstream of that tee (`isolate_source_from_branch_renegotiation` drops it at the tee's sink pad — forwarded, it makes `pipewiresrc` renegotiate with the portal producer, which then never sends another buffer, so the first destination of every share silently killed the source it was encoding; a `videotestsrc` renegotiates instantly, which is why the whole unit suite stayed green while real sharing was broken). **`capture::destination` submodule**: the `DestinationState` machine, the monotonic wire-PTS invariant, the stall protocol and the three-tier change model — GStreamer-free and unit-tested against injected `Instant`s. See `session-model.md` "Per-destination lifecycle" and `superdoc/brainstorm/ui-redesign-multiroom-streaming.md`'s "branch-swap" design. **Live encoder-tuning controls** (`TuneMode`: `LowLatency`/`BestQuality`; `QualityLevel`: `Performance`/`Quality`) are per-destination knobs, each routed through tier 2 (`BranchSwap`) like resolution/fps/backend, mapped to `x264enc`/`vah264enc` properties per backend in `EncoderBackend::launch_fragment` (`tune` has no VAAPI analogue and is a documented no-op there; there is no rate-control knob any more, encoding is always Dynamic/VBR, #88, and the tune is no longer user-visible, it follows the preset). **A webcam** (I8, Linux only) is a second capture kind next to the screen: `capture::webcam` (`list_cameras`, modes, `v4l2src` ! `jpegdec`/raw ! the same encode branch; `SSS_FAKE_CAMERA=1` is the rig's fake camera), an ordinary `StreamKind::Video` stream whose label starts with `Webcam` (no wire change). See `superdoc/features/stream-control.md`'s "Encoder tuning controls" and "Webcam capture". |
| `audio.rs` | Per-application audio capture via PipeWire CLI tools (`pw-dump`/`pw-cli` null sink/`pw-link`, #121), Opus encode → encrypt → send. Linux-only; no Windows equivalent exists yet (`superdoc/features/windows.md`). Sender pipeline has a live gain stage (`volume` + `rglimiter`, #114) and runs on the monotonic system clock, never `pipewiresrc`'s (#128, `features/audio-sharing.md`). A 1 s poll links/re-links the streams of the shared name into the running chain. Since I7 (2026-11-04) **Desktop audio** (every app's streams except those on the **exclusion list**, `AppSettings::audio_exclusions`, matched by name) is the default source of the audio button, apps are sorted into **source groups** (`classify`), and an **audio-only share** (no screen needed) shows to viewers as a Listen tile; all Linux only, the browser has audio-only shares but no app list (`features/audio-sharing.md`, `features/website.md`). |
| `audio_dump.rs` | Debug dumps of shared audio, only with `SSS_AUDIO_DUMP=<dir>` (#128): the sender's and the native viewer's Opus packets (`.pkt`, decode with `client/scripts/vm-audio/pktdump.py`) and the viewer's jitter-buffer/timeline actions (`.log`). Tells "sent broken" from "played broken". |
| `audio_gain.rs` | Pure logic of the sender-side audio boost (#114): the gain range (50-200 %, #119) and its clamp/dB conversions. `features/audio-sharing.md`. |
| `leveller.rs` | Viewer-side loudness levelling (#119): `Leveller` (slow AGC to -18 dBFS RMS + zero-attack -1 dBFS peak limiter, pure DSP on interleaved f32), `install` (a buffer pad probe on the decode chain's F32 capsfilter, zero latency) and the process-wide `ENABLED` switch behind Settings > Playback > Level loudness. Browser twin: `docs/js/core/leveller.js`. `features/audio-sharing.md`. |
| `sounds.rs` | Notification sounds (I9): four generated sounds (join / leave my stream, someone else starts / stops one; no files shipped), the pure `synth`, the pure per-room edge detector `RoomSounds`, and `play` (one short GStreamer burst behind a `Gate`; off until `ui::App::new` calls `enable_output`, so tests and `--gallery` never play; `SSS_SOUND_SINK` replaces `autoaudiosink`, the rig sets `fakesink`). Own volume (`AppSettings::sound_volume`), Settings > Sounds. Browser twin `docs/js/ui/sounds.js`, the replica's `SOUNDS`: keep the three equal. `features/audio-sharing.md`. |
| `decode.rs` + `decode/` | GStreamer decode for both media kinds. **Video (native path, shown on screen by the wgpu renderer `decode/render.rs` + `render.wgsl`; `ui/video.rs` keeps one `VideoView` per watch):** `decode/codec.rs` sniffs H.264 / H.265 / AV1 from the first keyframe access unit (no wire tag, `protocol-spec.md` §6.4); `decode/video.rs` runs the pipeline on its own thread — `appsrc ! {h264,h265,av1}parse ! va{h264,h265,av1}dec ! video/x-raw,format=NV12 ! appsink(sync=false)` — with a software fallback (`avdec_h264` / `avdec_h265` / `av1dec`; dav1d isn't installed) on build/link failure, bus error or no frame within 2 s, replaying the pushed AUs and remembering the choice per codec; `decode/frame.rs` holds the zero-copy `Arc<DecodedFrame>` (Y/UV slices, strides, `pts_us`, `YuvMatrix`, `full_range`) and the cloneable `FrameSource` (`frame_counter`, `latest_if_newer`, `note_presented`, `stats`). Decoded fps (pad probe) and presented fps are counted separately. `StreamDecoder::latest_frame() -> ColorImage` (CPU NV12→RGBA) remains only for tests. Details and the `Why:` notes: `features/video-pipeline.md` "Native decode". **A/V sync** (`decode/avsync.rs`, 2026-11-01): audio is delayed to match the video's latency (per-sender latency floors found through a `ConnId`-keyed registry; one process-wide sender clock for wire PTS), `features/audio-sharing.md`. **Audio is live**: `opusdec` → F32 capsfilter with the loudness-leveller probe (#119) → `volume` → `autoaudiosink`, fronted by an `AudioJitterBuffer` that restores PTS order and turns lost packets into `GAP` events so `opusdec` (`plc=true`) can conceal them (each packet's length comes from its Opus TOC byte, #128). `PacingState` (pts-budget design, `Why:` note below) is kept as a `pub` building block for a presenter; the decoder itself no longer paces. **Playout buffer** (`decode/playout.rs`, row 136): a per-stream delay before the decoder that evens out a clump of late frames (a keyframe's transfer); `features/video-pipeline.md` "Playout buffer". **Viewer recovery** (`decode/recovery.rs`, #101, tests `recovery_tests.rs`): the pure L1/L2/L3 state machine behind `StreamDecoder::tick` (keyframe request, decoder reset, room reconnect); "Viewer recovery" in the same doc. **Timing report** (`decode/timing.rs`, row 133): where a frame spends its time between its first fragment and the screen, one `video timing` log line per window. `decode.rs` also does preview JPEG decode (used by the sidebar/Streams-list thumbnails, still live). |

| `preview.rs` | Periodic (~30s) thumbnail sender: capture-agnostic, fed a generic `FrameStream`, downscales + JPEG-encodes + encrypts + sends over a reliable QUIC uni-stream. Shares the connection's single nonce counter with video/audio. |
| `rooms.rs` | Local TOML-backed storage of saved rooms ("lounges") — stores the whole join string plus a friendly name. Also `AppSettings` — preferences only, never session state: language, render presentation mode, and the Stream Control bandwidth preferences (`bandwidth_mode`, `absolute_total_kbps`, `destination_floor_kbps`; the two modes' numbers are separate fields so switching modes is lossless). |
| `theme.rs` | Central egui theme (bin-only): design tokens (colors, sizes, spacing, radii, type scale, embedded fonts, icon lookup) and `theme::apply`. Rules and conventions: `superdoc/ui/STYLING-GUIDE.md`. |
| `widgets/` | Bin-only egui building blocks: `layout.rs` (vbox/hbox/fixed-slot/fill-slot `HBox` with explicit width math, since egui has no flexbox), `paint.rs`, `controls.rs`, `rows.rs` (the slot-row patterns), `containers.rs`, `emoji.rs` (Emoji avatars: any emoji drawn from the embedded Noto Color Emoji font (OFL 1.1; one grapheme, <= 16 bytes, shaping to one bitmap glyph, else the initial), the colour taken from the emoji, the crop centred on its balance point (`balance_point`: midpoint of mass centre and enclosing-circle centre), the picker of 56 (`EMOJI`); `AvatarList` is the roster-order list of who has which emoji), `gallery.rs` (the `--gallery` screen for design QC). Pure view: plain data in, small action enums out, no app state. Documented in `superdoc/ui/STYLING-GUIDE.md`. |
| `presets.rs` | The encoder presets (lib module): `EncoderPreset` {Text, Quality, Gaming} (2026-11-04, plan D8; replaced Low-Latency / Balanced / Quality / Gaming and the Advanced tuning rows) mapping to the FPS it starts from, a `QualityLevel` (only Performance and Quality remain; old stored Balanced / Low-Latency load as Quality) and the keyframe interval (Text 15 fps / Quality / 250 ms, Quality 60 / Quality / 1000 ms, Gaming 120 / Performance / 250 ms), `EncoderPreset::detect` (the active preset is always derived from quality level + keyframe interval, never stored; the FPS is applied once and not part of detection), the hidden tune (Gaming = LowLatency, others BestQuality), inherent `apply_preset*` / `active_preset` methods on `DestEdit` and `AppState`, and `framerate_options_for`. `Why in the lib:` the egui UI must stay a thin view over `AppState`, so the logic lives next to it. Rate control is not a field: always Dynamic/VBR (#88). `features/stream-control.md` "Presets". |
| `session.rs` | The state/logic layer (extracted from the old single-file `ui.rs` during the Tauri port, kept as the UI-independent model since): `AppState` (renamed/restructured `App`) and the **client session model** (rooms / captures / destinations, described in `session-model.md`), plus every business-logic method that used to live in `impl App`/`impl RoomSession` in `ui.rs` — connect/room lifecycle (`start_connect`, `insert_room`, `disconnect_room`, `leave_needs_confirm`, `request_leave_room`, `poll_connect_result` returning a [`ConnectOutcome`](#) so UI-only fields the "+ Add Room" card owns never have to live on `AppState`, `drain_events`, `rename_saved_room`), watching (`toggle_watch`, `toggle_listen`), capture/share (`start_video_share`, `start_audio_share`, `stop_share`, `poll_capture_states`, `refresh_audio_sources`), **stage-2 fan-out/transfer/edit** (`add_destination` — fan-out, `transfer_destination` — make-before-break, `poll_pending_destinations`, `apply_destination_edit`/`poll_destination_edits` routed through `capture::destination::classify_change`, `with_destination`), **bandwidth allocation** (`BandwidthController` — rate-limited/hysteresis-filtered live wiring for Absolute-Maximum mode, `apply_bandwidth_allocation`, `set_bandwidth_mode`, `reapply_manual_bitrates`, `allocation_input`), `spawn_media_ingestion_task`, and the `RoomHandle`/`CaptureSourceId`/`DestinationId`/`PendingRole` types. Deliberately egui-light: it depends on `egui` only for the handful of types (`egui::TextureHandle`) that already lived on this data, and `drain_events` still takes `&egui::Context` to upload preview thumbnails — both pre-existing, not new dependencies introduced by the split. Exported from `lib.rs` (`pub mod session;`, which also registers `rust_i18n`'s `t!` macro for the lib target) so tests and tools can drive the session logic without `eframe` rendering. `ui/`, `theme.rs` and `widgets/` stay bin-only and do the egui rendering. **Since I4 (2026-10-14)** the behaviour that used to exist only in `src-tauri/src/commands.rs` also lives on `AppState`, so every front-end gets it: `create_room` / `join_room` / `forget_room`, the settings setters with their clamps (`set_language`, `set_display_name`, `set_presentation_mode`), start-stream defaults, `edit_destination`, `set_bandwidth_total` / `set_bandwidth_floor`, `allocated_kbps` / `bandwidth_overshoot_kbps`, the audio-level tap (`insert_audio_level_tap` & co.), volume resolution, and `supported_codecs`. `commands.rs` is now thin. **Since task #36** it also owns auto-reconnect (`reconnects`, `suspend_room`, `poll_reconnects`, `reconnect_now`, `cancel_reconnect`, `reconnect_info`, the pure `backoff_delay`/`retry_delay` policy) and clean exit (`shutdown`/`shutdown_blocking`, `install_shutdown_signal_handlers`); see `client-reconnect-and-watches.md`. |
| `ui/` | The native egui UI (bin-only): one window, room-centric. `mod.rs` is the shell (`App::update` polls the session, lays out a left sidebar 272 px, a 56 px room header band and the central area, which is the stage (streams, people strip, control bar) for a joined room and Watched streams, then modals, pop-out viewports and toasts) and holds the contract every screen follows: one `<X>State` plus `show*` functions taking `&mut Ui, &mut Cx`. Screens: `sidebar.rs` (brand, rooms, user band), `room.rs` (header, body), `stage/` (the **stage**, see below), `menu.rs` (the app's own dropdown field `select` and `switch_row`; every dropdown goes through it, the stream-settings modal's too), `share.rs` (the bar's Share screen and Share audio / Webcam menu bodies, level taps, "Also send to / Move to" lists, formatting; this was `action_bar.rs` until 2026-11-04), `stream_settings.rs` (the staged/live stream-settings modal), `audio_settings.rs` (the audio settings modal: source, Boost, the exclusion list; `Modal::AudioSettings`), `webcam_settings.rs` (the webcam modal: camera, mode, codec, bitrate; `Modal::WebcamSettings`), `modals.rs` (create/join, settings incl. Sounds, leave/forget, rename, choose name), `popout.rs` (one deferred viewport per popped-out stream, and the popped-out stage window), `video.rs` (per-stream GPU `VideoView`s synced to the watches), `apply.rs` (the one place `UiAction`s change state, as thin calls into `session.rs`), `drive_dump.rs` (the `SSS_UI_DUMP` test hook of the rig's scripts: control rects and values, names in `client/scripts/README.md`), `tests.rs`. **`ui/stage/`** (tasks I1-I10): `geom.rs` (pure geometry: Split / Crop grid, resize bars, Floating windows, the webcam-in-picture maths; tested without egui), `mod.rs` (the stage: arena, layout modes, per-view state), `tile.rs` (what a tile shows: watched video + hover overlay, a stream you can still watch, an audio-only sender's Listen tile, your own live preview `mine_tile`), `bar.rs` (the control bar with its split buttons), `strip.rs` (people strip, Emoji avatars, ring, LIVE badge, hover card), `menus.rs` (the bar's dropdowns and the stream menu, drawn from `Nav::popover`), `chrome.rs` (the pure fullscreen auto-hide rule), `pip.rs` (one webcam in picture, drag / snap to a PIP spot / resize). Vocabulary: `meta/TERMINOLOGY.md`. Screens never mutate `AppState` themselves. Layout, row states and decisions: `superdoc/ui/APP-SHELL.md`; widget and token rules: `superdoc/ui/STYLING-GUIDE.md`; render presentation mode: `superdoc/features/presentation-modes.md`; own-tile live preview: `superdoc/features/self-preview.md`; stream-control feature set (fan-out, transfer, live edits, bandwidth modes): `superdoc/features/stream-control.md`. |
| `end_to_end_test.rs` | Real integration test: two in-process `ScreenshareClient`s + a real relay server child process, `videotestsrc` standing in for portal capture. `#[cfg(test)]`-only, declared in `lib.rs`. Finds the relay binary under the workspace `target/{debug,release}` (newest wins) unless `SSSHARE_SERVER_BIN` / `SSSHARE_SERVER_CWD` override it; `audio.rs`'s live test reuses `spawn_relay_server`. |

`Why:` (video pacing, `PacingState`) a design that only reset pacing on individual frames each found
badly-behind on their own (even a "second consecutive" one) never engages when a sender's
sustained output framerate exceeds what a viewer can decode/display by a steady margin
(e.g. 240fps sender vs. 120fps-sustained viewer): the per-frame lag accumulates in tiny
increments that never individually cross the threshold, so it's never seen as "badly
behind" at all and nothing gets skipped — that showed up as slow, ever-worsening drift
instead of a stall. `PacingState::decide` instead runs a small accumulator (a `pts_budget`
that every frame adds its own pts-time advance to and every *displayed* frame spends
real-elapsed-time out of, the standard DDA/Bresenham technique for frame-rate
down-conversion): it settles into skipping close to the exact proportion the sustained
mismatch demands (e.g. ~50% for the 240-vs-120 case) instead of 0% or ~100%, and recovers
within a frame or two once the mismatch resolves. A single pathological single-step gap
(multi-second stall/suspend) still hard-resets the budget and displays immediately rather
than spending many frames paying the backlog down. See `architecture/implementation-notes.md`'s
"Bugs found via manual testing (2026-07-25)" in `bug-history.md` for the full writeup.


## Client session model (rooms / captures / destinations)

The client's *ephemeral* per-launch state lives on `session.rs`'s `AppState` in three collections;
nothing here is persisted (only `rooms.rs`'s `RoomStore` and `AppSettings` survive a restart):

- **`RoomSession`** (`AppState::rooms`, keyed by `RoomHandle`): one joined room = one QUIC connection.
- **`LocalCapture`** (`AppState::captures`, keyed by `CaptureSourceId`): one local GStreamer pipeline (screen or audio source).
- **`Destination`** (`LocalCapture::destinations`, keyed by `DestinationId`): one room a capture is sent into, with its own encoder, room key and `stream_id`.

Captures own their destinations (not rooms own captures) because one capture can feed several rooms
and leaving a room must not tear down a pipeline another room still uses. Cross-room pieces
(`BandwidthController`, `pending_destinations`), the remaining single-room seams (`pending_connect`,
the global `status`) and all the `Why:` notes: **`architecture/session-model.md`**.

## Per-destination lifecycle (state machine, wire PTS, stall protocol)

Lives in `capture.rs`'s `destination` (pure state) and `branch` (GStreamer mechanics) submodules.
Three change tiers (`classify_change`: bitrate write / branch swap / source restart), the
`DestinationState` machine (`Starting`, `Live`, `Rebuilding`, `Stalled`, `Failed`), the stall
keepalive protocol (`MEDIA_FLAG_STALLED` is advisory only), the wire-PTS continuity invariant,
the single-`AppSinkStream` rule and per-branch encoder fallback: **`architecture/session-model.md`**
("Per-destination lifecycle"). Code comments in `capture.rs` cite this heading.

## Where to look for X

- Overall plan, scope, original design decisions → `superdoc/brainstorm/simple-screensharing-platform.md`
- Binding wire protocol (headers, nonce construction, message catalog) → `superdoc/architecture/protocol-spec.md`
- Browser/WebTransport leg: URL path, ALPN, pinned-cert lifecycle, join-link `certhash` field, the 1000-byte datagram cap → `protocol-spec.md` §11 and §6.3
- Where the real implementation deviated from or refined the plan/spec, and bugs/QC findings from building it → `superdoc/architecture/implementation-notes.md` (index of the topic pages; also the removed-Electron history and the status list), `client-reconnect-and-watches.md`, `webtransport-cert.md`, `protocol-decisions.md`, `relay-media-forwarding.md`, `bug-history.md`
- Documentation discipline for this workspace → `superdoc/claude-instructions/documentation.md`
- Project vocabulary → `superdoc/meta/TERMINOLOGY.md`
- Desktop client's egui theme/widget conventions, the slot layout model, `--gallery` → `superdoc/ui/STYLING-GUIDE.md`, `client/src/widgets/mod.rs`, `client/src/theme.rs`
- An HTML replica of the native UI to sketch redesigns in (fake data, tokens named after `theme.rs`) → `superdoc/ui/replica/index.html` (STYLING-GUIDE "HTML replica")
- Desktop client's window layout — behavioral rules (sidebar, ROOMS/Outgoing sections, room-row states, empty states) → `superdoc/ui/APP-SHELL.md`; code map → the `ui/` row of the module map above and `client/src/ui/mod.rs`'s module doc. (`superdoc/ui/FRONTEND-SHELL.md` documents the removed HTML frontend: history only.)
- Desktop client's render presentation-mode (vsync/Immediate/Mailbox) setting → `superdoc/features/presentation-modes.md`
- Sharer's own-tile live preview (native + browser, focus-gated) → `superdoc/features/self-preview.md`
- Selective retransmission (NACK) of lost media fragments, relay cache, sender ring, compatibility → `protocol-spec.md` §6.6, `superdoc/features/video-pipeline.md` "Selective retransmission" ; browser side (`docs/js/core/nack.js`, `Reassembler` NACK mode, #111) → `superdoc/features/website.md` "Selective retransmission in the browser"
- QUIC congestion control / datagram eviction / idle timeout of relay and native client, loss and narrow-link behaviour → `superdoc/features/relay-transport.md`
- Per-destination state machine, wire-PTS continuity, stall protocol, three-tier change model → `architecture/session-model.md` "Per-destination lifecycle", and `capture.rs`'s `destination`/`branch` submodules
- Multi-destination fan-out, stream transfer between rooms, the two bandwidth modes, the allocator's live wiring, and the encoder presets and Quality level → `superdoc/features/stream-control.md`
- Wire types/constants (single definition) → `protocol/src/lib.rs`; codec detection (H.264/H.265/AV1, no wire tag) → `client/src/decode/codec.rs` and `protocol-spec.md` §6.4
- Native video decode (VA hardware + software fallback, NV12 `DecodedFrame`, `FrameSource`, decode benchmark `client/examples/decode_bench.rs`) → `superdoc/features/video-pipeline.md` "Native decode" and `client/src/decode.rs`'s module doc
- Encoder presets (Text/Quality/Gaming) → `client/src/presets.rs`, `superdoc/features/stream-control.md` "Presets"
- Video delivery to the screen — native decode, the wgpu renderer, how encoder-tuning changes interact with an in-flight watch → `superdoc/features/video-pipeline.md`
- The per-stream pop-out window (F11 fullscreen, multi-window) → `superdoc/features/viewer-window.md`, `client/src/ui/popout.rs`
- The process model (one native client process, relay threads) → "Process model" above; the removed Electron + sidecar split → `architecture/implementation-notes.md` "History: the Electron + Tauri-sidecar era (removed)"
- Linux AppImage / Flatpak packaging (container build, bundled GStreamer, supported hosts) → `client/packaging/README.md`; Windows (MSVC build, `package.ps1`, verified headless; GUI/capture unverified) → `superdoc/features/windows.md`
- The browser client (core, UI, dev harness) → `superdoc/features/website.md`, `docs/js/core/README.md`, `docs/js/ui/README.md`, `docs/dev/README.md`
- Scripted real-app test helpers and how to launch the client safely → `client/scripts/README.md`, `client/.claude/skills/run/SKILL.md`, `superdoc/claude-instructions/testing-and-verification.md`
- Client session model (rooms / captures / destinations), the destination state machine, wire PTS, stall protocol → `superdoc/architecture/session-model.md`
- Auto-reconnect, suspended rooms, clean exit, resume-after-stop → `superdoc/architecture/client-reconnect-and-watches.md`
- Relay WebTransport leg and the rotating pinned certificate (decisions) → `superdoc/architecture/webtransport-cert.md`
- Why the wire looks the way it does (ConnId = nonce prefix, frame_id, AAD, PTS, postcard) → `superdoc/architecture/protocol-decisions.md`
- Relay media forwarding and lock design → `superdoc/architecture/relay-media-forwarding.md`
- Bugs found building it (pacing, audio, capture stop) → `superdoc/architecture/bug-history.md`
- What is built vs pending, the removed Electron + Tauri era → `superdoc/architecture/implementation-notes.md`
