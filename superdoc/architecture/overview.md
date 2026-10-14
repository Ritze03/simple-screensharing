# Architecture Overview

One-paragraph orientation to this file's role: the first doc to read, links out to
sibling docs rather than duplicating them.

## Big picture

`simple-screensharing` (this directory) is **one monorepo** (private GitHub repo
`Ritze03/simple-screensharing`, all rework on branch `rework`) holding the relay server, the
desktop client, the shared wire-protocol crate and this documentation. It used to be a docs-only
workspace over separate `simple-screensharing-server` / `simple-screensharing-client` repos;
those were imported with full history (old branches kept as `archive/client/*` and
`archive/server/*`). `Why one repo:` server, desktop client and the upcoming browser client must
change in lockstep.

| Path | Role |
| --- | --- |
| `server/` | Rust QUIC relay (`simple-screensharing-server`): broadcasts encrypted room traffic between clients, never decrypts it, plus a ratatui admin TUI. Also accepts browsers over WebTransport. |
| `client/` | Desktop client. The Rust library + bin crate (`simple-screensharing-client`, `client/src/`) holds all the real logic: screen/audio capture, encode, encrypt, decode, session state. **Today it still ships as an Electron shell (`client/electron/`) driving an HTML/CSS/JS frontend (`client/frontend/`), backed by a Rust sidecar (`client/src-tauri/`, a headless Tauri binary).** That stack is **being replaced** by one native egui app (see `implementation-notes.md`'s 2026-10-14 block); until it lands, the video-frame path on screen is still decoded in the Chromium renderer via WebCodecs, not by the Rust side — see `superdoc/features/video-pipeline.md`. |
| `protocol/` | Shared crate `simple-screensharing-protocol`: the single definition of wire types, header layouts and constants. `client/src/protocol.rs` and `server/src/protocol.rs` re-export it. Spec: `protocol-spec.md`. |
| `superdoc/` | This agent-facing knowledge base. |
| `docs/` | **Coming, not yet created:** the browser client (static site served by GitHub Pages, WebTransport + WebCodecs, no Electron). **The relay side is done:** it accepts browsers over WebTransport on the same UDP port as native clients, with a pinned auto-rotating self-signed cert whose hash rides in the join link — `protocol-spec.md` §11. Not to be confused with the desktop client's own WebCodecs decode, which is Chromium-inside-Electron, not a browser tab. |

The root `Cargo.toml` is a workspace (resolver 3) with members `client`, `client/src-tauri`,
`server` and `protocol`, sharing one `Cargo.lock`. Root scripts: `build-appimage.sh`,
`launch-test.sh`, `launch-test-tmux.sh`. This page is the up-to-date map of what is in the
tree; the historical design record and the binding wire contract live in their own pages (see
"Where to look for X" below) and are **not** duplicated or rewritten here.

> **Rework in progress (2026-10-14).** Electron, the HTML frontend and the Tauri sidecar are
> being removed in favour of a native egui app. Sections below that describe them (Process
> model, Packaging, the `src-tauri/` tables) are accurate for the code that still exists but
> are marked as being removed; a full rewrite of this page is planned once the removal lands.

## Process model

> **Desktop-client half of this section: being removed by the rework.** The two-process
> Electron + sidecar model below is what still runs today; the target is a single native egui
> process that is a thin view over `session::AppState`.

- **Relay server**: a single async process (tokio multi-thread runtime). The QUIC
  listener runs as a tokio task; the ratatui admin TUI runs on its own dedicated OS
  thread (not a tokio task), so crossterm's blocking terminal I/O never blocks the
  networking runtime — see `server/src/tui.rs`'s module doc comment.
  One tokio task per accepted QUIC connection ("connection actor") owns that
  connection's control bidi stream and `select!`s over control frames, media datagrams,
  and incoming preview uni-streams. Logs go to a daily-rolling file (`logs/`), never
  stdout/stderr, since the TUI owns the terminal — see `src/main.rs`.
- **Desktop client — two OS processes, not one.** This replaced an original single-process
  eframe/egui GUI (see `Why:` below); the old egui code (`src/ui.rs`) still
  compiles (the bin crate now links the lib instead of re-declaring its modules) but has
  **zero live callers** in the Electron build — dead code kept
  around rather than deleted mid-port, not the front-end in use.
  - **Electron process (parent)** — `electron/main.js` is the app shell: a `BrowserWindow`
    loading `frontend/index.html` (renderer, sandboxed via `contextBridge`, no direct
    Node/IPC access — `electron/preload.js` is the only bridge) plus any number of optional
    additional `BrowserWindow`s, one per popped-out stream
    (`superdoc/features/viewer-window.md`). On startup it spawns the Rust binary below as
    a **child process** ("the sidecar"), captures an `API_TOKEN=<hex>`/`API_PORT=<port>`
    pair the sidecar prints on stdout once, waits for the sidecar's HTTP port to actually
    accept connections, then creates the window. Every `window.__TAURI__.core.invoke(...)`
    call the frontend JS makes is forwarded by the main process to
    `POST http://127.0.0.1:<port>/command/<name>` (bearer-token authenticated); every
    backend event is relayed from a single `GET /events` Server-Sent-Events connection
    (opened by the *main* process, since a sandboxed renderer's `EventSource` can't set
    the `Authorization` header the sidecar requires) out to the renderer via
    `webContents.send`.
  - **Rust sidecar process (child)** — still the same `simple-screensharing-client-tauri`
    binary crate (`src-tauri/`) built during the Tauri era: a tokio multi-thread runtime
    entered for the process's lifetime, the real `AppState`/`session.rs` business logic,
    every capture/net/decode pipeline. It still runs as an actual (headless — `visible:
    false`) Tauri app internally, for two reasons: reuse of Tauri's managed-state/command
    macros without a rewrite, and revertibility (its `invoke_handler!` command surface is
    still fully registered and would still work if some future caller drove it over real
    Tauri IPC instead of HTTP). What changed is the **transport** in front of that command
    surface: `src-tauri/src/api_server.rs` is a small REST+SSE server (a raw
    `std::net::TcpListener` on an OS-assigned port, matching `video_server.rs`'s existing
    pattern) that dispatches `POST /command/<name>` straight into the *same*
    `#[tauri::command]` functions `commands.rs` always had, and re-broadcasts every event
    `driver.rs` used to `app_handle.emit()` through an `EventBus` (a bounded 64-entry
    sequence-numbered ring buffer, so a reader that's mid-reconnect can replay what it
    missed instead of silently losing events) over the SSE stream. `video_server.rs` is a
    **separate**, parallel raw-HTTP server on its own ephemeral port, serving the actual
    H.264 bitstream — see `superdoc/features/video-pipeline.md`.

  Each active capture pipeline (`CaptureHandle` / `AudioCaptureHandle`) still owns
  GStreamer element threads outside tokio entirely, bridged into async Rust the same way
  as before (`gstreamer-app`'s `AppSink::stream()`, `gstreamer`'s `Bus::stream()`) — none
  of that changed by this migration, only what drives `AppState`'s polling and how the
  frontend talks to it (see `driver.rs`'s module doc, referenced below, for the
  poll-loop/event mechanism itself, which is unchanged in shape).

  `Why two processes, and why keep the Tauri scaffolding instead of a clean rewrite:` an
  earlier session found the Tauri/webkit2gtk-based shell had enough platform-integration
  friction (window chrome, multi-window support for the viewer, GPU/WebCodecs behavior)
  that swapping to Electron's Chromium fixed it outright — but `session.rs`/`capture.rs`
  and the whole command/DTO/event surface built during the Tauri port were sound and not
  worth re-deriving. Keeping them as a headless sidecar spoken to over loopback HTTP
  isolates the swap to the transport layer alone: every `#[tauri::command]` fn, every
  `driver.rs` poll/diff/emit, every DTO shape is untouched code, just reached a different
  way. `Why a bearer token, given the server only binds `127.0.0.1`:` loopback-only is
  *not* sufficient alone — any webpage open in the user's regular browser can still reach
  a loopback port via `fetch()` regardless of its own origin, so the token (32 random
  bytes, hex-encoded, generated fresh per launch, passed parent→child only via the
  stdout-scraping handshake above, never written to disk) is what actually makes the
  server's necessarily-permissive `Access-Control-Allow-Origin: *` safe.

## Packaging (Linux AppImage) — being removed with Electron

**Added 2026-07-30.** The desktop client packages as a single self-contained
`.AppImage` via `electron-builder`, configured in the `build` key of
`client/electron/package.json` (no separate
`electron-builder.yml`). Build it with:

```
cd client/electron
npm run build:appimage
```

That script chains `cargo build --release -p simple-screensharing-client-tauri` (the
sidecar, built against the workspace `Cargo.toml` one level up) with
`electron-builder --linux AppImage`, and produces
`client/dist/simple-screensharing-client-<version>.AppImage`.

`Why extraResources, not the default `files` packaging`: electron-builder's app
directory is `electron/` (where its `package.json` lives), but the sidecar binary
(`../target/release/simple-screensharing-client-tauri`) and the frontend
(`../frontend/`) both live *outside* that directory, as siblings. `build.extraResources`
copies both into the packaged app's `resources/` folder
(`resources/sidecar/simple-screensharing-client-tauri`, `resources/frontend/`) — the
one electron-builder mechanism for pulling in files from outside the app directory.

`Why main.js needs a packaged-vs-dev path branch`: in dev, `electron/main.js` resolves
the sidecar/frontend paths relative to `__dirname` (its own directory) via `../target`
and `../frontend`. Once packaged, `__dirname` resolves inside `app.asar` and those
sibling source-tree directories no longer exist next to it — the packaged layout only
has what `extraResources` copied into `process.resourcesPath`. `main.js` now derives a
`RESOURCES_ROOT` constant (`process.resourcesPath` when `app.isPackaged`, else
`path.join(__dirname, "..")`) and roots `SIDECAR_PATH`/`FRONTEND_INDEX`/
`VIEWER_HTML`/`APP_ICON` off it instead of `__dirname` directly. The `SIDECAR_BIN` env
var override (for dev testing against a `--release` build) still takes precedence over
both branches.

Icon: reuses the existing `src-tauri/icons/icon.png` (32×32) via `build.linux.icon` —
electron-builder generates the AppImage's icon set from that single source PNG, no
separate `.icns`/`.ico` needed since only the Linux AppImage target is configured.

Verified end-to-end 2026-07-30: `npm run build:appimage` produces a ~130MB
`.AppImage`; launching it self-extracts to a `/tmp/.mount_*` dir, spawns the bundled
sidecar from `resources/sidecar/`, and the sidecar prints its normal
`API_TOKEN=`/`API_PORT=` handshake lines and starts listening — confirming the
packaged app is self-contained (no separate install step, no reliance on the source
tree's `target/`/`frontend/` directories).

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
   pipeline for real playback (`autoaudiosink`). Video, **in the Electron build that still
   ships**, is tapped raw and undecoded off `session.rs`'s `spawn_media_ingestion_task` by
   `src-tauri/src/video_server.rs`, which relays it byte-for-byte over local HTTP to the
   Electron renderer for the browser's own WebCodecs `VideoDecoder` — see
   `superdoc/features/video-pipeline.md` (display side: being replaced). In parallel the
   native decoder (`decode.rs`'s `StreamDecoder`: codec sniffed from the bitstream, VA
   hardware decode with software fallback, NV12 `Arc<DecodedFrame>` out) is the path the
   egui GPU renderer (I9, in progress) will consume; the old `ui.rs` reaches it only through
   the legacy `latest_frame() -> ColorImage` adapter.

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
| `main.rs` | Entry point: CLI (`[PORT] [--cert/--key] [--state-dir] [--san]`, see `--help`), logging setup, starts the QUIC listener task and the TUI thread, on TUI quit or SIGINT/SIGTERM sets the TUI stop flag, runs `relay::shutdown` (close 0x200, ≤2 s), then `process::exit(0)` after the log guard drops (<0.1 s measured); a second signal forces exit(130). Why: the runtime used to wait forever on the `spawn_blocking` join of the TUI thread, which nothing told to stop. Check: `server/tests/exit_check.sh <bin> [port]`. |
| `protocol.rs` | Relay-only helpers over the shared `simple-screensharing-protocol` crate (`protocol/`, which owns the wire types, header layouts, transport constants and cert-hash encoding): the 64 KiB-capped control-frame codec and `clamp_utf8`. |
| `cert.rs` | The TLS certificate: ECDSA P-256 self-signed (13 days, persisted in `state/`, rotated when < 2 days left via `Endpoint::set_server_config`) or a provided CA chain (`--cert/--key`, reloaded on change). Publishes the current hash/expiry for the TUI. See `protocol-spec.md` §11.3. |
| `relay.rs` | Accept layer + relay logic. One UDP port, two ALPNs: `ssshare/1` (native, raw QUIC) and `h3` (browser, WebTransport on `/ssshare`), both turned into one `web_transport_quinn::Session` type so everything after `accept_session` is transport-agnostic. One connection-actor task per client plus one **media forwarder task** per client (`media_forward_task`) that owns that client's datagram fan-out, content-blind preview forwarding, keyframe-request debouncing. The actor only validates a datagram header and `try_send`s it to its forwarder over a bounded, drop-on-full queue, so fan-out never delays the actor's next read. |
| `room.rs` | Room/connection/stream state (`Registry`): outer `RwLock` over the room map, per-room `Mutex` for **control-plane** mutations (join, announce, subscribe, leave); the **media data plane** deliberately avoids that mutex, reading a per-stream `StreamRoute` fan-out snapshot instead. Also the admin-log ring buffer (`EventLog`) the TUI reads. Read this module's docs before touching either — they carry the lock-ordering rules. |
| `tui.rs` | Ratatui admin TUI on its own OS thread: Overview (incl. native/browser connection counts and the TLS cert hash + expiry/rotation countdown) / Rooms / Room Detail tabs, reading the shared `Registry` via `blocking_read`/`blocking_lock`. |
| `browser_smoke.rs`, `transport_tests.rs`, `tests/browser_smoke.html` | Test-only. `browser_smoke` drives **headless Chromium** (skips if none on `PATH`) through a real hash-pinned `new WebTransport(...)` join, datagrams and previews against an in-process relay with native peers; `transport_tests` covers ALPN dispatch and certificate rotation under live connections. |

### Client — `client/src/`

| Module | Role |
| --- | --- |
| `main.rs` | Entry point: CLI-flag parsing, tokio runtime setup, i18n init, then the eframe window (old UI) or, with `--gallery`, the widget gallery. **The bin links the `simple-screensharing-client` lib** and declares only its own `ui`, `theme` and `widgets` modules; it no longer re-declares `audio`, `net`, `session`, ... (they used to compile twice, once per crate). |
| `bandwidth.rs` | Pure `allocate()` bandwidth allocator: splits a total kbit/s budget across destinations by `width*height*fps` weight (largest-remainder rounding, floor/ceiling clamping with fixed-point redistribution, floor-overshoot-and-warn). No I/O, no wall-clock, no dependency on the room/capture/destination data model — plain data in (`AllocInput`), plain data out (`Allocation`). **Wired for real** by `ui.rs`'s `BandwidthController` (Absolute-Maximum mode): an `AllocInput` is built from every active destination across every room, and each result is applied as a live encoder-bitrate write, behind a ~1s recompute rate limit and a 5%-or-100 kbit/s hysteresis threshold — both of which live *outside* this module so `allocate` stays pure. See `superdoc/features/stream-control.md`. |
| `protocol.rs` | Thin re-export of the shared `simple-screensharing-protocol` crate (`protocol/`, below). It used to hold an independently derived copy of the wire types; now the types cannot drift from the relay's. |
| `net.rs` | QUIC connect/join, control-message send/receive, AEAD encrypt/decrypt (`encrypt_frame`/`decrypt_frame`), the join-string format (`server|room-id|key`), and the `ClientEvent` channel everything else consumes. `build_client_endpoint()` builds the **one** local `quinn::Endpoint` a front-end owns; `ScreenshareClient::connect()` takes it by reference and opens a connection on it — see the "One endpoint, N connections" `Why:` in its module docs. |
| `fragment.rs` | Pure, synchronous frame↔datagram fragmentation and reassembly (`fragment_frame` / `Reassembler`) — no I/O, unit-testable in isolation. `fragment_frame_with_flags` is the same thing with the header's whole `flags` byte supplied directly, used only by `capture.rs`'s stall protocol (which also raises `MEDIA_FLAG_STALLED`). |
| `capture.rs` | Screen capture → H.264 / H.265 / AV1 encode (`features/stream-control.md` "Video codec selection") → encrypt → fragment → send. **The pipeline contains no fixed encoder branch**: it is `source ! videoconvert ! capsfilter(one pinned raw format) ! tee` plus the two fixed preview branches, and *every* outgoing destination — including the first — is a dynamic branch added to that tee (`spawn_destination`), with its own encoder, bitrate, pause valve, `stream_id`, room key and pump task (`destination_pump`). One capture therefore feeds N rooms as N independently encoded and encrypted streams. `CaptureHandle` owns the pipeline plus a `Vec<DestinationHandle>` and exposes `spawn_add_destination`/`spawn_remove_destination` (fan-out and transfer), `change_destination` (the single classified entry point for settings edits) and `set_source_framerate` (tier 3, which only records a raised rate — the real source is variable-rate and has no ceiling to restart for). See `superdoc/features/stream-control.md`. Linux: `ashpd` portal + PipeWire, behind `#[cfg(unix)]`. Windows: `d3d11screencapturesrc`, behind `#[cfg(windows)]`. Auto-selects VAAPI (`vah264enc`) hardware encoding with automatic `x264enc` software fallback. Also drives the preview tee branch and a dedicated third tee branch for the sender-side self-preview: `superdoc/features/self-preview.md`. **`capture::branch` submodule**: adds/removes/swaps *dynamic* encode branches on the live pipeline's shared `tee` at runtime (`add_branch` sync; `remove_branch` async — block tee src pad, inject EOS, drain appsink, then unlink/remove/null/release; `swap_branch` = make-before-break, never touches the old branch). These are the GStreamer mechanics behind multi-destination fan-out, per-destination live settings changes, and stream transfer. Stress-tested (50 add/remove cycles, sibling-branch and fixed-branch isolation). Four invariants are load-bearing, all documented in-code: every branch needs `queue leaky=downstream` (a non-leaky queue blocks upstream through the shared tee and stalls *every* branch — the original "freezes solid under sustained motion" bug); `remove_branch`'s pad probe must return `PadProbeReturn::Drop`, not `Remove` (letting the blocked buffer reach an already-EOS'd branch returns `GST_FLOW_EOS` upstream, which makes the capture source shut down and push a real EOS through the shared tee, killing every branch and poisoning every future request pad via sticky-event forwarding); each branch has **exactly one** `AppSinkStream` for its whole life (`EncodeBranch::stream`, created lazily by `next_sample`, reused by `remove_branch` — see "Per-destination lifecycle" below); and adding a branch to the live tee must **not** let a `RECONFIGURE` event reach upstream of that tee (`isolate_source_from_branch_renegotiation` drops it at the tee's sink pad — forwarded, it makes `pipewiresrc` renegotiate with the portal producer, which then never sends another buffer, so the first destination of every share silently killed the source it was encoding; a `videotestsrc` renegotiates instantly, which is why the whole unit suite stayed green while real sharing was broken). **`capture::destination` submodule**: the `DestinationState` machine, the monotonic wire-PTS invariant, the stall protocol and the three-tier change model — GStreamer-free and unit-tested against injected `Instant`s. See "Per-destination lifecycle" below and `superdoc/brainstorm/ui-redesign-multiroom-streaming.md`'s "branch-swap" design. **Live encoder-tuning controls** (`TuneMode`: `LowLatency`/`BestQuality`; `QualityLevel`: `Performance`/`Quality`; `RateControlMode`: `Constant`/`Adaptive`/`Quality` — two enum variants and three, respectively, not a uniform three-way split across all controls) are three more per-destination knobs, each routed through tier 2 (`BranchSwap`) like resolution/fps/backend, mapped to `x264enc`/`vah264enc` properties per backend in `EncoderBackend::launch_fragment` (`tune` has no VAAPI analogue and is a documented no-op there; `RateControlMode::Adaptive` has no `x264enc` analogue and is a documented no-op there — see that fn's doc comment for the exact property mapping and both asymmetries). See `superdoc/features/stream-control.md`'s "Encoder tuning controls" section. |
| `audio.rs` | Per-application audio capture via PipeWire CLI tools (`pw-dump`/`pw-loopback`/`pw-link`), Opus encode → encrypt → send. Linux-only; no Windows equivalent exists yet. |
| `decode.rs` + `decode/` | GStreamer decode for both media kinds. **Video (reworked, native GPU-ready path; not yet shown on screen in the Electron build — the egui GPU renderer is I9, in progress):** `decode/codec.rs` sniffs H.264 / H.265 / AV1 from the first keyframe access unit (no wire tag, `protocol-spec.md` §6.4); `decode/video.rs` runs the pipeline on its own thread — `appsrc ! {h264,h265,av1}parse ! va{h264,h265,av1}dec ! video/x-raw,format=NV12 ! appsink(sync=false)` — with a software fallback (`avdec_h264` / `avdec_h265` / `av1dec`; dav1d isn't installed) on build/link failure, bus error or no frame within 2 s, replaying the pushed AUs and remembering the choice per codec; `decode/frame.rs` holds the zero-copy `Arc<DecodedFrame>` (Y/UV slices, strides, `pts_us`, `YuvMatrix`, `full_range`) and the cloneable `FrameSource` (`frame_counter`, `latest_if_newer`, `note_presented`, `stats`). Decoded fps (pad probe) and presented fps are counted separately. Legacy `StreamDecoder::latest_frame() -> ColorImage` (CPU NV12→RGBA) remains for the old `ui.rs`. Details and the `Why:` notes: `features/video-pipeline.md` "Native decode". **Audio is live**: `opusdec` → `volume` → `autoaudiosink`, fronted by an `AudioJitterBuffer` that restores PTS order and turns lost packets into `GAP` events so `opusdec` (`plc=true`) can conceal them. `PacingState` (pts-budget design, `Why:` note below) is kept as a `pub` building block for a presenter; the decoder itself no longer paces. `decode.rs` also does preview JPEG decode (used by the sidebar/Streams-list thumbnails, still live). |

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
| `theme.rs` | Central egui theme (bin-only): the new design tokens (colors, sizes, spacing, radii, type scale, embedded fonts, icon lookup) and `theme::apply`, plus a legacy section (`install`, cards/buttons helpers) that only keeps the old `ui.rs` compiling. Foundation from I8; the `superdoc/ui/` docs (written by a separate task) describe the design. `superdoc/ui/STYLING-GUIDE.md` is the older, pre-rework guide. |
| `widgets/` | Bin-only egui building blocks (I8): `layout.rs` (vbox/hbox/fixed-slot/fill-slot `HBox` with explicit width math, since egui has no flexbox), `paint.rs`, `controls.rs`, `rows.rs` (the slot-row patterns), `containers.rs`, `gallery.rs` (the `--gallery` screen for design QC). Pure view: plain data in, small action enums out, no app state. Documented in `superdoc/ui/` (separate task, I21). |
| `presets.rs` | The encoder presets that used to be JS in the Electron frontend, now a lib module (I4): `EncoderPreset` {LowLatency, Balanced, Quality, Gaming} mapping to `EncoderTuning` (tune / quality / rate control / GOP ms), `EncoderPreset::detect` (the active preset is always derived from the four fields, never stored), inherent `apply_preset*` / `active_preset` methods on `DestEdit` and `AppState` (defined in this file so the table and everything that applies it stay together), and `framerate_options_for`. `Why in the lib:` the Electron/Tauri layers are being deleted and the egui UI must stay a thin view over `AppState`. LowLatency = LowLatency tune, Performance quality, CQP, 2000 ms GOP; Balanced = BestQuality, Balanced, QVBR, 2000; Quality = BestQuality, Quality, QVBR, 2000; Gaming = LowLatency, Performance, CQP, 750 ms GOP. |
| `session.rs` | **Tauri-port step 1, done.** The state/logic layer extracted out of `ui.rs`: `AppState` (renamed/restructured `App`) and the **client session model** (rooms / captures / destinations, described below), plus every business-logic method that used to live in `impl App`/`impl RoomSession` in `ui.rs` — connect/room lifecycle (`start_connect`, `insert_room`, `disconnect_room`, `leave_needs_confirm`, `request_leave_room`, `poll_connect_result` returning a [`ConnectOutcome`](#) so UI-only fields the "+ Add Room" card owns never have to live on `AppState`, `drain_events`, `rename_saved_room`), watching (`toggle_watch`, `toggle_listen`), capture/share (`start_video_share`, `start_audio_share`, `stop_share`, `poll_capture_states`, `refresh_audio_sources`), **stage-2 fan-out/transfer/edit** (`add_destination` — fan-out, `transfer_destination` — make-before-break, `poll_pending_destinations`, `apply_destination_edit`/`poll_destination_edits` routed through `capture::destination::classify_change`, `with_destination`), **bandwidth allocation** (`BandwidthController` — rate-limited/hysteresis-filtered live wiring for Absolute-Maximum mode, `apply_bandwidth_allocation`, `set_bandwidth_mode`, `reapply_manual_bitrates`, `allocation_input`), `spawn_media_ingestion_task`, and the `RoomHandle`/`CaptureSourceId`/`DestinationId`/`PendingRole` types. Deliberately egui-light: it depends on `egui` only for the handful of types (`egui::TextureHandle`) that already lived on this data, and `drain_events` still takes `&egui::Context` to upload preview thumbnails — both pre-existing, not new dependencies introduced by the split. Exported from `lib.rs` (`pub mod session;`, which also registers `rust_i18n`'s `t!` macro for the lib target — the first lib module to need translated strings) so `src-tauri` can drive the same session/business logic as a library, without pulling in `eframe`/`egui` rendering. `ui.rs` and `theme.rs` remain bin-only (not re-exported from `lib.rs`) and keep the actual egui rendering, including the three rendering-only `RoomSession`/`App` methods that still need `egui::Context`/textures/viewports (`update_watch_textures`, `pop_out`, `show_popouts`, `update_self_preview_textures`) in a second `impl` block alongside the business one. **Since I4 (2026-10-14)** the behaviour that used to exist only in `src-tauri/src/commands.rs` also lives on `AppState`, so any front-end gets it: `create_room` / `join_room` / `forget_room`, the settings setters with their clamps (`set_language`, `set_display_name`, `set_presentation_mode`, `set_max_jitter_buffer_ms`, `set_catch_up_speed_percent`), start-stream defaults, `edit_destination`, `set_bandwidth_total` / `set_bandwidth_floor`, `allocated_kbps` / `bandwidth_overshoot_kbps`, the audio-level tap (`insert_audio_level_tap` & co.), volume resolution, and `supported_codecs`. `commands.rs` is now thin. **Since task #36** it also owns auto-reconnect (`reconnects`, `suspend_room`, `poll_reconnects`, `reconnect_now`, `cancel_reconnect`, `reconnect_info`, the pure `backoff_delay`/`retry_delay` policy) and clean exit (`shutdown`/`shutdown_blocking`, `install_shutdown_signal_handlers`); see `implementation-notes.md`. |
| `ui.rs` | egui UI, built as an **always-on app shell**: a left sidebar (wordmark / three-pill nav rail / ROOMS / STREAMS) plus a right content area rendering whichever nav tab is selected (Stream Control / View / Settings). There is no join screen — joining is one affordance inside the sidebar's ROOMS section, which unifies the saved-room store with live sessions (`room_store ⟕ rooms`). **The nav is a pure view-switcher: switching tabs never stops an active share or watch.** Holds presence roster, sharing controls (incl. the live self-preview widget), stream watching, pop-out viewer windows, localization. `App` here is now a **thin wrapper**: a `state: session::AppState` field (see `session.rs`, above) plus the handful of fields that are genuinely just egui view state (`nav_tab`, `add_room_open`, `expanded_rooms`, `leave_confirm`, `theme_installed`) — every `ui_*` render function calls into `self.state`'s methods rather than owning logic itself. Stage 2's Stream Control feature set (multi-destination fan-out, make-before-break transfer between rooms, per-destination live settings edits routed through `capture::destination::classify_change`, and the two bandwidth modes with `bandwidth::allocate` applied to live encoders via `BandwidthController`) is **built** and wired through here (`StreamAction::FanOut`/`Transfer` dispatching to `AppState::add_destination`/`transfer_destination`) — see `superdoc/features/stream-control.md` for the design. Layout, row states, empty states and the decisions behind them: `superdoc/ui/APP-SHELL.md`. Widget/theme conventions: `superdoc/ui/STYLING-GUIDE.md`. Render presentation mode: `superdoc/features/presentation-modes.md`. Self-preview: `superdoc/features/self-preview.md`. `ui.rs` is the client's old egui front-end, scheduled to be rebuilt on `widgets/` — a `tui.rs` terminal front-end existed early on to exercise `net.rs`/`capture.rs`/`decode.rs` without the full GUI and was removed once that was confirmed working; see `architecture/implementation-notes.md`. |
| `end_to_end_test.rs` | Real integration test: two in-process `ScreenshareClient`s + a real relay server child process, `videotestsrc` standing in for portal capture. `#[cfg(test)]`-only, included from `main.rs`. Locates the relay binary via `server_repo_candidates()` — **stale since the monorepo move:** it still probes the old multi-repo layout, so the `--ignored` tests cannot find the workspace `target/` binary without `SSSHARE_SERVER_BIN`/`SSSHARE_SERVER_CWD` overrides; `audio.rs`'s live test reuses that helper. |

### `src-tauri/` — the Rust sidecar (Tauri-port step 2; now spoken to over REST/SSE, not Tauri IPC — see "Process model" above)

A second binary crate, `simple-screensharing-client-tauri`, that depends on the client crate's `[lib]` target (`session.rs` and everything below it) as a path dependency. It still builds as an actual (headless) Tauri app — `tauri::Builder`, `#[tauri::command]`, `app.manage(...)` are all still real and still work — but the Electron migration put `api_server.rs` in front of that command/event surface as the transport the frontend actually uses; see "Process model" above for why both layers were kept. No business logic lives here — every command is a thin wrapper that locks the one managed `AppState` and calls straight into a `session.rs` method; see `superdoc/features/stream-control.md`/`superdoc/ui/APP-SHELL.md` for what those methods actually do.

| Module | Role |
| --- | --- |
| `state.rs` | `Managed { state: Mutex<AppState>, egui_ctx: egui::Context }`, installed via `app.manage(...)`. The `egui::Context` exists for one reason only — see its `Why:` note below. |
| `commands.rs` | Every `#[tauri::command]` fn (**thin since I4**: validation, clamps and defaults moved onto `session::AppState`), plus (for testability without a running `tauri::App`) an `*_impl(state: &mut AppState, ...)` free function underneath each one that the command wrapper calls. See the command table below. |
| `dto.rs` | Serializable payloads for the IPC boundary (`SavedRoomRow`, `RoomDetail`, `DestinationRow`, `SettingsDto`, ...) plus the string-key helpers (`bandwidth_mode_str`/`parse_bandwidth_mode`, `presentation_mode_key`/`parse_presentation_mode`) that translate `session.rs`'s Rust enums to/from the plain strings IPC carries. Exists because `session.rs`'s own types aren't `Serialize` and are out of scope to change for this step. |
| `driver.rs` | The one background poll loop — see "Who drives `AppState`'s polling" below. Its events go out over `EventBus::publish()` (see `api_server.rs` below), not `app_handle.emit()` any more. |
| `api_server.rs` | **The Electron-era transport.** A raw `std::net::TcpListener` (same "no HTTP framework" reasoning as `video_server.rs`, bound to an OS-assigned ephemeral port so multiple sidecar instances can coexist) serving `POST /command/<name>` (dispatches into the exact same `#[tauri::command]` fn `commands.rs` declares, via `AppHandle::state::<Managed>()` — no command logic duplicated) and `GET /events` (one long-lived SSE connection fanning out every `EventBus`-published event as standard `event: <name>\ndata: <json>\n\n` frames, plus a `: keep-alive` comment every ~15s). Every request needs `Authorization: Bearer <API_TOKEN>`; the token (32 random bytes, hex) and the bound port are printed once on stdout (`API_TOKEN=...`/`API_PORT=...`) for `electron/main.js` to scrape before making its first request. Owns the `EventBus` type: a `Mutex`-guarded ring buffer capped at `MAX_BUFFERED = 64` entries, each tagged with a monotonic sequence number, so an SSE client can hand back the last sequence number it saw (`subscribe()`'s return value) and the bus replays only what it missed instead of the reader silently losing events across a reconnect. |
| `video_server.rs` | The View tab's real video-frame delivery: a direct H.264 (Annex-B) bitstream relay over a never-ending `application/octet-stream` HTTP response body, one route per watched stream, decoded browser-side with WebCodecs. Owns its own `TcpListener` (bound at `Managed::new()`, same ephemeral-port pattern as `api_server.rs`), started/stopped per stream by `commands::toggle_watch`. Full wire format, jitter buffer, and encoder-tuning-knob interaction documented in `superdoc/features/video-pipeline.md`; see that doc and `video_server.rs`'s own module doc for the exact byte layout, the codec-string discovery mechanism (parsed from every keyframe's SPS, not just the first — see the 2026-07-28 `Why:` entry below), and status. |
| `main.rs` | `tauri::Builder` setup: manages `Managed`, spawns `driver::spawn` **and** `api_server::spawn` in `.setup()`, registers every command in `invoke_handler!` (kept live for revertibility even though Electron only ever calls through `api_server.rs`). No Tauri window is ever shown — `tauri.conf.json` sets `"visible": false`; Electron's `BrowserWindow` is the only visible window in the whole app. |

`Why:` (who drives `AppState`'s polling, now that there's no egui `update()` called every frame) `session.rs`'s `poll_capture_states`/`poll_connect_result`/`poll_pending_destinations`/`poll_destination_edits`/`drain_events` all exist because the old egui frontend called every one of them once per repaint. Two different mechanisms replace that here, matched to what's actually latency-sensitive:

- **Already has its own async completion signal**: `spawn_media_ingestion_task` (spawned once per room by `AppState::insert_room`, completely unchanged) still reacts to a `MediaFrame` the instant it arrives — nothing here touches it.
- **Everything else** (capture start/stop settling, a connect attempt resolving, destination edits/fan-out/transfer completing, and the room event queues `drain_events` empties) is driven by **one** `tokio::time::interval` loop (`driver::spawn`, 250ms — generous relative to human perception, this is not the video-frame path) spawned once at Tauri startup: it locks the single `Mutex<AppState>`, runs every poll once in sequence, diffs a couple of cheap snapshots (per-room member/stream-count signature, per-destination state label) against the previous tick, and emits a Tauri event for whatever changed. One shared timer rather than one per concern: every poll here is a cheap non-blocking scan over in-memory state, so buying one lock acquisition per tick is simpler than N timers contending over the same mutex for no responsiveness gain.

`Why:` (`state.rs` carries a standalone, never-displayed `egui::Context`) `AppState::drain_events` takes `&egui::Context` to upload decoded preview thumbnails as egui textures — pre-existing, and `session.rs` is out of scope to modify for this port step. `egui::Context::default()` needs no window, renderer or event loop; constructing one purely to satisfy that signature and never showing it is a harmless bridge — **provided something still drains it** (see the caveat below). The frontend gets the same preview bytes a different way: `list_active_streams`/`room_detail` are DTOs built by reading `RoomSession::preview_cache`/`captures` directly, not by rendering anything onto that context.

> **Caveat found and fixed by a 2026-07-26 QC pass:** `egui::TextureHandle::drop` frees a texture's *metadata* immediately (independent of any repaint), but the raw pixel bytes of every `load_texture`/`TextureHandle::set` call also land in `TextureManager`'s internal `TexturesDelta` queue, which a real egui integration drains once per repaint via `take_delta()` — and nothing ever repaints this standalone context. Left alone, that queue grows for the life of the process: one leaked full-size preview image per texture ever allocated or updated, never reclaimed even after the stream/room it belonged to is long gone (a real concern for an app meant to run for hours). Fixed by having `driver.rs`'s poll tick call `managed.egui_ctx.tex_manager().write().take_delta()` and discard the result right after `drain_events`, once per 250ms tick — bounds the queue to what changed within one tick instead of the session's whole history.

> **Second bug found and fixed by the same pass:** `driver::spawn` originally called raw `tokio::spawn` from inside `Builder::setup`, which runs synchronously on the main thread *before* Tauri's event loop (and its Tokio runtime context) is entered — the app panicked instantly on launch ("there is no reactor running, must be called from the context of a Tokio 1.x runtime"), confirmed by actually launching the built binary. Fixed by using `tauri::async_runtime::spawn` instead, Tauri's documented runtime-agnostic entry point that dispatches onto the one Tokio runtime Tauri itself owns, regardless of the caller's context.

#### Commands

| Command | Delegates to | Notes |
| --- | --- | --- |
| `list_saved_rooms` | `dto::saved_room_rows` (reads `room_store` ⟕ `rooms`) | Sidebar ROOMS rows; never renders the join string (see `APP-SHELL.md`) but does include it, since IPC is a same-machine, already-trusted boundary — display discipline is the frontend's job. |
| `join_room(join_string, display_name)` | `AppState::start_connect` | Result resolves later via `room-connected`/`room-connect-failed`. |
| `leave_needs_confirm(room_handle)` | `AppState::leave_needs_confirm` | Query-only, called *before* `leave_room` so the frontend can decide whether to raise a confirm dialog. |
| `leave_room(room_handle)` | `AppState::request_leave_room` | Returns `bool`: `true` = torn down immediately, `false` = the frontend must confirm and call `force_leave_room`. |
| `force_leave_room(room_handle)` | `AppState::disconnect_room` | Unconditional teardown, for after a confirm dialog. |
| `rename_room(join_string, new_name)` | `AppState::rename_saved_room` | |
| `select_active_room(room_handle)` | sets `AppState::active_room` | |
| `room_detail(room_handle)` | `dto::room_detail` | Roster/streams/watch-set for one room, on demand (also pushed via `presence-updated`). |
| `get_settings` / `set_language` / `set_presentation_mode` | `AppState` fields + `session::save_language`/`save_presentation_mode` | |
| `start_video_share(room_handle)` / `start_audio_share(room_handle)` / `stop_share(capture_id)` | `AppState::start_video_share`/`start_audio_share`/`stop_share` | |
| `list_audio_sources` | `audio::list_audio_sources` directly (`spawn_blocking`) | Bypasses `AppState::refresh_audio_sources`/`poll_audio_sources` — a Tauri command already has a natural async return point, so it just awaits the blocking listing instead of firing-then-polling next frame. |
| `select_audio_source(id, name)` | sets `AppState::selected_audio_source` | |
| `set_bitrate(kbps)` / `set_framerate(fps)` | `AppState` fields | Defaults for the *next* capture, per `session.rs`. |
| `list_active_streams` | `dto::destination_rows` | Cross-room, mirrors the sidebar STREAMS section / Stream Control's Active Shares. |
| `start_destination(capture_id, room_handle)` | `AppState::add_destination(_, _, PendingRole::FanOut)` | Fan-out ("Also send to…"). |
| `transfer_destination(destination_id, target_room)` | `AppState::transfer_destination` | Make-before-break. |
| `edit_destination(destination_id, scale_percent, framerate, bitrate_kbps, tune, quality_level, rate_control)` | `Destination::edit` fields + `AppState::apply_destination_edit` | Routed through `capture::destination::classify_change` inside `apply_destination_edit`, same as the egui "Apply" button. `tune`/`quality_level`/`rate_control` are snake_case strings parsed via `dto::parse_tune_mode`/`parse_quality_level`/`parse_rate_control_mode` — see `features/stream-control.md`'s "Encoder tuning controls" section. |
| `stop_destination(destination_id)` | `AppState::stop_destination` | |
| `retry_destination(destination_id)` | `AppState::retry_destination` | Failed-row "Retry". Preserves the failed destination's own params (unlike `stop_destination`+`start_destination`, which it replaced) and never tears down the rest of the capture — see `features/stream-control.md`. |
| `set_bandwidth_mode(mode)` / `set_bandwidth_total(total_kbps)` / `set_bandwidth_floor(floor_kbps)` | `AppState::set_bandwidth_mode` + fields + `session::save_bandwidth_settings` | |
| `toggle_watch(room_handle, owner, stream_id)` / `toggle_listen(...)` | `RoomSession::toggle_watch`/`toggle_listen` | `toggle_watch` also starts/stops that stream's `video_server.rs` route in lockstep and returns the relay URL (or `None` on stop) — see that module's doc for the wire format the frontend reads from it. `owner` is a lowercase-hex `ConnId` string on every command in this row, not a bare `u64` — see `dto.rs`'s `hex_u64` module doc for why. |
| `stream_video_url(room_handle, owner, stream_id)` | `video_server::VideoServer::url_for` | Query form of the URL `toggle_watch` returns — lets `view.js` rebuild its tile list (e.g. on tab reload) without needing to have been the caller that started the watch. |
| `set_volume(room_handle, owner, stream_id, volume_percent)` | `AudioWatch::volume_percent` + `AudioDecoder::set_volume` | |

**`pop_out_stream` — removed 2026-07-28.** Used to open a real second OS window via
`tauri::WebviewWindowBuilder` (`index.html?popout=1&room=...&owner=...&stream=...`, the
Tauri replacement for egui's `show_viewport_deferred`), but no frontend code ever read that
`?popout=1` query param, so it loaded the entire app shell in a second window rather than
just the one stream — a leftover from before the Tauri→Electron shell migration that
duplicated what the Electron-native pop-out mechanism below already did properly. Deleted
from `commands.rs`/`main.rs`'s `invoke_handler` list/`api_server.rs`'s dispatch table as
part of consolidating this app's three overlapping "view a stream in its own window"
mechanisms into one; see `superdoc/features/viewer-window.md`'s 2026-07-28 consolidation
entry for the full before/after.

**The one surviving "view a stream in its own window" mechanism lives entirely in
Electron, not through this command table at all: the per-stream pop-out window.**
`electron/main.js` exposes `open-popout-window`/`viewer-set-fullscreen`/`viewer-close` as
plain Electron `ipcMain.handle` calls (reached from `frontend/` via `window.__ELECTRON__`, a
bridge `preload.js` exposes *alongside* `window.__TAURI__` — deliberately separate
namespaces, since one is a narrow Tauri-invoke compatibility shim and the other is
Electron-native window management with no Tauri equivalent) — opens a real, lean, chrome-free
`BrowserWindow` per stream (not a singleton — any number can be open simultaneously) loading
`frontend/viewer.html`, reusing the same `window.TileVideo` WebCodecs decode module the main
window's Watch grid uses. F11 inside a pop-out window toggles real OS fullscreen without
closing it. See `superdoc/features/viewer-window.md`.

#### Events (`app_handle.emit`, all from `driver.rs` unless noted)

| Event | Payload | Fires when |
| --- | --- | --- |
| `room-connected` | `RoomDetail` | `poll_connect_result` resolves `Joined`. |
| `room-connect-failed` | `{ error, fromAddCard }` | `poll_connect_result` resolves `Failed`. |
| `room-disconnected` | `{ roomHandle, reason }` | A room handle present before `drain_events` is gone after it (control-plane close, or an explicit leave — both look the same to the frontend, which is idempotent either way). |
| `presence-updated` | `RoomDetail` | A room's `(member count, stream count)` signature changes since the last tick. |
| `destination-state-changed` | `{ destinationId, roomHandle, state }` | A destination's `DestinationState::label()` changes since the last tick (`starting`/`live`/`rebuilding`/`stalled`/`failed`). |
| `bandwidth-overshoot` | `{ overshootKbps }` | After `apply_bandwidth_allocation`, `BandwidthController::allocation()` reports `overshoot_kbps > 0` (naturally rate-limited by that controller's own 1s recompute interval). |

Explicitly **not** built in *this* pass: the video-frame delivery path and any `frontend/`
changes — the command names above were a target for that separate frontend work to `invoke()`
against, replacing its `// TODO: replace with invoke(...)` stubs. That separate work has since
happened — see the next paragraph and `Why:` note below for its mechanism and current status.

`Why this table used to call the video-frame path "a separate, already-prototyped
MJPEG-over-HTTP mechanism" and no longer does (2026-07-27):` that phrase was asserted (here,
in `view.js`'s header comment, and in `driver.rs`'s module doc) without ever pointing at real
prototype code. A later pass (see `superdoc/ui/FRONTEND-SHELL.md`'s 2026-07-27 entries)
confirmed by direct live testing — two real Tauri instances, real relay, real portal capture,
a watched tile screenshotted twice 4 seconds apart and diffed pixel-for-pixel (zero
difference) — that the View tab renders a static CSS-gradient placeholder today, not real
decoded video, and separately confirmed that no MJPEG/`turbojpeg`/`socket2`-clamped-buffer
prototype exists anywhere in this repo's history, dependencies, or the brainstorm doc that was
supposed to contain it (`ui-redesign-multiroom-streaming.md` — read in full, no such content).
The video-frame delivery path was therefore an **unimplemented, undesigned gap**, not an
already-solved mechanism waiting to be wired in.

**Status as of 2026-07-27, later still (superseded by the entry below):** the video-frame
delivery path was first built as MJPEG-over-HTTP — `src-tauri/src/video_server.rs` (local raw
`TcpListener`, single-slot generation-counted mailbox, `socket2`-clamped send buffer,
`turbojpeg` JPEG encoding, wired to `toggle_watch`/a new `stream_video_url` query command and
to `frontend/scripts/view.js`'s real tiles). Building it surfaced and fixed one real,
pre-existing bug (`u64` `ConnId`/`owner` values silently losing precision as bare JSON numbers
over the Tauri IPC boundary — see `src-tauri/src/dto.rs`'s `hex_u64` module doc) and surfaced,
but did **not** fix, a second, deeper pre-existing bug: no watcher who subscribes after a share
has already been running for a while ever receives a real keyframe on the wire, in either the
VAAPI or software encoder backend, so no such watcher can currently decode anything at all
regardless of the delivery mechanism. Full diagnostic trail in
`superdoc/ui/FRONTEND-SHELL.md`'s matching 2026-07-27 entry.

**2026-07-27, even later — MJPEG-over-HTTP replaced with a direct H.264 relay:**
`video_server.rs` no longer decodes with GStreamer (`decode.rs`'s `StreamDecoder`) and
re-JPEG-encodes every frame; it now relays the raw Annex-B H.264 access units `net.rs` already
hands `session.rs` (`ClientEvent::MediaFrame(DecryptedMediaFrame).payload` — access-unit-aligned,
no GStreamer-specific wrapping, confirmed against `capture.rs`'s encoder caps) straight over the
wire, for the browser to decode itself with WebCodecs' `VideoDecoder`. `Why:` this eliminates a
decode+re-encode round trip on every single frame (previously the dominant per-frame cost) and
removes the MJPEG path's ~60fps poll-rate ceiling (`POLL_INTERVAL`) — frames now forward the
instant they arrive. `session.rs`'s `WatchEntry` gained an optional `raw_tx` (a plain
`std::sync::mpsc::Sender<DecryptedMediaFrame>`, not the `tokio::sync::mpsc` used elsewhere in
that struct) that `commands::toggle_watch` wires up right after creating the watch, so the
Tauri-only relay can tap the same raw frames `spawn_media_ingestion_task` already feeds to
`StreamDecoder::feed`, without `session.rs` (the front-end-agnostic library crate) depending on
`src-tauri` (which owns the relay) — dependency direction stays one-way. (*History, 2026-07-27; since changed: `decode.rs`'s video path was reworked in 2026-10-14 into the live native decoder, see the module map.*) `decode.rs`/`ui.rs` are
untouched and still exist as dead code (`StreamDecoder` now has zero live consumers — only the
already-dead `ui.rs` egui front-end and the old `video_server.rs` ever read from it) —
left for a separate future cleanup pass, out of scope for this change. `turbojpeg` was removed
from `src-tauri/Cargo.toml` (confirmed unused anywhere else in the tree). Full wire format (a
4-byte length prefix + 10-byte header + raw payload, repeated; a CodecInfo chunk always sent
first per connection, carrying an `avc1.PPCCLL` string derived by parsing `profile_idc`/
`constraint_flags`/`level_idc` straight out of the first keyframe's SPS NAL — profile isn't
statically knowable since neither `x264enc` nor `vah264enc` sets an explicit `profile=`) is
documented precisely in `video_server.rs`'s own module doc, verified with a real loopback-socket
test in that module (`video_server::tests::relay_streams_codec_info_then_frames_in_wire_format`)
that connects a plain `TcpStream`, feeds synthetic frames through the same channel the real app
uses, and parses the wire bytes back out. This module's part of the redesign does **not** touch
the still-outstanding no-keyframe-on-late-subscribe bug above, or `frontend/`'s WebCodecs
consumer (separate work) — treat "the View tab shows real live video" as **still not true**
until both land.

**2026-07-28 — `view.js` gained a client-side jitter buffer:** `startTileVideoInner` used to
call `decoder.decode()` immediately on every VideoFrame chunk as it arrived off the wire, so
network jitter translated directly into visible playback stutter. It now queues each
already-parsed, still-encoded chunk (`controller.pendingChunks`, cheap — KBs each) and delays
the `decoder.decode()` call itself until a per-chunk scheduled time, computed from an
RFC 3550 §6.4.1-style EWMA of arrival-time jitter measured off the wire format's `pts_us`
header. `decoder.output`'s immediate draw-and-`close()` is untouched — only *when* `decode()`
fires moved, not what happens after. `Why:` buffering happens before decode, not after,
specifically because decoded `VideoFrame`s (~3MB at 1080p) are far too memory-heavy to hold
hundreds of at high framerates, while raw chunk data is cheap to queue. The buffer ceiling is a
module-level `maxJitterBufferMs` — structured as a single `let` binding so it can be sourced from
elsewhere without touching the jitter math itself, and it now is: `init()` fetches the real
`max_jitter_buffer_ms` from `get_settings()` once at startup and assigns it in
(`fetchMaxJitterBufferMs`), falling back to `DEFAULT_MAX_JITTER_BUFFER_MS = 2000` on the mock
branch. The setting is user-configurable from the Settings tab (a 0–5000ms range slider,
`frontend/scripts/settings.js`) via the same `AppSettings` (`rooms.rs`) →
`AppState`/`save_max_jitter_buffer_ms` (`session.rs`) → `SettingsDto` (`dto.rs`) →
`set_max_jitter_buffer_ms` command (`commands.rs`) → REST dispatch (`api_server.rs`) pattern
`language`/`presentation_mode` established; clamped to 0..=5000 both client-side (the slider) and
server-side (`AppSettings::clamp_max_jitter_buffer_ms`, defense in depth). See `view.js`'s
"Jitter buffer" comment block for the constants and per-function reasoning
(`updateJitterEstimate`, `updateAppliedBuffer`, `maybeScheduleNext`/`enqueueChunk`), and
`stopController`'s doc comment for why the scheduled `setTimeout` is cleared there (same
already-diagnosed class of teardown-ordering bug this file hit once before with the fetch/reader
cleanup).

Same day, a real-hardware-verified reliability bug: a watcher connecting **after** the sharer
toggled the live encoder-tuning controls (`tune`/`quality_level`/`rate_control`, each a
`capture.rs` `BranchSwap`) got stuck at 0fps with the `Stream error` placeholder (3/3 real runs;
2/2 reliable in isolation with no toggles first). Root cause, confirmed empirically: `x264enc`
picks its AVC profile/level automatically from `speed-preset` when no explicit `profile=`
property is set (`capture.rs` never sets one) — building the exact pipeline fragment
`capture.rs` constructs and comparing real SPS bytes showed `speed-preset=ultrafast` emits
`avc1.42c01f` (Baseline, level 3.1) while `speed-preset=veryslow` at the same resolution emits
`avc1.640032` (High, level 5.0). `video_server.rs`'s codec-string cache was `Mailbox::set_codec`,
a no-op once already set, on the documented (and, it turns out, false) assumption that "profile
doesn't change mid-stream" — so a route/connection that had already latched a codec string kept
handing it to `VideoDecoder.configure()` even after a `BranchSwap` changed the actual bitstream's
profile/level, and every `decode()` call after that silently failed. Fixed by re-deriving the
codec string from every keyframe's SPS (not just the first) and giving the mailbox a
`codec_generation` counter that only bumps on an actual value change; `handle_connection`'s relay
loop now watches that generation alongside the frame generation and pushes a fresh CodecInfo
chunk to every already-connected watcher the instant it changes, not only to new connections —
`tile-video.js`'s CodecInfo handler already called `decoder.configure()` unconditionally on every
such chunk (not gated to "first ever"), so no frontend change was needed. Regression coverage:
`video_server::tests::relay_pushes_a_fresh_codec_info_chunk_when_a_branch_swap_changes_profile`
(confirmed to fail against the pre-fix no-op `set_codec` and pass against the fix).

`Why the three tuning controls (`tune`/`quality_level`/`rate_control`) are routed through tier 2
(`BranchSwap`) rather than a live property write (tier 1), even for `RateControlMode::Adaptive`
and other cases that turn out to be x264enc no-ops:` GStreamer's `x264enc`/`vah264enc` don't
expose `speed-preset`/`target-usage`/`tune`/`rate-control` as properties safely writable on a
`Playing` element the way `bitrate` is (`launch_fragment` bakes them into the element's
construction string) — so "change one of these" is definitionally "build a new encoder", which
is exactly what a branch swap is for. Routing them through the *existing* `classify_change`/
`swap_to_new_branch` machinery (the same one resolution/fps/backend edits already use) rather
than inventing a parallel "rebuild the encoder in place" path means these three knobs inherit
make-before-break, the wait-for-first-IDR readiness gate, and the `KeptOld`-on-failure safety net
for free — a destination live-editing its tuning gets the exact same "nothing the viewer notices"
guarantee a resolution change already had, with zero new state-machine code. The `frontend/`
`pillSwitch` controls in the destination edit panel call `edit_destination` with the new
`tune`/`quality_level`/`rate_control` snake_case string params exactly like `scale_percent`/
`framerate`/`bitrate_kbps` already did — see `superdoc/features/stream-control.md`.

**2026-07-28 — the UI redesign ("Stream Control Redesign Options" design handoff), built in
several restyle passes on top of the already-`built` stage-1/stage-2 shell and feature set
above; no `.rs` change in any of it.** A shared 4-family status vocabulary (`progress` — a
marching-dashed bar, for `starting`/`rebuilding`; `live` — a static dot, deliberately
*non*-animated, since calm is itself the "fine" signal; `degraded` — amber 45°-hatch fill +
elapsed-seconds counter, for `stalled`; `failed` — not a pill at all, a squared card with a
headline, the raw error demoted to a monospace sub-block, and Retry/Dismiss) lives in
`frontend/styles/status.css` and `components.js`'s `Components.statusBadge`, consumed by every
tab rather than each rolling its own status chrome. Destination cards, the sidebar (a "nav well"
around the three-pill nav, the ROOMS list, and the outgoing-destinations section — now labelled
"Outgoing" in the UI, though `id="streams-list"`/internal names stayed `streams*` to avoid
churning working code for a label-only rename), and empty states across all three tabs were
restyled to this vocabulary. A room's expanded detail is now a slide-over panel
(`shell.js`'s "Room-detail slide-over") replacing the old inline-expand-in-place row. The View
tab was restyled as the "Watch grid": computed grid columns, hover-overlay chrome per tile, and
a "held-slot" placeholder (`view.js`'s `poppedOutIds`) that keeps a tile's spot in the grid —
labelled "in a separate window" — when that stream has been popped out to its own window
instead of actually disappearing from the grid. See `superdoc/ui/FRONTEND-SHELL.md`'s matching
2026-07-28 entry for the full list of restyle commits and `superdoc/features/viewer-window.md`
for the pop-out window mechanism this stage originally wired the View tab up to (later
consolidated onto its current per-stream shape the same day — see that doc's own
"consolidation" entry).

**2026-07-28 — re-investigated a 0fps/`Stream error` repro that looked like it contradicted the
already-`15/15`-verified late-subscriber-keyframe fix above; concluded residual test-harness
flakiness, not a regression, and added permanent regression coverage.** A full end-to-end smoke
test of the redesigned app reproduced "stuck at 0fps, `Stream error` placeholder" in 1 of 2 runs
of the exact scenario the fix above targets (a watcher connecting after the sharer had toggled
all three tuning controls). Two independent follow-up investigations (14/14 clean total) plus a
diff audit concluded this is **not** a code regression:

- A tight, real-hardware repro test,
  `late_watch_after_all_three_tuning_controls_toggled_gets_a_real_keyframe_every_time`
  (`src/end_to_end_test.rs`), was run twice — once standalone, then 12 more iterations via
  `E2E_BRANCH_SWAP_LATE_WATCH_ITERATIONS=12` — all clean.
- A diff audit confirmed none of the redesign's restyle commits touch `video_server.rs`,
  `tile-video.js`, or the set of events that trigger a tile to (re-)render — i.e. nothing in the
  redesign could plausibly have reintroduced the bug the earlier fix closed.

`Why the permanent regression test lives in `src/end_to_end_test.rs` (the `capture`/`net` layer)
and not in `src-tauri`:` the bug this test guards against — a stale WebCodecs codec string
surviving a `BranchSwap` that changed the bitstream's actual profile/level — is reproducible
purely from `capture.rs`'s encoder output and `net.rs`'s decrypted-frame stream; it needs a real
sharer, a real viewer, and real `BranchSwap`s, but needs **no** Electron process, no sidecar HTTP
transport, and no browser/WebCodecs at all — `video_server.rs`'s own
`relay_pushes_a_fresh_codec_info_chunk_when_a_branch_swap_changes_profile` test already covers
the *relay's* half (does it forward a fresh CodecInfo chunk) in isolation with synthetic frames.
Putting the *real*, expensive, real-hardware-encoder repro in the `capture`/`net` layer keeps it
runnable (and fast to iterate on) without spinning up the sidecar's HTTP surface or Electron at
all, and keeps the two tests' failure modes cleanly separated: a `src-tauri` failure means the
relay's forwarding logic broke; an `end_to_end_test.rs` failure means the *encoder* stopped
actually emitting the profile/level change the whole mechanism depends on.

`Why "flaky, not broken" is the right conclusion to record, despite a real-looking 1-in-2
failure:` the fix's own regression test is deterministic and real-hardware-verified at 15/15
(the original fix) plus 14/14 (this re-investigation) — 29/29 across two independent sessions
with zero failures of the *targeted* mechanism. A single failure in a much larger, multi-window
Electron/portal smoke test, with no corresponding failure in the tight repro and no code path in
the diff that could explain it, is the signature of test-harness flakiness (window-focus timing,
portal-picker race, or some other piece of the smoke harness's own machinery) rather than a
reintroduced bug — recording it as "residual flakiness" rather than either quietly dropping it or
overclaiming a fix keeps this doc honest about a real, if low-confidence, unresolved data point
without inventing a root cause that wasn't actually found. The tight repro test stays in the tree
specifically so the *next* time this is seen, there's already a fast, targeted way to tell "the
mechanism broke again" apart from "the smoke harness hiccupped."

## Client session model (rooms / captures / destinations)

The desktop client's *ephemeral* per-launch state lives on `session.rs`'s `AppState` (moved
out of `ui.rs`'s `App` in the Tauri-port split — see the `session.rs`/`ui.rs` table rows above)
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
Tauri port needs held clean. `poll_connect_result` instead returns a `ConnectOutcome`
(`None` / `Joined(RoomHandle)` / `Failed { error, from_add_card }`) and `request_leave_room`
returns a `bool` (tore down immediately, or the caller must raise its own confirm dialog);
`ui.rs`'s same-named `App` methods are thin wrappers that react to the return value to
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
`videorate`, so anything at or below the source's rate is downconverted inside that one
branch and costs nothing shared. `videorate` cannot invent frames the source never
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
branch-backed paths are driven from `ui.rs` for real too — `swap_to_new_branch` behind every
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
on a video destination), and stage 3's View-tab work.

The **UI redesign is being built in three stages** (see `superdoc/ui/APP-SHELL.md`).
Stage 1 — the app shell itself: always-on sidebar, three-pill nav, the unified
ROOMS section with its three row states and inline Add Room flow, the cross-room Outgoing
section, and minimal Stream Control / View / Settings tabs — is **built**. Stage 2 —
Stream Control's real feature set: multi-destination fan-out, make-before-break transfer,
per-destination live settings edits (now including the three live encoder-tuning controls,
Tune/Quality Level/Rate Control), the Absolute Maximum / Per-Stream Maximum bandwidth
modes with the allocator wired to live encoders, and the overshoot warning banner — is
**built** (`superdoc/features/stream-control.md`). Stage 3 (View's real feature set) is
**partially built**: the View tab is a real auto-tiled "Watch grid" showing real decoded
video (H.264-over-HTTP relay, WebCodecs decode client-side — `superdoc/features/
video-pipeline.md`) with per-tile hover overlays and a per-stream pop-out window with real
F11 fullscreen (`superdoc/features/viewer-window.md`); the tiered capture-restart placeholders and
live audio-toggle affordances from the original stage-3 scope are still not built. A separate,
later pass (2026-07-28) restyled the whole app — status vocabulary, destination cards,
sidebar nav well, a room-detail slide-over, empty states, and the Watch grid's held-slot
placeholders — against a design handoff; see `superdoc/ui/FRONTEND-SHELL.md`'s matching
entry.

## Status

**Built and exercised:**

- Full QUIC control-plane (join/presence/subscribe/leave), media-datagram fragmentation
  and reassembly, AEAD encryption/decryption, and preview thumbnails — proven end-to-end
  (real networking, real crypto, real relay routing, real GStreamer decode) by
  `client/src/end_to_end_test.rs` using `videotestsrc` in place of
  real screen content.
- Screen capture and H.264 encode on Linux (portal + PipeWire), with VAAPI hardware
  encoding confirmed working on the dev machine's AMD GPU and automatic software
  fallback.
- Per-application audio capture and Opus encode on Linux (PipeWire CLI tooling),
  confirmed live against a real audio source.
- Relay admin TUI (ratatui) with live bandwidth/room/stream stats.
- Windows client build: confirmed compiling via real GitHub Actions CI on a
  `windows-latest` runner, with a downloadable `.exe` + GStreamer-runtime-DLL artifact.
- Resource cleanup (`Drop` backstops for both capture handle types) and the
  nonce/AEAD security discipline, each verified by a dedicated QC pass.
- **Multi-destination fan-out, transfer and the bandwidth modes** — one capture feeding
  several rooms as independently encoded/encrypted streams, make-before-break transfer
  between rooms, per-destination live settings edits routed through the three-tier change
  model, and `bandwidth::allocate` applied to live encoders in Absolute-Maximum mode.
  Proven against a real relay child process and a real capture pipeline by
  `ui::tests::fan_out_transfer_and_absolute_bandwidth_across_three_rooms`.
- **Sender-side idle-pause and forced-keyframe-on-subscribe**, now *per destination*: each
  destination's branch has its own `valve`, so a stream nobody is watching pauses its own
  encoder without touching a fanned-out sibling.
- **Electron shell + Rust sidecar over REST/SSE**, replacing the Tauri-webview shell: real
  loopback HTTP command dispatch, bearer-token auth, SSE event fan-out via a bounded
  sequence-numbered ring buffer. Both processes launch and hand off correctly (Electron
  waits for the sidecar's port to accept connections before showing a window); see
  "Process model" above.
- **Real video delivery on the View tab**: raw H.264-over-HTTP relay decoded client-side
  with WebCodecs, including a client-side jitter buffer and correct behavior across a live
  encoder-tuning-triggered `BranchSwap` (29/29 clean across two independent real-hardware
  investigation sessions — see the 2026-07-28 entry above). `superdoc/features/
  video-pipeline.md`.
- **Live encoder-tuning controls** (Tune/Quality Level/Rate Control), each routed through
  the existing tier-2 `BranchSwap` machinery. `superdoc/features/stream-control.md`.
- **Per-stream pop-out window**, one independent `BrowserWindow` per popped-out stream with
  real F11 OS fullscreen, reusing the Watch grid's own decode module.
  `superdoc/features/viewer-window.md`.

**Still pending:**

- **The literal Wayland portal handoff of real screen content** — the one piece
  `end_to_end_test.rs` cannot cover, since it requires interactive human consent through
  the desktop portal's picker UI. Everything downstream of that handoff is proven; the
  handoff itself hasn't been run yet.
- **Windows audio sharing** — `audio.rs`'s PipeWire-CLI approach has no Windows
  equivalent. Within original scope (audio was only ever required for Linux), but a
  real limitation of the current Windows build.
- **Browser/WebTransport viewer** — not started; `protocol-spec.md` §11 marks the seam.
- **Audio as an attachment on a video destination** — audio is still its own capture with
  its own `stream_id`, so the plan's always-on-`audiomixer` design (invisible live
  toggling, the `attached_to` protocol field) and its UI are not built.
- **The View tab's remaining stage-3 scope** — the tiered (800ms/6s) capture-restart
  placeholders and live audio-toggle affordances. The auto-tiled grid, real video, and
  per-tile hover overlays are now built (see above).
- **Windows hardware encode** (Media Foundation `mfvideoenc`) — Windows always falls back
  to `x264enc` software encoding today; a deliberate fast-follow, not an oversight.

## Where to look for X

- Overall plan, scope, original design decisions → `superdoc/brainstorm/simple-screensharing-platform.md`
- Binding wire protocol (headers, nonce construction, message catalog) → `superdoc/architecture/protocol-spec.md`
- Browser/WebTransport leg: URL path, ALPN, pinned-cert lifecycle, join-link `certhash` field, the 1000-byte datagram cap → `protocol-spec.md` §11 and §6.3
- Where the real implementation deviated from or refined the plan/spec, and bugs/QC findings from building it → `superdoc/architecture/implementation-notes.md`
- Documentation discipline for this workspace → `superdoc/claude-instructions/documentation.md`
- Project vocabulary → `superdoc/meta/TERMINOLOGY.md`
- Desktop client's egui theme/widget conventions (the dead-code egui frontend's spec — the live frontend's token/CSS layer is `frontend/styles/tokens.css`, ported from it) → `superdoc/ui/STYLING-GUIDE.md`
- Desktop client's window layout — behavioral rules (sidebar, nav tabs, ROOMS/Outgoing sections, room-row states, empty states) → `superdoc/ui/APP-SHELL.md` (the original egui spec) and `superdoc/ui/FRONTEND-SHELL.md` (the live Electron/HTML frontend's own implementation notes, including the redesign and the Electron migration)
- Desktop client's render presentation-mode (vsync/Immediate/Mailbox) setting → `superdoc/features/presentation-modes.md`
- Desktop client's sender-side live self-preview (share panel) → `superdoc/features/self-preview.md`
- Per-destination state machine, wire-PTS continuity, stall protocol, three-tier change model → "Per-destination lifecycle" above, and `capture.rs`'s `destination`/`branch` submodules
- Multi-destination fan-out, stream transfer between rooms, the two bandwidth modes, the allocator's live wiring, and the live encoder-tuning controls (Tune/Quality Level/Rate Control) → `superdoc/features/stream-control.md`
- Wire types/constants (single definition) → `protocol/src/lib.rs`; codec detection (H.264/H.265/AV1, no wire tag) → `client/src/decode/codec.rs` and `protocol-spec.md` §6.4
- Native video decode (VA hardware + software fallback, NV12 `DecodedFrame`, `FrameSource`, decode benchmark `client/examples/decode_bench.rs`) → `superdoc/features/video-pipeline.md` "Native decode" and `client/src/decode.rs`'s module doc
- Encoder presets (LowLatency/Balanced/Quality/Gaming) → `client/src/presets.rs`
- egui design tokens and slot-row widgets, `--gallery` → `client/src/theme.rs`, `client/src/widgets/mod.rs`, and `superdoc/ui/` (new docs by a separate task)
- The View tab's real video delivery — H.264-over-HTTP wire format, WebCodecs decode, client-side jitter buffer, how encoder-tuning changes interact with an in-flight watch → `superdoc/features/video-pipeline.md`
- The per-stream pop-out window (F11 fullscreen, multi-window) → `superdoc/features/viewer-window.md`
- The Electron ⇄ Rust-sidecar process split, the REST+SSE transport and its bearer-token auth → "Process model" above and `src-tauri/src/api_server.rs`'s module doc
- Linux AppImage packaging (electron-builder config, `npm run build:appimage`, dev-vs-packaged path resolution in `main.js`) → "Packaging (Linux AppImage)" above
