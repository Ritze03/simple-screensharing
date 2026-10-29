# simple-screensharing

**Website (watch/share in the browser): https://ritze03.github.io/simple-screensharing/**

Screensharing for a group of friends: share a screen (plus an app's audio) into a room
and everyone with the room's link watches, in low latency and end to end encrypted. The
relay forwards ciphertext and never holds the room key.

This is a monorepo.

| Part | What |
| --- | --- |
| [`client/`](client/README.md) | native desktop app (Rust, egui): capture, hardware encode/decode, GPU rendering |
| [`server/`](server/) | relay: QUIC for native clients, WebTransport for browsers, ratatui admin TUI, self-signed rotating ECDSA cert whose hash rides in the join link |
| [`protocol/`](protocol/) | shared wire-types crate used by client and relay |
| [`docs/`](docs/) | browser client (watch and share), static site for GitHub Pages; `docs/js/core` protocol and media core, `docs/js/ui` UI, `docs/dev` local test harness |
| [`superdoc/`](superdoc/architecture/overview.md) | design docs, protocol spec, feature notes |

## Dependencies

Only for building and running from source; the AppImage and Flatpak bundle GStreamer.

<details>
<summary><b>Arch Linux</b></summary>

```bash
sudo pacman -S --needed rust base-devel pkgconf \
  gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad gst-plugins-ugly gst-libav \
  gst-plugin-pipewire gst-plugin-va libva \
  pipewire pipewire-pulse wireplumber xdg-desktop-portal \
  vulkan-icd-loader libxkbcommon wayland libx11
```

- **Relay only** (`server/`): just `rust` and `base-devel`.
- **Portal backend** matching your compositor (screen capture needs one): `xdg-desktop-portal-wlr` (sway,
  river, ...), `-hyprland`, `-gnome` or `-kde`.
- **GPU drivers**: a Vulkan driver (`vulkan-radeon`, `vulkan-intel` or `nvidia-utils`) for rendering, and a
  VA-API driver for hardware encode/decode: `mesa` (AMD, already includes it) or `intel-media-driver` (Intel).
  Without VA-API the client falls back to software x264/libav, which is too slow for high fps.
- `pw-loopback`/`pw-link`/`pw-dump` (per-app audio) come with `pipewire-pulse` / `pipewire`.

</details>

## Run it

```bash
cargo run -p simple-screensharing-server     # relay, needs a tty for its TUI; UDP port 4433, cert in ./state/
cargo run -p simple-screensharing-client     # desktop app
```

Create a room in the app and copy the join link (`server|room-id|key|certhash`); paste it into
a second client or the browser client. Details: [`client/README.md`](client/README.md).

## Downloads and platforms

- **Linux (recommended):** run `ritz-get install simple-screensharing` (or, without ritz-get, clone this repo and
  run `./install.sh --install`; `./install.sh --remove` uninstalls it again), then start `simple-screensharing`.
  It builds the native client from source and needs PipeWire (incl. `pw-loopback`) and the GStreamer
  pipewire/good/bad/libav plugins at runtime. You also need a relay (self-host one from this repo, or use a
  friend's); the app takes a join link.
- **Linux AppImage:** `./build-appimage.sh` builds `dist/simple-screensharing-client-x86_64.AppImage`
  (and a relay tarball) in a Debian 13 container; it bundles GStreamer and uses the host's GPU
  stack, PipeWire and portal. Supported: Fedora 42+, Ubuntu 25.10 / 26.04, Debian 13, Arch
  (Ubuntu 24.04 is not). See [`client/packaging/README.md`](client/packaging/README.md).
- **Linux Flatpak:** `./build-flatpak.sh` builds `dist/simple-screensharing-client.flatpak` (client only);
  install with `flatpak install --user ./simple-screensharing-client.flatpak`, run with
  `flatpak run io.github.ritze03.SimpleScreensharing`. Uses the Freedesktop 25.08 runtime's GStreamer;
  screen capture goes through the portal. See [`client/packaging/README.md`](client/packaging/README.md).
- **Windows:** builds with MSVC + GStreamer 1.26.9 and is packaged by `client/packaging/windows/package.ps1`; Linux<->Windows sharing is verified headless. The GUI window and screen capture on a real desktop session are not verified yet. See [`superdoc/features/windows.md`](superdoc/features/windows.md).
- **Browser:** the client in `docs/` is meant to be served by GitHub Pages (not enabled yet).
  Locally: `node docs/dev/headless.mjs serve --port 8123`, see [`docs/dev/README.md`](docs/dev/README.md).

## Performance

240 fps end to end is reached: H.265 with VA-API encode and decode; AV1 and the H.264 Gaming
presets get about 240 with the Performance setting.

## Contributing / agents

Start at [`CLAUDE.md`](CLAUDE.md) and [`superdoc/architecture/overview.md`](superdoc/architecture/overview.md).

## License

MIT, see [`LICENSE`](LICENSE). The AppImage and Windows zip bundle the GPL-licensed x264 GStreamer plugin and LGPL GStreamer/FFmpeg libraries; those keep their own licenses.
