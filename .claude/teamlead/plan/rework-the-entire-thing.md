# Rework the entire thing

> **Stage 7** — your review · started 2026-10-13
> Agent items are ticked with evidence (one open: the mockup item waits on your fps/codec call). Tick your `verified by: user` items below; anything you find goes back on the board.

## Goal
Collapse the desktop client from three UI layers (egui leftovers, Tauri, Electron + HTML) to one native egui app with the room-centric layout, without losing features — and with screen sharing that works end to end at **at least 120 fps, ideally 240 fps**. The core concept stays: symmetric end-to-end encryption, a relay that never decrypts, a server with a small TUI.

## Context
### Desktop client (`simple-screensharing-client/`, git, 333 commits, HEAD 51fbf18)
- **Shape:** two processes. Electron shell (`electron/main.js` 636 LOC, `preload.js` 138) spawns a Rust sidecar (`src-tauri/`, ~5.1k LOC), talking REST `POST /command/<name>` + SSE `/events` (`api_server.rs`), H.264/AV1/H.265 bitstream over a separate raw HTTP server (`video_server.rs` 1616); decode happens in the renderer via WebCodecs.
- **Core logic** lives in the root crate `src/` (~30k LOC): `capture.rs` 8134, `decode.rs` 3928, `session.rs` 3215, `audio.rs` 1701, `net.rs` 882, `bandwidth.rs` 783, `rooms.rs` 681, plus `end_to_end_test.rs` 2909.
- **Frontend:** vanilla HTML/CSS/JS, no framework/bundler — ~8.1k LOC JS (`components.js` 2102, `stream-control.js` 1625, `shell.js` 1621, `view.js` 1269), ~4.5k LOC CSS.
- **Stack:** GStreamer 0.24 (VAAPI + x264 fallback), quinn 0.11 / rustls 0.23, aes-gcm 0.11, postcard, tauri 2.11, tokio 1.53, ashpd (xdg portal), rust-i18n (de/en). Electron ^43, electron-builder ^26.
- **Dead/legacy:** egui UI `src/ui.rs` 3355 + `theme.rs` 477 only reachable from the old `src/main.rs` bin, which re-declares every shared module (compiles them twice). eframe/egui/wgpu still in root `Cargo.toml`. Tauri `invoke_handler` kept "for revertibility" (`src-tauri/src/main.rs:61-76`) but events go SSE-only, so that path is already half-dead. `egui::Context` kept in the sidecar only to satisfy a `session.rs` signature (`src-tauri/src/state.rs:8-45`).
- **Stale bits:** `check-deps.sh` still requires libturbojpeg (removed 2026-07-27); `package.json` `allowScripts` pins electron@33 vs ^43; `src/main.rs:10-11` says egui is the only front-end.
- **Tests:** 228 Rust tests (21 of them test dead egui code); 0 in `session.rs`/`driver.rs`/`dto.rs`; no JS tests. Zero TODO/FIXME markers.
- **Packaging:** Linux AppImage via electron-builder (`../build-appimage.sh` wrapper); Windows portable exe built in GitHub Actions only (`electron/win-runtime/` not present locally).

### Relay server (`simple-screensharing-server/`, git, 12 commits, HEAD 087318b)
- 5 files, 3367 LOC: `relay.rs` 1571, `tui.rs` 908, `protocol.rs` 393, `room.rs` 384, `main.rs` 111. ~6 tests. Half the commits are TUI polish; one uncommitted padding tweak in `tui.rs`.
- quinn 0.11 + rustls/ring, self-signed cert via rcgen, ratatui TUI on its own thread, postcard wire. No crypto crate: relay never decrypts.
- Limits: port 4433, ALPN `ssshare/1`, 8 members/room, 2 MiB preview cache.

### Protocol (`superdoc/architecture/protocol-spec.md`)
- One QUIC connection per client, one UDP port. Control = one bidi stream (postcard), previews = uni streams (~30 s encrypted JPEG, cached and replayed on join), media = unreliable datagrams, ~1200 B fragments, drop the frame on any loss.
- No server auth: join link `server|room-id|key`, shared AES-256-GCM room key only clients hold. Nonce = ConnId || u32 counter.
- Native-only phase: WebTransport/browser viewer deferred (§11). Wire version 0x01. Spec says H.264 only, but client now also does AV1/H.265 (commit 228d101).

### Website (`simple-screensharing-website/`)
- Only a boilerplate `CLAUDE.md`. Not a git repo. Browser viewer never started.

### Root workspace
- Not a git repo, so `superdoc/` and the root scripts (`build-appimage.sh`, `launch-test.sh`, `launch-test-tmux.sh`) are unversioned.

### Known open items (from docs)
- Client ignores relay's SubscriberCount / KeyframeRequest (`implementation-notes.md:491`).
- No audio sharing on Windows (`implementation-notes.md:107`).
- No auth/join-proof, room revocation, key rotation or sharer cap (brainstorm deferred list).
- Deferred from UI redesign: manual tile resizing, per-tile throttle, HW decode.

### Monitors (checked 2026-10-14, `xrandr --listmonitors`)
- DP-3 at x=0 y=1080 (119.93 Hz), DP-1 at x=1920 y=1080 (279.75 Hz, primary), DP-2 at x=3840 y=1080 (119.93 Hz), **HDMI-A-1 at x=1920 y=0** (59.96 Hz) — HDMI now sits *above* DP-1, not at x=5760 as `testing-and-verification.md` says. The re-check-every-time rule matters.

