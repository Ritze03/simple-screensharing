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
./hs-drive.sh share                       # the Share arrow menu's "Share your screen": portal grants, no picker; modal opens
./hs-drive.sh shareauto                   # the Share button itself: live at once with the remembered settings, no modal (fails if one opens)
./hs-drive.sh restart 1                   # restart client 1 keeping its config (what the app saved: remembered stream settings)
./hs-drive.sh setup 240 h265 quality      # fps codec preset [kbps]; every value is read back
./hs-drive.sh golive                      # "Go live in <room>"
./hs-drive.sh watch                       # viewer: click Watch
./hs-drive.sh hover                       # viewer: tooltip "Decoded X fps / shown Y fps, H.265 via vah265dec"
./hs-drive.sh shot /path/viewer.png       # grim -o HEADLESS-3 (viewer);  `shot f.png HEADLESS-2` = sharer
./hs-resize.sh 1280x720                   # change the captured output's size under a live share; `hs-resize.sh burst [N] [GAP_MS] [FINAL]` = a window edge being dragged
./hs-extra-client.sh 3 Alice 4            # a third (fourth, ...) client on HEADLESS-3, workspace 4: more viewers, or a second stream (SHARER=3 ./hs-drive.sh share|setup|golive)
./hs-drive.sh uclick 2 ctl.layout         # by dumped name: any control of client 2 (names below)
./hs-drive.sh udrag 2 bar.v0.0 60 0 1500 & # a real drag, held 1.5 s at the end; meanwhile:
./hs-drive.sh val 2 chrome                # read a dumped value
./hs-rig.sh down                          # stop everything, remove the private runtime dir
```

`hs-scenario.sh LABEL CODEC FPS PRESET` does share -> setup -> go live -> watch -> 7 tooltip
screenshots + sender per-stage rates (`gst-rate.py` on the tracer log) -> stop, in one call.
**Settings changes on a live share (2026-11-04, task #139).** `setup` only works in the staged modal; `hs-drive.sh live [fps=N]
[preset=text|quality|gaming] [quality=performance|quality] [gop=MS] [kbps=K] [codec=C]` opens the *live* modal (the share button), applies
the named values in that order through the real controls (FPS and Keyframe are dropdowns, Preset and Encoding segments, Bitrate the value
box typed with `hs_type`), reads each back from the dump, prints one `live: ...` line and closes it with Done. Each value is one
`EditDestination`/`ApplyPresetToDestination`, so it takes the same tier the app gives it (a bitrate write, a branch swap, a source
restart). `hs-live-cycle.sh LABEL CODEC FPS PRESET KBPS SECS [GAP]` shares, goes live, watches, then runs a cycle of `live` calls
(`CYCLE="step|step"` to change it) every GAP seconds and prints counts from the sender's and the viewer's logs (capture deaths, portal
restarts, branch flow errors, rate-control restarts, codec fallbacks, no-IDR, viewer `never completed`); `final-viewer.png` shows
whether the viewer kept a picture. The rig must be up with `HS_PATTERN=none` plus a pattern started through `hs-rig.sh run -- python3 hs-pat-photo.py FPS PIDFILE`:
`hs-pat-photo.py` is a detailed desktop of smooth colour with grain and a moving window (needs ~935 kB keyframes at 1080p 60 fps
Quality: the hard case for `vaav1enc`'s coded buffer), `hs-pat-gray.py` the same with 7-gray 3x3 blocks, which a palette encoder
shrinks 8x (see `features/stream-control.md` "Settings changes on a live share"). `SSS_VAENC_EXTRA="min-qp=90,mbbrc=1"` in a client's
environment appends encoder properties (commas become spaces).
`hs-rig.sh status` lists processes/outputs/windows, `hs-rig.sh run -- CMD...` runs CMD inside
the rig's environment (`grim`, `swaymsg`, `wtype`, `busctl`, ...).
Headless sway's `wtype` loses the first key of each run: type through `hs_type` (`hs-lib.sh`), never bare `wtype`. It starts with a shift modifier and a pause, no key. *Why not a dummy key:* wtype numbers its keys from evdev code 1, which is Escape in the keymap the client still has when the first key arrives. A dummy Shift_L (the old `hs_type`) reached the client as an Escape in about 1 of 3 runs, so one `key N -k Escape` could close a menu and then leave fullscreen (2026-11-04, the "Escape left fullscreen" of I3).

What `up` starts, all under one private `XDG_RUNTIME_DIR` (`/run/user/<uid>/sss-hs-<hash>`, short
because unix socket paths are limited to 108 bytes; `WAYLAND_DISPLAY` is always the absolute
socket path, so a stray command cannot reach the real session):

| Piece | What / why |
| --- | --- |
| `sway` (`WLR_BACKENDS=headless`) | 3 virtual outputs: **HEADLESS-1** 1920x1080@240 (captured, test pattern), **HEADLESS-2** (sharer = client 1), **HEADLESS-3** (viewer = client 2). Output positions 0 / 1920 / 3840. |
| `vkcube` (mailbox) fullscreen on HEADLESS-1 | an idle output delivers no frames to the portal (capture is damage-driven); mailbox renders unthrottled, so the output has new content at its full refresh rate. `HS_PATTERN=gst` for a videotestsrc ball + timeoverlay. |
| private `dbus-daemon` | `$RT/bus`. One service dir, `$SCRATCH/dbus-services`, holding only xdpw: when xdpw has exited, the next ScreenCast request makes the bus start it again through `$SCRATCH/bin/xdpw-activate`, as on a real session (the dead instance's log is appended to `$SCRATCH/xdpw-crashed.log`, the new one is `xdpw.log`). Everything else is started by `hs-rig.sh`. |
| private `pipewire` + `wireplumber -p policy` | the policy-only profile: no ALSA/bluez/v4l2 monitors, so the user's audio devices and webcam are never touched, and the user's graph (and its possible stuck stream, task #48) is out of the picture. `HS_PIPEWIRE=shared` symlinks the user's sockets instead (portal probe tested; Spotify untouched). |
| `xdg-desktop-portal-wlr` | `chooser_type=none`, `output_name=HEADLESS-1`, `max_fps=240`: a ScreenCast request is granted with no picker. Default binary: the stock `/usr/lib/xdg-desktop-portal-wlr` (what ordinary sway users run); `HS_XDPW=patched` runs the user's `~/.local/bin/xdg-desktop-portal-wlr-patched` (wlr-screencopy only), `HS_XDPW=<path>` any binary. **Why stock**: it is what ordinary sway users run, and it has a known weakness: it asks the compositor for a frame on every PipeWire `streaming` and dies (`ext_image_copy_capture_session_v1#N: error 1: session already has a frame object`) when `pipewiresrc`'s start-up streaming -> paused -> streaming flap resumes while the first frame is still outstanding. The client defuses it (#52 queue behind `pipewiresrc`, #112 30 ms pause between PAUSED and PLAYING, `capture::PORTAL_START_SETTLE`); if the error appears in `xdpw.log` / `xdpw-crashed.log` a lot again, that mitigation regressed. Its other face is a stuck stream (`out of buffers`, no frame; #122): the client repairs it (`capture::repair_portal_start`, log line `portal stream stalled by the start-up flap`) (before #112: 17-28 % of starts crashed, with the bus restarting xdpw and the client's one retry (#59) rescuing nearly all of them). |
| `xdg-desktop-portal` | private instance on the private bus, `ScreenCast=wlr` via `$SCRATCH/xdgcfg/xdg-desktop-portal/portals.conf`, `XDG_CURRENT_DESKTOP=sway`. |
| `hs-vptr` (`hs-vptr.c`, built on demand with wayland-scanner + gcc) | headless sway has no input device, so its seat has no pointer capability and clients never get a `wl_pointer`. A `zwlr_virtual_pointer_v1` held open fixes that; moving/clicking is then plain `swaymsg seat - cursor set X Y` / `cursor press\|release button1`. |
| relay in tmux | unique session `sss-hs-<hash>-<pid>`, random free UDP port (`HS_PORT`), scratch cwd `$SCRATCH/relay`, fresh cert every run. |
| two clients | per-client `XDG_CONFIG_HOME`/`XDG_DATA_HOME` in `$SCRATCH`, auto-connect to the same room (`launch-lib.sh` helpers), `SSS_UI_DUMP`, `PULSE_SERVER` pointed at a dead socket and `SSS_SOUND_SINK=fakesink` (no audio ever; audio sharing is off by default), sharer started with the GStreamer stats tracer (`$SCRATCH/gst1.log`, can reach ~100 MB per minute; `HS_TRACE=0` to skip). |

