# Windows client

> **Status (2026-10-14).** The egui client builds and its networking, crypto, capture-encode and
> decode paths run on Windows 11 (verified in the WinBoat test VM, over SSH only). What is **not**
> verified: the window on a real desktop session and `d3d11screencapturesrc` capture (the VM had
> no interactive session at the time), and every GPU path (the VM has no GPU, see "Verified").
> Windows has no audio capture or sharing; the viewer side plays audio through `autoaudiosink`
> only in principle (untested).

Everything is in the same crate as Linux: `client/`, binary `simple-screensharing-client`. Platform
differences are `#[cfg(windows)]` / `#[cfg(unix)]` gates; the Linux build is unchanged.

## Prerequisites

| What | Version tried | Notes |
| --- | --- | --- |
| Windows | 11 24H2 (10.0.26100), x86_64 | |
| Rust | stable-x86_64-pc-windows-msvc (1.96) | `rustup` default |
| MSVC C++ build tools | VS 2026 Community, MSVC 14.51, Windows SDK 10.0.26100 | the "Desktop development with C++" workload; `link.exe` is found through `vswhere` |
| GStreamer MSVC x86_64 **runtime** and **devel** | 1.26.9 | both, `msiexec /i <msi> /qn /norestart ADDLOCAL=ALL` (a full set of plugins; the devel one has the `.pc`/`.lib` files and its own `pkg-config.exe`) |

The installers put GStreamer in `C:\Program Files\gstreamer\1.0\msvc_x86_64\` and set the machine
variable `GSTREAMER_1_0_ROOT_MSVC_X86_64`. Disk: about 2.4 GB for GStreamer, about 4 GB for a release
target dir; keep the target dir on a local disk.

Why 1.26 and not the 1.28 that Linux hosts run: matches the Linux AppImage (`client/packaging/README.md`),
and `gstreamer-rs` 0.24 builds against both. winget has `gstreamerproject.gstreamer` (1.28.x), but it
fetches the same URL as below and installs only the runtime.

**Getting the MSIs when freedesktop.org is flaky.** On 2026-10-14 `gstreamer.freedesktop.org/data/pkg/windows/`
answered `503 Backend unavailable` for most requests, and a plain download advanced about 1 MB per
minute. Ranged requests in parallel (32 connections, 2 to 4 MB each, retry per part) finished
the 390 MB devel MSI in about 8 minutes: the backend serves what it already has in the cache, and
every part fetched pulls the next one in. `curl.exe -C -` in a retry loop also works, only slowly.

## Build

```powershell
# from the repository root, in PowerShell
powershell -ExecutionPolicy Bypass -File client\packaging\windows\build.ps1 -TargetDir C:\sss-target
# -> C:\sss-target\release\simple-screensharing-client.exe
```

`build.ps1` does what the gstreamer-rs README asks for: `PKG_CONFIG` = GStreamer's own `pkg-config.exe`,
`PKG_CONFIG_PATH` = `...\lib\pkgconfig`, `PATH` gets `...\bin`, then `cargo build --release -p
simple-screensharing-client` (`-Server` adds the relay). Cold: a few minutes on 4 vCPUs (wgpu, quinn);
a rebuild after a source change is under a minute.

Running the exe from a developer build needs GStreamer's `bin` on `PATH`
(`$env:PATH = 'C:\Program Files\gstreamer\1.0\msvc_x86_64\bin;' + $env:PATH`).

### Package (nothing to install on the target machine)

```powershell
powershell -ExecutionPolicy Bypass -File client\packaging\windows\package.ps1
# -> dist\simple-screensharing-windows\ (63 MB) and dist\simple-screensharing-windows.zip (25 MB)
```

The folder holds the exe, 36 GStreamer/GLib/codec DLLs, `gst-plugins\` (the 24 plugins of
`windows-plugins.txt`) and `gst-plugin-scanner.exe`. The exe finds `gst-plugins\` itself
(`main.rs` `use_bundled_gstreamer`), so no launcher script is needed, and it then ignores any GStreamer
on the machine. `package.ps1` computes the DLL closure with `dumpbin /dependents` and finishes by running
`gst-inspect-1.0` for every element in `windows-elements.txt` with the machine's GStreamer off the path:
the build fails if one is missing.

Why curated: GStreamer's `bin\` is 620 MB and its plugins 1 GB; the client needs about 20 plugins.
Same decision as the Linux AppImage.

**No console window (2026-10-15).** The release exe is a Windows GUI-subsystem binary
(`client/src/main.rs`: `#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]`;
check: `dumpbin /headers <exe> | findstr subsystem` says `Windows GUI`). Debug builds stay console
binaries for `cargo run`. Consequences, all handled in `main.rs`:

- stderr no longer exists, so `init_logging` appends the `tracing` log to
  `%LOCALAPPDATA%\simple-screensharing\client.log` (truncated when over 1 MB; stderr if the file cannot be opened).
  A panic hook and an `Err` returned from `main` are written to the same file, since their default output also
  goes to stderr. **A startup failure on a user's machine: read that file.** (Not a logging system: just where
  the existing output lands.)
- `--help` / bad-argument output: `attach_parent_console` attaches to the launching terminal's console when the
  process has no stdout handle (a plain cmd/PowerShell prompt). With stdout redirected (`> file`, a pipe, ssh) the
  inherited handle is used; attaching in that case replaced the handles and lost the output (seen over SSH).
  Exit codes are unchanged (`--help` 0, bad argument 2). `--gallery` is unaffected.

Why: the user double-clicked the exe in the WinBoat VM on 2026-10-15 and a black Windows Terminal tab opened next to
the window. A shipped GUI app must not show a console.

Where the output lands for the user: after `package.ps1`, the folder and the zip are also copied to the VM user's
Desktop (`C:\Users\mo\Desktop\simple-screensharing-windows\` and `...\simple-screensharing-windows.zip`) so it can be
started by double-click; the zip is also copied back to the Linux repo's `dist\` (gitignored).

**A missing GStreamer element breaks every share (2026-10-15, task #73).** The first real share in the VM
failed with `failed to construct encode branch bin: ... no element "capssetter"`: the `debug` plugin was not in
`windows-plugins.txt` (the Linux list has it) and `capssetter` was not in `windows-elements.txt`, so
`package.ps1`'s self-check could not catch it. Both are listed now (24 plugins). The codec was never the
problem: `x264enc` (H.264, the only software encoder the client uses) was packaged and the `vah264enc` probe
falls back to it as designed. Lesson: every element `capture.rs`/`decode/` puts in a pipeline string must be in
`windows-elements.txt`; the headless `test_sender` run against the packaged folder (below) exercises the encode
branch and is the check that would have shown it. Why: the user hit it on 2026-10-15 double-clicking the packaged exe.

The window icon is set from `client/assets/icon.png` (see `ui/APP-SHELL.md`); the exe itself has no icon resource.

What an installer would take (not done): wrap that folder with Inno Setup or WiX (Start Menu shortcut,
per-user install under `%LOCALAPPDATA%\Programs`, uninstaller), and Authenticode-sign the exe and the
installer, otherwise SmartScreen warns on first start. The zip is the "friends" distribution until then.

## What is Windows-specific in the code

| Where | What | Why |
| --- | --- | --- |
| `client/Cargo.toml` | `wgpu` gets the `dx12` feature on Windows only | The shared dependency enables only `vulkan`. A VM's basic display adapter has no Vulkan driver (`Unable to find extension: VK_KHR_surface` in the log), so without `dx12` there is no adapter at all; on real GPUs D3D12 is the better-supported backend anyway. The runtime picks between the two |
| `client/src/main.rs` | the "relaunch with Vsync" fallback used `std::os::unix::process::CommandExt::exec` | does not compile on Windows. There it runs the second instance as a child and exits with its status |
| `client/src/main.rs` | `use_bundled_gstreamer` | see Package |
| `client/src/decode/video.rs` | hardware decoder is `d3d11h264dec` / `d3d11h265dec` / `d3d11av1dec` (const fn `hw`) instead of the VA ones; software AV1 is `dav1ddec` | The same probation logic demotes an unusable hardware decoder to software. The official Windows build has no libaom `av1dec`, only dav1d. D3D11 rather than D3D12 (`d3d12h264dec`): shipped in every 1.2x build, works on every vendor |
| `client/src/rooms.rs`, `session.rs` | config dir = `$SSS_CONFIG_HOME` if set, else `dirs::config_dir()` | `dirs` ignores `XDG_CONFIG_HOME` on Windows (Roaming AppData comes from the shell API), so the unit tests could not be isolated from the real config: 42 tests failed their own safety assertion (good: nothing was overwritten). Also gives scratch runs a way to avoid a user's saved rooms on every platform |
| `client/src/capture.rs` | test source path in gst-launch syntax uses `/` | a backslash is an escape in `gst-launch` syntax, so `filesrc location=C:\Users\...` pointed nowhere (2 orientation tests) |
| `client/src/capture.rs`, `Cargo.toml` | cargo feature `test-source` exposes the `videotestsrc` capture seam | for the headless `test_sender` example, see below |

`capture.rs` already had the Windows capture source from the Electron era (`d3d11screencapturesrc
monitor-index=0`, `CaptureSource::Windows`); it compiles unchanged. Encoding is `x264enc` (software):
`vah264enc` is looked up, not found, and the software path is taken, logged at info. Windows hardware encode
(`mfh264enc`, `nvh264enc`, `qsvh264enc`) is the documented fast follow-up, not done.

## Headless tools (no display, no portal, no audio)

`client/examples/test_sender.rs` and `test_viewer.rs`, both on the real `net`, `capture` and `decode` modules:

```text
cargo run --release -p simple-screensharing-client --features test-source --example test_sender -- \
    '<join-link>' --name win-sender --size 1920x1080 --fps 30 --kbps 8000 --pattern smpte --seconds 60
cargo run --release -p simple-screensharing-client --example test_viewer -- \
    '<join-link>' --from win-sender --seconds 15 --dump frame.ppm
```

The viewer prints one line per second (decoder element, codec, decoded fps, received kbit/s). `--from` filters by the
sender's display name: a killed sender leaves a stale stream in the room until the relay times it out, and the
viewer would otherwise pick that one.

## Reaching the VM and syncing the sources (agents; SSH only, never the WinBoat window)

- **SSH**: `ssh -i ~/.ssh/winboat mo@172.18.0.2` (the WinBoat container's address; it NATs port 22 on to the guest;
  `172.30.0.2` refuses). The user/password are in `~/.winboat/docker-compose.yml` (`USERNAME`). The login shell is
  **cmd.exe**; the host key changes with the VM, so use `-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null`.
  `scp` works the same (`scp -i ~/.ssh/winboat file mo@172.18.0.2:C:/Users/mo/tmp/file`). `tar`, `git` and `scp`
  are on the VM.
- **Sources**: `C:\sss-src` is a plain copy of the workspace, **not a git checkout**. Refresh it with
  `tar -cf src.tar client protocol server Cargo.toml Cargo.lock` on Linux (from the repo/worktree root), `scp` it,
  then `rmdir /s /q C:\sss-src & mkdir C:\sss-src & tar -xf <tar> -C C:\sss-src`. A single changed file can just be
  `scp`-ed over its twin.
- **Build and package**: `cd /d C:\sss-src` then `build.ps1 -TargetDir C:\sss-target`, `package.ps1` (output
  `C:\sss-src\dist\`). Long commands are easiest as a `.cmd` file `scp`-ed over and run in one `ssh` call (quoting
  through two shells is fragile). Check `tasklist` first: do not overwrite an exe the user is running
  (`C:\sss-target`); build into another `-TargetDir` then.
- Do not start the GUI over SSH expecting a window (session 0, no display): it logs to `client.log` and exits 101 at
  swapchain creation (`0x887A0022`), which is also the quickest way to check that logging works.

## Running a relay for a Windows VM (WinBoat network facts)

- The guest (172.30.0.2) cannot reach the Linux host: the host firewall drops everything arriving from the docker
  bridge (TCP and UDP, to 172.18.0.1 and to the LAN address), so a relay on the host is unreachable from the guest.
- The other direction works: the WinBoat container NATs **all UDP** sent to 172.18.0.2 on to the guest
  (`iptables -t nat -S PREROUTING` in the container), and TCP except a few ports. So the relay runs **inside the VM**
  and the Linux side connects to `172.18.0.2:4433`. Inside the VM the Windows firewall needs an inbound UDP rule
  (`New-NetFirewallRule -Direction Inbound -Protocol UDP -LocalPort 4433 -Action Allow`).
- The relay's TUI needs a console: started through `Win32_Process.Create` (a hidden session 0 console) or a
  scheduled task in the interactive session.
- **The relay builds and works on Windows as it is** (`cargo build --release -p simple-screensharing-server`, or
  `build.ps1 -Server`). Two Windows fixes are in `server/` (tested on Linux, 16 server tests, and in the VM, where
  Linux and Windows clients both connected):
  1. `tokio::signal::unix` in `server/src/main.rs` does not compile on Windows. A `Sigterm` shim (unix: the real
     signal; elsewhere: never fires, Ctrl-C still works) fixes it.
  2. `Endpoint::server` on `[::]` leaves `IPV6_V6ONLY` at the Windows default (on), so **IPv4 clients never connect**
     (`QUIC handshake failed: timed out`; `localhost` works because it resolves to `::1`). `build_endpoint_with` in
     `server/src/relay.rs` now binds the socket with `socket2` and `set_only_v6(false)` first, which fixes it (quinn's own doc: "platform defaults for dual-stack
     sockets vary").

## Windows virtual NIC: UDP send offload loses QUIC datagrams

In the WinBoat VM (Red Hat VirtIO Ethernet Adapter, UDP segmentation offload on), a Windows quinn endpoint
that **sends** media to a peer outside the VM loses most datagrams: the relay forwarded a 1080p30 stream to a
Linux viewer at about 1 frame per second instead of 30 (control streams are fine; loopback is fine). Mechanism:
quinn-udp uses `UDP_SEND_MSG_SIZE` (GSO) on Windows and batches same-size datagrams; in this VM the batches
do not survive the NIC. Measured, same relay, same stream, A/B:

| `netsh int udp set global uso=` | Windows relay -> Linux viewer, 300 kbit/s |
| --- | --- |
| `enabled` (default) | 1 to 2 frames received per second, no decode |
| `disabled` | 10/10 frames, 283 kbit/s; 1080p30 at 8 Mbit/s: 30 fps |

Workaround (admin, takes effect for new sockets): `netsh int udp set global uso=disabled`, or turn off
"UDP Segmentation Offload (IPv4)" in the adapter's advanced properties. This matters for the intended use
(a Windows VM sharing to a relay on the internet): the Windows **sender** is the affected side. Not fixed in
code: quinn does not expose a switch for GSO, a fix would mean a custom UDP socket in `net.rs`. To be
confirmed on real Windows hardware (physical NICs are likely unaffected).

## Verified

All numbers from the WinBoat VM: Windows 11 24H2, 4 vCPU (i9-10900K), 12 GB, Red Hat VirtIO GPU (no GPU video
decode, no Vulkan), release builds, GStreamer 1.26.9. Linux sender/viewer are the headless examples above on the
host; the relay runs in the VM.

| Check | Result |
| --- | --- |
| `build.ps1` | builds; warnings only (items that are used on Linux only, e.g. `PORTAL_MIN_BUFFERS`) |
| `cargo test --release -p simple-screensharing-client --lib` | **300 passed, 0 failed, 24 ignored** (Linux: 308 passed, 27 ignored; the difference is `#[cfg(unix)]` tests) |
| Linux sender (host, 1080p60 `videotestsrc`, `vah264enc`) -> relay in VM -> **Windows viewer** | `decoder=avdec_h264 codec=H264 hw=false`, 34 to 35 decoded fps = every frame the sender produced (the test pattern generator, not the encoder, limits it to ~35 fps), 158 kbit/s (ball pattern). Log: `H.264 hardware decode unavailable, using software: GStreamer element 'd3d11h264dec' is unavailable` then `H.264 decoding via avdec_h264 (software)` |
| **Windows sender** (`x264enc`, 1080p30 smpte, 8 Mbit/s) -> relay in VM -> **Linux viewer** | `decoder=vah264dec hw=true`, 30.0 decoded fps, 8.0 Mbit/s received, correct SMPTE bars in the dumped frame (needs `uso=disabled`) |
| the packaged folder alone (PATH without GStreamer) | viewer decodes at 34 to 35 fps through `avdec_h264` |
| Windows loopback (sender and viewer both in the VM) | 30 fps at 6 Mbit/s, 720p |
| packaged folder, `test_sender` (1080p60 smpte, 8 Mbit/s, software `x264enc`) + `test_viewer`, relay all in the VM, 2026-10-15 (#73) | encode branch builds (`capture started in the System domain`), viewer `avdec_h264` 37 to 76 decoded fps (sender and viewer share 4 vCPUs, `speed-preset=veryslow`), 5 to 11 Mbit/s. Procedure: copy the package folder, drop `test_sender.exe`/`test_viewer.exe` (built with `--features test-source --example ...`) into it, run with `PATH` = that folder + System32 and `GST_PLUGIN_SYSTEM_PATH_1_0`/`GST_PLUGIN_PATH_1_0`/`GST_PLUGIN_SCANNER_1_0` pointing into it |
| the egui window | **not verified** (no interactive session, see below). In session 0 the client starts, initialises D3D12 and fails only at swapchain creation (`0x887A0022`, no display), as expected |
| `d3d11screencapturesrc` capture, d3d11 decoders, `mfh264enc` | **not verified**: need a desktop session / a GPU. `d3d11screencapturesrc`, `d3d11convert`, `mfh264enc` exist in the plugin set |

Plugin availability in the 1.26.9 runtime (`gst-inspect-1.0`): present `d3d11screencapturesrc`, `d3d11convert`,
`d3d11download`, `avdec_h264`, `avdec_h265`, `dav1ddec`, `x264enc`, `mfh264enc`, `mfh265enc`, `openh264enc`,
`wasapi2src`, `wasapisrc`, `opusenc`, `opusdec`; absent `av1dec`, `mfav1enc`, `dxgiscreencapsrc`; absent in the VM only
(no capable GPU) `d3d11h264dec`, `d3d11h265dec`, `d3d11av1dec`, `d3d12h264dec`, `nvh264enc`, `qsvh264enc`, `amfh264enc`.

## Known limits

- No Windows audio capture/sharing: `audio.rs` is PipeWire-only (`#[cfg(not(target_os = "linux"))]` stubs).
- Software H.264 encode only; a full-HD 60 fps Windows share with `x264enc` costs real CPU.
- A `SSS_CONFIG_HOME`-less run uses `%APPDATA%\simple-screensharing\` (`rooms.toml`, `settings.toml`).
- The window needs a real desktop session: over SSH only (session 0) the GUI cannot present. To drive it remotely use a
  one-shot scheduled task with `/IT` for the logged-in user (`schtasks /Create ... /RU <user> /IT`) and take screenshots
  inside the guest (`System.Drawing.CopyFromScreen`).
- Unverified on real hardware: D3D11 hardware decode, D3D12 vs Vulkan choice, the USO finding on a physical NIC.

## Files

| File | Role |
| --- | --- |
| `client/packaging/windows/build.ps1` | environment + `cargo build --release` |
| `client/packaging/windows/package.ps1` | folder + zip with exe, DLL closure, curated plugins; self-check |
| `client/packaging/windows/windows-plugins.txt`, `windows-elements.txt` | the curated plugin list and the elements the packaged set must resolve |
| `client/examples/test_sender.rs`, `test_viewer.rs` | headless sender (feature `test-source`) and viewer |