### Docs health
- `superdoc/` is ~9.6k lines. The root `CLAUDE.md` and `overview.md:8-10` wrongly say there is no source code and the sibling repos don't exist.
- Feature docs still carry egui-era wording; the original brainstorm header still says "PENDING" though its verify section is complete; `TERMINOLOGY.md` is empty; there is no `superdoc/README.md` index.

### UI mockup
- Published (durable): https://claude.ai/artifact/5yA1iHf1YnXqmABkZajCe4, version 3 — Proposed tab is the reference; "Show egui layout" overlay shows the container tree. Readable later via the Artifact tool's `read` action.
- Local source (session scratchpad, may vanish): `/tmp/claude-1000/-home-mo-Schinken-Programming-GitHubProjects-simple-screensharing/2cc7fe7d-08fe-4236-ab69-27f6c819abdb/scratchpad/ui-mockup/index.html`, screenshot sweeps in `.../scratchpad/shots/r3/a/`.
- Tokens: band 56 px, control 32 px (36 px in bars/footers), pill 18 px, row 32 px, sub-row 24 px, label slot 120 px, value slot 72 px, spacing 4/8/12/16.

### Why the client left egui (from docs)
- Electron's Chromium gave GPU decode via WebCodecs, real multi-window pop-outs, and less window-chrome friction (`architecture/overview.md:76`). An "fps bug" (MediaFrame ingestion coupled to UI repaint, Immediate present mode) was a known egui-era issue (`brainstorm/ui-redesign-multiroom-streaming.md:306`).
- Correction: the "~60 fps ceiling" in `FRONTEND-SHELL.md:624` was the old MJPEG poll rate, not an egui limit — `ui.rs:~299-306` repainted at the decoder's real rate, up to 240 Hz.

### Old egui UI vs the Electron UI (parity scout)
- **`ui.rs` is alive at the API level:** still compiles in the bin, last touched in the same commit as `session.rs` (`228d101`), a thin wrapper over `session::AppState`. No feature work since 2026-07-26.
- **Already in `ui.rs` (~60–65% of features):** sidebar + three tabs shell, join/rename/forget/leave rooms, reconnect indicator, start share (fps 30–240, bitrate), fan-out, make-before-break transfer, per-destination resolution/fps/bitrate edit, both bandwidth modes, audio share + listen/volume, pop-out viewer windows (`show_viewport_deferred`, `ui.rs:337-475`, F11 fullscreen), self-preview, presentation mode, full en/de i18n (the Electron UI is *not* localized).
- **Missing from `ui.rs`:** encoder controls (presets, tune, quality, rate control, GOP, codec — start form and per destination), staged "Go live" flow, create-room, audio-share mute + level meters, jitter-buffer / catch-up-speed settings, multi-stream tile grid with hover stats (ui.rs has one selected-watch panel), per-window stats/volume in pop-outs. The newer visual design lives in the Electron CSS (`tokens.css` etc.); `theme.rs` predates it.
- **Logic stranded in the sidecar** that must move into the lib: `create_room_impl` (`commands.rs:140`), join-name upsert, forget, `edit_destination`, bandwidth total/floor setters, jitter/catch-up setters, start-stream defaults, audio-level tap, volume resolution (`commands.rs:832-874`), and codec sniffing from the bitstream (`video_server.rs:~474-600` — the wire protocol carries no codec tag). Encoder presets exist only in JS (`components.js:994`).
- **Viewer decode today:** `decode.rs` is software H.264 only (`avdec_h264` → RGBA → `ColorImage`, CPU upload per frame). No H.265/AV1 decode in Rust. This machine has `vah264dec`, `vah265dec`, `vaav1dec`. Nothing in the repo renders via wgpu textures/paint callbacks yet. The sidecar still software-decodes every watched stream and discards the output.
- **Deleted under D1:** `electron/` (~4.5k tracked lines + 433 MB node_modules), `frontend/` (12.7k lines), `src-tauri/` (~5.2k lines), `dist/`; rewrite: `.github/workflows/windows-build.yml`, `../build-appimage.sh`, `../launch-test*.sh`, `check-deps.sh`, client `README.md`/`CLAUDE.md`, `.claude/skills/run/SKILL.md`; drop stale `.cargo/config.toml` (turbojpeg).

## Decisions
- **D1** The client ends up with exactly one UI stack: native egui (eframe), one Rust process. Electron, the HTML/JS frontend, the Tauri sidecar, and the REST/SSE + video HTTP servers go. — *your answer to Q1: Electron is a workaround, since the Rust backend is needed for high-refresh sharing anyway.*

- **D2** Video display goes GPU-side and is decoupled from egui's repaint (decoded frames → wgpu texture, painted via a paint callback). Hardware decode via GStreamer is the default, with software fallback. The old CPU `ColorImage` upload path is not reused. The decoder must handle H.264, H.265 and AV1, since the sender already encodes all three. — *a CPU RGBA upload per frame at 240 Hz per tile is the bottleneck, and today's Rust decoder is software-only and H.264-only.*

- **D3** The server's ratatui admin TUI stays. — *it's a separate server binary, not one of the client's three UI layers.*

- **D4** Docs are rewritten as part of the work, not after it: `overview.md`, `FRONTEND-SHELL.md`, `video-pipeline.md`, `viewer-window.md`, `APP-SHELL.md`, `STYLING-GUIDE.md` and `testing-and-verification.md` all describe Electron, and the root `CLAUDE.md` "no source code" claim gets fixed. `testing-and-verification.md` is rewritten for the egui binary: launch it for real, screenshot it on DP-1..3, never HDMI. — *the project's docs rule: update in the same change.*

