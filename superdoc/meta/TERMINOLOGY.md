# Terminology

Project-specific vocabulary. When the user uses a term defined here, use the same
meaning.

**If the user uses a non-standard term that is not defined here, ask what they mean by
it before acting on it.** Once its meaning is clear, add it to this file. Keep this
file up to date automatically: whenever the user introduces or clarifies a term, add or
amend the entry here.

**Keep every entry short: what the term is, and where to read more.** One or two
sentences, about 200 characters. Details, behaviour, option names, history and rationale
belong on the linked doc page. If that page doesn't exist and there is more to say, write
the page; don't grow the entry. When you amend an entry, trim it to this form instead of
appending. This file stays in context for the whole session.

## Terms

- **Suspended room** — a joined room whose connection dropped unexpectedly: `RoomSession` gone, a `ReconnectState` keeps roster, watches and shares while the client retries. `architecture/client-reconnect-and-watches.md`.
- **Connecting vs Reconnecting** (UI) — `ui::ConnState::Connecting` is a room's *first* connect in flight; `Reconnecting` is only a suspended room. Different pill text, same panels. `ui/APP-SHELL.md`.
- **Dynamic / Static** (rate control, historical) — the two rate-control modes of 2026-10-15; removed the same day (#88), encoding is always Dynamic (a real bitrate ceiling). `features/stream/rate-control.md`.
- **Viewer recovery (L1 / L2 / L3)** — the native viewer's escalation for a stream that stops decoding: L1 re-requests a keyframe, L2 resets the decoder, L3 reconnects the room (#101). `features/video/viewer-recovery.md`.
- **NACK** (#104) — a viewer asking for missing media fragments instead of dropping the frame: Hello/HelloAck probe, Request, Miss; answered by the relay cache, then the sender's ring. `features/video/selective-retransmission.md`, `architecture/protocol-spec.md` §6.6.
- **Loudness levelling** ("Level loudness", #119) — the viewer-side AGC (about -18 dBFS) plus a peak limiter on every watched audio stream; not the sender's Boost or the volume slider. `features/audio/levelling-and-volume.md`.
- **Webcam in picture** (PIP, #137) — a Layout option that shows a person's webcam inside their screen-share tile, draggable and resizable; computed per viewer, no wire field. `ui/STAGE.md`, `features/website/webcam-and-audio.md`.
- **Stage** — the central area of a joined room and of "Watched streams": the streams as tiles, the people strip and the control bar (native `client/src/ui/stage/`, browser `docs/js/ui/stage.js`). `ui/STAGE.md`, `features/website/stage.md`.
- **Split / Crop / Floating** — the three Stage layouts: Split shows every picture whole in a grid, Crop fills the cells (cropped), Floating gives each stream a movable window. Not `ui::PresentationMode`. `ui/STAGE.md`.
- **Resize bar** — the draggable bar between two pictures of a Split or Crop grid; a double-click evens the grid out. `ui/STAGE.md`.
- **Chrome** (stage chrome) — the people strip, control bar and tile overlays over the stage; in fullscreen they auto-hide when the pointer rests. Not the browser's own chrome. `ui/STAGE.md`.
- **Split button** — a control-bar button whose dropdown arrow is cut off by a separator: the main part acts at once, the arrow opens the menu. `ui/STAGE.md`.
- **Desktop audio** — the audio button's default source (Linux): every app's audio except the exclusion list; the other choice is one named app. `features/audio/desktop-audio.md`.
- **Source group** — the heading an audio source is listed under (Games, Browsers, Media, Voice chat, Devices, Other), from `audio.rs` `classify`. `features/audio/desktop-audio.md`.
- **Exclusion list** — `AppSettings::audio_exclusions`: app names kept out of Desktop audio and every source list (Discord and TeamSpeak by default). `features/audio/desktop-audio.md`.
- **Emoji avatar** — the emoji a person picks as profile picture; its colour sets the avatar background, carried by the relay via `AvatarHello`/`AvatarList`. `features/website/viewer-features.md`, `architecture/protocol-spec.md` §9.2.
- **PIP spot** — one of the eight places (corners, side middles) a webcam in picture snaps to when released. `ui/STAGE.md`.
- **Playout buffer** (jitter buffer, row 136) — the dynamic per-stream viewer delay that holds encoded frames so late clumps play out evenly; capped by Settings > Playback > Jitter buffer. `features/video/playout-buffer.md`.
- **Gamescope capture** (row 142) — sharing a gamescope session's PipeWire `Video/Source` node directly, with no portal or picker. `features/stream/capture-sources.md`.
- **Wanted stream** — a watch carried over a reconnect, identified by owner name + kind + label, never by `ConnId`/`StreamId`. `session::WantedStream`, `architecture/client-reconnect-and-watches.md`.

- **Join link** — `server|room-id|key[|certhash[|name]]`: everything a client needs to enter a room; the optional name is a percent-encoded suggestion. `architecture/protocol-spec.md` §11.4.
- **Cert hash** (`certhash`) — SHA-256 of the relay's leaf TLS certificate, base64url; what a browser pins via WebTransport `serverCertificateHashes`. `architecture/protocol-spec.md` §11.3.
- **vbox / hbox / slot / fill** — the egui layout model: vertical container, `HBox` row, fixed-width cell, and the cell taking the remaining width (browser: `V`/`H`/`SL`/`FILL`). `ui/STYLING-GUIDE.md`.
- **GUI lock** (historical) — the lock agents held around a GUI session on the real shared desktop; unused since 2026-10-14 (the rig is private). `meta/glossary-details.md`.
- **Headless rig** (`hs-rig.sh`) — the private headless sway test environment (own bus, PipeWire, portal, relay, two clients) that all real-app tests run in. `testing/rig-reference.md`, `client/scripts/README.md`.
- **Captured output** (`HEADLESS-1`) — the rig's 1920x1080@240 Hz virtual output the portal shares, kept busy by `vkcube`; HEADLESS-2 hosts the sharer, HEADLESS-3 the viewer. `client/scripts/README.md`.
- **Legacy (real-desktop) script** — a script that acts on the user's real desktop session; carries a `LEGACY` banner, agents never run it. `client/scripts/README.md` "Legacy", `meta/glossary-details.md`.
- **Gallery** (`--gallery`) — `simple-screensharing-client --gallery`: every widget helper with fake data; for widget work, never proof that a screen works. `ui/STYLING-GUIDE.md`.
- **Sidecar** (historical) — the Rust/Tauri process of the old Electron shell, gone with the egui rebuild; old docs still say it. `architecture/overview.md`.
- **Sniffing** — detecting a stream's codec (H.264 / H.265 / AV1) from its bitstream, not a wire tag; `decode/codec.rs::sniff`, browser `docs/js/core/codec.js`. `architecture/protocol-spec.md` §6.4.
- **Capture domain** — which memory the capture tee carries (`CaptureDomain`): GPU (DMA-BUF to VA memory) or System (CPU, software x264). `features/stream/capture-pipeline.md`.
- **RateCap** — `capture::branch::RateCap`: the per-destination fps limiter on a branch's valve; a cap, never a floor. `features/stream/capture-pipeline.md`.
- **stableKey** (browser) — a stream's identity across reconnects: room + sharer name + kind + label; the native equivalent is **Wanted stream**. `features/website/stage.md`.
- **Live preview** (own-tile preview, #78) — the sharer's own Stage tile with a live picture of what they capture, while focused; not the ~30 s room-preview thumbnails. `features/self-preview.md`.
- **On air vs starting** (UI) — a share's phase in a room (`ui::MyPhase`): on air = a destination is Live, Rebuilding or Stalled; starting = only Starting ones. `meta/glossary-details.md`, `ui/SHARE-FLOW.md`.
