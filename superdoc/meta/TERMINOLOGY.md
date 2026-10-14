# Terminology

Project-specific vocabulary. When the user uses a term defined here, use the same
meaning.

**If the user uses a non-standard term that is not defined here, ask what they mean by
it before acting on it.** Once its meaning is clear, add it to this file. Keep this
file up to date automatically: whenever the user introduces or clarifies a term, add or
amend the entry here.

## Terms

- **Suspended room** — a joined room whose connection dropped unexpectedly: its `RoomSession` is gone but a `ReconnectState` (`AppState::reconnects`) keeps its handle, last-known roster and what the user had running (watches, our shares) while the client retries. Not the same as a room the user left. See `architecture/implementation-notes.md` (task #36).
- **Wanted stream** — a watch carried over a reconnect, identified by owner display name + kind + label (never by `ConnId`/`StreamId`, which change). `session::WantedStream`.

- **Join link** — `server|room-id|key` or `server|room-id|key|certhash`; everything a client needs to enter a room. `protocol-spec.md` §11.4.
- **Cert hash** (`certhash`) — SHA-256 of the relay's leaf TLS certificate (DER), base64url without padding; what a browser pins via WebTransport `serverCertificateHashes`. Changes whenever the relay rotates its self-signed cert. `protocol-spec.md` §11.3.
- **vbox / hbox / slot / fill** — the egui UI's layout model (`client/src/widgets/layout.rs`, module doc of `widgets/mod.rs`). A **vbox** is a vertical container (`ui.vertical`, or a card/well frame that only paints and pads); an **hbox** is a row built with `HBox`; a **slot** is a fixed-width cell of an hbox (`fixed(w)`); **fill** is the cell that takes the remaining width (`FILL`; several fills share it equally). egui has no flexbox, so `HBox` does explicit width math. The browser UI mirrors the names (`V`/`H`/`SL`/`FILL`). `ui/STYLING-GUIDE.md`, `features/website.md`.
- **GUI lock** (historical) — the `flock` on `/tmp/claude-1000/sss-gui.lock` (fd 9) that agents held around an interactive GUI session on the user's real shared desktop (launch, focus, `ydotool`, `grim`). Used until 2026-10-14; the headless rig is private per worktree and needs no lock. Only legacy scripts and the `SSS_GUI_LOCKED=1` opt-in name of the browser harness (`docs/dev/README.md`) still mention it. `claude-instructions/testing-and-verification.md` "History".
- **Headless rig** (`hs-rig.sh`) — the private headless sway test environment (`client/scripts/hs-rig.sh up|status|run|down`, driven with `hs-drive.sh`/`hs-scenario.sh`): virtual outputs, its own dbus/PipeWire/portal/relay/two clients, all under one private `XDG_RUNTIME_DIR`. All real-app tests run in it since 2026-10-14 (user request: stay off their desktop); every worktree has its own. `claude-instructions/testing-and-verification.md` "The rig", `client/scripts/README.md`.
- **Captured output** (`HEADLESS-1`) — the rig's 1920x1080@240 Hz virtual output the portal shares, kept busy by a fullscreen `vkcube` (the test pattern); `HEADLESS-2` hosts the sharer client, `HEADLESS-3` the viewer client. `client/scripts/README.md`.
- **Legacy (real-desktop) script** — a script that acts on the user's real MangoWC session (`mmsg`, `ydotool`, the real portal picker). Carries a `LEGACY (2026-10-14)` banner and, for the repo-root `launch-test*.sh` and `docs/dev/e2e/native-session.sh`, refuses without `SSS_ALLOW_REAL_DESKTOP=1` (the user's own manual use). Agents never run them. `client/scripts/README.md` "Legacy".
- **Gallery** (`--gallery`) — `simple-screensharing-client --gallery`: a screen that renders every widget helper with fake data (`client/src/widgets/gallery.rs`). Right for widget and token work, never proof that a screen or action works. `claude-instructions/testing-and-verification.md`, `ui/STYLING-GUIDE.md`.
- **Sidecar** (historical) — the Rust/Tauri process the old Electron shell spawned (`client/src-tauri`, with `video_server.rs`). Gone with the egui rebuild (deleted in the egui rework (I23)); old docs still say "sidecar". `features/video-pipeline.md` "History", `architecture/overview.md`.
- **Sniffing** — detecting a stream's video codec (H.264 / H.265 / AV1) from its bitstream instead of a tag on the wire: the first byte separates Annex-B from an AV1 OBU stream, and H.264/H.265 parameter-set NAL headers are disjoint. Native `decode/codec.rs::sniff`; browser `docs/js/core/codec.js`. `features/video-pipeline.md` "Native decode", `architecture/protocol-spec.md` §6.4.
- **Capture domain** — which memory the capture tee carries (`capture.rs` `CaptureDomain`, #39): **GPU** (Linux portal on a working VA stack: `pipewiresrc` DMA-BUF to `vapostproc` to VAMemory NV12, no CPU copy) or **System** (original CPU shape: Windows, software `x264enc`, tests). `features/stream-control.md` "Capture/encode performance".
- **RateCap** — `capture::branch::RateCap`: the per-destination fps limiter on a branch's valve. The configured fps is a cap, never a floor; nothing ever duplicates a frame. `features/stream-control.md` "Capture/encode performance".
- **stableKey** (browser) — a stream's identity across reconnects: room + sharer name + kind + label (a reconnect changes ConnId and so the stream key). The UI's 60 s resume list is keyed by it; the native equivalent is **Wanted stream**. `features/website.md`, `docs/js/core/README.md`.
- **On air vs starting** (UI) — a share's phase in a room (`ui::MyPhase`): **on air** = at least one destination there is Live, Rebuilding or Stalled; **starting** = none is on air yet but one is still Starting (e.g. waiting for the portal's first frame). Drives the sidebar cast icon and the "Live in N rooms" line. `ui/APP-SHELL.md`, `features/stream-control.md` "Idle monitors".
