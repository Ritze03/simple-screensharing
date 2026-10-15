# Architecture Overview

One-paragraph orientation to this file's role: the first doc to read, links out to
sibling docs rather than duplicating them.

## Big picture

`simple-screensharing` (this directory) is **one monorepo** (private GitHub repo
`Ritze03/simple-screensharing`, the rework landed on `master` 2026-10-15) holding the relay server, the
desktop client, the shared wire-protocol crate and this documentation. It used to be a docs-only
workspace over separate `simple-screensharing-server` / `simple-screensharing-client` repos;
those were imported with full history (old branches kept as `archive/client/*` and
`archive/server/*`; deleted from GitHub 2026-10-15, kept in the local git bundle `../simple-screensharing-archive-branches.bundle`, restore: `git fetch <bundle> 'refs/heads/archive/*:refs/heads/archive/*'`). `Why one repo:` server, desktop client and the browser client must
change in lockstep.

| Path | Role |
| --- | --- |
| `server/` | Rust QUIC relay (`simple-screensharing-server`): broadcasts encrypted room traffic between clients, never decrypts it, plus a ratatui admin TUI. Also accepts browsers over WebTransport. |
| `client/` | Desktop client: one native egui app. The Rust library + bin crate (`simple-screensharing-client`, `client/src/`) holds capture, encode, encrypt, decode and session state in the lib (`session.rs`'s `AppState` is the model) and the egui UI in the bin (`client/src/ui/` screens, `client/src/widgets/` building blocks, `theme.rs` tokens). Video is decoded natively (VA-API with software fallback, `client/src/decode/`) and drawn by a wgpu renderer. Linux first; Windows builds (MSVC + GStreamer 1.26.9) and is verified headless Linux<->Windows, see `superdoc/features/windows.md`. Packaging: `client/packaging/`. |
| `protocol/` | Shared crate `simple-screensharing-protocol`: the single definition of wire types, header layouts and constants. `client/src/protocol.rs` and `server/src/protocol.rs` re-export it. Spec: `protocol-spec.md`. |
| `superdoc/` | This agent-facing knowledge base. |
| `docs/` | The browser client (static site served by GitHub Pages, WebTransport + WebCodecs): watch and share from a tab. `docs/js/core/` is the protocol and media core (`docs/js/core/README.md`), `docs/js/ui/` the UI (`docs/js/ui/README.md`), `docs/dev/` the local test harness (`docs/dev/README.md`); map and rationale in `superdoc/features/website.md`. The relay accepts browsers over WebTransport on the same UDP port as native clients, with a pinned auto-rotating self-signed cert whose hash rides in the join link (`protocol-spec.md` §11). |

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
the control-frame codec helpers and the cert-hash encoding. `client/src/protocol.rs` and
`server/src/protocol.rs` re-export it. The `wire_bytes_unchanged` test pins the postcard bytes
of every enum variant against bytes captured from the pre-split code, so a refactor cannot
silently change the wire. There is no codec field: codec is sniffed from the bitstream
(`protocol-spec.md` §6.4).

### Server — `server/src/`

| Module | Role |
| --- | --- |
| `main.rs` | Entry point: CLI (`[PORT] [--cert FILE --key FILE] [--state-dir DIR] [--san NAME]... [--cert-validity-secs N] [--cert-rotate-before-secs N]`, see `--help`; the last two are test hooks that shorten the generated cert lifetime and rotation lead time), logging setup, starts the QUIC listener task and the TUI thread, on TUI quit or SIGINT/SIGTERM sets the TUI stop flag, runs `relay::shutdown` (close 0x200, ≤2 s), then `process::exit(0)` after the log guard drops (<0.1 s measured); a second signal forces exit(130). Why: the runtime used to wait forever on the `spawn_blocking` join of the TUI thread, which nothing told to stop. Check: `server/tests/exit_check.sh <bin> [port]`. |
| `protocol.rs` | Relay-only helpers over the shared `simple-screensharing-protocol` crate (`protocol/`, which owns the wire types, header layouts, transport constants and cert-hash encoding): the 64 KiB-capped control-frame codec and `clamp_utf8`. |
| `cert.rs` | The TLS certificate: ECDSA P-256 self-signed (13 days, persisted in `state/`, rotated when < 2 days left via `Endpoint::set_server_config`) or a provided CA chain (`--cert/--key`, reloaded on change). Publishes the current hash/expiry for the TUI. See `protocol-spec.md` §11.3. |
| `relay.rs` | Accept layer + relay logic. One UDP port, two ALPNs: `ssshare/1` (native, raw QUIC) and `h3` (browser, WebTransport on `/ssshare`), both turned into one `web_transport_quinn::Session` type so everything after `accept_session` is transport-agnostic. One connection-actor task per client plus one **media forwarder task** per client (`media_forward_task`) that owns that client's datagram fan-out, content-blind preview forwarding, keyframe-request debouncing. The actor only validates a datagram header and `try_send`s it to its forwarder over a bounded, drop-on-full queue, so fan-out never delays the actor's next read. |
| `room.rs` | Room/connection/stream state (`Registry`): outer `RwLock` over the room map, per-room `Mutex` for **control-plane** mutations (join, announce, subscribe, leave); the **media data plane** deliberately avoids that mutex, reading a per-stream `StreamRoute` fan-out snapshot instead. Also the admin-log ring buffer (`EventLog`) the TUI reads. Read this module's docs before touching either — they carry the lock-ordering rules. |
| `tui.rs` | Ratatui admin TUI on its own OS thread: Overview (incl. native/browser connection counts and the TLS cert hash + expiry/rotation countdown) / Rooms / Room Detail tabs, reading the shared `Registry` via `blocking_read`/`blocking_lock`. |
| `browser_smoke.rs`, `transport_tests.rs`, `tests/browser_smoke.html` | Test-only. `browser_smoke` drives **headless Chromium** (skips if none on `PATH`) through a real hash-pinned `new WebTransport(...)` join, datagrams and previews against an in-process relay with native peers; `transport_tests` covers ALPN dispatch and certificate rotation under live connections. |

### Client — `client/src/`

| Module | Role |
| --- | --- |
| `main.rs` | Entry point: CLI-flag parsing (`--gallery`, `--help`), tokio runtime setup, i18n init, then the eframe window (`ui::App`) or, with `--gallery`, the widget gallery. **The bin links the `simple-screensharing-client` lib** and declares only its own `ui`, `theme` and `widgets` modules; the lib modules (`audio`, `net`, `session`, ...) compile once. |
| `bandwidth.rs` | Pure `allocate()` bandwidth allocator: splits a total kbit/s budget across destinations by `width*height*fps` weight (largest-remainder rounding, floor/ceiling clamping with fixed-point redistribution, floor-overshoot-and-warn). No I/O, no wall-clock, no dependency on the room/capture/destination data model — plain data in (`AllocInput`), plain data out (`Allocation`). **Wired for real** by `session.rs`'s `BandwidthController` (Absolute-Maximum mode): an `AllocInput` is built from every active destination across every room, and each result is applied as a live encoder-bitrate write, behind a ~1s recompute rate limit and a 5%-or-100 kbit/s hysteresis threshold — both of which live *outside* this module so `allocate` stays pure. See `superdoc/features/stream-control.md`. |
| `protocol.rs` | Thin re-export of the shared `simple-screensharing-protocol` crate (`protocol/`, below). It used to hold an independently derived copy of the wire types; now the types cannot drift from the relay's. |
| `net.rs` | QUIC connect/join, control-message send/receive, AEAD encrypt/decrypt (`encrypt_frame`/`decrypt_frame`), the join-string format (`server|room-id|key[|certhash[|name]]`, see TERMINOLOGY "Join link"), and the `ClientEvent` channel everything else consumes. `build_client_endpoint()` builds the **one** local `quinn::Endpoint` a front-end owns; `ScreenshareClient::connect()` takes it by reference and opens a connection on it — see the "One endpoint, N connections" `Why:` in its module docs. |
| `fragment.rs` | Pure, synchronous frame↔datagram fragmentation and reassembly (`fragment_frame` / `Reassembler`) — no I/O, unit-testable in isolation. `fragment_frame_with_flags` is the same thing with the header's whole `flags` byte supplied directly, used only by `capture.rs`'s stall protocol (which also raises `MEDIA_FLAG_STALLED`). |
| `capture.rs` | Screen capture → H.264 / H.265 / AV1 encode (`features/stream-control.md` "Video codec selection") → encrypt → fragment → send. **The pipeline contains no fixed encoder branch**: it is `pipewiresrc(DMA-BUF) ! vapostproc ! VAMemory NV12 ! tee` (GPU domain; `source ! videoconvert ! capsfilter(one pinned raw format) ! tee` in the System domain, see `CaptureDomain`) plus the two fixed preview branches, and *every* outgoing destination — including the first — is a dynamic branch added to that tee (`spawn_destination`), with its own encoder, bitrate, pause valve, `stream_id`, room key and pump task (`destination_pump`). One capture therefore feeds N rooms as N independently encoded and encrypted streams. `CaptureHandle` owns the pipeline plus a `Vec<DestinationHandle>` and exposes `spawn_add_destination`/`spawn_remove_destination` (fan-out and transfer), `change_destination` (the single classified entry point for settings edits) and `set_source_framerate` (tier 3, which only records a raised rate — the real source is variable-rate and has no ceiling to restart for). See `superdoc/features/stream-control.md`. Linux: `ashpd` portal + PipeWire, behind `#[cfg(unix)]`. Windows: `d3d11screencapturesrc`, behind `#[cfg(windows)]`. Auto-selects VAAPI (`vah264enc`) hardware encoding with automatic `x264enc` software fallback. Also drives the 1 fps room-preview tee branch and the valve-gated own-tile live preview branch (#78): `superdoc/features/self-preview.md`. **`capture::branch` submodule**: adds/removes/swaps *dynamic* encode branches on the live pipeline's shared `tee` at runtime (`add_branch` sync; `remove_branch` async — block tee src pad, inject EOS, drain appsink, then unlink/remove/null/release; `swap_branch` = make-before-break, never touches the old branch). These are the GStreamer mechanics behind multi-destination fan-out, per-destination live settings changes, and stream transfer. Stress-tested (50 add/remove cycles, sibling-branch and fixed-branch isolation). Four invariants are load-bearing, all documented in-code: every branch needs `queue leaky=downstream` (a non-leaky queue blocks upstream through the shared tee and stalls *every* branch — the original "freezes solid under sustained motion" bug); `remove_branch`'s pad probe must return `PadProbeReturn::Drop`, not `Remove` (letting the blocked buffer reach an already-EOS'd branch returns `GST_FLOW_EOS` upstream, which makes the capture source shut down and push a real EOS through the shared tee, killing every branch and poisoning every future request pad via sticky-event forwarding); each branch has **exactly one** `AppSinkStream` for its whole life (`EncodeBranch::stream`, created lazily by `next_sample`, reused by `remove_branch` — see "Per-destination lifecycle" below); and adding a branch to the live tee must **not** let a `RECONFIGURE` event reach upstream of that tee (`isolate_source_from_branch_renegotiation` drops it at the tee's sink pad — forwarded, it makes `pipewiresrc` renegotiate with the portal producer, which then never sends another buffer, so the first destination of every share silently killed the source it was encoding; a `videotestsrc` renegotiates instantly, which is why the whole unit suite stayed green while real sharing was broken). **`capture::destination` submodule**: the `DestinationState` machine, the monotonic wire-PTS invariant, the stall protocol and the three-tier change model — GStreamer-free and unit-tested against injected `Instant`s. See "Per-destination lifecycle" below and `superdoc/brainstorm/ui-redesign-multiroom-streaming.md`'s "branch-swap" design. **Live encoder-tuning controls** (`TuneMode`: `LowLatency`/`BestQuality`; `QualityLevel`: `Performance`/`Quality`; `RateControlMode`: `Constant`/`Adaptive`/`Quality` — two enum variants and three, respectively, not a uniform three-way split across all controls) are three more per-destination knobs, each routed through tier 2 (`BranchSwap`) like resolution/fps/backend, mapped to `x264enc`/`vah264enc` properties per backend in `EncoderBackend::launch_fragment` (`tune` has no VAAPI analogue and is a documented no-op there; `RateControlMode::Adaptive` has no `x264enc` analogue and is a documented no-op there — see that fn's doc comment for the exact property mapping and both asymmetries). See `superdoc/features/stream-control.md`'s "Encoder tuning controls" section. |
| `audio.rs` | Per-application audio capture via PipeWire CLI tools (`pw-dump`/`pw-loopback`/`pw-link`), Opus encode → encrypt → send. Linux-only; no Windows equivalent exists yet (`superdoc/features/windows.md`). |
| `decode.rs` + `decode/` | GStreamer decode for both media kinds. **Video (native path, shown on screen by the wgpu renderer `decode/render.rs` + `render.wgsl`; `ui/video.rs` keeps one `VideoView` per watch):** `decode/codec.rs` sniffs H.264 / H.265 / AV1 from the first keyframe access unit (no wire tag, `protocol-spec.md` §6.4); `decode/video.rs` runs the pipeline on its own thread — `appsrc ! {h264,h265,av1}parse ! va{h264,h265,av1}dec ! video/x-raw,format=NV12 ! appsink(sync=false)` — with a software fallback (`avdec_h264` / `avdec_h265` / `av1dec`; dav1d isn't installed) on build/link failure, bus error or no frame within 2 s, replaying the pushed AUs and remembering the choice per codec; `decode/frame.rs` holds the zero-copy `Arc<DecodedFrame>` (Y/UV slices, strides, `pts_us`, `YuvMatrix`, `full_range`) and the cloneable `FrameSource` (`frame_counter`, `latest_if_newer`, `note_presented`, `stats`). Decoded fps (pad probe) and presented fps are counted separately. `StreamDecoder::latest_frame() -> ColorImage` (CPU NV12→RGBA) remains only for tests. Details and the `Why:` notes: `features/video-pipeline.md` "Native decode". **Audio is live**: `opusdec` → `volume` → `autoaudiosink`, fronted by an `AudioJitterBuffer` that restores PTS order and turns lost packets into `GAP` events so `opusdec` (`plc=true`) can conceal them. `PacingState` (pts-budget design, `Why:` note below) is kept as a `pub` building block for a presenter; the decoder itself no longer paces. `decode.rs` also does preview JPEG decode (used by the sidebar/Streams-list thumbnails, still live). |

`Why:` (video pacing) a design that only reset pacing on individual frames each found
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
"Bugs found via manual testing (2026-07-25)" for the full writeup.
| `preview.rs` | Periodic (~30s) thumbnail sender: capture-agnostic, fed a generic `FrameStream`, downscales + JPEG-encodes + encrypts + sends over a reliable QUIC uni-stream. Shares the connection's single nonce counter with video/audio. |
| `rooms.rs` | Local TOML-backed storage of saved rooms ("lounges") — stores the whole join string plus a friendly name. Also `AppSettings` — preferences only, never session state: language, render presentation mode, and the Stream Control bandwidth preferences (`bandwidth_mode`, `absolute_total_kbps`, `destination_floor_kbps`; the two modes' numbers are separate fields so switching modes is lossless). |
| `theme.rs` | Central egui theme (bin-only): design tokens (colors, sizes, spacing, radii, type scale, embedded fonts, icon lookup) and `theme::apply`. Rules and conventions: `superdoc/ui/STYLING-GUIDE.md`. |
| `widgets/` | Bin-only egui building blocks: `layout.rs` (vbox/hbox/fixed-slot/fill-slot `HBox` with explicit width math, since egui has no flexbox), `paint.rs`, `controls.rs`, `rows.rs` (the slot-row patterns), `containers.rs`, `gallery.rs` (the `--gallery` screen for design QC). Pure view: plain data in, small action enums out, no app state. Documented in `superdoc/ui/STYLING-GUIDE.md`. |
| `presets.rs` | The encoder presets that used to be JS in the Electron frontend, now a lib module (I4): `EncoderPreset` {LowLatency, Balanced, Quality, Gaming} mapping to `EncoderTuning` (tune / quality / GOP ms), `EncoderPreset::detect` (the active preset is always derived from the three fields, never stored), inherent `apply_preset*` / `active_preset` methods on `DestEdit` and `AppState` (defined in this file so the table and everything that applies it stay together), and `framerate_options_for`. `Why in the lib:` the preset table used to be JS in the (now deleted) Electron frontend; the egui UI must stay a thin view over `AppState`, so the logic lives next to it. LowLatency = LowLatency tune, Performance quality, 2000 ms GOP; Balanced = BestQuality, Balanced, 2000; Quality = BestQuality, Quality, 2000; Gaming = LowLatency, Performance, 750 ms GOP. Rate control is not a field: always Dynamic/VBR (#88). |
| `session.rs` | The state/logic layer (extracted from the old single-file `ui.rs` during the Tauri port, kept as the UI-independent model since): `AppState` (renamed/restructured `App`) and the **client session model** (rooms / captures / destinations, described below), plus every business-logic method that used to live in `impl App`/`impl RoomSession` in `ui.rs` — connect/room lifecycle (`start_connect`, `insert_room`, `disconnect_room`, `leave_needs_confirm`, `request_leave_room`, `poll_connect_result` returning a [`ConnectOutcome`](#) so UI-only fields the "+ Add Room" card owns never have to live on `AppState`, `drain_events`, `rename_saved_room`), watching (`toggle_watch`, `toggle_listen`), capture/share (`start_video_share`, `start_audio_share`, `stop_share`, `poll_capture_states`, `refresh_audio_sources`), **stage-2 fan-out/transfer/edit** (`add_destination` — fan-out, `transfer_destination` — make-before-break, `poll_pending_destinations`, `apply_destination_edit`/`poll_destination_edits` routed through `capture::destination::classify_change`, `with_destination`), **bandwidth allocation** (`BandwidthController` — rate-limited/hysteresis-filtered live wiring for Absolute-Maximum mode, `apply_bandwidth_allocation`, `set_bandwidth_mode`, `reapply_manual_bitrates`, `allocation_input`), `spawn_media_ingestion_task`, and the `RoomHandle`/`CaptureSourceId`/`DestinationId`/`PendingRole` types. Deliberately egui-light: it depends on `egui` only for the handful of types (`egui::TextureHandle`) that already lived on this data, and `drain_events` still takes `&egui::Context` to upload preview thumbnails — both pre-existing, not new dependencies introduced by the split. Exported from `lib.rs` (`pub mod session;`, which also registers `rust_i18n`'s `t!` macro for the lib target) so tests and tools can drive the session logic without `eframe` rendering. `ui/`, `theme.rs` and `widgets/` stay bin-only and do the egui rendering. **Since I4 (2026-10-14)** the behaviour that used to exist only in `src-tauri/src/commands.rs` also lives on `AppState`, so every front-end gets it: `create_room` / `join_room` / `forget_room`, the settings setters with their clamps (`set_language`, `set_display_name`, `set_presentation_mode`, `set_max_jitter_buffer_ms`, `set_catch_up_speed_percent`), start-stream defaults, `edit_destination`, `set_bandwidth_total` / `set_bandwidth_floor`, `allocated_kbps` / `bandwidth_overshoot_kbps`, the audio-level tap (`insert_audio_level_tap` & co.), volume resolution, and `supported_codecs`. `commands.rs` is now thin. **Since task #36** it also owns auto-reconnect (`reconnects`, `suspend_room`, `poll_reconnects`, `reconnect_now`, `cancel_reconnect`, `reconnect_info`, the pure `backoff_delay`/`retry_delay` policy) and clean exit (`shutdown`/`shutdown_blocking`, `install_shutdown_signal_handlers`); see `implementation-notes.md`. |
| `ui/` | The native egui UI (bin-only): one window, room-centric. `mod.rs` is the shell (`App::update` polls the session, lays out a left sidebar 272 px, a 56 px room header band, the central room body, an optional People panel and a 56 px action bar, then modals, pop-out viewports and toasts) and holds the contract every screen follows: one `<X>State` plus `show*` functions taking `&mut Ui, &mut Cx`. Screens: `sidebar.rs` (brand, rooms, user band), `room.rs` (header, body, people), `action_bar.rs` (share controls), `stream_settings.rs` (the staged/live stream-settings modal), `modals.rs` (create/join, settings, leave/forget, rename), `popout.rs` (one deferred viewport per popped-out stream), `video.rs` (per-stream GPU `VideoView`s synced to the watches), `apply.rs` (the one place `UiAction`s change state, as thin calls into `session.rs`), `tests.rs`. Screens never mutate `AppState` themselves. Layout, row states and decisions: `superdoc/ui/APP-SHELL.md`; widget and token rules: `superdoc/ui/STYLING-GUIDE.md`; render presentation mode: `superdoc/features/presentation-modes.md`; own-tile live preview: `superdoc/features/self-preview.md`; stream-control feature set (fan-out, transfer, live edits, bandwidth modes): `superdoc/features/stream-control.md`. |
| `end_to_end_test.rs` | Real integration test: two in-process `ScreenshareClient`s + a real relay server child process, `videotestsrc` standing in for portal capture. `#[cfg(test)]`-only, declared in `lib.rs`. Finds the relay binary under the workspace `target/{debug,release}` (newest wins) unless `SSSHARE_SERVER_BIN` / `SSSHARE_SERVER_CWD` override it; `audio.rs`'s live test reuses `spawn_relay_server`. |

## History: the Electron + Tauri-sidecar era (removed)

Until 2026-10-14 the desktop client was an Electron shell (`client/electron/`, HTML/CSS/JS frontend
`client/frontend/`) in front of a headless Tauri binary (`client/src-tauri/`, crate
`simple-screensharing-client-tauri`) that spoke to it over a loopback REST + SSE API
(`api_server.rs`, bearer-token authenticated) and relayed raw H.264 over local HTTP (`video_server.rs`)
for WebCodecs in the renderer. It was replaced by the single egui binary described above and deleted
(task I23). Nothing on this page's module map still exists from it; `session.rs` (`AppState`) is
the part that survived unchanged in shape, which is why the egui UI could be rebuilt as a thin
view over it. The long, dated notes that used to live here (the command and event tables, the video
relay wire format, the `view.js` jitter buffer, the 2026-07 flakiness investigation) are in git
history: `git log -- superdoc/architecture/overview.md`. The product-level history is in
`superdoc/architecture/implementation-notes.md`, `superdoc/features/video-pipeline.md` ("History")
and `superdoc/features/viewer-window.md` ("History").

One rule from that era still binds the live code. `Why the live encoder-tuning controls (tune /
quality level) go through tier 2 (`BranchSwap`) rather than a property write:`
GStreamer's `x264enc` / `vah264enc` don't expose those as properties writable on a `Playing`
element the way `bitrate` is (`launch_fragment` bakes them into the construction string), so
"change one" is definitionally "build a new encoder", which is what a branch swap is. Routing
them through the existing `classify_change` / `swap_to_new_branch` machinery gives them
make-before-break, the wait-for-first-IDR gate and the `KeptOld`-on-failure net for free
(`superdoc/features/stream-control.md`). The regression test for the profile/level change a swap
can cause lives at the `capture`/`net` layer, `end_to_end_test.rs`
(`late_watch_after_all_three_tuning_controls_toggled_gets_a_real_keyframe_every_time`).

## Client session model (rooms / captures / destinations)

The desktop client's *ephemeral* per-launch state lives on `session.rs`'s `AppState` (moved
out of the old single-file `ui.rs`'s `App` in the Tauri-port split — see the `session.rs` and `ui/` table rows above)
in three collections. Nothing here is persisted — only `rooms.rs`'s `RoomStore` (saved lounges)
and `AppSettings` (language, presentation mode, bandwidth preferences, max jitter buffer) survive
a restart, by design.

| Thing | Owner | Identified by | Holds |
| --- | --- | --- | --- |
| `RoomSession` | `AppState::rooms: IndexMap<RoomHandle, _>` | `RoomHandle(u32)` | One joined room = one QUIC connection: `ScreenshareClient`, its (forwarded, non-media) `ClientEvent` receiver, roster, announced streams, `watches`/`audio_watches`, preview cache + textures, its own `next_stream_id`, and `Vec<DestinationId>` back-references to what it's sending. |
| `LocalCapture` | `AppState::captures: IndexMap<CaptureSourceId, _>` | `CaptureSourceId(u32)` | One local GStreamer pipeline (a screen grab or an audio source): its `CaptureHandle`/`AudioCaptureHandle` state machine, its self-preview texture, and its `destinations`. |
| `Destination` | `LocalCapture::destinations: IndexMap<DestinationId, _>` | `DestinationId(u32)` | One room this capture is sent into: the `stream_id` announced on *that* room's connection, this destination's own `VideoParams` (resolution/fps/bitrate/backend), a shared handle onto its `capture::destination::DestinationRuntime` (see below), a `pending: Option<PendingRole>` marking it as a fan-out/transfer target or transfer source still settling, and an `edit: DestEdit` (the row's in-progress scale/framerate/bitrate buffer, applied through `classify_change` on "Apply"). |

Stage 2 added two more pieces of session state, both cross-room by design (see
"Multi-destination captures are live" below):

- **`AppState::bandwidth: BandwidthController`** — Absolute-Maximum mode's live wiring:
  remembers what each destination's encoder was last told (`applied: HashMap<DestinationId, u32>`),
  rate-limits recomputes to one per second (`ALLOC_RECOMPUTE_INTERVAL`), and suppresses
  writes that don't clear a 5%-or-100kbit/s hysteresis threshold (`needs_reapply`). Wraps
  the pure `bandwidth::allocate()` — see that module's row above — with the timing/memory
  behaviour that makes applying its output to live encoders not cause visible bitrate
  oscillation. Never persisted; it describes only this session's live encoders.
- **`AppState::pending_destinations: Vec<PendingDestination>`** — fan-out/transfer
  destinations whose encode branch is still being built (`capture::CaptureHandle::
  spawn_add_destination`), polled once per frame by `poll_pending_destinations`. A
  transfer is two of these events glued together: the *old* destination is marked
  `pending: Some(PendingRole::TransferSource(target_room))`, a *new* one is added in the
  target room marked `PendingRole::TransferTarget(source_room)`, and only once the new one
  confirms live does `poll_pending_destinations` tear the old one down — make-before-break,
  never a bare re-point (a destination's encryption keys/nonce space are room-bound, so
  "the same stream in another room" isn't expressible any other way).

`Why:` (`RoomSession::watches`/`audio_watches` are `Arc<Mutex<HashMap<...>>>`, not plain
`HashMap`s, and `event_rx` is a *forwarded* channel rather than the raw one
`ScreenshareClient::connect` returns) `session.rs`'s `insert_room` spawns one always-on
`spawn_media_ingestion_task` per room that owns the raw connection channel directly and
intercepts every `ClientEvent::MediaFrame` there, feeding `watches`/`audio_watches` — hence
the shared, lockable maps — in real time, completely independent of `App::update()`'s
repaint-driven cadence. Everything else (`ClientEvent::PresenceDelta`, `SubscriberCount`,
`KeyframeRequest`, `ControlClosed`) is forwarded on to a fresh channel, which is what
`RoomSession::event_rx` actually is; `drain_events` keeps handling those exactly as before, at
its normal throttled pace. This closes the "fast-forward catch-up" bug: before this task
existed, `MediaFrame` sat in the same channel as everything else until `drain_events` next ran,
which only happens once every `UNFOCUSED_REPAINT_INTERVAL` (500ms) while the window is
unfocused — so a backlog piled up and then drained in one burst, and `PacingState::decide`
paced each backlogged frame out with a real-time sleep (a literal real-time replay of history).
See `session.rs`'s `spawn_media_ingestion_task` doc comment for the full root-cause writeup and the
option-1-vs-option-2 design choice, and `decode.rs`'s `PacingState::epoch` doc comment for the
complementary, already-separately-merged wall-clock staleness backstop inside the pacing math
itself (a defensive mitigation for a burst that still forms, not a fix for backlog formation).

`Why:` (`spawn_media_ingestion_task` races `raw_events.recv()` against `forward_tx.closed()`,
rather than only detecting a left room via a failed `forward_tx.send()`) the task now owns
`raw_events` — the receiver `RoomSession` used to hold directly — so dropping a `RoomSession`
(`disconnect_room`) has no *direct* effect on it any more. Relying solely on a future failed
send to notice would leave the task, every decoder/GStreamer pipeline it feeds, and the
`net.rs` background readers (which only exit once *their* send into `raw_events` fails) running
for as long as the remote side keeps sending `MediaFrame`s and nothing else — the common case
for an active share, since every one of those frames is consumed here without ever reaching the
forwarding arm. `forward_tx.closed()` resolves the instant `RoomSession::event_rx` drops,
independent of whether any event is in flight, so teardown on disconnect stays as prompt as it
was before this task existed.

`Why:` (captures owned by `App`, and owning their destinations, rather than living under a
room) one capture is meant to eventually feed several rooms at once — one real screen grab,
N independently encoded and independently encrypted outgoing streams, because encryption
keys and nonce spaces are room-bound and can never be shared across rooms. If a room owned
its captures, leaving that room would tear down a pipeline other rooms still use. With this
ownership, leaving a room removes only that room's destinations and the capture survives as
long as any destination remains.

`Why:` (ids are opaque `u32` newtypes from a monotonic counter, not map indices and not the
protocol's own ids) `IndexMap` shifts indices on removal, so an index held across a removal
would silently address a *different* entry; a counter that never rewinds means a stale id can
only miss, never alias. The protocol's `StreamId` is a single byte unique only *within one
connection* — exactly the ambiguity multi-room introduces — so every room-scoped collection
either lives inside its `RoomSession` or is looked up by `(room, stream_id)`, never
`stream_id` alone.

**Multi-destination captures are live.** One capture feeds N rooms: every destination —
including the first — is a dynamic tee branch with its own encoder, room key, `stream_id`
and pump task, driven by `spawn_add_destination`/`spawn_remove_destination` (fan-out and
transfer). See "Per-destination lifecycle" below and `superdoc/features/stream-control.md`.

The **sidebar** is genuinely multi-room (ROOMS lists every joined room, STREAMS lists
outgoing destinations across all of them), as are `drain_events`, watch-texture upload
and pop-out viewports. The **content tabs** are not yet: `App::active_room` names the one
`RoomSession` that Stream Control and View render, re-pointed by clicking a room row.
Retiring it belongs to the stages that make those two tabs multi-room in their own right.

The remaining single-room seams are all at the **view** level, listed here so the later
UI stages fix them deliberately rather than rediscovering them:

- `App::pending_connect` is a single slot — one join can be in flight at a time. (It does
  now carry the join string it is for, so the sidebar can put the right saved row into the
  "reconnecting" state.)
- `App::status` is global, so one room's `ControlClosed` writes an error line under the
  ROOMS list that is not attributed to any particular room.
- The share panel's "waiting for permission" spinner tests `App::captures` globally, not
  the active room's destinations.

Fixed by the app-shell stage: `App::join_string` is gone — each `RoomSession` now carries
the `join_string` it connected with, which is also the key the sidebar joins saved rooms
to live sessions on (`RoomStore::name_for`).

`Why:` (`disconnect_room` is destination-scoped, not capture-scoped) it walks the leaving
room's `Vec<DestinationId>` back-references and, for each, drops the whole `LocalCapture`
**only** when that was its last destination; otherwise it stops just that one destination.
Dropping the capture unconditionally would tear down a pipeline another room is still
using, which is precisely the invariant `App`-owned captures exist to protect.

`Why:` (`AppState::disconnect_room`/`request_leave_room`/`poll_connect_result` don't touch
`expanded_rooms`/`leave_confirm`/the "+ Add Room" card's fields directly, even though the
pre-split `ui.rs` code they came from did) those three are genuinely UI-only view state —
`session.rs` never had them and must not grow them, since that's exactly the boundary the
UI/lib split needs held clean. `poll_connect_result` instead returns a `ConnectOutcome`
(`None` / `Joined(RoomHandle)` / `Failed { error, from_add_card }`) and `request_leave_room`
returns a `bool` (tore down immediately, or the caller must raise its own confirm dialog);
the UI's (`ui/apply.rs`) same-named handlers are thin wrappers that react to the return value to
update their own UI-only fields. `disconnect_room` similarly has a same-named `App` wrapper
that calls `self.state.disconnect_room(handle)` and then clears `expanded_rooms`/
`leave_confirm` itself. A prior QC pass validated this exact pattern (identical
behavior before/after, verified line-by-line) before it was carried over to the real,
larger split described in the `session.rs` module-map row above.

## Per-destination lifecycle (state machine, wire PTS, stall protocol)

Lives in `capture.rs`'s **`destination`** submodule (state, pure logic — no GStreamer, no
clock of its own) and its **`branch`** submodule (the GStreamer mechanics plus the
orchestrators joining the two). `session.rs`'s `Destination` holds a `SharedDestination`
(`Arc<Mutex<DestinationRuntime>>`) clone and reads the state to render; `capture.rs`'s
`pump_loop` writes it as frames flow.

### The three-tier change model

Every settings change costs exactly one of three things, and only the third is visible to
the viewer. `destination::classify_change(source, from, to) -> ChangeTier` is the single
place that decides which:

| Tier | Trigger | Mechanism | Viewer sees |
| --- | --- | --- | --- |
| 1 `Bitrate` | bitrate only | live encoder property write | nothing (no state change at all) |
| 2 `BranchSwap` | resolution / fps / encoder backend, **at or below** the source's current fps | `branch::swap_to_new_branch`: build the new branch, wait for **its** first IDR, flip authority in Rust, then remove the old one | nothing (`Rebuilding` → `Live`; there is always exactly one live branch) |
| 3 `SourceRestart` | the capture **source** changes (monitor/window switch, fps above the source's rate, source crash) | `branch::stall_all` → restart the capture → `branch::rebuild_after_source_restart` per destination | a low-framerate pause (`Stalled`), never a disconnect |

`Why:` (fps down is tier 2 but fps up is tier 3) each branch already carries its own
`RateCap`, so anything at or below the source's rate is downconverted inside that one
branch and costs nothing shared. A cap cannot invent frames the source never
delivered, so going *above* the source's rate requires restarting the source element
itself — which is shared, so it hits every co-located destination. This asymmetry is why
the capture runs at `destination::required_source_framerate(...)` = `max(fps)` across its
destinations rather than at any single destination's rate.

`Why:` (`clamp_to_source` never upscales, and clamps *proportionally*) upscaling burns
encode budget on invented pixels and adds no detail, so a requested resolution is clamped
down to at most the capture's native size. Clamping each axis independently would silently
change the aspect ratio when only one axis overshoots (3840x1080 off a 1920x1080 capture
would become 1920x1080, i.e. stretched), so both axes are scaled by the single tightest
overshoot ratio. Framerate is deliberately *not* clamped to the source's rate — asking for
more is legal, it's just tier 3.

### `DestinationState`

`Starting` (branch built, no IDR yet) → `Live` → `Rebuilding{since}` (tier-2 swap in
flight, invisible) / `Stalled{since, reason}` (tier-3, the **only** viewer-visible state) /
`Failed(String)`.

`Why:` (`Rebuilding` is exited only by `finish_rebuild`, never by an IDR) during a swap the
*old* branch is still authoritative and still emitting its own periodic IDRs on the 2s GOP.
Letting those end the rebuild would declare the swap complete before the new branch had
produced anything, defeating make-before-break. `Starting` and `Stalled`, by contrast, do
end on the first IDR — there the authoritative branch *is* the one being waited on.

`Why:` (a failed swap returns to `Live`, not `Stalled` — `branch::SwapOutcome::KeptOld`) a
swap consumes the old branch by value, so a plain `Err` would destroy the caller's only
handle to a branch that is still linked, still `Playing`, and still the destination's sole
source of frames. A settings change that can't be applied must leave the viewer exactly as
they were, never worse.

### Stall protocol

A destination enters `Stalled` on a deliberate source restart (`branch::stall_all`) and,
since 2026-10-14, when a `Live` un-paused branch produces nothing for
`destination::silence_threshold` (`DestinationRuntime::stall_if_silent`). See
`features/stream-control.md` ("Idle monitors") for why a capture can also have *no first
frame at all*, which is handled before a branch exists (`branch::wait_for_source_frame`).

While `Stalled`, the destination is **never torn down** — no `StopStream`, so the viewer
never sees a disconnect/resubscribe. Instead `capture.rs`'s `send_stall_keepalive` re-sends
the destination's last cached IDR every `STALL_RESEND_INTERVAL` (500ms, ~2fps), freshly
re-encrypted (new nonce every time, per spec §5 — a nonce is never reused, not even for
identical bytes) with an advancing wire PTS, and sets `protocol::MEDIA_FLAG_STALLED` in the
media header.

> **`Why:` `MEDIA_FLAG_STALLED` is advisory-only and must stay that way.** The AEAD's AAD is
> exactly `[PROTOCOL_VERSION, stream_id]` — the media header's `flags` byte is **not**
> covered by it, and cannot be: the relay reads and forwards that header without holding the
> room key, so it is cleartext by design (spec §6.2). Anyone on the path can flip this bit
> without breaking decryption. That is acceptable for a UI hint (worst case: a spurious or
> missing spinner) but means it must **never** drive teardown, subscribe/unsubscribe, key
> handling, resource release, or anything else with a security or protocol consequence. What
> keeps that safe in practice: a stalled destination keeps sending real, *authenticated*
> frames and never sends `StopStream`, so the viewer's actual media state never depends on
> this bit being truthful. `MEDIA_FLAG_KEYFRAME` sits in the same byte with the same
> standing. Documented at every point it is set or read (`protocol.rs`,
> `capture::send_media_payload`, `net::DecryptedMediaFrame::stalled`,
> `decode::StreamDecoder::is_stalled`).

Viewer side: `decode.rs`'s `StreamDecoder::is_stalled()` is true if the flag was seen **or**
no frame has arrived in >3s (`NO_FRAME_STALL_TIMEOUT`). Both halves are needed — the flag
covers a graceful restart (the sender is still sending, so silence never fires), the timeout
covers a sender that died without being able to signal anything. `is_stalled_at(now)` takes
an injected `Instant` so the timing half is testable without sleeping. This stage builds
only the *signal*; the placeholder UI that consumes it (pulsing bar → spinner →
"still waiting" tiers) is a later UI stage.

### The wire-PTS continuity invariant

**Wire PTS is a property of the `Destination`, never of the pipeline.** A GStreamer
pipeline's running time restarts at 0 every time it is rebuilt, so sending buffer PTS
straight onto the wire makes a capture restart look like the stream jumped *backwards*,
which `decode.rs`'s `PacingState` can only read as a discontinuity to hard-reset on — a
visible glitch.

- `pts_epoch: Instant` is set once at destination creation and **never reassigned**.
- `pts_offset_us: u64` is recomputed on every pipeline/branch (re)start:
  `pts_epoch.elapsed() - pipeline_running_time_at_restart` (`DestinationRuntime::on_pipeline_start`,
  which must run *before* any buffer from the new pipeline is timestamped).
- Every send goes through `wire_pts = max(raw + offset, last_sent + 1)` — the `max` is a
  **hard clamp, not a formula**. It guarantees monotonicity even if the offset arithmetic is
  wrong, if a stale pre-restart buffer arrives, or if a stall re-send interleaves with a real
  frame. `capture.rs` funnels every wire timestamp through `DestinationRuntime::advance_pts`,
  so there is exactly one place the invariant can be broken.

The result: a restart reads to the viewer as *a period of low framerate*, not a break —
which is exactly the illusion the stall protocol exists to sustain.

### `EncodeBranch` and the single-`AppSinkStream` rule

`gst_app::AppSink::stream()` is not a view onto an appsink — it *installs* callbacks
(`gst_app_sink_set_callbacks`, which replaces whatever was there), and on this crate's
pinned feature set its `Drop` resets the appsink's callbacks to an empty set. So two
`stream()` calls on one appsink silently break each other: the first's waker is
unregistered by the second, and dropping either leaves the survivor parked (it only wakes
again by luck, since `poll_next` opportunistically `try_pull_sample`s before parking). This
matters because `remove_branch` must itself drain the appsink to EOS while a production
reader is already consuming it.

The fix is structural rather than a rule to remember: `EncodeBranch` owns the branch's
**one and only** `AppSinkStream`, created lazily on first `next_sample()` and reused by
`remove_branch`, which receives it by taking the whole `EncodeBranch` **by value**. Handing
the branch to `remove_branch` *is* the reader's handoff; there is no window in which two
live streams exist. Regression-guarded by
`a_branch_already_being_read_removes_promptly_without_a_second_appsink_stream`, which
asserts on *removal latency* — a clobbered callback set doesn't fail loudly, it just burns
the internal 5s drain timeout and logs "did not drain to EOS" while still returning `Ok`.

Also in `remove_branch`: the removal pad probe fires once per dropped buffer for the whole
drain window, and only the first firing does anything (EOS into an already-EOS'd branch is a
no-op), so a one-shot `AtomicBool` keeps a per-buffer `send_event` — which runs the branch's
whole event chain inline on the tee's streaming thread — out of the hot path.

### Per-branch encoder fallback

`branch::add_branch_with_fallback` (construction) and `branch::add_branch_live`
(construction *plus* "actually produced an IDR within `FIRST_IDR_TIMEOUT`") apply the same
VAAPI→software fallback `start_sharing` applies to the whole pipeline, but **per branch**.
`Why:` the whole-pipeline fallback only ever ran with one encoder in flight. With fan-out
there are N concurrent encode sessions off one capture and VAAPI's concurrent-session limit
is small (commonly 2–4), so the *third* destination can fail on a machine where the first two
succeeded and where `detect_encoder_backend`'s process-wide probe still correctly reports
VAAPI as available. That failure is per-branch and transient — exactly what a process-wide
probe cannot express.

### What is wired vs. what waits for a later stage

**All of it is wired as of stage 2.** Every outgoing destination is a dynamic branch with
its own `DestinationRuntime`, driven for real by its own `destination_pump`: wire PTS, IDR
caching, `Starting` → `Live` on the first IDR, and the stall keepalive ticker. The
branch-backed paths are driven from the UI (`ui/apply.rs`) for real too — `swap_to_new_branch` behind every
tier-2 settings edit, `add_branch_live`/`remove_branch` behind fan-out and transfer.
`bandwidth::allocate` is applied to live encoders in Absolute-Maximum mode. See
`superdoc/features/stream-control.md`.

`rebuild_after_source_restart`, `begin_stall` and `note_source_restarted` are the
orchestrators with no production caller. Tier 3 turned out to need no pipeline surgery at
all: measured against the real portal, the source is variable-rate (`framerate=0/1`) with
no ceiling to raise, so `set_source_framerate` just records the higher rate and the
caller's ordinary tier-2 branch swap applies it — nothing stalls, so nothing has to be
stalled (see the `Why:` on `set_source_framerate`). They stay as the mechanism a *real*
source change (portal session revoked, device unplugged — `StallReason::SourceLost`) would
need, and remain covered by their own tests.

Still not built: the audio mixer rework (audio is still its own capture, not an attachment
on a video destination).

The UI was rebuilt on egui as a room-centric window (the Discord-like layout of `superdoc/ui/APP-SHELL.md`
and `superdoc/ui/STYLING-GUIDE.md`; code in `client/src/ui/`). The Stream Control feature set
(fan-out, make-before-break transfer, per-destination live edits including the encoder-tuning controls,
both bandwidth modes with the overshoot warning) is built (`superdoc/features/stream-control.md`), as are
real decoded video in the room view with per-stream pop-out windows (`superdoc/features/video-pipeline.md`,
`superdoc/features/viewer-window.md`). `superdoc/ui/FRONTEND-SHELL.md` documents the removed HTML
frontend and is kept as history.

## Status

**Built and exercised:**

- Full QUIC control-plane (join/presence/subscribe/leave), media-datagram fragmentation
  and reassembly, AEAD encryption/decryption, and preview thumbnails — proven end-to-end
  (real networking, real crypto, real relay routing, real GStreamer decode) by
  `client/src/end_to_end_test.rs` using `videotestsrc` in place of real screen content.
- Screen capture on Linux (portal + PipeWire) with H.264 / H.265 / AV1 VA-API hardware
  encode and software fallback; per-application audio capture and Opus encode (PipeWire).
- **240 fps end to end**: H.265 with VA encode and decode; AV1 and the H.264 Gaming
  presets reach about 240 with the Performance setting (`features/stream-control.md`
  "Capture/encode performance").
- Native video decode (VA with software fallback) and the wgpu renderer, in the room view
  and in pop-out viewports (`features/video-pipeline.md`, `features/viewer-window.md`).
- **Multi-destination fan-out, transfer and the bandwidth modes**, with sender-side
  idle-pause and forced-keyframe-on-subscribe per destination; live encoder-tuning
  controls through the tier-2 `BranchSwap` machinery (`features/stream-control.md`).
  Proven against a real relay and capture pipeline by
  `ui::tests::fan_out_transfer_and_absolute_bandwidth_across_three_rooms`.
- **Auto-reconnect and clean exit** (task #36/#37): a dropped connection becomes a
  suspended room that retries and restores its watches and shares (`session.rs`).
- Relay admin TUI (ratatui), WebTransport for browsers, rotating self-signed cert.
- **Browser client** (`docs/`, `features/website.md`).
- **Linux AppImage** and **Flatpak** (`client/packaging/README.md`).

**Still pending:**

- **Windows**: `superdoc/features/windows.md`. Verified: MSVC + GStreamer 1.26.9 build, D3D11 hardware
  decoders with software fallback, Linux<->Windows sharing headless (VM, over SSH), packaging via `package.ps1`,
  the relay builds as-is. **Not yet verified**: the egui window on a real desktop session and
  `d3d11screencapturesrc` capture (the VM had no logged-in session). Known gaps: no per-app audio capture,
  software (`x264enc`) encode only, no installer/signing.
- **The literal Wayland portal handoff of real screen content** in automated tests — it
  needs interactive consent through the portal picker, so `end_to_end_test.rs` cannot
  cover it; it is exercised by the scripted real-app runs (`client/scripts/README.md`).
- **Audio as an attachment on a video destination** — audio is still its own capture with
  its own `stream_id`, so the plan's always-on-`audiomixer` design (invisible live
  toggling, the `attached_to` protocol field) and its UI are not built.
- **Enabling GitHub Pages** for `docs/` — the site is ready, the repo setting is the owner's.

## Where to look for X

- Overall plan, scope, original design decisions → `superdoc/brainstorm/simple-screensharing-platform.md`
- Binding wire protocol (headers, nonce construction, message catalog) → `superdoc/architecture/protocol-spec.md`
- Browser/WebTransport leg: URL path, ALPN, pinned-cert lifecycle, join-link `certhash` field, the 1000-byte datagram cap → `protocol-spec.md` §11 and §6.3
- Where the real implementation deviated from or refined the plan/spec, and bugs/QC findings from building it → `superdoc/architecture/implementation-notes.md`
- Documentation discipline for this workspace → `superdoc/claude-instructions/documentation.md`
- Project vocabulary → `superdoc/meta/TERMINOLOGY.md`
- Desktop client's egui theme/widget conventions, the slot layout model, `--gallery` → `superdoc/ui/STYLING-GUIDE.md`, `client/src/widgets/mod.rs`, `client/src/theme.rs`
- Desktop client's window layout — behavioral rules (sidebar, ROOMS/Outgoing sections, room-row states, empty states) → `superdoc/ui/APP-SHELL.md`; code map → the `ui/` row of the module map above and `client/src/ui/mod.rs`'s module doc. (`superdoc/ui/FRONTEND-SHELL.md` documents the removed HTML frontend: history only.)
- Desktop client's render presentation-mode (vsync/Immediate/Mailbox) setting → `superdoc/features/presentation-modes.md`
- Sharer's own-tile live preview (native + browser, focus-gated) → `superdoc/features/self-preview.md`
- Per-destination state machine, wire-PTS continuity, stall protocol, three-tier change model → "Per-destination lifecycle" above, and `capture.rs`'s `destination`/`branch` submodules
- Multi-destination fan-out, stream transfer between rooms, the two bandwidth modes, the allocator's live wiring, and the live encoder-tuning controls (Tune/Quality Level/Rate Control) → `superdoc/features/stream-control.md`
- Wire types/constants (single definition) → `protocol/src/lib.rs`; codec detection (H.264/H.265/AV1, no wire tag) → `client/src/decode/codec.rs` and `protocol-spec.md` §6.4
- Native video decode (VA hardware + software fallback, NV12 `DecodedFrame`, `FrameSource`, decode benchmark `client/examples/decode_bench.rs`) → `superdoc/features/video-pipeline.md` "Native decode" and `client/src/decode.rs`'s module doc
- Encoder presets (LowLatency/Balanced/Quality/Gaming) → `client/src/presets.rs`
- Video delivery to the screen — native decode, the wgpu renderer, how encoder-tuning changes interact with an in-flight watch → `superdoc/features/video-pipeline.md`
- The per-stream pop-out window (F11 fullscreen, multi-window) → `superdoc/features/viewer-window.md`, `client/src/ui/popout.rs`
- The process model (one native client process, relay threads) → "Process model" above; the removed Electron + sidecar split → "History" above
- Linux AppImage / Flatpak packaging (container build, bundled GStreamer, supported hosts) → `client/packaging/README.md`; Windows (MSVC build, `package.ps1`, verified headless; GUI/capture unverified) → `superdoc/features/windows.md`
- The browser client (core, UI, dev harness) → `superdoc/features/website.md`, `docs/js/core/README.md`, `docs/js/ui/README.md`, `docs/dev/README.md`
- Scripted real-app test helpers and how to launch the client safely → `client/scripts/README.md`, `client/.claude/skills/run/SKILL.md`, `superdoc/claude-instructions/testing-and-verification.md`