Env knobs (all optional): `SCRATCH` (one rig per scratch dir; default per worktree), `HS_PORT`,
`PROFILE` (`release` default; use release for fps), `HS_NO_BUILD=1`, `HS_FPS` (capture output
refresh, default 240), `HS_PATTERN`, `HS_XDPW`, `HS_PIPEWIRE`, `RUST_LOG`,
`START_CODEC/START_FPS/START_KBPS` (pre-fill the stream form; the reliable way to set kbps),
`SSS_LANGUAGE`, `CLIENT_ENV_<n>` (extra env for client n, e.g. a `GST_DEBUG` for the sharer; `CLIENT_ENV_1="SSS_FAKE_CAMERA=1"` makes client 1 list one fake camera whose frames are a moving `videotestsrc` ball, task I8, the host webcam has its lens blocked; without it a client lists the real `/dev/video0`, which the rig can open: close it again (`fuser /dev/video0`); `CLIENT_ENV_2="SSS_UI_FAKE_AUDIO=1"` makes every stream that client watches act as if it had audio: volume row, ring, pop-out pill, nothing played). Other client envs: `SSS_SOUND_SINK=<element>` (the notification sounds' sink; `hs_start_client` sets **`fakesink` by default** so a rig run plays nothing, a `CLIENT_ENV_<n>` value overrides it), `SSS_UI_FAKE_AUDIO_SOURCES=1` (the audio menu lists the replica's example sources through the real grouping; no audio stream can be created, sharing is refused), `HS_NAME_<n>` (display name of client n, default `client-<n>`), `HS_EMOJI_<n>` (emoji avatar of client n, one of the picker's), `HS_SEND_DIAG_<n>=1` (client n starts with Settings > Diagnostics > "Send diagnostics to the relay" ticked, #110),
`HS_RELAY_ENV` (`"A=1 B=2"` for the relay process, e.g. `SSSHARE_NACK=0` for a relay that behaves like one without NACK), `SSSHARE_AVATARS=0` (relay carries no emoji avatars: acts like an older relay), `HS_RELAY_BIN` / `HS_CLIENT_BIN_<n>` (run another binary for the relay / client n, e.g. an older build for mixed-version checks, #104).

Input details: pointer = `swaymsg seat - cursor ...` (needs `hs-vptr`); mouse wheel = `$SCRATCH/bin/hs-vptr scroll N`
(one-shot: N notches at the cursor, N > 0 wheel down, N < 0 up; a second virtual pointer on the same seat, run inside
`hs-rig.sh run --`; task #113). **Drags** = `$SCRATCH/bin/hs-vptr drag W H X1 Y1 X2 Y2 [STEPS [HOLD_MS]]` (`hs-drive.sh drag|udrag`), a real press, motion and release: `swaymsg seat - cursor set` sends the client no motion while a button is down. `HOLD_MS` keeps it pressed at the end; run the step with `&` to read the dump or take a screenshot mid-drag. Keys = `wtype`, which
creates a virtual keyboard only while it runs (`hs-drive.sh key N ...` focuses client N first; to reach a popped-out window, focus it yourself and call `wtype` through `hs-rig.sh run`).
Never `ydotool`: it is a uinput device and injects into the user's REAL session. Click
coordinates all come from the `SSS_UI_DUMP` rects (`$SCRATCH/ui<n>.txt`) plus the window origin from `swaymsg -t get_tree`.

### `SSS_UI_DUMP` (`client/src/ui/drive_dump.rs`)

Every frame, when it changed, each client rewrites `$SCRATCH/ui<n>.txt`: `rect NAME x0 y0 x1 y1` (points, window-relative) and `key value...` lines, ending in `ppp` and `screen W H`. `$SCRATCH/ui<n>.actions` logs every dispatched `UiAction`. What the popped-out stage window draws has `pw.` in front of its name or key (`pw.ctl.pop`, `pw.chrome`). A stream's own pop-out window (a deferred viewport) is not dumped.

| Names | What |
| --- | --- |
| `tile.N`, `cell.N`; `tileinfo N KIND ar R who=.. label=..`; `tiles`, `layout` | stage tile N: the picture and its grid cell, in stage order. `tile.N` is the drawn rect, mid-glide while one runs; `tile_goal.N` is the layout rect. A script that clicks right after a swap waits 0.2 s or uses `tile_goal.N`. |

Settings controls (`sound.volume`, `sound.test.*`, `sound_volume`) are dumped only while their tab is showing; click `settings.tab.<profile|interface|playback|diagnostics>` first.

| `bar.v0.0`, `bar.h0`; `barstate v0.0 hidden\|grey\|blue` | resize bars (hit rect) and how they show |
| `win.N`, `grip.N` | floating window and its corner handle |
| `tool.N.more\|fs\|pop\|settings`, `tag.N`, `watch.N` (`uclick .. watch.0 L40` = Watch stream); `stats.N` | a tile's hover buttons (only while shown), name tag, idle button row |
| `share_pick.screen`, `share_pick.gamescope` (the "What do you want to share?" question, only while a gamescope node exists; `hs-drive.sh sharepick screen\|gamescope`) | the gamescope question |
| `menu.share_start` (the Share menu's start entry); `share idle\|picking\|staged\|live\|paused` (the bar's Share state); `ctl.share\|audio\|webcam\|layout\|room` and `.arrow`, `ctl.people\|leave\|sidebar\|invite\|pop\|fs`, `ctl.group.X` | the control bar |
| `menu`, `menu.LABEL` (spaces become `_`), `menu.always_names`, `menu.show_stats`, `menu.hide_own`, `menu.volume`, `menu.mute`; `volume` | the open menu and its rows; the volume row and its value |
| `opt.I` | row I of an open dropdown (`menu::select`) |
| `strip.NAME` (`strip.you`; `strip.NAME~2` is the second person of that name), `strip.NAME.live`; `person NAME streams N popped N ring 0\|1`; `cardof NAME rows N` | avatars and LIVE badges; one `cardof` line per open hover card |
| `card`, `card.row.I`, `card.back.I`; `cardrow I audio 0\|1 label=..` | the hover card |
| `stage.bring_back`; `boost` | "Bring it back" while the stage is popped out; `boost` is the Boost percent (the old `settings.boost` rect is gone: Boost left Settings for the audio menu, `audio.boost`) |
| `side.settings` | the sidebar's Settings button |
| `avatar.btn`, `avatar.field` | Settings > Profile: the Avatar button that opens the emoji picker, and the picker's "Paste any emoji" text field (only while the Profile tab / picker shows) |
| `sound.volume`, `sound.test.join\|leave\|start\|stop`; `sound_volume N` | Settings > Sounds: the volume slider and its value (percent), the four Test buttons |
| `audio.source`, `audio.boost`, `audio.excl_add`, `audio.excl.I`, `audio.excl.I.remove`, `audio.excl_text`, `audio.excl_add_text`; `boost N`, `excl A\|B` | the audio settings menu (`audio_settings.rs`): source dropdown, Boost slider (percent), the exclusion list (Add button, row I and its remove, the text fields of the Add popover) |
| `chrome`, `chrome_alpha`, `stage_full W H`, `popover`, `modal`, `stage_fs`, `stream_fs`, `open` | state: fullscreen chrome, open menu / modal, fullscreen, the stream settings modal |
| `fps`, `fps_options`, `codec.I`, `preset.I`, `gop` (Keyframe select; value `gop`, `gop_options`), `quality.0\|1` (Encoding segments Performance\|Quality; value `quality`), `change`, `footer`, `bitrate`, `mode`, `ready`, ... | the stream settings modal (`stream_settings.rs`) |
| `pip.N`, `pip.N.grip` (`pip.fs` in stream fullscreen), `pipinfo N spot=C,R w=.. handle=tl\|tr\|bl\|br who=..`, `menu.pip` | a webcam in picture (task I8): N is its parent's tile index (`pw.` prefix in a popped-out stage); `spot` is column,row 0..2 (`free` only while dragged); the handle is on the corner facing the middle |
| `webcam.camera`, `webcam.mode`, `webcam.bitrate`, `webcam.footer` (`uclick N webcam.footer L60` = Start / Stop webcam), `webcam_codec.I`, `webcam_modes N`, `webcam_mode KEY`, `webcam camera=.. codec=.. kbps=..` | the webcam modal (`webcam_settings.rs`); the bar button is `ctl.webcam` / `ctl.webcam.arrow`, its menu rows `menu.<camera name>` and `menu.All_webcam_settings…` |

| Script | Purpose |
| --- | --- |
| `hs-rig.sh up\|status\|run\|down` | the rig |
| `hs-lib.sh` | shared functions (sourced; sources `launch-lib.sh` for build/config/join-link helpers) |
| `hs-drive.sh STEP` | share (menu path: modal), shareauto (main button: no modal), restart [N], setup, golive, live, discard, watch, hover, sample, stopshare, unwatch, shot, crop, click, move, key, modal, mclick (`mclick NAME L60` = 60 pt from the rect's left edge), tree; by dumped name for any client: rect, val, await, uclick, udclick, umove, uscroll, udrag; drag (window coordinates); relay-restart [DOWN_SECS] |
| `hs-scenario.sh` | one measured share (screenshots + per-stage rates) |
| `hs-live-cycle.sh` | a share whose Preset / FPS / Encoding / Keyframe / Bitrate are changed over and over while live (`hs-drive.sh live`), with log counts per run |
| `hs-pip-spots.py` | drags a webcam in picture onto all 8 spots with the real pointer, checks `pipinfo` after each release, optional screenshot per spot (needs a PIP on screen: `CLIENT_ENV_1="SSS_FAKE_CAMERA=1"`, share + webcam on client 1, Watch both on client 2, Layout > Webcam in picture) |
| `hs-probe.sh` | `portal-probe.py` against the rig's portal (no picker) |
| `hs-vptr.c`, `hs-vptr-protocol.xml` | the virtual-pointer keepalive, one-shot `scroll` and `drag` (protocol XML from wlroots, MIT) |
| `xdph-emu.c` | a PipeWire producer that offers formats exactly like xdg-desktop-portal-hyprland 1.4.1 (DMA-BUF modifier list with `DONT_FIXATE` + fixation handshake); the rig's sway/xdpw cannot show xdph-only negotiation bugs (task #76, `packaging/README.md`). Build and usage in its header; rig only |

## Shared helpers (work in both worlds)

| Script | Purpose |
| --- | --- |
| `launch-lib.sh` | build (own `target/`), scratch dir, relay cert hash, join string, per-client scratch config (`rooms.toml` auto-connect + `settings.toml`; `SSS_LANGUAGE=de` seeds the UI language; `START_CODEC/FPS/KBPS` the stream form). Its monitor/window-placement functions (`hyprctl` on Hyprland, `mmsg` on MangoWC) are legacy. |
| `gst-rate.py` | buffers/s per pad from the tracer log (`--summary`: capture / encode-branch input / encoder output) |
| `portal-probe.py` | own ScreenCast portal session, rate of `pipewiresrc` into a chosen tail (run it with `hs-probe.sh`). `--src` adds pipewiresrc properties, `--watch` prints buffers/s of named tail elements. Use a tail that starts with a video caps filter (`video/x-raw ! ...`), never a bare `fakesink` (see its docstring). #39 found the 140 fps portal ceiling with it, see `superdoc/features/stream-control.md` "Capture/encode performance" |
| `enc-bench.sh` | raw `vah264enc`/`vah265enc`/`vaav1enc` throughput at 1080p (no desktop involved). Optional env (#92): `SRC=file.nv12` reads raw 1920x1080 NV12 instead of `videotestsrc` (which is CPU-bound at ~180 fps on a loaded box and caps every encoder; FRAMES must not exceed the file's frames * `SRC_REPEAT`; `SRC_FPS`, default 60, is the clip's rate), `SRC_REPEAT=N` plays the file N times, `OUT=file` also writes the encoded stream (for a PSNR/bitrate check against `SRC`). The recipe, with the `ffmpeg` PSNR line, is in the script header |
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

## vm-audio/ (real audio tests, inside the test VMs only)

Scripts for the shared-audio soak in the `arch-ssd` / `fedora-ssd` VMs (`host-run.sh`, `vm-sync.sh` on the host; `soak.sh`, `ff-setup.sh`, `fakehw.sh`, `emu_src.py`, `watch.py`, `probe.py` + `probe-run.sh` (the capture chain alone under graph changes, #128), `probe-view.py` + `probe-view-run.sh` (the viewer's playback tail) in the VM; `analyze.py`, `summary.py`, `phases.py`, `pps.py`, `pktdump.py` (decodes an `SSS_AUDIO_DUMP` packet file) on the recordings). Never run on the host (no audio streams there). Method: `superdoc/claude-instructions/testing-and-verification.md` "Audio tests in the VMs".
