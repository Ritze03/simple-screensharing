# client/scripts

Dev tooling for running and measuring the real native stack (relay + egui client).

**Since 2026-10-14 all real-app testing runs in a private headless sway rig (`hs-*`).** The
user's real desktop (Hyprland; MangoWC also supported) is never used for tests; the scripts that only work there are kept
as **legacy** (bottom of this page) and carry a banner. The rule and the machine facts:
`superdoc/claude-instructions/testing-and-verification.md`.

## The headless rig (the way to test)

```bash
cd client/scripts                         # SCRATCH defaults to <target>/hs-scratch/rig
./hs-rig.sh up                            # build (release) + everything below, ~15 s warm
./hs-drive.sh share                       # click "Share screen": portal grants, no picker; modal opens
./hs-drive.sh setup 240 h265 balanced     # fps codec preset [kbps]; every value is read back
./hs-drive.sh golive                      # "Go live in <room>"
./hs-drive.sh watch                       # viewer: click Watch
./hs-drive.sh hover                       # viewer: tooltip "Decoded X fps / shown Y fps, H.265 via vah265dec"
./hs-drive.sh shot /path/viewer.png       # grim -o HEADLESS-3 (viewer);  `shot f.png HEADLESS-2` = sharer
./hs-rig.sh down                          # stop everything, remove the private runtime dir
```

`hs-scenario.sh LABEL CODEC FPS PRESET` does share -> setup -> go live -> watch -> 7 tooltip
screenshots + sender per-stage rates (`gst-rate.py` on the tracer log) -> stop, in one call.
`hs-rig.sh status` lists processes/outputs/windows, `hs-rig.sh run -- CMD...` runs CMD inside
the rig's environment (`grim`, `swaymsg`, `wtype`, `busctl`, ...).
Headless sway's `wtype` drops the first key of each run: type through `hs_type` (`hs-lib.sh`, prepends a dummy Shift_L), never bare `wtype`.

What `up` starts, all under one private `XDG_RUNTIME_DIR` (`/run/user/<uid>/sss-hs-<hash>`, short
because unix socket paths are limited to 108 bytes; `WAYLAND_DISPLAY` is always the absolute
socket path, so a stray command cannot reach the real session):

