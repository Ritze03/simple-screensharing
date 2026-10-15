# Implementation Notes: Where the Real Code Diverged from the Plan

One-paragraph orientation: `superdoc/brainstorm/simple-screensharing-platform.md` is the
original design plan and `superdoc/architecture/protocol-spec.md` is the binding wire
contract — neither is rewritten here. This page is the third leg: the record of what
changed, was refined, or was discovered *while building against those two documents*, so a
future reader isn't left wondering why the code doesn't match them exactly. Read this after
`architecture/overview.md` if you're about to touch the relay or client and something looks
different from the spec/brainstorm.

Sources for everything below: `server/` and `client/` in this monorepo (module doc comments,
code, and version history). Entries dated before 2026-10-14 were written when these were the
separate `simple-screensharing-server` / `simple-screensharing-client` repos, and their file
references use those old names (now `server/` and `client/`). Cross-checked against
`architecture/protocol-spec.md`.

## Native client: stopped streams, AV1 size, staged-modal meter (2026-10-14, task #40)

Found by I19 in a real two-client run.

- **A stopped stream ends the watch.** `drain_events` ignored the relay's `StreamStopped` for
  `RoomSession::watches`, so the viewer kept a frozen 0 fps tile (and "Watched streams 1"). It now
  calls `RoomSession::end_watch_of_stopped` (before `note_stream_stopped`, which forgets the name and
  label): the watch entry, hence the decoder, goes; `ui::video::VideoViews::sync` then drops the GPU
  `VideoView` on the next frame. The selection moves on. No `unsubscribe` is sent (the relay already
  dropped the stream and its subscriber list). The relay sends `StreamStopped` for every stream of a
  member that leaves or drops, so that case is covered too. The former Electron shell (removed) did this in
  `driver::prune_stale_watches`; the egui path never had it.
- **Re-share resumes the watch (decision).** The stopped watch is remembered in
  `RoomSession::resume_after_stop` for `RESUME_AFTER_STOP` (30 s) and matched like a reconnect's
  (`WantedStream`: owner name + kind + label, `rewatch_one`), so the same person going live again
  takes the dead tile's place with no click, with the same selection and volume.
  `Why resume:` stop/start is how a sharer switches monitor or fixes a stuck share; a viewer who was
  watching means to keep watching, and the reported symptom was the new share showing up as a separate
  card. `Why only 30 s (not the reconnect's 60 s):` a deliberate stop is not an outage; minutes later an
  automatic subscribe would be a surprise. A user's own "stop watching" never queues a resume.
  Separate from `rewatch` because each entry has its own deadline and a different cause.
- **AV1 1080p arrives as 1082 lines.** See `features/video-pipeline.md` ("Visible size"): the
  sender (VCN4) really encodes 1082 lines with no render size; the receiver crops to the announced
  size.
- **Staged "Set up your stream" modal: the meter never moved.** Level taps exist only after
  `UiAction::StartLevelTaps`, and only the bar's audio popover sent it; a staged capture has no audio
  share to read a level from either. `stream_settings::sync_level_taps` now starts a tap for every audio
  source while the staged modal is open (restarted when the source list changes) and stops them on
  any footer button / Escape / click-outside; the Source card reads the chosen source's tap
  (`cx.source_level`). Known gap: if the shell closes the modal by itself (capture or room vanished)
  the taps run until the next `StartLevelTaps`/`StopLevelTaps`; fixing that needs a hook in
  `ui/mod.rs`.

## Native client: auto-reconnect and clean exit (2026-10-14, task #36)

Found by I20 in real runs: after a relay restart the egui client sat on "Not connected" until a
manual Connect + Watch, and a killed/closed client lingered in the roster for 60-75 s (relay idle
timeout) because nothing sent `Leave` or closed the QUIC connection. Both fixed in `session.rs` /
`net.rs`; the browser core already behaved this way.

