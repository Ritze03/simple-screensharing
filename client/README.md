# simple-screensharing-client

The native desktop client: one Rust binary with an egui UI. Share your screen (and an
app's audio) to one or more rooms, or watch others', end to end encrypted; the relay
(`../server/`) only forwards ciphertext. Linux first (Wayland portal + PipeWire, VA-API
GPU encode and decode); Windows is in progress, see `../superdoc/features/windows.md`.

## What it does

- **Share** a screen and/or one application's audio to several rooms at once (fan-out,
  move a stream between rooms, edit resolution / fps / bitrate while live).
- **Codecs** H.264, H.265, AV1 (picked per stream; options the GPU cannot do are greyed
  out), four encoder presets (Low-Latency, Balanced, Quality, Gaming) plus hand tuning.
  Hardware VA-API encode with a software `x264enc` fallback.
- **Watch** many streams: hardware decode (software fallback), NV12 uploaded to the GPU and
  converted in a shader, per-stream pop-out windows.
- **Reconnects by itself** after a dropped connection and restores watches and shares.
- 240 fps end to end is reached: H.265 with VA encode and decode, AV1 and H.264 Gaming
  presets about 240 with the Performance setting.

## Build and run

Needs a Rust toolchain and GStreamer (with the VA plugins for hardware codecs; `check-deps.sh`
reports what is missing). From the repo root:

```bash
cargo run -p simple-screensharing-server     # relay; its TUI needs a tty, port 4433, cert state in ./state/ of the cwd
cargo run -p simple-screensharing-client     # the app (--release for frame-rate work)
cargo run -p simple-screensharing-client -- --gallery   # every widget with fake data (design QC only)
```

Create a room in the client, copy the join link (`server|room-id|key|certhash`) and join
from a second client or from the browser client in `../docs/`.

## AppImage

`./build-appimage.sh` (repo root) builds a self-contained Linux AppImage in a Debian 13
container, bundling GStreamer 1.26; GPU drivers, libva, PipeWire and the portal come from
the host. Supported hosts, build steps and the reasoning: [`packaging/README.md`](packaging/README.md).

## Where the code lives

| Path | What |
| --- | --- |
| `src/ui/` | the egui app shell, screens and modals (`mod.rs` is the map), `apply.rs` is the one place UI actions change state |
| `src/widgets/` | pure egui building blocks (vbox/hbox/slot rows) and `gallery.rs`; `src/theme.rs` design tokens |
| `src/session.rs` | `AppState`: rooms, captures, destinations, watches, reconnect; the UI is a thin view over it |
| `src/capture.rs`, `src/audio.rs` | screen / audio capture and encode pipelines |
| `src/decode/`, `src/decode.rs` | codec sniffing, VA/software decode, the GPU renderer (`render.rs`) |
| `src/net.rs`, `src/fragment.rs`, `src/protocol.rs` | QUIC, encryption, fragmentation; wire types come from `../protocol/` |
| `scripts/` | scripted real-app test helpers ([`scripts/README.md`](scripts/README.md)) |

Full module map and data flow: [`../superdoc/architecture/overview.md`](../superdoc/architecture/overview.md).

## Testing

`cargo test -p simple-screensharing-client` is necessary but not sufficient. User-visible
changes are verified against the real binary and a real relay on a real compositor, never
the gallery alone. Rules and machine safety: [`../superdoc/claude-instructions/testing-and-verification.md`](../superdoc/claude-instructions/testing-and-verification.md);
launch recipe: [`.claude/skills/run/SKILL.md`](.claude/skills/run/SKILL.md).