| Piece | What / why |
| --- | --- |
| `sway` (`WLR_BACKENDS=headless`) | 3 virtual outputs: **HEADLESS-1** 1920x1080@240 (captured, test pattern), **HEADLESS-2** (sharer = client 1), **HEADLESS-3** (viewer = client 2). Output positions 0 / 1920 / 3840. |
| `vkcube` (mailbox) fullscreen on HEADLESS-1 | an idle output delivers no frames to the portal (capture is damage-driven); mailbox renders unthrottled, so the output has new content at its full refresh rate. `HS_PATTERN=gst` for a videotestsrc ball + timeoverlay. |
| private `dbus-daemon` | `$RT/bus`, no service dir: nothing is bus-activated. |
| private `pipewire` + `wireplumber -p policy` | the policy-only profile: no ALSA/bluez/v4l2 monitors, so the user's audio devices and webcam are never touched, and the user's graph (and its possible stuck stream, task #48) is out of the picture. `HS_PIPEWIRE=shared` symlinks the user's sockets instead (portal probe tested; Spotify untouched). |
| `xdg-desktop-portal-wlr` | `chooser_type=none`, `output_name=HEADLESS-1`, `max_fps=240`: a ScreenCast request is granted with no picker. Default binary: the stock `/usr/lib/xdg-desktop-portal-wlr` (what ordinary sway users run); `HS_XDPW=patched` runs the user's `~/.local/bin/xdg-desktop-portal-wlr-patched` (wlr-screencopy only), `HS_XDPW=<path>` any binary. **Why**: stock 0.8.4 on sway 1.12 captures through ext-image-copy-capture and used to die with `ext_image_copy_capture_session_v1#18: error 1: session already has a frame object` when the client's GPU pipeline flapped the PipeWire stream (streaming -> paused -> streaming) at start. Fixed in the client by #52 (a `queue` behind `pipewiresrc`), so stock is now the default and covers what most users run; if that error reappears in `xdpw.log` the flap fix regressed. |
| `xdg-desktop-portal` | private instance on the private bus, `ScreenCast=wlr` via `$SCRATCH/xdgcfg/xdg-desktop-portal/portals.conf`, `XDG_CURRENT_DESKTOP=sway`. |
| `hs-vptr` (`hs-vptr.c`, built on demand with wayland-scanner + gcc) | headless sway has no input device, so its seat has no pointer capability and clients never get a `wl_pointer`. A `zwlr_virtual_pointer_v1` held open fixes that; moving/clicking is then plain `swaymsg seat - cursor set X Y` / `cursor press\|release button1`. |
| relay in tmux | unique session `sss-hs-<hash>-<pid>`, random free UDP port (`HS_PORT`), scratch cwd `$SCRATCH/relay`, fresh cert every run. |
| two clients | per-client `XDG_CONFIG_HOME`/`XDG_DATA_HOME` in `$SCRATCH`, auto-connect to the same room (`launch-lib.sh` helpers), `SSS_UI_DUMP`, `PULSE_SERVER` pointed at a dead socket (no audio ever; audio sharing is off by default), sharer started with the GStreamer stats tracer (`$SCRATCH/gst1.log`, can reach ~100 MB per minute; `HS_TRACE=0` to skip). |

Env knobs (all optional): `SCRATCH` (one rig per scratch dir; default per worktree), `HS_PORT`,
`PROFILE` (`release` default; use release for fps), `HS_NO_BUILD=1`, `HS_FPS` (capture output
refresh, default 240), `HS_PATTERN`, `HS_XDPW`, `HS_PIPEWIRE`, `RUST_LOG`,
`START_CODEC/START_FPS/START_KBPS` (pre-fill the stream form; the reliable way to set kbps),
`SSS_LANGUAGE`, `CLIENT_ENV_<n>` (extra env for client n, e.g. a `GST_DEBUG` for the sharer).

Input details: pointer = `swaymsg seat - cursor ...` (needs `hs-vptr`). Keys = `wtype`, which
creates a virtual keyboard only while it runs (`hs-drive.sh key N ...` focuses client N first).
Never `ydotool`: it is a uinput device and injects into the user's REAL session. Click
coordinates: the stream-settings modal is driven from `SSS_UI_DUMP` rects (`$SCRATCH/ui1.txt`) plus
the window origin from `swaymsg -t get_tree`; the few fixed ones (Share, Stop, Watch, tile pill,
tile x) are constants at the top of `hs-drive.sh`, measured on the 1920x1080 fullscreen windows.

| Script | Purpose |
| --- | --- |
| `hs-rig.sh up\|status\|run\|down` | the rig |
| `hs-lib.sh` | shared functions (sourced; sources `launch-lib.sh` for build/config/join-link helpers) |
| `hs-drive.sh STEP` | share, setup, golive, watch, hover, sample, stopshare, unwatch, shot, crop, click, move, key, modal, mclick, tree |
| `hs-scenario.sh` | one measured share (screenshots + per-stage rates) |
| `hs-probe.sh` | `portal-probe.py` against the rig's portal (no picker) |
| `hs-vptr.c`, `hs-vptr-protocol.xml` | the virtual-pointer keepalive (protocol XML from wlroots, MIT) |
| `xdph-emu.c` | a PipeWire producer that offers formats exactly like xdg-desktop-portal-hyprland 1.4.1 (DMA-BUF modifier list with `DONT_FIXATE` + fixation handshake); the rig's sway/xdpw cannot show xdph-only negotiation bugs (task #76, `packaging/README.md`). Build and usage in its header; rig only |

## Shared helpers (work in both worlds)

| Script | Purpose |
| --- | --- |
| `launch-lib.sh` | build (own `target/`), scratch dir, relay cert hash, join string, per-client scratch config (`rooms.toml` auto-connect + `settings.toml`; `SSS_LANGUAGE=de` seeds the UI language; `START_CODEC/FPS/KBPS` the stream form). Its monitor/window-placement functions (`hyprctl` on Hyprland, `mmsg` on MangoWC) are legacy. |
| `gst-rate.py` | buffers/s per pad from the tracer log (`--summary`: capture / encode-branch input / encoder output) |
| `portal-probe.py` | own ScreenCast portal session, rate of `pipewiresrc` into a chosen tail (run it with `hs-probe.sh`). `--src` adds pipewiresrc properties, `--watch` prints buffers/s of named tail elements. Use a tail that starts with a video caps filter (`video/x-raw ! ...`), never a bare `fakesink` (see its docstring). #39 found the 140 fps portal ceiling with it, see `superdoc/features/stream-control.md` "Capture/encode performance" |
| `enc-bench.sh` | raw `vah264enc`/`vah265enc`/`vaav1enc` throughput at 1080p (no desktop involved) |
| `pw-node-rate.sh` | rate of a PipeWire node (against whatever PipeWire the env points at) |
| `stop-launch.sh` | stop what a legacy launch scratch dir started |

## Legacy: real desktop only (Hyprland; MangoWC also supported; do not use for tests)

Kept for history and for the user's own use. All of them move windows with `hyprctl`/`mmsg`, click with
`ydotool`, or answer the real portal picker, i.e. they act on the user's real session.

Only `launch-lib.sh` (so repo-root `launch-test*.sh`, `rig.sh`, `stop-launch.sh`) was ported to
Hyprland (2026-10-14, the user's desktop changed); the other scripts here, e.g. `test-launch-fg.sh`
and `drive.sh`, still use `mmsg` and only work on MangoWC.

| Script | Replaced by |
| --- | --- |
| repo-root `launch-test.sh`, `launch-test-tmux.sh`, `rig.sh`, `session.sh`, `test-launch-fg.sh`, `test-watchdog.sh` | `hs-rig.sh` |
| `drive.sh`, `ui-click.sh`, `picker-answer.sh`, `run-scenario.sh` | `hs-drive.sh`, `hs-scenario.sh` |
| `animate.sh` | the rig's own pattern (`HS_PATTERN`) |
| `probe-run.sh` | `hs-probe.sh` |
| `wlr-capture-rate.sh` | (wf-recorder on a real output; no headless twin yet) |
| `run-audio.sh`, `audio-probe.py` | nothing: **no audio tests are run any more** |

## Troubleshooting

- `stream error: target not found` from `pipewiresrc`: the node linking policy (WirePlumber)
  found no matching target. Causes seen: a bare `fakesink` tail (caps ANY, see above); the portal
  node already gone because xdpw crashed (check `$SCRATCH/xdpw.log` for `Protocol error`).
- Share hangs ~30 s on the user's shared PipeWire with a stuck native stream: see
  `superdoc/features/stream-control.md` "Capture/encode performance" (task #48). The private
  PipeWire of the rig is immune.
- `hs-rig.sh up` failed half way: it tears itself down; logs stay in `$SCRATCH`
  (`sway.log`, `xdpw.log`, `xdp.log`, `wireplumber.log`, `client1.log`, `relay.err`).

Rules baked in: every run owns uniquely named resources (runtime dir, tmux session, scratch);
never `pkill` by name (it matches other workers' processes), only recorded process groups
(`$SCRATCH/hs.pids`); every child is started with `setsid` so its whole group can be killed.
