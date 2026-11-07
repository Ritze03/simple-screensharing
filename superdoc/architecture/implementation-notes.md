# Implementation Notes: Where the Real Code Diverged from the Plan

One-paragraph orientation: `superdoc/brainstorm/simple-screensharing-platform.md` is the
original design plan and `superdoc/architecture/protocol-spec.md` is the binding wire
contract — neither is rewritten here. This page is the third leg: the record of what
changed, was refined, or was discovered *while building against those two documents*, so a
future reader isn't left wondering why the code doesn't match them exactly. Read this after
`architecture/overview.md` if you're about to touch the relay or client and something looks
different from the spec/brainstorm.

**Split 2026-11-07:** this page used to be one chronological dump. The decisions that still explain current code now live in topic pages (below); this page keeps the rework foundation, platform support, the removed-Electron history, the status list and one resolved gap.

Sources for everything below: `server/` and `client/` in this monorepo (module doc comments,
code, and version history). Entries dated before 2026-10-14 were written when these were the
separate `simple-screensharing-server` / `simple-screensharing-client` repos, and their file
references use those old names (now `server/` and `client/`). Cross-checked against
`architecture/protocol-spec.md`.

## Topic pages

- `client-reconnect-and-watches.md`: stopped streams end the watch, re-share resume, auto-reconnect, clean exit (#36, #40).
- `webtransport-cert.md`: WebTransport library choice, datagram cap, certificate lifecycle (decisions behind `protocol-spec.md` §11).
- `protocol-decisions.md`: ConnId = nonce prefix, `frame_id`, AAD, authenticated PTS, keyframe-on-subscribe, postcard.
- `relay-media-forwarding.md`: media forwarder task, `StreamRoute` snapshot, bounded lossy queue.
- `bug-history.md`: capture stop hang, frame pacing, audio start / jitter / late join, QC passes.

## Rework foundation: monorepo, shared protocol, native decoder, lib consolidation (2026-10-14)

Same-day sibling of the WebTransport block below (newest first). **Context for everything
here:** the whole client was reworked. Electron, the HTML frontend and the Tauri sidecar were
replaced by **one native egui app with a room-centric UI**. A **browser client** is coming
in `docs/`, served by GitHub Pages. The relay gains WebTransport (the block below). The plan and
its decisions (Goal, D1-D34) were in a plan file that no longer exists in the tree; recover it from git history if needed. The
requirement driving the foundation work: screen sharing at **>= 120 fps end to end, ideally 240**.
The foundation tasks below landed before any UI work and are the divergence from the
former "Electron + sidecar" architecture (older entries on this page describe it; `overview.md`
has the current map).

- **I1, monorepo.** The workspace root is now one repository (GitHub
  `Ritze03/simple-screensharing`, the rework was done on branch `rework`, merged into `master` 2026-10-15). `client/` and `server/` were
  imported from the old separate repos **with full history** (a `filter-branch` subdirectory
  rewrite, then a merge with `--allow-unrelated-histories`); the old branches are kept as
  `archive/client/*` (86) and `archive/server/*` (7) (deleted from GitHub 2026-10-15; kept in a local git bundle outside the repo; where and how to restore: `.claude/local/reference-repos.md` in the main checkout). Root Cargo workspace: members `client`,
  `client/src-tauri` (since deleted), `server`, `protocol`, resolver 3; the root `Cargo.lock` started from the
  client's. The old nested repo directories and the empty `simple-screensharing-website/` were
  deleted. `Why:` server, desktop client and the upcoming browser client must change in
  lockstep, and the project owner asked for one repo. Consequences still visible in the tree:
  a few older entries in this file still use the pre-monorepo paths
  (`end_to_end_test.rs::server_repo_candidates`, which looked for them, was replaced by `server_binary_path`;
  see "Verification gap" in `webtransport-cert.md`).
- **I2, shared protocol crate.** New `protocol/` (`simple-screensharing-protocol`) is the
  single definition of the wire types and constants; `client/src/protocol.rs` and
  `server/src/protocol.rs` re-export it. I6 then deleted the relay-only header wrappers and moved
  the relay onto the shared types. The `wire_bytes_unchanged` test pins the postcard bytes of every
  enum variant against bytes captured from the old code, so the extraction provably did not
  change the wire. `Why:` the client's `protocol.rs` was an independently derived copy of the
  spec (its old doc said to "diff the two if suspecting drift"); one crate makes drift
  impossible and gives the browser client's work one reference.
- **I3, decoder rework** (`client/src/decode.rs` + `decode/{codec,video,frame}.rs`).
  *Diverges from the plan/spec, which said H.264 only, and from the earlier notes that call
  `StreamDecoder` dead code.* Codec is **sniffed** from the first keyframe AU carrying parameter
  sets (no wire tag: D5, keeps protocol and relay unchanged); pipeline
  `parse ! va{h264,h265,av1}dec ! NV12 ! appsink(sync=false)` with software fallback
  (`avdec_h264` / `avdec_h265` / `av1dec`; dav1d not installed) on build/link failure, bus error
  or no frame in 2 s, replaying pushed AUs and remembered per codec. The leaky queue was replaced
  by the decoder thread's own backlog (above 64 AUs it jumps to the newest keyframe) because the
  leaky queue dropped the keyframe while VA initialised. Output is zero-copy
  `Arc<DecodedFrame>` via a cloneable `FrameSource`; decoded fps is counted by a pad probe,
  separately from presented fps, which fixes the old bug where `current_fps` only counted inside
  `latest_frame()`. Legacy `latest_frame() -> ColorImage` stays for the old UI. 1080p
  benchmark (`client/examples/decode_bench.rs`): H.264 541 HW / 250 SW fps, H.265 488 / 166,
  AV1 463 / 110. `Why:` the sender already encoded H.265/AV1 but the Rust decoder could only do
  software H.264, which cannot reach 120-240 fps. Detail: `features/video/decode-and-display.md` "Native
  decode".
- **I4, sidecar logic moved into the lib.** What only lived in
  `client/src-tauri/src/commands.rs` (since deleted) now lives on `session::AppState`; the sidecar kept only thin wrappers:
  create/join/forget room, the settings setters with their clamps, start-stream defaults,
  `edit_destination`, bandwidth total/floor, allocated kbps and overshoot, the audio-level tap,
  volume resolution, supported codecs. The JS encoder presets became `client/src/presets.rs`:

  | preset | tune | quality | GOP |
  |--------|------|---------|-----|
  | LowLatency | LowLatency | Performance | 2000 ms |
  | Balanced | BestQuality | Balanced | 2000 ms |
  | Quality | BestQuality | Quality | 2000 ms |
  | Gaming | LowLatency | Performance | 750 ms |

  (Table as of 2026-10-15, #88: the rate-control column is gone, every encode is Dynamic/VBR; before that LowLatency and Gaming were `Cqp` (UI "Static"), see `features/stream/rate-control.md`.)

  Current presets are Text / Quality / Gaming (2026-11-04, D8): `client/src/presets.rs`, `features/stream/encoder-tuning.md` "Presets: Text, Quality, Gaming".

  `Why:` the Electron/Tauri layers were deleted in the egui rework, and the new egui UI must stay a thin view
  over `AppState` rather than re-implement what the sidecar did.
- **I5, the bin links the lib.** `client/src/main.rs` no longer re-declares the core modules
  (they previously compiled twice, once per crate); it declares only `ui`, `theme` and
  `widgets`. `lib.rs` exports `presets` and `decode` alongside the older modules.
- **I8, egui design foundation.** New `theme.rs` tokens, the `widgets/` slot-row helpers and a
  `--gallery` screen (design QC). Documented under `superdoc/ui/` by a separate task; the
  module map in `overview.md` has the one-line entry.

## Platform support

- **VAAPI hardware encoding with automatic software fallback.** This fulfills the original
  "GPU-accelerated with CPU fallback" requirement. `capture.rs`'s `detect_encoder_backend`
  probes for a working `vah264enc` element once per process (cached via `OnceLock`,
  confirmed working against an AMD GPU via `gst-inspect-1.0 vah264enc`) and
  transparently falls back to software `x264enc` if VAAPI construction or startup fails for
  any reason. Callers of `start_sharing` never choose a backend explicitly. See
  `client/src/capture.rs`, `EncoderBackend`,
  `detect_encoder_backend`, `probe_vaapi_encoder`.
- **Windows support required platform-gating the Linux capture path and adding a native
  Windows one.** `ashpd` (the xdg-desktop-portal `ScreenCast` binding, Linux/D-Bus-only) is
  now behind `#[cfg(unix)]`; a parallel `#[cfg(windows)]` path launches GStreamer's
  `d3d11screencapturesrc` instead, needing no portal/consent flow. Everything downstream of
  the platform-specific source element (`videoconvert`, the `tee`, the encoder, the preview
  branch) is identical on both platforms. See `client/src/capture.rs`'s
  module doc comment and `acquire_capture_source`. Confirmed compiling successfully via real
  GitHub Actions Windows CI
  (the workflow file `client/.github/workflows/windows-build.yml`, `windows-latest`
  runner, **no longer exists**: removed; how Windows is built today: `features/windows.md`), which also uploads a downloadable `.exe` + GStreamer runtime DLL bundle as a build
  artifact so a Windows-using friend can smoke-test it without a local toolchain.
- **Windows has no audio-sharing capability yet — a known limitation, not a bug.** This is
  within original scope: the brief only ever required audio capture on Linux.
  `client/src/audio.rs`'s whole approach (shelling out to `pw-dump`/
  `pw-cli` (a null sink, since #121; it was `pw-loopback`)/`pw-link`) is PipeWire-specific and has no Windows equivalent implemented. A
  Windows build compiles (the module isn't `cfg`-gated out) but audio source discovery and
  capture will simply fail at runtime there.

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
`superdoc/architecture/implementation-notes.md` (this page), `superdoc/features/video-pipeline.md` ("History")
and `superdoc/features/viewer-window.md` ("History").

One rule from that era still binds the live code. `Why the live encoder-tuning controls (tune /
quality level; Tune is no longer a user-visible control, Quality level remains) go through tier 2 (`BranchSwap`) rather than a property write:`
GStreamer's `x264enc` / `vah264enc` don't expose those as properties writable on a `Playing`
element the way `bitrate` is (`launch_fragment` bakes them into the construction string), so
"change one" is definitionally "build a new encoder", which is what a branch swap is. Routing
them through the existing `classify_change` / `swap_to_new_branch` machinery gives them
make-before-break, the wait-for-first-IDR gate and the `KeptOld`-on-failure net for free
(`superdoc/features/stream/go-live-and-destinations.md`). The regression test for the profile/level change a swap
can cause lives at the `capture`/`net` layer, `end_to_end_test.rs`
(`late_watch_after_all_three_tuning_controls_toggled_gets_a_real_keyframe_every_time`).

## Status

**Built and exercised:**

- Full QUIC control-plane (join/presence/subscribe/leave), media-datagram fragmentation
  and reassembly, AEAD encryption/decryption, and preview thumbnails — proven end-to-end
  (real networking, real crypto, real relay routing, real GStreamer decode) by
  `client/src/end_to_end_test.rs` using `videotestsrc` in place of real screen content.
- Screen capture on Linux (portal + PipeWire) with H.264 / H.265 / AV1 VA-API hardware
  encode and software fallback; per-application audio capture and Opus encode (PipeWire).
- **240 fps end to end**: H.265 with VA encode and decode; AV1 and the H.264 Gaming
  presets reach about 240 with the Performance setting (`features/stream/capture-pipeline.md`
  "Capture/encode performance").
- Native video decode (VA with software fallback) and the wgpu renderer, in the room view
  and in pop-out viewports (`features/video-pipeline.md`, `features/viewer-window.md`).
- **Multi-destination fan-out, transfer and the bandwidth modes**, with sender-side
  idle-pause and forced-keyframe-on-subscribe per destination; live encoder edits
  (presets, Quality level) through the tier-2 `BranchSwap` machinery (`features/stream/go-live-and-destinations.md`).
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

## Known gaps at time of writing (2026-07-24) (resolved)

**Resolved (checked 2026-11-07):** both gaps below are handled in `session.rs` `drain_events`: the `ClientEvent::SubscriberCount` arm calls `notify_subscriber_count` and the `ClientEvent::KeyframeRequest` arm calls `notify_keyframe_request` on the capture feeding that stream. Kept as the record of the original gap.

Not deviations from a decision, just work the plan described that had not landed yet — see
the "Status" section above for the current list. One worth flagging
specifically here because it's easy to miss by reading the protocol code alone: the relay
already sends `RelayToClient::SubscriberCount` and `RelayToClient::KeyframeRequest` (per
spec §9, e.g. `server/src/relay.rs`'s `Subscribe` handler), and the
client's `net.rs` already receives and forwards both as `ClientEvent`s — but
`client/src/ui/mod.rs`'s event loop (then `ui.rs`) currently just discards them (see the
`ClientEvent::SubscriberCount { .. } | ClientEvent::KeyframeRequest { .. } => { /* ... */ }`
arm). So the brainstorm's idle-stream optimization (pause/reduce encoding at 0 subscribers)
and forced-keyframe-on-subscribe are wired at the protocol level but not yet acted on by the
sender's encoder — `capture.rs` doesn't currently listen for either event.