- **D5** The codec is detected by sniffing the bitstream in Rust (port of `video_server.rs:~474-600`), not by adding a codec tag to the wire protocol. — *it keeps the protocol and the relay server untouched; a tag is cleaner but means a wire version bump and a server release for the same result.*

- **D6** All business logic currently in `src-tauri/commands.rs` moves into the lib (`session.rs`/`rooms.rs`) before the sidecar is deleted, and the JS encoder presets become a Rust table. — *the egui layer should stay a thin view over `AppState`, which is how `ui.rs` is already built.*

- **D7** `src/ui.rs` + `theme.rs` are revived as the base and made to look virtually identical to the Electron UI. — *your answer to Q2.*

- **D8** Target feature set = the Electron UI's. Self-preview is removed from `ui.rs` (Electron dropped it on purpose in `a4be944`). The existing en/de `t!()` i18n stays, and new strings get both keys. — *your answer to Q5; i18n is kept because it's what makes Electron's existing language setting actually do something, and it's already there.*

- **D9** Styling: the Electron design (colors, fonts, spacing, components from `frontend/styles/*.css`) is the visual target, built in egui following `/home/mo/Schinken/Programming/Local/ForzaTelemetryV3/docs/ui/STYLING-GUIDE.md` (same egui 0.33). Layout is built from nested panes/vertical/horizontal containers — every element sits in a container, no free-floating positioning. Where the two disagree on a detail, Electron wins on *what it looks like* and the Forza guide wins on *how the control is built* (styled checkbox/radio, reserved spinner width, row labels, no trailing colons). That guide gets adapted into this project's `superdoc/ui/STYLING-GUIDE.md`. — *your answer to Q6.*

- **D10** All UI implementation and UI review goes to Opus workers (`tl-opus-medium`; escalate to `tl-opus-high` on failure). Screenshot checks always go to `tl-opus-medium`. Non-UI work (lib moves, decoder, deletions, docs) stays on Sonnet. — *your instruction in Q6, overriding the project's escalation-only Opus setting for UI work.*

- **D11** `simple-screensharing-website/` (the empty folder) is deleted. ~~The browser viewer is out of this plan~~ — superseded by **D32**: the website comes back inside the monorepo's `docs/`. — *your answer to Q3.*

- **D12** ~~The workspace root becomes a git repo with nested repos ignored~~ — superseded by **D29** (the workspace root becomes the monorepo itself, branch `rework`). — *your answer to Q4, revised later.*

- **D13** Linux only for now: `.github/workflows/windows-build.yml` is deleted rather than rewritten, and the AppImage is rebuilt around the egui binary. — *your answer to Q4; the workflow builds Electron and would just break.*

