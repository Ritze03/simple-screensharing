# simple-screensharing-client

Desktop client for the simple-screensharing platform — share your screen (and mic) or
watch others', end-to-end encrypted. An Electron app shell (`electron/`) driving an
HTML/CSS/JS frontend (`frontend/`), backed by a headless Rust/Tauri sidecar process
(`src-tauri/`) that does the real work: screen/audio capture, hardware/software encode,
encrypt, and the reverse (decode/decrypt/display) for watching others. The Electron
renderer and the sidecar talk over a local REST+SSE API, not Tauri IPC.

## Features

- **Screen and audio sharing**, to one or more rooms at once, with live per-destination
  fan-out, transfer between rooms, and mid-stream resolution/framerate/bitrate edits.
- **Codec selection** — H.264, AV1, or H.265, user-selectable per stream. Options
  unsupported by the local hardware/VAAPI driver are automatically grayed out.
- **Four encoder presets** — Low-Latency, Balanced (default), Quality, Gaming — each a
  combination of Tune, Quality Level, Rate Control and (for Gaming) a shorter keyframe
  interval; the Advanced section below the preset pills lets any of those be hand-tuned
  independently.
- **Rate Control modes** — "Efficient" (constant-quality CQP: conservative, predictable
  bandwidth use) and "Max Quality" (VBR-tuned: spends up to the configured bitrate
  ceiling for the best achievable quality, the default).
- Automatic VAAPI hardware encoding with software (`x264enc`) fallback.
- Multi-room, multi-stream viewing with pop-out windows per stream.

## Architecture

Two OS processes, not one:

- **Electron process (parent)** — `electron/main.js` is the app shell: a `BrowserWindow`
  loading `frontend/index.html` as the renderer. On startup it spawns the compiled
  `src-tauri` binary as a child process ("the sidecar"), waits for it to report its
  locally-bound API port and start accepting connections, then opens the window. Every
  frontend `invoke()` call is forwarded to the sidecar's `POST /command/<name>`
  (bearer-token authenticated); every backend event arrives over a single `GET /events`
  Server-Sent-Events stream.
- **Rust sidecar process (child)** — `src-tauri/`, a headless Tauri app (`visible: false`)
  built around `src-tauri/src/api_server.rs`, a small REST+SSE server in front of the
  same command surface and capture/network/decode pipeline the project's Rust code
  implements (`src/capture.rs`, `src/audio.rs`, `src/net.rs`, `src/protocol.rs`, etc.).

See `superdoc/architecture/overview.md` (one directory up, in the `simple-screensharing`
planning workspace) for the full data-flow and module map.

## Prerequisites

- Rust (stable, 2024 edition) with the workspace's `src-tauri` member buildable.
- Node.js + npm, for the `electron/` shell.
- Linux: GStreamer with VAAPI plugins (`vah264enc`/`vah265enc`/`av1enc` as available) for
  hardware encoding; falls back to software (`x264enc`) automatically if unavailable.

## Development

```bash
cargo build -p simple-screensharing-client-tauri   # build the sidecar
cd electron
[ -d node_modules ] || npm install
electron .                                          # launch the real app
```

See `.claude/skills/run/SKILL.md` for the tested, scripted-verification recipe (Playwright's
`_electron` driver), including the safety rules around monitor placement during testing.

## Packaging

- **Linux AppImage** — self-contained, via `electron-builder`:
  ```bash
  cd electron && npm run build:appimage
  ```
  or, from the monorepo root, the wrapper script:
  ```bash
  ./build-appimage.sh
  ```
  (builds the sidecar in release mode, then packages it with `electron-builder`; output
  lands in `dist/`).
- **Windows `.exe`** — built natively on a `windows-latest` GitHub Actions runner (see
  `.github/workflows/windows-build.yml`), packaged as an `electron-builder --win portable`
  build alongside the cross-compiled (`x86_64-pc-windows-msvc`) sidecar and its GStreamer
  runtime DLLs. Not committed to the repo — delivered as a GitHub Release asset (rolling
  `windows-dev-build` tag, recreated on each run) rather than a repo file.
