# Linux packaging: AppImage and Flatpak

The AppImage is described first; the Flatpak is the last section (`## Flatpak`).

## Linux AppImage

`./build-appimage.sh` (repo root) builds `dist/simple-screensharing-client-x86_64.AppImage`
(about 23 MB): the egui client plus its own GStreamer. It also tars the relay
(`dist/simple-screensharing-server-x86_64-linux.tar.gz`; `--no-server` skips it).

```bash
./build-appimage.sh            # needs docker or podman, no root beyond access to it
chmod +x dist/*.AppImage && ./dist/simple-screensharing-client-x86_64.AppImage
```

The AppImage is a single file, nothing is installed. Config lives where the app always puts
it (`$XDG_CONFIG_HOME/simple-screensharing/`). `dist/` is gitignored; intermediates and the
cargo target live under `target/appimage/`. The first build takes ~10 minutes (builder image
with a source-built FFmpeg, crates, release compile), later ones ~2.

## Files

| File | Role |
| --- | --- |
| `Dockerfile` | builder image: Debian 13, GStreamer 1.26 dev + plugins, pinned Rust, a minimal FFmpeg + gst-libav built from source, a patched `gst-plugin-pipewire` built from Debian's PipeWire source, pinned linuxdeploy / appimagetool / type2 runtime |
| `patches/pipewire-gst-keep-possible-caps.patch` | the one fix applied to PipeWire 1.4.2's `pipewiresrc` (see "Decision: the bundled `pipewiresrc` is a patched build") |
| `build-in-container.sh` | runs inside the image: `cargo build --release`, assemble the AppDir, linuxdeploy, self-checks, appimagetool |
| `AppRun` | launcher inside the AppImage: points GStreamer at the bundled plugins, warns about missing/too-old host pieces, execs the client |
| `gst-plugins.txt` | the curated list of bundled GStreamer plugins |
| `gst-elements.txt` | elements the build verifies resolve from the bundled set alone |
| `simple-screensharing.desktop` | desktop entry (icon: `docs/assets/icon.svg`, rendered at build time) |

## Decision: bundle GStreamer, take GPU / PipeWire from the host

Why bundle GStreamer instead of requiring the host's: the client needs ~20 plugins from four
packages (base, good, bad, ugly) plus gst-libav, and distros split or strip them differently
(Fedora ships no x264 / libav without RPM Fusion, Ubuntu splits `va`/`pipewire` into extra
packages, versions range from 1.24 to 1.28). A friend should not have to learn which packages.
With a bundled set every distro runs the same pipelines, and the startup check only has to
cover the few host-tied pieces below. A curated plugin list (not linuxdeploy-plugin-gstreamer's
"everything", ~250 plugins) keeps it at 23 MB.

