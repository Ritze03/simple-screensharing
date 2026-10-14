---
name: run
description: Launch and drive the real simple-screensharing egui client (plus a relay, a second client, a real portal screencast) inside the private HEADLESS sway rig for verification. Use whenever asked to run, start, or screenshot this app, or to confirm a change works for real. Never use the user's real desktop. The widget gallery alone is not proof of app behaviour.
---

# Run: simple-screensharing-client (egui) in the headless rig

The tested procedure for launching the **real** native app: the egui binary
`simple-screensharing-client`, talking to a real relay (`simple-screensharing-server`), a second
client as viewer, a real xdg-desktop-portal screencast and real GPU capture/encode/decode, all
inside a **private headless sway** started by one script. The rule and the machine facts live in
`superdoc/claude-instructions/testing-and-verification.md` (force-loaded via the root
`CLAUDE.md`); the scripts are documented in `client/scripts/README.md`. This skill is only the
*how*.

## Hard rules, every time

- **Never use the user's real desktop (MangoWC: DP-1/2/3, HDMI-A-1).** No windows there, no
  `mmsg`, **no `ydotool`** (uinput: injects into the REAL session), no bare `grim`, no real
  portal picker. The user is working at the machine. The GUI lock is not needed.
- **No audio tests** (no audio sharing, meters, tones; nothing that creates a PipeWire audio
  stream). A live `audiotestsrc ! pipewiresink` stuck at `S` wedges PipeWire linking server-wide
  (task #48); Spotify stays untouched.
- Build what you launch in your own worktree target (`hs-rig.sh up` does); never a target under
  `/tmp`. Never `pkill` by name; never sudo.
- **Always finish with `hs-rig.sh down`.**

## 1. Bring the rig up (one command)

```bash
cd client/scripts
./hs-rig.sh up        # release build of client + relay, then: headless sway (3 outputs), private
                      # dbus + PipeWire + portal (no picker), relay, test pattern, sharer + viewer
```

Layout: HEADLESS-1 1920x1080@240 Hz = captured output (fullscreen `vkcube`), HEADLESS-2 =
sharer (client 1), HEADLESS-3 = viewer (client 2); both clients are auto-connected to one room.
`./hs-rig.sh status` shows processes, outputs, windows and the join link. Useful env:
`START_CODEC=h265 START_FPS=240 START_KBPS=...` pre-fill the stream form, `RUST_LOG`,
`HS_NO_BUILD=1`, `SCRATCH=<dir>` (default one rig per worktree), `CLIENT_ENV_1="A=1 B=2"` extra env
for the sharer. A fresh build proves itself: `grep -c '<new string>' target/release/simple-screensharing-client`.

## 2. Drive a share

```bash
./hs-drive.sh share                      # click "Share screen": the portal grants at once; modal opens
./hs-drive.sh setup 240 h265 balanced    # FPS CODEC PRESET [KBPS]; reads every value back, exits 1 on a miss
./hs-drive.sh golive                     # "Go live in <room>"
./hs-drive.sh watch                      # viewer clicks Watch
./hs-drive.sh hover                      # tooltip: "Decoded X fps / shown Y fps, H.265 via vah265dec"
./hs-drive.sh shot /tmp/x/viewer.png     # grim -o HEADLESS-3; `shot f.png HEADLESS-2` for the sharer
./hs-drive.sh stopshare; ./hs-drive.sh unwatch
```

One call for a measured run (7 tooltip screenshots over 30 s + the sender's per-stage rates from
the GStreamer stats tracer): `./hs-scenario.sh LABEL h265 240 balanced` -> `<target>/hs-results/LABEL/`
(set `OUT=` to put it elsewhere; read `montage.png` and `summary.txt`).

Other steps: `modal` (the sharer's `SSS_UI_DUMP`), `mclick NAME` (click a dumped control),
`click|move N X Y` (client N window coordinates), `key N <wtype args>`, `crop FILE OUTPUT X Y W H`,
`tree`. Click targets: the stream-settings modal uses the rects in `$SCRATCH/ui1.txt`; a few fixed
constants at the top of `hs-drive.sh` (Share/Stop/Watch buttons, tile pill) were measured on the
1920x1080 windows, so re-measure them if you move the UI. Run anything else inside the rig with
`./hs-rig.sh run -- CMD...` (it sets `XDG_RUNTIME_DIR`, the absolute `WAYLAND_DISPLAY`, `SWAYSOCK`,
`DBUS_SESSION_BUS_ADDRESS`).

What to look at (evidence for a report): the viewer screenshot (moving cube), the tooltip text
(decoder element, fps, dropped), `$SCRATCH/client1.log` (a `GPU capture path failed to start`
warning means the CPU fallback is in use; none = `CaptureDomain::Gpu`), `gst-rate.py` summary
(`gpu_in` = the GPU capture vapostproc; capture / encode-branch input / encoder output fps).
`hs-probe.sh [SECS] [TAIL]` runs `portal-probe.py` against the rig's portal to isolate
"portal produces N fps" from the app (use a tail starting with `video/x-raw`, never a bare `fakesink`).

## 3. Tear down (always)

```bash
./hs-rig.sh down              # kills the recorded process groups + the relay tmux session, removes
                              # the private runtime dir; add --purge to delete the scratch dir too
```

Say in your report that you ran the rig and tore it down. The sharer's tracer log can be ~100 MB:
`down --purge` or `HS_TRACE=0` if you do not need rates.

## Widget gallery (widget/token work only)

```bash
# rig up first; the window opens on the focused workspace (swaymsg 'workspace number 4' for a clean one)
./hs-rig.sh run -- env XDG_CONFIG_HOME=<scratch> XDG_DATA_HOME=<scratch> "$CLIENT" --gallery
```

Good for a new row helper or a token tweak; `widgets::set_debug_slots` draws the slot overlay. It
does not exercise the app: don't report it as app verification. Always a scratch config/data dir.

## Tests that need no window

`cargo test -p simple-screensharing-client` (UI shell tests in `client/src/ui/tests.rs`,
widget tests, the `every_icon_name_used_in_src_is_in_icons` test) and
`python3 -I client/locales/check_keys.py` (i18n keys). Run these in addition to, never
instead of, the rig run for anything user-visible. For video checks without the rig,
`docs/dev/` has the browser-side sender/viewer harness (`docs/dev/README.md`, headless Chromium only).

## If it breaks

`$SCRATCH/{sway,xdpw,xdp,wireplumber,pipewire,client1,client2,relay.err}.log` (default
`<target>/hs-scratch/rig/`). Known traps (details in `client/scripts/README.md`): `session already has a frame object` in `xdpw.log` means the start-up stream flap is back (#52 fixed it; the rig runs the stock
xdg-desktop-portal-wlr, `HS_XDPW=patched` picks the user's build); a bare `fakesink` probe tail
cannot link (`target not found`); the legacy real-desktop scripts (`drive.sh`, `ui-click.sh`,
`picker-answer.sh`, `animate.sh`, `session.sh`, `rig.sh`, `run-audio.sh`) are off limits.