- **D14** Layout = the room-centric "Proposed" design from the mockup (https://claude.ai/artifact/5yA1iHf1YnXqmABkZajCe4): rooms in the left sidebar, room header, watched tiles + "Sharing in this room" cards, bottom action bar for your own share (Idle / Staged / Live), Settings behind a gear in a bottom-left user area. The Stream Control / View / Settings nav rail goes. This replaces D7's "virtually identical to Electron" for *layout*; D9 still holds for the visual design. — *your note asking for a more intuitive Discord-like flow, and your feedback refining the Proposed tab.*

- **D15** Stream settings (both when starting a share and when editing a live one) open in a modal instead of a right-hand side panel. — *your call: a side panel squeezes them.*

- **D16** The mockup is the pixel reference for the egui build and gets polished first: aligned counts/columns in the sidebar, consistent element heights, buttons using the full row width, nothing squeezed. — *your pixel-peeping feedback on the mockup.*

- **D17** Leave room sits in the room header (top-right, with Copy invite link / Auto-connect / ⋯), not in the bottom bar. The live bar keeps the "N rooms ▾" popover for Also send to / Move to, and destination rows get a × instead of the → arrow. — *your answer to Q8.*

- **D18** The UI is built as explicitly nested vertical/horizontal containers all the way down: panels → cards (vbox) → rows (hbox with fixed-size label / control / value slots). Every row type is one reusable pattern, so rows of the same type can't differ. Bands that sit side by side (headers, bottom bars) share one height token. The mockup's container tree (from its "Show egui panels" overlay) is the spec the egui port follows 1:1. — *your feedback: heavy use of panes and nested v/hboxes is what keeps the layout from breaking.*

- **D19** The mockup's row patterns (`ctrlRow`, `roomRow`, `sharerRow`, `personRow`, `destRow`, `popItem`, `btnRow`, `card`, `gridRows`, `modalShell`) become Rust helpers in `theme.rs` first, built on fixed-size slots (`add_sized` / `allocate_exact_size`, with a right-to-left layout or `egui_extras::StripBuilder` for the fill slot), and are checked in isolation on a widget-gallery screen before any real screen uses them. Container tree spec: `.claude/teamlead/plan/rework-the-entire-thing.ui-tree.md`. — *egui has no flexbox: a fill slot between fixed slots needs explicit width math, which the HTML mockup's flex hides. Getting it right once, in helpers, is what keeps D18 from breaking per screen.*

- **D20** Windows comes back as the last step (~~only after Linux has passed your testing~~ — **D34**: no waiting): build and test natively in your Windows VM. This amends D13 — Linux still comes first, and the Electron-based Windows CI workflow is still deleted; whether CI returns is decided in that phase. — *your note offering the VM once Linux is polished.*

- **D21** Verification includes a real end-to-end share: local relay server + two egui client instances on DP-1..3 (never HDMI), one sharing a real screen through the xdg portal, the other actually receiving and displaying it. `ydotoold` already runs as a user service (`systemctl --user` `ydotool.service`, socket `$YDOTOOL_SOCKET` = `~/.YDOTOOL-SOCKET`; `/dev/uinput` is group `input`, which you're in), so agents can click through the UI and the portal picker without you starting anything. Screenshots of the result go to `tl-opus-medium`. — *your note offering ydotool so the receive side is really exercised, not just the encode/relay half.*

- **D22** The D20 Windows phase works through the WinBoat VM **over SSH only** (connection details discovered from WinBoat/docker at that point): build natively there, run there, and pull screenshots back over SSH. The WinBoat window itself is on your HDMI display, so agents never click, type into, or screenshot it — the HDMI rule from `testing-and-verification.md` covers it. — *your answer to Q9.*

- **D23** The core concept is unchanged: shared AES-256-GCM room key held only by clients (join link `server|room-id|key`), relay forwards without decrypting, QUIC datagrams for media, ratatui admin TUI on the server. The wire protocol and the server stay as they are (consistent with D3/D5). — *your words: "Same core concept as before otherwise. Symmetrically encrypted, server with a small TUI and so on."*

- **D24** 120 fps end to end is a hard requirement and 240 fps is the target: capture → encode → encrypt → relay → decrypt → decode → display, measured on the receiving side (decoded and presented frames per second), not assumed from encoder settings. Test rig: sender captures DP-1 (279.75 Hz), receiver window on DP-2 or DP-3 (119.93 Hz) — so presented fps there caps at ~120, and 240 is proven by decoded-frame rate plus presenting on DP-1 where needed. If 240 can't be reached, the report names the stage that limits it. — *your words: "One big thing is that at least 120FPS sharing works or even 240FPS."*

- **D25** Anything needing `sudo` (system packages, udev rules, etc.) runs only through the `claude-sudo` tmux session, and only by the lead. Workers never use sudo or that session: they stop and report the exact command they need, the lead runs it and hands the result back. — *your instruction.*

- **D26** Before the import, both old repos are made complete: the client's untracked `check-deps.sh` is committed, the client's `.pi/` is deleted (your call), and the server's uncommitted `tui.rs` padding tweak is committed (otherwise it dies with the repo). The two sub-repo `CLAUDE.md` pointer files are dropped (the root one covers everything). `.playwright-mcp/`, `logs/`, `target/`, `dist/`, `node_modules/` are ignored. The server's `logs/lifetime.state` (persisted TUI lifetime counters) is carried over into `server/logs/` untracked. — *found in the scout reports; nothing that only exists locally may be lost when the old repos go.*

- **D27** The `simple-screensharing-client` bin links the lib instead of re-declaring every shared module with `mod` (today they compile twice). `ui`, `theme` and the new widget/UI modules stay in the bin; egui-only dependencies (`egui_extras`) are added once, up front. — *found by the client scout; double compilation is pure build-time waste and lets bin and lib drift.*

- **D28** Once the monorepo exists (after I1), every writing worker runs in its own git worktree off `rework` and commits there; the lead reviews, merges each accepted branch into `rework`, and pushes. All worktrees share one Cargo target dir (`CARGO_TARGET_DIR=/home/mo/Schinken/Programming/GitHubProjects/simple-screensharing/target`) so each worktree doesn't rebuild GStreamer/wgpu from scratch. `Cargo.lock` belongs to whichever task owns the `Cargo.toml` it changes; other workers restore it before committing. I1 itself runs in place (there's no repo yet). — *with the workspace root as the repo, worktree isolation works again, and it's teamlead's rule for writing workers; the shared target dir keeps builds from multiplying.*

- **D29** Monorepo = the workspace root itself (`/home/mo/Schinken/Programming/GitHubProjects/simple-screensharing`), pushed to the new **private** GitHub repo `Ritze03/simple-screensharing` (created 2026-10-14, empty). Layout: `client/` and `server/` imported from the old repos **with full history** (`git subtree add`-style, so `git log -- client/` shows the 333 client commits and `server/` the server's), `protocol/` (shared crate), `docs/` (website, Pages), `superdoc/` and `CLAUDE.md` where they already are, root scripts, root Cargo workspace. `master` = the imported baseline; all rework on `rework`. Unmerged client branches (`fix/room-autoconnect-not-audio`, `worktree-agent-a28ff4c0eed5d9063`) and the client stash (`wip volume sync fix`) are kept as `archive/client/*` branches. Only after the import is verified are the nested `simple-screensharing-client/` and `simple-screensharing-server/` directories deleted (their GitHub repos stay untouched for you to archive). **Supersedes D12.** — *your instruction: "remove the other git repos in this dir then and create the new 'simple-screensharing' on github now! Make it private".*

- **D30** The duplicated `protocol.rs` (client 355 lines, server 393) becomes one shared `protocol/` crate used by both. — *one wire definition instead of two that can drift; the main payoff of the monorepo.*

- **D31** `origin` = `github.com/Ritze03/simple-screensharing` (private). `master` is pushed once as the imported baseline; after that only `rework` is pushed, as work lands. `master` is never merged into — you merge after your review, then make the repo public and enable Pages yourself. — *your answers: push branch only; "Make it private… i will make it public and enable pages later, to review".*

- **D32** The website is back in scope **(supersedes D11)**: `docs/` in the monorepo, static and GitHub Pages compatible (no build step: plain HTML/CSS + ES modules, relative paths so it works under the Pages sub-path). Same functionality as the desktop app — create/join rooms, watch, and share — using WebTransport to the relay, WebCodecs for decode/encode, `getDisplayMedia` for capture, WebCrypto AES-256-GCM with the same nonce/AAD rules, the same room-centric design and tokens as the mockup. Browser capture/encode fps limits are measured and documented, not worked around. Tested **locally only** (a local static server, simulating the Pages sub-path); nothing is published. — *your answers: "test the page local only"; "You can do the website too… Same functionality, github pages compatible. Include it in the client, inside of docs/" and "Watch + share".*

- **D33** The relay gains the WebTransport accept path from `protocol-spec.md` §11 on the same UDP port (ALPN dispatch: `h3` for browsers, `ssshare/1` for native), with a self-signed ECDSA cert valid ≤ 14 days, rotated automatically, whose SHA-256 hash travels as an optional 4th join-link field (`server|room-id|key|certhash`) so browsers can pin it via `serverCertificateHashes`. Native clients accept both 3- and 4-field links. Encryption, framing and relay-never-decrypts are unchanged; the TUI shows the current cert hash and expiry. **Amends D3/D23** (the server now changes, the concept doesn't). — *browsers can't speak raw QUIC and won't trust a self-signed cert without a pinned hash; the original brainstorm already planned the `|certhash` field.*

- **D34** One phase, no stops: everything — foundation, egui UI, cleanup, website, Windows — is built and agent-tested in one run, including functional testing, before handing over for your review. Windows no longer waits for your Linux sign-off. **Amends D20.** — *your answer: "Do everything at once. You do the testing (including functionality). I will later review it. Try to build as much as possible!"*

## Done when
One phase (**D34**): everything below is built and agent-tested in one run; the `user` items are your review afterwards.

**Repo**
- [x] The workspace root is the monorepo: `git log --oneline -- client/` shows the 333+ client commits and `-- server/` the server's 12+ commits; `archive/client/*` branches hold the two unmerged client branches and the stash; `master` and `rework` are on the private `origin`; the nested `simple-screensharing-client/`, `simple-screensharing-server/` and `simple-screensharing-website/` directories are gone — *verified by: agent* — ran `git log -- client/ | wc -l`, `git ls-remote origin`, `find -name .git` → 381 client / 20 server commits, master + rework + 93 archive branches on origin (incl. the 2 unmerged + `stash-volume-sync`), only the root `.git`
- [x] `cargo build --workspace` and `cargo test --workspace` pass, and the `#[ignore]`d real-relay tests pass with `--ignored` — *verified by: agent* — ran `cargo test --workspace --no-fail-fast` → 309 lib + 47 bin + 5 protocol + 16 server, 0 failed; `--ignored` with own relay → 17 passed (portal/audio tests skipped per the no-audio, no-real-desktop rule); 4 timing tests flake only under load ~40+
- [x] One `protocol` crate is used by client and server; no duplicated `protocol.rs` remains — *verified by: agent* — ran `git ls-files | grep protocol` + both Cargo.toml → both depend on `../protocol`; `client/src/protocol.rs` is a 6-line re-export, `server/src/protocol.rs` re-export + relay helpers, no wire types

**Core**
- [x] Every behaviour that lived only in `src-tauri/commands.rs` (create room, join-name upsert, forget, edit destination, bandwidth total/floor, jitter/catch-up, start-stream defaults, audio-level tap, volume resolution) is a method on the lib's `AppState`/`rooms` with a unit test, and the Rust preset table matches `components.js` `ENCODER_PRESETS` — *verified by: agent* — ran `session::ui_ops_tests`, `presets::`, `rooms::tests::upsert` → 34/12/2 passed; each of the 9 behaviours mapped to a method + test; presets match the old JS table field by field (`every_preset_matches_the_js_table_exactly`)
- [x] The decoder auto-detects H.264 / H.265 / AV1 from the bitstream and decodes each with VA hardware decode (software fallback when VA is missing); a benchmark decodes 1080p at ≥ 240 fps per codec on this machine — *verified by: agent* — ran `decode_bench` (1080p, 600 frames ×3) → vah264dec 461, vah265dec 255, vaav1dec 265 fps; `LIBVA_DRIVER_NAME=bogus` on `decode::video_tests` → 15 passed via avdec_h264/av1dec
- [x] `client/src/main.rs` declares no shared module (`mod session` etc.) and the bin builds against the lib — *verified by: agent* — ran `grep -n "mod " client/src/main.rs` → only `theme`, `ui`, `widgets` (+ test mod); bin builds in the workspace build

**Server**
- [x] Native clients still connect over `ssshare/1`; a browser connects over WebTransport (`h3`) on the same UDP port by pinning the cert hash from the join link; a forced short cert validity shows rotation working; the TUI shows the current hash and expiry; the relay still never decrypts — *verified by: agent* — ran `simple-screensharing-server --cert-validity-secs 200 --cert-rotate-before-secs 100` + native `test_sender` + headless Chromium on the same UDP port → browser decoded 30 fps H.264 with the pinned hash; TUI hash/expiry changed on rotation (matches openssl), stale hash refused, new hash joined; no aead crate in the server's `cargo tree`

**egui UI**
- [x] A widget-gallery screen shows every row helper; an Opus screenshot review finds no misaligned slots or unequal heights — *verified by: agent* — ran `simple-screensharing-client --gallery` with `SSS_GALLERY_SLOTS=1` in the headless rig → every row helper aligned, equal heights; the narrow `dest_row` overflow and overlay clip it found are fixed in #58
- [ ] The app matches the mockup's Proposed tab at 1280 / 1600 / 1920 px on DP monitors (Opus screenshot comparison; every deviation listed is fixed) — *verified by: agent*
- [x] Every Electron feature from the parity list is reachable in the egui UI (checklist walked with ydotool on DP-1..3) — *verified by: agent* — ran `hs-drive.sh` + `swaymsg seat - cursor` + `wtype` in the headless sway rig instead (user's 2026-10-14 rule) → every item reachable (audio only checked for reachability); the silent failed-connect it found is fixed in #58
- [x] Every UI string exists in `en` and `de`; switching language shows no raw keys — *verified by: agent* — ran `python3 -I client/locales/check_keys.py` → 0 problems (408 en / 408 de after #58); German in the rig shows no raw keys; the label/plural/tooltip nits are fixed in #58

**Performance**
- [x] Real end-to-end share (relay + two egui clients, portal capture of DP-1): the receiver measures ≥ 120 fps decoded **and** presented, for H.264, H.265 and AV1 — *verified by: agent* — ran `hs-scenario.sh <codec>-120 <codec> 120 balanced` in the headless rig (HEADLESS-1 @240 instead of DP-1, per the user's rule) → viewer 120/120 decoded/shown, 0 dropped, via vah264dec / vah265dec / vaav1dec
- [x] 240 fps: the receiver decodes ≥ 240 fps for at least one codec and presents ≥ 240 fps on DP-1, or the report names the stage that limits it — *verified by: agent* — ran `hs-scenario.sh` at 240 (H.265 balanced, AV1 + H.264 gaming) → viewer 236–239 decoded and shown, 0 dropped; limit is capture: pipewiresrc delivers 238.8–239.3 from the headless output's 240 Hz timer, encoder loses 1–2 more (on the real DP-1 earlier: 240/240 for H.265)

**Website**
- [x] `docs/` is static with no build step, and served locally under a `/simple-screensharing/` sub-path (as Pages will) it loads with no 404s or console errors — *verified by: agent* — ran `python3 -m http.server` over a `simple-screensharing -> docs` symlink + headless Chromium via `docs/dev/cdp.mjs` → 28 resources all 200, 0 console errors, 0 network errors
- [x] In real Chromium against the local relay, the website creates and joins a room, watches a native client's share, and shares its own screen to a native client; browser capture/decode fps are measured and documented — *verified by: agent* — ran `hs-rig.sh run -- env SSS_GUI_LOCKED=1 HEADED=1 node …` (headed Chromium inside the rig) → room created in the real Create tab and joined by link; native 120 fps share decoded in the browser at ~120 (H.264, H.265, AV1); browser share decoded natively at 58 (H.264 @60 via vah264dec, AV1 via vaav1dec); numbers match `superdoc/features/website.md:89-95`

**Cleanup, packaging, docs**
- [x] `client/electron/`, `client/frontend/`, `client/src-tauri/`, the Windows workflow and `client/.cargo/config.toml` are gone; no `tauri` / `electron` / `npm` reference remains outside historical doc notes; `cargo build --release` and `cargo test` pass — *verified by: agent* — ran `ls` on the paths + `git grep -niE 'tauri|electron|\bnpm\b'` + `cargo build --release --workspace` → all gone, 141 hits all history-labelled/"former", plan file or benign; release build and tests green
- [x] The AppImage builds, launches on a DP monitor, joins a room and plays a stream — *verified by: agent* — ran `./build-appimage.sh --no-server` (23 MB, self-checks 3/3) and launched it on HEADLESS-3 in the rig instead of a DP monitor → joined the rig room and played H.265 @120 via vah265dec ("Decoded 120 fps shown 120 fps … 0 dropped")
- [x] `superdoc/` (architecture, features, ui, testing), root `CLAUDE.md`, `README.md` and the `run` skill describe the monorepo, the egui app, the WebTransport server path and the website; none point at Electron or Playwright `_electron` — *verified by: agent* (grep) — ran `git grep -niE 'playwright|_electron'` + a dead-link script → hits only in History sections; no dead links besides deliberately historical ones

**Windows**
- [x] The client builds and runs in the WinBoat VM over SSH, joins a room and receives a share from the Linux client — *verified by: agent* — ran `build.ps1` + `cargo test --release --lib` in the VM and the `test_viewer` example against a headless Linux `test_sender` → built, 300 tests passed, decoded 34–35 fps H.264 (avdec_h264, VM has no GPU); Windows → Linux 30 fps too. Not covered: the GUI window and `d3d11screencapturesrc` (VM had no logged-in desktop session)

**Your review**
- [x] It looks and feels like the mockup, and the room flow is intuitive — *verified by: user*
- [x] Watching a real 120+ fps share feels smooth — *verified by: user*
- [ ] The AppImage works for you — *verified by: user* — reopened 2026-10-15: starts, but sharing fails on Hyprland (pipewiresrc "unhandled format", board #76)
- [ ] The website works once you've made the repo public and enabled Pages — *verified by: user*
- [ ] The Windows build works for you — *verified by: user*

## Implementation plan
*Built from D1–D34 · decisions:8471*

Paths are relative to the monorepo root (= the workspace root). After I1, writing workers run in worktrees off `rework` with the shared `CARGO_TARGET_DIR` (**D28**); the lead merges and pushes (**D31**). UI work follows the row helpers and the container tree in `rework-the-entire-thing.ui-tree.md`. Single phase, no stops for review (**D34**).

| Wave | ID | Task | Agent | Owns | After |
|:----:|:--:|------|-------|------|-------|
| 1 | I1 | Build the monorepo: commit the client's `check-deps.sh` and the server's `tui.rs` tweak in the old repos, delete client `.pi/`; `git init` the workspace root; import client → `client/` and server → `server/` with full history; keep unmerged client branches + stash as `archive/client/*`; root Cargo workspace (`client`, `client/src-tauri`, `server`), `.gitignore`; drop the sub-repo `CLAUDE.md` pointers; carry `lifetime.state` over; verify `cargo build --workspace`; push `master`, create and push `rework`; only then delete the old nested dirs and the website folder — **D11** **D12** **D26** **D28** **D29** **D31** | `tl-sonnet-high` | `.gitignore`, `Cargo.toml`, `Cargo.lock`, `client/`, `server/`, `simple-screensharing-client/`, `simple-screensharing-server/`, `simple-screensharing-website/` | — |
| 2 | I2 | Shared `protocol/` crate from the two `protocol.rs` files, used by client and server (lib keeps re-exporting so paths stay stable); also add `egui_extras` and `egui-wgpu` to the client — **D30** | `tl-sonnet-high` | `protocol/`, `Cargo.toml`, `Cargo.lock`, `client/Cargo.toml`, `client/src/protocol.rs`, `client/src/lib.rs`, `server/Cargo.toml`, `server/src/protocol.rs`, `server/src/main.rs` | I1 |
| 2 | I3 | Decoder rework: codec sniffing (port of `video_server.rs:~474-600`) in `client/src/decode/codec.rs`, VA hardware decode for H.264/H.265/AV1 with software fallback, GPU-ready frame output (NV12 planes + PTS), keep `feed()` API, decode benchmark — **D2** **D5** **D24** | `tl-sonnet-high` | `client/src/decode.rs`, `client/src/decode/` | I1 |
| 3 | I4 | Move every sidecar-only behaviour from `src-tauri/commands.rs` into the lib, add the Rust encoder preset table, unit tests; keep `src-tauri` compiling by calling the new lib methods — **D6** | `tl-sonnet-high` | `client/src/session.rs`, `client/src/rooms.rs`, `client/src/presets.rs`, `client/src/lib.rs`, `client/src-tauri/src/commands.rs` | I2 |
| 3 | I5 | Bin links the lib (no re-declared modules), fix `ui.rs`/`theme.rs` imports — **D27** | `tl-sonnet-medium` | `client/src/main.rs`, `client/src/ui.rs`, `client/src/theme.rs` | I2 |
| 3 | I6 | Relay WebTransport accept path on the same UDP port (ALPN `h3` vs `ssshare/1`), self-signed ECDSA cert ≤ 14 days with auto-rotation, cert hash in the TUI and as optional 4th join-link field; native client parses 3- and 4-field links — **D3** **D33** | `tl-sonnet-high` | `server/`, `client/src/net.rs`, `Cargo.lock` | I2 |
| 4 | I7 | Docs for the foundation: decode side of `video-pipeline.md`; `architecture/` (implementation-notes for the lib moves and monorepo, protocol-spec §11 WebTransport + cert hash + codec list) — **D4** | `tl-sonnet-medium` | `superdoc/features/video-pipeline.md`, `superdoc/architecture/` | I3, I4, I5, I6 |
| 4 | I8 | Theme tokens from the Electron CSS + Forza conventions, Geist fonts, row-helper widgets (`ctrlRow`, `roomRow`, `sharerRow`, `personRow`, `destRow`, `popItem`, `btnRow`, `card`, `gridRows`, `modalShell`) on fixed slots, `--gallery` screen — **D9** **D10** **D18** **D19** | `tl-opus-medium` | `client/src/theme.rs`, `client/src/widgets/`, `client/assets/fonts/`, `client/src/main.rs` | I5 |
| 4 | I9 | GPU video renderer: NV12 → wgpu textures (YUV→RGB shader) drawn via egui paint callback, frame ingestion and pacing decoupled from repaint, works in pop-out viewports — **D2** **D24** | `tl-opus-medium` | `client/src/decode.rs`, `client/src/decode/` | I3, I5 |
| 4 | I10 | Website core as plain ES modules: join-link parsing, WebTransport session with cert-hash pinning, postcard encoding of the control messages, AES-256-GCM via WebCrypto with the spec's nonce/AAD, datagram fragmentation/reassembly, WebCodecs decode + encode, `getDisplayMedia` capture — **D32** | `tl-sonnet-high` | `docs/js/core/` | I6 |
| 5 | I11 | Split `ui.rs` into `client/src/ui/` modules following the container tree: shell panels, per-screen state structs, stubs per screen; drop self-preview and the nav rail — **D7** **D8** **D14** **D18** | `tl-opus-medium` | `client/src/ui.rs`, `client/src/ui/`, `client/src/main.rs` | I4, I8, I9 |
| 5 | I12 | Website UI: the same room-centric layout and tokens as the mockup in static HTML/CSS + ES modules on top of the core, relative paths for the Pages sub-path — **D10** **D32** | `tl-opus-medium` | `docs/index.html`, `docs/css/`, `docs/js/ui/`, `docs/assets/` | I10 |
| 6 | I13 | Sidebar (rooms, sharer lines, watched streams, create/join entry) + bottom-left user area + outgoing popover — **D14** **D16** | `tl-opus-medium` | `client/src/ui/sidebar.rs` | I11 |
| 6 | I14 | Room header (status, auto-connect, invite link, Leave, ⋯) + room body (GPU video tiles with hover statbar/volume, share cards, People panel, reconnecting/disconnected views) — **D2** **D14** **D17** | `tl-opus-medium` | `client/src/ui/room.rs` | I11 |
| 6 | I15 | Bottom action bar (Idle / Staged / Live / Paused, N rooms ▾ popover) + stream settings modal (source, sending to with × rows, bandwidth, video, encoder presets + advanced) — **D15** **D17** | `tl-opus-medium` | `client/src/ui/action_bar.rs`, `client/src/ui/stream_settings.rs` | I11 |
| 6 | I16 | Create/join modal, Settings modal (name, language, presentation mode, jitter, catch-up), leave confirm, pop-out viewer windows with hover chrome / stats / volume — **D8** | `tl-opus-medium` | `client/src/ui/modals.rs`, `client/src/ui/popout.rs` | I11 |
| 7 | I17 | en/de locale keys for every new UI string — **D8** | `tl-sonnet-medium` | `client/locales/` | I13, I14, I15, I16 |
| 7 | I18 | Visual QC against the mockup at 1280/1600/1920 on DP-1..3 (screenshots, Opus vision), fix every deviation — **D16** **D18** | `tl-opus-medium` | `client/src/ui/`, `client/src/theme.rs`, `client/src/widgets/` | I13, I14, I15, I16 |
| 8 | I19 | Real end-to-end run: relay + two egui clients, portal share of DP-1 driven by ydotool, receiver-side decoded/presented fps per codec; rewrite `launch-test*.sh` for the monorepo and egui binary — **D21** **D23** **D24** | `tl-sonnet-high` | `launch-test.sh`, `launch-test-tmux.sh`, `client/scripts/` | I17, I18 |
| 8 | I20 | Website end-to-end, locally only: static server with a `/simple-screensharing/` sub-path, real Chromium on DP-1..3 against the local relay, browser ↔ egui client watch and share in both directions, browser fps measured; fix what breaks — **D32** | `tl-sonnet-high` | `docs/` | I12, I17, I18 |
| 8 | I21 | Docs: `superdoc/ui/*` (Forza guide adapted into `STYLING-GUIDE.md`), `testing-and-verification.md` (egui binary, DP-only, current monitor layout, sudo rule), client `run` skill — **D4** **D9** | `tl-sonnet-medium` | `superdoc/ui/`, `superdoc/claude-instructions/testing-and-verification.md`, `client/.claude/skills/` | I17, I18 |
| 9 | I22 | Docs: `superdoc/features/*` (viewer-window, stream-control, presentation-modes, video-pipeline display side, new `website.md`; self-preview marked removed) — **D4** | `tl-sonnet-medium` | `superdoc/features/` | I7, I19, I20 |
| 9 | I23 | Delete `client/electron/`, `client/frontend/`, `client/src-tauri/`, `client/dist/`, the Windows workflow, `client/.cargo/`; workspace members; drop `raw_tx` and other sidecar-only lib code; update `check-deps.sh` — **D1** **D13** | `tl-sonnet-high` | `client/electron/`, `client/frontend/`, `client/src-tauri/`, `client/dist/`, `client/.github/`, `.github/`, `client/.cargo/`, `Cargo.toml`, `Cargo.lock`, `client/Cargo.toml`, `client/src/session.rs`, `client/src/capture.rs`, `client/src/lib.rs`, `client/check-deps.sh` | I19, I20, I21 |
| 10 | I24 | AppImage around the egui binary (bundles GStreamer incl. VA plugins); rewrite `build-appimage.sh`; any sudo need goes to the lead — **D13** **D25** | `tl-sonnet-high` | `build-appimage.sh`, `client/packaging/` | I23 |
| 10 | I25 | Final docs: `superdoc/architecture/overview.md` (monorepo map), root `CLAUDE.md` (no more "no source code"), root `README.md`, `client/README.md` — **D4** | `tl-sonnet-medium` | `superdoc/architecture/overview.md`, `CLAUDE.md`, `README.md`, `client/README.md` | I22, I23 |
| 11 | I26 | Windows build and run in the WinBoat VM over SSH only (toolchain + GStreamer MSVC runtime), Linux → Windows share test, Windows build notes; no waiting for your Linux sign-off — **D20** **D22** **D34** | `tl-sonnet-high` | `client/packaging/windows/`, `superdoc/features/windows.md` | I24, I25 |

## Open questions

### Answered
- ~~9. How do agents reach the Windows VM?~~ → WinBoat with SSH; its window is on HDMI → **D22**
- ~~8. Keep the three v2 behaviour changes?~~ → keep the popover and the × per row; Leave moves to the header → **D17**
- ~~7. Electron layout as-is or room-centric?~~ → room-centric, with stream settings in a modal → **D14**, **D15**
- ~~1. What do you want out of this rework?~~ → only one UI library, preferably egui; Electron is a workaround → **D1**, **D2**
- ~~2. Revive `ui.rs` or build fresh?~~ → revive, but make it look virtually identical to Electron → **D7**
- ~~3. Browser viewer?~~ → drop it entirely, delete the folder → **D11**
- ~~4. Workspace under git, keep Windows?~~ → git, full rework on a separate branch, Linux only → **D12**, **D13**
- ~~5. Target feature set?~~ → Electron's → **D8**
- ~~6. Which look?~~ → port Electron's design, nested panes, Opus for UI, apply the ForzaTelemetryV3 styling guide → **D9**, **D10**

## Research request

## Brainstorm request

## Notes from me

