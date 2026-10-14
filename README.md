# simple-screensharing

**Website (watch/share in the browser): https://ritze03.github.io/simple-screensharing/**

Screensharing for a group of friends: share a screen (plus an app's audio) into a room
and everyone with the room's link watches, in low latency and end to end encrypted. The
relay forwards ciphertext and never holds the room key.

This is a monorepo on the `rework` branch (not merged to master yet).

| Part | What |
| --- | --- |
| [`client/`](client/README.md) | native desktop app (Rust, egui): capture, hardware encode/decode, GPU rendering |
| [`server/`](server/) | relay: QUIC for native clients, WebTransport for browsers, ratatui admin TUI, self-signed rotating ECDSA cert whose hash rides in the join link |
| [`protocol/`](protocol/) | shared wire-types crate used by client and relay |
| [`docs/`](docs/) | browser client (watch and share), static site for GitHub Pages; `docs/js/core` protocol and media core, `docs/js/ui` UI, `docs/dev` local test harness |
| [`superdoc/`](superdoc/architecture/overview.md) | design docs, protocol spec, feature notes |

## Run it

```bash
cargo run -p simple-screensharing-server     # relay, needs a tty for its TUI; UDP port 4433, cert in ./state/
cargo run -p simple-screensharing-client     # desktop app
```

Create a room in the app and copy the join link (`server|room-id|key|certhash`); paste it into
a second client or the browser client. Details: [`client/README.md`](client/README.md).

## Downloads and platforms

- **Linux AppImage:** `./build-appimage.sh` builds `dist/simple-screensharing-client-x86_64.AppImage`
  (and a relay tarball) in a Debian 13 container; it bundles GStreamer and uses the host's GPU
  stack, PipeWire and portal. Supported: Fedora 42+, Ubuntu 25.10 / 26.04, Debian 13, Arch
  (Ubuntu 24.04 is not). See [`client/packaging/README.md`](client/packaging/README.md).
- **Windows:** builds with MSVC + GStreamer 1.26.9 and is packaged by `client/packaging/windows/package.ps1`; Linux<->Windows sharing is verified headless. The GUI window and screen capture on a real desktop session are not verified yet. See [`superdoc/features/windows.md`](superdoc/features/windows.md).
- **Browser:** the client in `docs/` is meant to be served by GitHub Pages (not enabled yet).
  Locally: `node docs/dev/headless.mjs serve --port 8123`, see [`docs/dev/README.md`](docs/dev/README.md).

## Performance

240 fps end to end is reached: H.265 with VA-API encode and decode; AV1 and the H.264 Gaming
presets get about 240 with the Performance setting.

## Contributing / agents

Start at [`CLAUDE.md`](CLAUDE.md) and [`superdoc/architecture/overview.md`](superdoc/architecture/overview.md).