| Bundled | Notes |
| --- | --- |
| GStreamer 1.26.2 core + libs, glib, the plugins in `gst-plugins.txt` | `x264enc` (libx264, GPL), `av1dec` (libaom), `opus*`, `pulsesink` (+ libpulse), parsers, `libgstva`, `libgstpipewire`, `capssetter`, `level`, ... |
| `avdec_h264` / `avdec_h265` | gst-libav built against an FFmpeg with *only* the H.264 + HEVC decoders and no external libs (Debian's `gstreamer1.0-libav` drags in ~100 libraries, +150 MB, several of which would be taken from the host, so the plugin could silently fail to load) |

| From the host (checked at startup by `AppRun`: warns, never blocks) | Why it cannot be bundled |
| --- | --- |
| GPU stack: Mesa / driver, Vulkan loader + ICD, libdrm, libGL/EGL | tied to the host's kernel driver and GPU; wgpu `dlopen`s `libvulkan.so.1` |
| `libva` + the VA driver (`radeonsi`, `iHD`, ...) | the host's libva matches the host's drivers; a bundled one would look in the wrong directory / ABI. Missing libva = the VA elements do not load, the client falls back to x264 / libav |
| PipeWire: daemon, `libpipewire-0.3.so.0`, and the tools `pw-loopback` / `pw-link` / `pw-dump` | the tools are executables the client spawns (`client/src/audio.rs`) and parses; a bundled copy of the whole client stack (libpipewire + modules + SPA + tools, tried) made the per-app audio list come back empty against a newer daemon, the host's own tools are what the client is tested with. Packages: `pipewire` (Arch), `pipewire-utils` (Fedora), `pipewire-bin` (Debian/Ubuntu) |
| `xdg-desktop-portal` + a backend | the screen picker, a desktop component |
| Wayland / X11 client libs, libxkbcommon, libudev, libdbus, libsystemd | tied to the compositor / session; winit loads them at runtime |
| `libstdc++`, `libgcc_s`, glibc | host Mesa / Vulkan drivers loaded into our process need the host's (newer) ones; a bundled older libstdc++ would break them |

`dist/appimage-host-libs.txt` lists the exact sonames the AppImage takes from the host; the
build fails if that list contains something not expected (`build-in-container.sh`).

No "fall back to host GStreamer plugins" hybrid: a host plugin is built against the host's
GStreamer core, the process runs the bundled one, and GStreamer refuses plugins from another
minor version outright (and mixing ABIs is a crash lottery anyway). `AppRun` unsets the dev
`GST_PLUGIN_*` variables, sets `GST_PLUGIN_SYSTEM_PATH_1_0` to the bundled dir only and keeps
its own registry file (`~/.cache/simple-screensharing/gst-registry-appimage.bin`), so the
host's `~/.cache/gstreamer-1.0` is never touched. Audio playback (`autoaudiosink`) gets the
bundled `pulsesink` (works with pipewire-pulse and PulseAudio, the path the client is tested
with); `AppRun` ranks the native `pipewiresink` just below it as a fallback for a PipeWire
host without a pulse server. Upstream `pipewiresink` has rank NONE, so without that
`autoaudiosink` finds no usable sink.

## Decision: the bundled `pipewiresrc` is a patched build (2026-10-15, #76)

**Symptom (user, Hyprland 0.56 + xdg-desktop-portal-hyprland 1.4.1, PipeWire 1.6.9):** every
share from the AppImage ended with the toast `Share error: failed to bring up the encode
branch for stream 0: software fallback encode branch produced no IDR either`, and the
terminal said `capture pipeline error from .../GstPipeWireSrc:pipewiresrcN: stream error:
unhandled format (../src/gst/gstpipewiresrc.c(748): on_state_changed ())`. The same share
from the source build (host GStreamer 1.28, host `gst-plugin-pipewire` 1.6.9) ran at 120+ fps.

**Root cause (reproduced, not inferred):** Debian 13's `pipewiresrc` (PipeWire 1.4.2) clears
`pwsrc->possible_caps` (what the pipeline downstream accepts) right after the *first*
format negotiation (`gst_pipewire_src_negotiate`). `handle_format_change` needs it to turn
a format into caps; a second `SPA_PARAM_Format` finds it NULL, leaves `pwsrc->caps` NULL
and calls `pw_stream_set_error(-EINVAL, "unhandled format")`. A second format change
happens with every producer that picks the DMA-BUF modifier itself: it offers a modifier
list marked `DONT_FIXATE`, the consumer's list overlaps in two or more modifiers,
libpipewire hands the unfixed format to the producer, the producer fixates one (xdph:
`gbm_bo_create_with_modifiers2`) and PipeWire then delivers the fixed format as a second
`param_changed`. `xdg-desktop-portal-wlr` never hit it (its offer reduces to one modifier,
no handshake): the rig's sway share worked with the old AppImage. PipeWire 1.6 keeps
`possible_caps` for the stream's life (`gst_caps_replace`, cleared in `stop()`), which is
why the host plugin works.

Evidence: `client/scripts/xdph-emu.c` (a small C producer copying xdph 1.4.1's
`build_format`/`fixate_format` verbatim, run inside the headless rig, consumed by
`gst-launch-1.0 pipewiresrc ! ...`; the AppImage's GStreamer was exercised by running the
builder image's `gst-launch-1.0` with `LD_LIBRARY_PATH` and `GST_PLUGIN_SYSTEM_PATH_1_0`
pointing into the extracted AppImage): the AppImage's GStreamer 1.26.2 + plugin
1.4.2 against the rig's PipeWire 1.6.9 daemon fails with exactly
`gstpipewiresrc.c:748:on_state_changed ... stream error: unhandled format` once the
producer's modifier list has two entries in common with the consumer; the host's 1.6.9
plugin and the patched 1.4.2 plugin both go to `streaming`. It is a race (does the second
`param_changed` land before `negotiate` returns?): the emulator fixates instantly and the
old plugin failed 7 of 10 runs, the rebuilt AppImage 0 of 10 (xdph needs a `gbm` allocation
before it answers, hence the user's failures every time). With a single common modifier, or
an 8-bit/10-bit SHM-only offer, there is no handshake and the old plugin works too.
A 10-bit-only offer (the user's Hyprland has `bitdepth = 10`) fails in *every* plugin version
with `no more input formats`: `gstpipewireformat.c` has no mapping for the `xRGB_210LE`
family; it works for the user because xdph also offers an 8-bit format.

**Verified 2026-10-15 (headless rig, stock xdpw):** the rebuilt AppImage as the sharer
(extracted, `AppRun`), second client (source build) as viewer: capture in the Gpu domain,
H.265 240 fps requested, viewer tooltip "Decoded 235 fps / shown 234 fps, H.265 via
vah265dec, 0 dropped". The rig cannot run xdph itself (Hyprland-only protocols), so the
Hyprland negotiation is covered by the emulator above, not by a real xdph.

**Fix: bundle a `libgstpipewire` built from Debian's own PipeWire 1.4.2 source plus a
6-line patch** (`patches/pipewire-gst-keep-possible-caps.patch`, the 1.6 behaviour). The
Dockerfile builds only that plugin (`ninja src/gst/libgstpipewire.so`) into
`/opt/pipewire-gst`; `build-in-container.sh` prefers it over the distro plugin and fails the
build if the bundled file is not that one. Runtime linking is unchanged: the plugin still
links the **host** `libpipewire-0.3.so.0` (built against the same 1.4.2 headers, same
symbol floor as before).

`Why not the alternatives:`

- *Use the host's plugin.* A host `libgstpipewire.so` is built against the host's GStreamer
  core (1.24 to 1.28 across distros), the process runs the bundled 1.26 one; GStreamer
  refuses or crashes on that mix (see above), and its path differs per distro. Taking
  `libpipewire` from the host is safe because the PipeWire client library is
  ABI-stable towards the daemon; a GStreamer plugin is not stable towards a foreign core.
- *Bundle libpipewire too.* Tried before (see the host table): per-app audio list came back
  empty against a newer daemon.
- *Constrain the caps the client offers* (so exactly one modifier is common). The common
  set depends on the GPU, driver and compositor; the client cannot know it. Dropping
  DMA-BUF entirely would cost the 240 fps GPU path (SHM readback halves the portal's rate).
- *Catch the error and retry.* Deterministic: the retry negotiates the same way and fails
  the same way.
- *A newer PipeWire source (1.6.x).* More build dependencies and a risk that the plugin
  needs libpipewire symbols newer than the 1.4 floor; the 6-line backport keeps the plugin
  identical to what Debian ships otherwise. Drop the patch when the builder image moves to
  a base with PipeWire >= 1.6 (check `gst_caps_replace (&pwsrc->possible_caps` in
  `gstpipewiresrc.c` of that release).

The client side of the same report: a capture source that errors during branch bring-up now
leads the share error (`screen capture failed: pipewiresrcN: stream error: unhandled
format`) instead of being blamed on the encoder; see `superdoc/features/stream-control.md`
"Capture source errors lead the share error".

## Decision: build in a Debian 13 container, supported hosts

An AppImage with bundled glib / GStreamer needs a host glibc at least as new as the one it
was built against. Built on a rolling distro (this repo's dev machine: glibc 2.44) it would
run only on equally fresh systems, so the build runs in Debian 13 ("trixie": GStreamer 1.26.2,
PipeWire 1.4.2, libva 2.22, glibc 2.41).

The bundled `libgstpipewire` / `libgstva` link the *host* `libpipewire` / `libva` and use
symbols that exist since PipeWire 1.4 / libva 2.22, so those are the floor. Tested:

| Host | Result |
| --- | --- |
| Arch / CachyOS (dev machine; glibc 2.44, Mesa 26.2, PipeWire 1.6.9, GStreamer 1.28) | full share + watch, see below |
| Fedora 42 container (glibc 2.41, libva 2.22, PipeWire 1.4.11) | binary runs, no unresolved libs, every required element resolves from the bundle (no GPU in the container, so no VA elements) |
| Ubuntu 25.10 container (glibc 2.42) | binary runs, all 21 bundled plugins load |
| Ubuntu 24.04 container (glibc 2.39, libva 2.20, PipeWire 1.0) | binary runs, but `libgstpipewire` (`undefined symbol: pw_stream_set_rate`) and `libgstva` (`vaMapBuffer2`) fail to load: **no capture, no VA**. `AppRun` warns about the old PipeWire / libva. Not supported |

So: Fedora 42+, Ubuntu 25.10+ / 26.04, Debian 13, Arch and equivalent. Supporting Ubuntu 24.04
LTS would need the Dockerfile's `BASE_IMAGE` set to `ubuntu:24.04` (GStreamer 1.24 and its
older PipeWire / libva plugins, untested), or bundling libpipewire + libva as well.

## Tool provenance

Pinned in the `Dockerfile` with SHA-256 (the release's published digest): rustup 1.29.1
(+ Rust 1.99.0), linuxdeploy 1-alpha-20251107-1, appimagetool 1.9.1, type2 runtime 20251108
(static, libfuse3 inside: the AppImage needs no libfuse2 on the host). Downloads happen
inside the container build only. The ffmpeg / gst-libav sources come through Debian's apt
(signed Release metadata) with Debian's security patches (minus `00_plugin-dependencies`,
which adds a registry dependency on Debian's libavcodec path).

## Licences

The app's own code is MIT (see `LICENSE`); bundled third-party libraries keep their own licenses. The AppImage contains GPL code (x264) and LGPL code (FFmpeg, GStreamer, glib, ...). The client
is not linked against GPL code, x264 is loaded as a GStreamer plugin. linuxdeploy copies the
Debian copyright files of the bundled libraries into `usr/share/doc/` inside the AppImage.

## Debugging a user's report

- `SSS_NO_HOST_CHECK=1` silences the startup warnings.
- `stream error: unhandled format` from `pipewiresrc` in the log: the unpatched plugin
  (see "Decision: the bundled `pipewiresrc` is a patched build"); a current AppImage should
  never print it. Other portal-side failures: `GST_DEBUG=pipewiresrc:6` shows the formats
  the portal offers (`peer caps`) against what the pipeline accepts (`possible caps`).
- `GST_DEBUG=2 ./...AppImage` shows plugin load failures. `--appimage-extract` unpacks it,
  `--appimage-extract-and-run` runs it without FUSE.
- The decoder in use is shown in a tile's tooltip (`vah265dec` = hardware, `avdec_h265` =
  software).
- To exercise the software path on a machine with a GPU: `LIBVA_DRIVER_NAME=bogus`.

## Flatpak

Added 2026-10-15 (user request: "Also add a flatpak release"), next to the AppImage, not instead of it.

```bash
./build-flatpak.sh                                        # -> dist/simple-screensharing-client.flatpak
flatpak install --user ./dist/simple-screensharing-client.flatpak
flatpak run io.github.ritze03.SimpleScreensharing
```

Client only (the relay has no sandbox story to tell: it is a server binary, the AppImage build
already tars it). Build dir, flatpak-builder state and the OSTree repo live under
`target/flatpak-{build,state,repo}` (on disk); `dist/` is gitignored.

| File (`client/packaging/flatpak/`) | Role |
| --- | --- |
| `io.github.ritze03.SimpleScreensharing.yml` | the manifest: Freedesktop 25.08 runtime + SDK + `rust-stable` extension, one `simple` module that runs `cargo build --release --locked -p simple-screensharing-client` |
| `...desktop`, `...metainfo.xml`, `...svg` | desktop entry, AppStream metadata, the brand icon (copy of `docs/assets/icon.svg`); all named after the app id as Flatpak requires |

**Build prerequisites** (no root): `flatpak install --user flathub org.flatpak.Builder
org.freedesktop.Sdk.Extension.rust-stable//25.08` plus the `org.freedesktop.Platform` and `Sdk`
`//25.08` runtimes. `build-flatpak.sh` uses a host `flatpak-builder` if there is one, else
`flatpak run org.flatpak.Builder`.

**Decision: app id `io.github.ritze03.SimpleScreensharing`** (matches the GitHub Pages domain
ritze03.github.io, which is what Flathub would verify later). The binary's Wayland app_id
/ X11 class is `simple-screensharing` (the AppImage's `StartupWMClass`); inside the sandbox
`main.rs` `app_id()` switches it to `$FLATPAK_ID`, so a shell can match the window to the
installed desktop file. Outside Flatpak nothing changes.

**Decision: GStreamer comes from the runtime, not a bundle.** The Freedesktop runtime ships
GStreamer 1.26 with `pipewiresrc`, the `va` plugin, `pulsesink`, libav and opus; the only
non-runtime piece is `x264enc` (the software H.264 encode fallback, GPL), which lives in the
`org.freedesktop.Platform.codecs-extra` extension that Flathub installs with the runtime
(`autodelete`: it is pulled in when the runtime is). Without that extension the client still
has hardware encode (`vah264enc`, `vah265enc`, `vaav1enc`) but loses the x264 software encoder.
Every element the AppImage's `gst-elements.txt` lists, plus the VA ones, resolves inside the
sandbox (checked with `gst-inspect-1.0` under `--device=dri`). *Why not bundle like the AppImage:*
the AppImage must carry GStreamer because distros split the plugins differently; the Flatpak
runtime is one fixed set for every distro, which is the point of the format.

**Permissions** (`finish-args`): `--share=network` (relay), `--socket=wayland`,
`--socket=fallback-x11`, `--share=ipc`, `--device=dri` (Vulkan for wgpu, VA-API for the hardware
codecs), `--socket=pulseaudio` (playback through `pulsesink`), and
`--filesystem=xdg-run/pipewire-0` (per-app audio sharing spawns the runtime's `pw-dump`,
`pw-link`, `pw-loopback`, `client/src/audio.rs`; all three exist in the runtime, but audio
sharing is NOT tested in the Flatpak: no audio tests, see `testing-and-verification.md`).
Screen capture needs no permission: it goes through the xdg-desktop-portal ScreenCast API.
No `--filesystem=home`. Config and data land in `~/.var/app/io.github.ritze03.SimpleScreensharing/`
(`config/simple-screensharing/` is the `$XDG_CONFIG_HOME` the app already uses), so a
Flatpak install starts with its own empty room list, separate from an AppImage's.

**Decision: network at build time.** The module builds with `--share=network` so cargo fetches
the crates; fine for a local bundle, forbidden on Flathub. Upgrade path (a `ponytail:` note in
the manifest): generate `cargo-sources.json` with `flatpak-cargo-generator.py Cargo.lock`, list
it under `sources:` and build `--offline`. The metainfo has no screenshots, which
Flathub needs.

**Verified 2026-10-15** (headless rig, `client/scripts/hs-rig.sh`): the bundle (6.6 MB) installed
`--user`, then the rig's two native clients were replaced by Flatpak instances: the window opens
(wgpu/Vulkan works under `--device=dri`; its app_id is the Flatpak id), auto-connects to the
rig's relay room, a share goes live through the rig's xdg-desktop-portal ScreenCast
(`capture started in the Gpu domain`, VA hardware encode, H.265 @ 240 fps requested), and a
second Flatpak instance watched it: "Decoded 236 fps / shown 235, H.265 via vah265dec, 0
dropped" (a native viewer got 204 fps on the same share while the machine was also compiling).
Audio sharing, the software encoder (`x264enc` from codecs-extra) and a non-sway compositor are
untested.

Testing notes: flatpak ignores `--env=XDG_CONFIG_HOME=...` (the sandbox always sets
`XDG_CONFIG_HOME`/`XDG_DATA_HOME` to `~/.var/app/<id>/`), so a scratch config is redirected with
`--env=SSS_CONFIG_HOME=<dir>` (`rooms.rs` `config_base_dir`) plus `--filesystem=<dir>`. The sandbox
`/tmp` is private. `flatpak run` must only ever be given the rig's environment
(`hs-rig.sh run -- ...`): without it the app would open on the real desktop.