- **Dropped room = suspended, not torn down.** `drain_events` used to call `disconnect_room` on
  `ClientEvent::ControlClosed`. It now calls `suspend_room`: the `RoomSession` goes, a
  `ReconnectState` in `AppState::reconnects` takes its place (kept `RoomHandle`, last-known roster
  and streams, attempt count, next retry, last error, cause). `is_reconnecting(join_string)` is true
  for the whole outage (also between attempts), `Status` stays `Connecting` (the UI only repaints
  on that), so the existing UI shows its Reconnecting banner with no edits. Only for rooms still in
  the saved list; user Leave / Forget / Cancel and `shutdown` never reconnect.
  `Why the handle is kept:` everything keyed by `RoomHandle` (a destination's `room`,
  `active_room`, the UI's staged shares and views) stays valid across the outage with no remapping.
  `Why no session while reconnecting:` the UI derives Reconnecting from "no session for a saved
  room but `is_reconnecting`", so a half-dead session would show as joined.
- **Retry policy** (`session::retry_delay`, pure): first retry immediate, then 0.5 s, 1, 2, 4, 8,
  capped at 15 s, forever. Per-attempt timeout 8 s (`RECONNECT_ATTEMPT_TIMEOUT`; a manual connect
  keeps 15 s) so a dead relay cannot hold the single connect slot and starve the backoff. The
  failure kind comes from the QUIC application close code (`net::CloseCause`, read with
  `ScreenshareClient::close_cause()` / `CloseCause::from_error`): `0x200` relay shutdown retries
  at once, `0x102` room full retries but never faster than 5 s (the error is kept in
  `last_error`, `ReconnectState::room_full()`), `0x101` malformed stops and surfaces
  (`ConnectOutcome::Failed`, `Status::Error`), anything else backs off. `connect()` now returns a
  typed `JoinRejectedError` instead of a bare `bail!`, and falls back to the close code if the
  relay's close beat the `JoinRejected` frame.
- **Our shares come back by rebuilding the branch, not re-pointing it.** A branch is bound to the
  old connection (client clone, nonce space, announced `StreamId`; the new connection has a fresh
  `ConnId`). `suspend_destination` removes the branch (`spawn_remove_destination`) and records
  the params; the *capture* (screen grab, portal session) keeps running, so there is no new picker
  prompt. After the reconnect `add_destination_with_params` rebuilds each branch on the new room
  (the same primitives `retry_destination` uses): no `capture.rs` change needed. Audio shares are
  bound to the connection as a whole, so they are stopped and restarted with the source they were
  started with (`AppState::audio_share_sources`). A capture left with no destination is invisible
  to the UI; cancelling/forgetting the room stops it. Transfer halves are not resumed.
- **Watches come back by name.** `WantedStream { owner_name, kind, label }` (the browser's
  `stableKey` minus the room); `match_wanted` picks the first not-yet-watched stream of a member
  with that name, so two "Screen"s of one owner map to two streams. Unmatched ones wait in
  `RoomSession::rewatch` for 60 s (`REWATCH_GRACE`, the browser's resume window) and resolve as
  re-announces arrive, because a sharer that reconnected is indistinguishable from one that
  stopped and started again, and after a relay restart the owners rejoin at different moments.
  Selected watch and per-owner volume carry over; mute and pop-out do not.
- **Clean exit.** `disconnect_room` (user Leave, Forget) sends `Leave` and closes the connection
  (`ScreenshareClient::leave_and_close`, code `0x300`). `AppState::shutdown` /
  `shutdown_blocking(timeout)` do that for every room, close the endpoint with a bounded (500 ms)
  `wait_idle` so the close frames really leave the socket, and stop the captures alongside (a
  pipeline can take seconds to drain; it must not delay the leave). `install_shutdown_signal_handlers`
  turns SIGTERM/SIGINT/SIGHUP into a window close so the normal `on_exit` path runs; a second
  signal or 4 s without exit force-quits. `Why via the window:` the state belongs to the UI
  thread, so a signal task can only ask that thread to run the shutdown.
- **Known gaps.** (1) `capture.rs` binds a capture's room-preview task to the *first*
  destination's client; after a reconnect it keeps failing on the dead connection, so others see
  no thumbnail of our share until it is restarted. Fix: `ensure_preview_task` restarts the task
  when its client's connection is closed, which needs `preview_sink` kept (cloned) instead of
  `take()`n. (2) A relay that dies without a `CONNECTION_CLOSE` (SIGKILL, power loss) is only
  noticed on the next packet or after the 60 s idle timeout.
- **UI wiring (task #37).** The banner's Retry now / Cancel are `UiAction::RetryNow(key)` /
  `CancelReconnect(key)` (`reconnect_now` / `cancel_reconnect`; Cancel also aborts a first-time
  connect); the old `CancelConnect` action is gone. The banner adds "Attempt N · Retrying in N s"
  (or "Trying now…", "Room full, still trying") and one short "Last error" line from
  `reconnect_info`. The People panel lists the last-known roster dimmed, with our own row
  prepended from the display name (the lib's copy leaves us out). `on_exit` calls
  `shutdown_blocking(1.5 s)` and `App::new` installs the signal handlers. A staged (not yet
  live) share survives an outage; one whose room is really gone has its capture stopped.
  "Live" (People pill, sidebar cast icon, "Live in N rooms") now means a destination that is
  on air (`Cx::my_phase` / `on_air_rooms`); while Starting the UI says so, and the share card
  carries the idle-screen hint as a tooltip. `Why:` a share waiting for an idle screen's
  first frame used to read as Live although nobody saw anything.

## Rework foundation: monorepo, shared protocol, native decoder, lib consolidation (2026-10-14)

Same-day sibling of the WebTransport block below (newest first). **Context for everything
here:** the whole client was reworked. Electron, the HTML frontend and the Tauri sidecar were
replaced by **one native egui app with a room-centric UI**. A **browser client** is coming
in `docs/`, served by GitHub Pages. The relay gains WebTransport (the block below). The plan and
its decisions live in `.claude/teamlead/plan/rework-the-entire-thing.md` (Goal, D1-D34). The
requirement driving the foundation work: screen sharing at **>= 120 fps end to end, ideally 240**.
The foundation tasks below landed before any UI work and are the divergence from the
former "Electron + sidecar" architecture (older entries on this page describe it; `overview.md`
has the current map).

- **I1, monorepo.** The workspace root is now one repository (private GitHub
  `Ritze03/simple-screensharing`, the rework was done on branch `rework`, merged into `master` 2026-10-15). `client/` and `server/` were
  imported from the old separate repos **with full history** (a `filter-branch` subdirectory
  rewrite, then a merge with `--allow-unrelated-histories`); the old branches are kept as
  `archive/client/*` (86) and `archive/server/*` (7) (deleted from GitHub 2026-10-15; kept in the local git bundle `../simple-screensharing-archive-branches.bundle`, restore: `git fetch <bundle> 'refs/heads/archive/*:refs/heads/archive/*'`). Root Cargo workspace: members `client`,
  `client/src-tauri` (since deleted), `server`, `protocol`, resolver 3; the root `Cargo.lock` started from the
  client's. The old nested repo directories and the empty `simple-screensharing-website/` were
  deleted. `Why:` server, desktop client and the upcoming browser client must change in
  lockstep, and the project owner asked for one repo. Consequences still visible in the tree:
  `end_to_end_test.rs::server_repo_candidates` and a few older entries in this file still use
  the pre-monorepo paths (see "Verification gap" in the block below).
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
  software H.264, which cannot reach 120-240 fps. Detail: `features/video-pipeline.md` "Native
  decode".
- **I4, sidecar logic moved into the lib.** What only lived in
  `client/src-tauri/src/commands.rs` (since deleted) now lives on `session::AppState`; the sidecar kept only thin wrappers:
  create/join/forget room, the settings setters with their clamps, start-stream defaults,
  `edit_destination`, bandwidth total/floor, allocated kbps and overshoot, the audio-level tap,
  volume resolution, supported codecs. The JS encoder presets became `client/src/presets.rs`:

  | preset | tune | quality | rate control | GOP |
  |--------|------|---------|--------------|-----|
  | LowLatency | LowLatency | Performance | Qvbr | 2000 ms |
  | Balanced | BestQuality | Balanced | Qvbr | 2000 ms |
  | Quality | BestQuality | Quality | Qvbr | 2000 ms |
  | Gaming | LowLatency | Performance | Qvbr | 750 ms |

  (Table as of 2026-10-15, #87: LowLatency and Gaming were `Cqp` until then; `Qvbr` = UI "Dynamic", `Cqp` = "Static", see `features/stream-control.md` "Rate control renamed Dynamic/Static".)

  `Why:` the Electron/Tauri layers were deleted in the egui rework, and the new egui UI must stay a thin view
  over `AppState` rather than re-implement what the sidecar did.
- **I5, the bin links the lib.** `client/src/main.rs` no longer re-declares the core modules
  (they previously compiled twice, once per crate); it declares only `ui`, `theme` and
  `widgets`. `lib.rs` exports `presets` and `decode` alongside the older modules.
- **I8, egui design foundation.** New `theme.rs` tokens, the `widgets/` slot-row helpers and a
  `--gallery` screen (design QC). Documented under `superdoc/ui/` by a separate task; the
  module map in `overview.md` has the one-line entry.

## WebTransport leg and the rotating pinned certificate (2026-10-14)

Spec: `protocol-spec.md` §11. What was decided while building it, and why:

- **`web-transport-quinn` 0.13 (kixelated/moq-dev), not `wtransport` or `h3` +
  `h3-webtransport`.** It runs on a `quinn::Connection` the relay's own endpoint already
  accepted (quinn 0.11, rustls/ring — `default-features = false, features = ["ring"]` to
  avoid dragging in aws-lc), so ALPN dispatch is one `match` after the handshake and
  `Endpoint::set_server_config` rotation keeps working. `wtransport` wants to own its
  endpoint (cannot share the port with the native ALPN); the `h3` route means hand-assembling
  the session plumbing. Its `Session::raw(conn)` ("pretend a QUIC connection is a WebTransport
  session so raw QUIC and WebTransport are supported simultaneously") made the planned
  transport trait unnecessary: **both transports are the one `Session` type** from
  `accept_session` on, and `ConnState`/`RouteTarget` simply hold a `Session`.
- **Datagram cap 1000, measured.** The first guess (1100, derived from QUIC MTU arithmetic)
  was wrong for browsers: headless Chromium 153 reports `datagrams.maxDatagramSize === 1024`
  against this relay while quinn would allow ~1162 natively. Since the relay cannot re-fragment
  (no key), the cap is applied by every sender in `fragment.rs`
  (`MAX_MEDIA_DATAGRAM_LEN` in the protocol crate).
- **Cert lifecycle.** 13-day ECDSA P-256 (browsers: ≤ 14 days, P-256 only), `notBefore`
  back-dated by 1 h *inside* the 13 days; rotation at < 2 days left; persisted to
  `state/` (gitignored, key file 0600) so a restart keeps the hash. A companion `cert.meta` file
  stores `notAfter` and the SAN list because reading `notAfter` back out of the PEM would need
  an X.509 parser dependency; missing/mismatched meta simply regenerates. Test hooks:
  `--cert-validity-secs` / `--cert-rotate-before-secs`, and `CertSource::SelfSigned`'s fields.
- **Hash encoding: base64url, no padding** — shortest, and the same codec as the key field,
  so the website decodes both with one helper. 4th, optional join-link field.
- **Native client does not enforce the link's hash.** Rotation every ~11 days would turn every
  saved link into a connect failure. It logs the mismatch and records the presented hash for
  `current_cert_hash()`. Known limitation: a native connection that outlives a rotation keeps
  reporting the old hash until it reconnects (no protocol message announces rotation).
- **Relay cleanup done in passing:** `server/src/protocol.rs` no longer wraps
  `MediaHeader`/`PreviewHeader` — the relay uses the shared crate's types directly.
- **Verification gap noticed:** `client/src/end_to_end_test.rs::server_repo_candidates` still
  looks for the relay binary under the old multi-repo layout
  (the old per-repo `simple-screensharing-server/target/...`), so its
  `--ignored` tests cannot find the monorepo's `target/` binary without help.

## TUI front-end removed (2026-07-26)

- **`src/tui.rs` (the `--tui` terminal front-end) is deleted, along with the `--tui`/
  `--join`/`--name`/`--log-file` CLI flags in `main.rs` and the `ratatui`/`crossterm`/
  `ratatui-image` dependencies in `Cargo.toml`.** It served its purpose — proving
  `net.rs`/`capture.rs`/`decode.rs` worked correctly without needing the full GUI, see
  "Terminal UI mode for the client" below for why it was built — and the project owner has
  since confirmed that functionality and moved on. `ui.rs`'s egui GUI is about to get a full
  redesign and is now the client's only front-end; keeping a second, simpler front-end in
  sync with that redesign (as the QC follow-ups below already found necessary once) would be
  pure maintenance cost with no one relying on it. `main()` now always launches the eframe
  window; no flag is needed or accepted to choose a front-end (`-h`/`--help` still works).
  `net.rs`/`capture.rs`/`decode.rs` themselves were untouched — nothing in them was
  TUI-exclusive; they were always shared, front-end-agnostic modules, exactly as the module
  map below still describes.

## Protocol refinements (settled during protocol-spec design)

These aren't bugs — they're places where writing the precise wire spec (needed because the
brainstorm left the nonce/counter discipline underspecified, see the brainstorm's "Final
Verify Results" section) produced a cleaner design than "implement the brainstorm literally"
would have.

- **`ConnId` and the AEAD `client_id` are one 64-bit id, not two.** The brainstorm treated
  "the relay's routing/presence identifier for a connection" and "the AES-GCM nonce prefix"
  as separate concepts. `protocol-spec.md` §3.1 unifies them: a client picks one fresh-random
  `u64` per connection, presented in `Join`, and it does double duty as the relay's `ConnId`
  *and* the nonce's `client_id` prefix. Why: the nonce is transmitted in cleartext anyway
  (nonces are public by design), so there's no confidentiality cost to reusing it as the
  public routing handle, and it removes an entire class of "these two ids drifted out of
  sync" bugs. See `server/src/protocol.rs` (`pub type ConnId = u64`)
  and the identical type in `client/src/protocol.rs`.
- **`frame_id` in the media datagram header is the sender's nonce counter value, not a
  separate counter.** Per spec §6.2/§6.5, each frame is encrypted once with
  `nonce = conn_id ‖ counter`, and that same `counter` is written into the header as
  `frame_id`. This gives fragment-grouping ("which fragments belong to the same frame") and
  nonce reconstruction for free from one field, instead of maintaining two counters that
  would need to stay in lockstep. See `client/src/fragment.rs`
  (`fragment_frame`'s `frame_id: u32` parameter) and `net.rs`'s decrypt path.
- **A 2-byte AAD (`version ‖ stream_id`) authenticates stream identity.** The nonce already
  binds `conn_id` and `counter`, but not `stream_id` (a sender's multiple streams — e.g. two
  monitors, or video + audio — share the same nonce space). Without AAD, a relay could
  relabel a fragment's `stream_id` in the cleartext header and the receiver would decrypt it
  successfully under the wrong stream. Binding `stream_id` (and `version`, for good measure)
  as AAD makes that undetectable relabeling impossible — a mismatch fails AEAD
  authentication outright. Spec §5.3; implemented in both `net.rs`'s `encrypt_frame`/
  `decrypt_frame` and mirrored in `preview.rs`'s `encrypt_preview`.
- **An 8-byte authenticated PTS is prepended to frame plaintext.** The brainstorm explicitly
  called out (§"Final Verify Results") that no A/V-sync timestamp scheme existed and flagged
  it as worth adding, even though "glitches are fine" made it non-blocking. The spec resolved
  it: a `u64` big-endian capture timestamp (microseconds) is prepended to the codec payload
  *before* encryption, so it's authenticated by the same AEAD tag as the frame data — a relay
  can't forge or strip it without breaking authentication. See spec §6.4 and
  `client/src/decode.rs`, which strips the PTS after decrypt.
- **Keyframes are requested on every new subscriber, not just the 0→1 transition**, with a
  ~100ms server-side debounce per stream to collapse a burst of near-simultaneous
  `Subscribe`s into one `KeyframeRequest`. The brainstorm's "SUBSCRIBE-triggered forced
  keyframe" language reads as viewer-count-triggered (0→1); the actual relay logic sends one
  on *every* successful new subscription (a second viewer joining also gets a fresh keyframe
  immediately, rather than waiting for the encoder's normal 2-second GOP boundary), simply
  debounced so a thundering herd of subscribers doesn't cause an IDR burst. See
  `server/src/relay.rs`, `KEYFRAME_REQUEST_DEBOUNCE` (100ms) and the
  `Subscribe` handler's `newly_added`/`send_keyframe` logic.
- **`postcard`, not `bincode`, is the control-message wire encoding — decided mid-scaffold.**
  Both repos were scaffolded in parallel before the protocol spec was written. The client's
  initial scaffold commit pinned `bincode 2.0.1` (bincode 3.0.0 on crates.io is a maintainer
  tombstone — an intentional compile error pointing at the unmaintained crate); the server's
  parallel scaffold had already picked `postcard`. The two were reconciled onto `postcard`
  once the protocol spec settled the wire format (spec §2), and both `Cargo.toml`s now agree
  on `postcard = "1.1.3"`.

## Platform support

- **VAAPI hardware encoding with automatic software fallback.** This fulfills the original
  "GPU-accelerated with CPU fallback" requirement. `capture.rs`'s `detect_encoder_backend`
  probes for a working `vah264enc` element once per process (cached via `OnceLock`,
  confirmed working against the dev machine's AMD GPU via `gst-inspect-1.0 vah264enc`) and
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
  (`client/.github/workflows/windows-build.yml`, `windows-latest`
  runner), which also uploads a downloadable `.exe` + GStreamer runtime DLL bundle as a build
  artifact so a Windows-using friend can smoke-test it without a local toolchain.
- **Windows has no audio-sharing capability yet — a known limitation, not a bug.** This is
  within original scope: the brief only ever required audio capture on Linux.
  `client/src/audio.rs`'s whole approach (shelling out to `pw-dump`/
  `pw-loopback`/`pw-link`) is PipeWire-specific and has no Windows equivalent implemented. A
  Windows build compiles (the module isn't `cfg`-gated out) but audio source discovery and
  capture will simply fail at runtime there.

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

Three bugs the user found by hand and filed in the repo-root `issues.md`. Issue #2 (high
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
  audio frequently produced a share error instead of working audio. Root cause: PipeWire
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
  Confirmed live against a real Spotify audio stream: 6 of 299 buffers reached the sink
  before the fix, 296 of 299 after. Fix: a new `AudioTimeline` (`decode.rs:1462`) re-bases
  incoming sender PTS onto the receiver pipeline's own running time (100ms initial lead),
  using the same self-healing re-base-on-discontinuity approach as `PacingState` on the
  video side. Note: the video path was never exposed to this specific footgun because it
  runs `sync=false` and paces frames manually instead of relying on GStreamer's own
  PTS-based sink sync (see the frame-pacing fix above); audio still relies on
  `autoaudiosink`'s built-in sync, so it needed its own PTS re-basing layer instead.

## Terminal UI mode for the client — `--tui` (2026-07-25, removed 2026-07-26)

**Removed 2026-07-26**: `src/tui.rs`, the `--tui` flag, and its `ratatui`/`crossterm`/
`ratatui-image` dependencies are gone — see "TUI front-end removed" below. The entries in
this section are kept as the historical record of why the mode was added and how it worked;
none of it describes current code any more.

Filed as its own dated block rather than folded into "Bugs found via manual testing
(2026-07-25)" above, which is specifically that day's bug list; this is an additive
feature decision, not a bug.

- **Why it exists: making the client drivable by an AI coding agent.** The project owner
  wanted an agent to participate in the dev loop, and an eframe/egui window is the one part
  of this client an agent can neither drive nor observe. `simple-screensharing-client
  --tui` runs `src/tui.rs`, a ratatui front-end that can be launched in a tmux pane, driven
  with `tmux send-keys`, and read back with `tmux capture-pane` — so "did sharing and
  watching actually work?" becomes a scriptable, seconds-long check instead of something
  only a human at a monitor can answer. It is explicitly a **development/testing
  convenience and additive**: no flag means the egui window, byte-for-byte as before, and
  `ui.rs` remains the real UI.
- **Golden path only, on purpose.** Join (typed/pasted link, or a saved lounge from
  `rooms.rs`), presence roster, one screen share, watching one video stream, leave/quit.
  Deliberately omitted: audio share/listen and volume, preview thumbnails, pop-out windows,
  saving new lounges, i18n. None of them is needed to answer the question above, and each
  would be a second implementation to keep in sync with `ui.rs`. Everything below the UI —
  `net.rs`'s `ScreenshareClient`/`ClientEvent` channel, `capture.rs`, `decode.rs`,
  `rooms.rs` — is reused verbatim; there is no TUI-specific networking or decoding.
- **Image backend: Unicode half-blocks, forced — not auto-detected.** Video is rendered
  with `ratatui-image`, pinned to `ProtocolType::Halfblocks` via `Picker::halfblocks()`
  rather than `Picker::from_query_stdio()` (which would auto-select Sixel/Kitty/iTerm2).
  Reason: half-blocks are just the characters `▀`/`▄` with a foreground and background
  colour, i.e. ordinary styled text in the ratatui buffer, so they survive tmux and SSH
  with zero terminal support **and** come back out of `tmux capture-pane` as readable
  content. The graphics protocols are out-of-band escape sequences that need a specific
  terminal emulator, may not pass through a tmux pane, and would be invisible to
  `capture-pane` — which would defeat the entire purpose. `Picker::halfblocks()` is also
  the only constructor that touches neither stdin nor stdout, which matters with the
  alternate screen already up. **The accepted tradeoff:** half-block output is low
  resolution (one cell = 2 pixels vertically) and colour-quantised — fine for "is this
  stream live, moving, and roughly the right picture?", useless for judging image quality.
  Judge quality in the egui GUI. The crate is pulled in with `default-features = false`
  specifically to avoid its default `chafa-dyn` feature, which links system libchafa; the
  built-in pure-Rust encoder needs nothing installed. `ratatui`/`crossterm` versions match
  the relay server's `Cargo.toml` on purpose, so the two repos don't drift apart.
- **Threading: the render/input loop owns the main thread; the tokio runtime is entered,
  not `block_on`-ed.** This is exactly the arrangement `ui.rs` already lives in, reused
  rather than reinvented: crossterm's blocking `poll`/`read` must not sit on a tokio
  worker, and every async call (connect, subscribe, start/stop capture) is `tokio::spawn`ed
  with its result collected next tick via `try_recv` — `ui.rs`'s pattern with eframe's
  `update()` callback swapped for an explicit loop. A dedicated OS thread (the shape
  `server/src/tui.rs` uses) would work too but buys nothing here: the
  server needs one because its TUI must coexist with a QUIC listener that owns the process,
  whereas in `--tui` mode this loop simply *is* the process's foreground.
- **`tracing` is redirected to a file, for the same reason the relay server's is.** The TUI
  owns the terminal, so stdout/stderr logging would scribble over the alternate screen.
  `--tui` writes to `$TMPDIR/simple-screensharing-tui.log` (overridable with `--log-file`,
  which matters when running two instances in one tmux session) and shows the path in the
  footer. The default GUI path still uses `tracing_subscriber::fmt::init()` unchanged.
- **Run it `--release`.** Re-encoding a 1920x1080 frame to half-blocks is an `image`
  downscale per frame; optimized that lands at the intended ~8-10fps cap, unoptimized it
  costs about two seconds per frame and the video pane crawls. A build-profile artifact,
  but a confusing one if you hit it without knowing.
- **Verified live, not just compiled.** Two `--tui` instances in a tmux session against a
  real relay: join, roster, real portal-based screen capture of a 1920x1080 desktop, and
  the far side watching it as half-blocks in its pane at 8fps with real 24-bit colour
  (~124 MB relayed). Regression coverage lives in `src/tui.rs`'s tests — including an
  `#[ignore]`d end-to-end one that drives the module's real `drain_events`/`refresh_video`/
  `draw` against a real relay child process and a real `videotestsrc` capture into a
  ratatui `TestBackend`, asserting the decoded SMPTE pattern actually reaches the terminal
  buffer as distinctly-coloured cells. It uses `capture.rs`'s `#[cfg(test)]` test-source
  seam for the same reason `end_to_end_test.rs` does: the portal consent dialog is the one
  step that can't be automated.

### QC follow-ups on `--tui` (2026-07-25)

Two gaps found reviewing the first pass, both fixed in `src/tui.rs`:

- **An externally delivered signal used to leave the terminal bricked.** crossterm's raw
  mode clears `isig`, so a Ctrl-C *keypress* never becomes a signal — it arrives as a plain
  key event and was always handled. But a signal from *outside* the terminal (`kill`/`pkill`
  from another shell, a supervisor, a shell killing the process group) hit the default
  disposition and killed the process between `ratatui::init()` and `ratatui::restore()`.
  Reproduced live: `kill -INT` on a running `--tui` exited 130 with `stty -a` still
  reporting `-echo -isig` and the TUI's last frame frozen on the alternate screen — the user
  has to blind-type `reset`. `watch_for_termination_signals` now catches SIGINT/SIGTERM/
  SIGHUP and **sets a flag** rather than restoring the terminal from the handler: that
  routes the signal through the render loop's normal exit, so `App::shutdown`'s
  share-stop/Leave teardown still runs and `ratatui::restore()` still happens exactly once,
  on the main thread that owns the terminal. Worst-case latency is one `TICK` (50 ms).
  Verified live for all three signals: exit 0, `echo`/`isig` restored, alternate screen
  exited, in-band Ctrl-C and Esc still unaffected. No hand-rolled *panic* hook is needed —
  `ratatui::init()` installs one that restores the terminal before unwinding.
- **Leaving while a share was still starting leaked the portal session.** `disconnect` only
  matched `Share::Active`, so an in-flight `Share::Starting` was dropped along with its
  `oneshot` receiver. The spawned `start_sharing` runs to completion regardless, and its
  `CaptureHandle` then lands in a `send` nobody is listening to and is dropped bare —
  `Drop` stops the GStreamer pipeline but explicitly *cannot* close the portal session (an
  async D-Bus call), so on the `l`-to-leave path, where the process keeps running, the OS
  screen-recording indicator stays lit and the relay never hears StopStream. `disconnect`
  now awaits the in-flight start and then calls `stop()` on it properly.

Checked and found *not* to be problems, recorded so the next pass doesn't re-investigate:
`drain_events` matches all six `ClientEvent` and all four `PresenceEvent` variants
explicitly with no `_ => {}`, so a new variant is a compile error rather than a silent drop;
audio frames can never be fed to a video decoder because `ui.rs` allocates audio and video
stream ids from one per-connection counter, so a peer's ids never collide; the `pw-loopback`
orphan-process class cannot occur in `--tui` at all, since the mode spawns no child
processes and has no audio path; and `default-features = false` genuinely keeps `chafa`/
libchafa and `pkg-config` out of the build graph (`cargo tree -e build` has no `pkg-config`
edge, and the 122 new `Cargo.lock` entries are mostly the feature-independent union Cargo
records — `termwiz`/`termina` never compile).

## Performance fixes

- **One high-refresh-rate sharer used to slow down everyone else in the room.** Root cause
  was lock *volume*, not lock duration: media forwarding ran inline in the connection
  actor's `select!` and re-acquired the **room-wide** `tokio::sync::Mutex` once per QUIC
  **fragment**. Since a frame is split into ~1200-byte datagrams (spec §6.3), a 60 fps
  sharer takes that room-wide lock hundreds of times a second — serializing every other
  member's control messages and every other sharer's media behind one busy stream. The
  fan-out work itself was never the problem (`quinn::Connection::send_datagram` is
  non-blocking). Fixed in two halves: (1) each connection got a **media forwarder task**,
  so the actor just validates a header and `try_send`s onto a bounded queue and returns
  immediately; (2) each stream got a `StreamRoute` — an immutable `Arc<[RouteTarget]>`
  fan-out snapshot behind a `std::sync::RwLock`, republished under the room mutex on every
  subscriber change (`RoomState::refresh_route`) and read *without any room lock* per
  fragment. `StreamState::subscribers` stays authoritative; the route is a derived cache.
  See `server/src/room.rs`'s module docs for the lock-ordering rules
  (the route lock is a strict leaf) and `src/relay.rs`'s `media_forward_task`.
- **Why the media queue is bounded and lossy (depth 512, drop-on-full).** The old design
  was lossless but room-stalling; the new one is lossy but isolated. This is safe because
  media is unreliable by construction all the way down: the spec's media plane is
  drop-and-continue with no retransmit (§6.2/§6.5), quinn's own `send_datagram` already
  silently drops the oldest queued datagram when a connection's send buffer overflows, and
  the client's `Reassembler` (`client/src/fragment.rs`) discards an
  incomplete frame as soon as a newer `frame_id` arrives, with a 200 ms backstop. A
  relay-side drop is therefore indistinguishable to the receiver from a drop in transit —
  same missing fragment, same discarded frame, no confusion and no double-counting. An
  unbounded queue would instead grow without limit and add latency to every later fragment.
  Regression coverage: `relay::tests::media_forwarding_follows_subscription_changes` and
  `relay::tests::subscription_churn_under_media_load_keeps_routes_consistent` (the latter
  runs on a multi-threaded runtime so the room mutex and the route lock are genuinely
  contended, and asserts the route snapshot never diverges from the subscriber set).

## QC findings

> **History note:** the 2026-07-27 entry below was written in the Electron/Tauri era and names
> the removed Tauri app and `#[tauri::command]` layer. Kept as a dated record; the egui client
> has no such layer.

### 2026-07-27 — final pre-egui-removal QC pass: real relay + real Tauri app, GUI automation blocked

Ran against current master (`79d4276`, includes the `join_room` UDP-bind fix). Set up a real,
isolated environment: a headless nested `sway --backend headless` compositor, a real relay
server child process (the old `simple-screensharing-server` repo, now `server/`,
release build), and two Tauri app instances (`target/debug/simple-screensharing-client-tauri`)
each with fully isolated `XDG_CONFIG_HOME`/`XDG_DATA_HOME`/`XDG_CACHE_HOME`, all screenshotted
via `grim`.

**Launch/render verified real and clean**: both instances started with no crash (confirming
the `join_room` UDP-bind fix's class of regression — the app previously panicked on startup
before that fix), rendered the full shell correctly, and produced genuinely isolated on-disk
state (separate WebKit/mesa caches, separate app-data dirs; no cross-instance bleed).

**GUI automation (clicking/typing through the real window) could not be made reliable in this
sandbox — an environment limitation, not a product bug.** Diagnosed with `wev` (Wayland event
viewer) as ground truth against a fresh compositor instance: injecting synthetic input via
`wlr-virtual-pointer-unstable-v1`/`virtual-keyboard-unstable-v1` (tried via `ydotool`'s uinput
path — never reached the nested compositor at all, since its headless backend has zero
`libinput` input devices to pick up a uinput device from — and via `wlrctl`, built from source
against this system's wlroots headless output) reliably produces `enter`/`modifiers` but the
compositor never delivers the actual `wl_pointer.button` or `wl_keyboard.key` event to the
client — instead sending two spurious `leave` events, every time, regardless of whether the
motion and the click/keypress happen in the same virtual-device connection or separate ones,
and regardless of an inserted `wl_display_roundtrip` between them (both fixes attempted and
rebuilt into a patched local `wlrctl`; scratch copy at `/tmp/wlrctl-src`, not committed
anywhere — this system's sway 1.12 headless backend appears not to complete the
virtual-input→seat-focus→event-delivery path for either protocol). No screen-capture portal
compatible with plain `sway` (not Hyprland/GNOME/KDE) is installed either
(`xdg-desktop-portal-wlr` is not present and installing it requires `sudo`, unavailable in this
session) — consistent with `architecture/overview.md`'s Status section already listing "the
literal Wayland portal handoff of real screen content" as not yet exercised.

**What was verified instead, as the closest real substitute:** all 5 real, non-`#[cfg(test)]`
`#[ignore]`d integration tests that exercise this exact session/business logic against a real
relay child process and a real GStreamer pipeline (`videotestsrc` standing in only for the
portal handoff, per their own module docs) pass cleanly on current master:
`end_to_end_test::real_capture_encrypt_fragment_relay_decode_round_trip`,
`end_to_end_test::repeated_pause_resume_cycles_keep_resuming`,
`end_to_end_test::resubscribe_after_pause_resumes_encoder_and_frames_flow`,
`ui::tests::fan_out_transfer_and_absolute_bandwidth_across_three_rooms`,
`ui::tests::media_ingestion_keeps_feeding_the_decoder_while_drain_events_is_starved`. Full
`cargo test --workspace` (159 + 2 passed, 0 failed) also stayed green. No code changes were
made this pass — nothing was found that was fixable at the client/server code level; the
blocker is this sandbox's headless-Wayland input plumbing.

`Why this matters for the egui-removal decision:` this pass could not literally click through
the compiled Tauri app's UI to prove the frontend JS → `#[tauri::command]` → `session.rs`
wiring end-to-end at the pixel level, which is what items 2–11 of the requesting task strictly
asked for. It *did* prove the app launches correctly under the fix that was the main open risk
going into this pass, and it re-confirmed (fresh, on current master) that every piece of logic
those commands delegate to is real and working. Recommend one follow-up pass with either a
real display session (not headless-sandboxed) or a working input-injection path before treating
GUI-level coverage as closed — this pass could not deliver that half honestly.

- **`Drop` backstops for both capture handles.** `CaptureHandle` and `AudioCaptureHandle`
  each gained a synchronous `Drop` impl (found necessary via a dedicated resource-cleanup QC
  pass, not the end-to-end test above) that force-stops the GStreamer pipeline — and, for
  audio, also kills the orphaned `pw-loopback` child process (whose own `Drop`, per
  `std::process::Child`, does *not* kill the OS process) — for the case where `.stop()` is
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

## Known gaps at time of writing (2026-07-24)

Not deviations from a decision, just work the plan describes that hasn't landed yet — see
the Status section of `architecture/overview.md` for the full list. One worth flagging
specifically here because it's easy to miss by reading the protocol code alone: the relay
already sends `RelayToClient::SubscriberCount` and `RelayToClient::KeyframeRequest` (per
spec §9, e.g. `server/src/relay.rs`'s `Subscribe` handler), and the
client's `net.rs` already receives and forwards both as `ClientEvent`s — but
`client/src/ui.rs`'s event loop currently just discards them (see the
`ClientEvent::SubscriberCount { .. } | ClientEvent::KeyframeRequest { .. } => { /* ... */ }`
arm). So the brainstorm's idle-stream optimization (pause/reduce encoding at 0 subscribers)
and forced-keyframe-on-subscribe are wired at the protocol level but not yet acted on by the
sender's encoder — `capture.rs` doesn't currently listen for either event.
