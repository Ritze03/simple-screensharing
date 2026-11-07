# Rig reference

Reference for the headless-sway test rig, moved out of the force-loaded `claude-instructions/testing-and-verification.md` on 2026-11-07 (row 167). The rules stay there; facts and failure history are here. Launch recipe: `client/.claude/skills/run/SKILL.md`; scripts: `client/scripts/README.md`. GPU incidents: `gpu-safety-incidents.md`.

## The rig

```bash
client/scripts/hs-rig.sh up            # build + sway + bus + PipeWire + portal + relay + pattern + 2 clients
client/scripts/hs-drive.sh share|shareauto|setup|golive|watch|hover|shot ...   # share = the Share menu (modal), shareauto = the Share button (live at once); or hs-scenario.sh for one measured share; hs-resize.sh changes the captured output's size (one change or a burst)
client/scripts/hs-rig.sh down          # always, when you are done (also removes the private runtime dir)
```

It starts, all in one private `XDG_RUNTIME_DIR` under `/run/user/<uid>/sss-hs-<hash>`:
a headless sway with three outputs (HEADLESS-1 1920x1080@240 Hz = captured, fullscreen
`vkcube` on it; HEADLESS-2 = sharer client; HEADLESS-3 = viewer client), a private dbus session,
a private PipeWire + WirePlumber (`-p policy`, no audio/bluetooth/camera devices), an
`xdg-desktop-portal` + `xdg-desktop-portal-wlr` (`chooser_type=none`, so a ScreenCast request
is granted with no picker), a virtual pointer so the seat accepts mouse input, the relay in a
uniquely named tmux session on a random free UDP port, and two clients with scratch
`XDG_CONFIG_HOME`/`XDG_DATA_HOME` auto-connected to one room. A scripted share goes live
through the real portal -> `pipewiresrc` (DMA-BUF) -> `vapostproc` -> VA encode path
(`CaptureDomain::Gpu`); the viewer decodes it with `vah265dec`/`vah264dec`; screenshot with
`grim -o HEADLESS-3` (through `hs-drive.sh shot`). Measured 2026-10-14: H.265 @ 240 fps,
capture 239 fps, encoder output 233 fps, viewer "Decoded 234 fps / shown 234 fps, 0 dropped".

Rules for working in it:

- **Always `hs-rig.sh down`** (success, failure, early exit). `up` tears itself down if it fails
  half way. One rig per `SCRATCH` dir (default: one per worktree, `<target>/hs-scratch/rig`).
- **Input**: pointer = `swaymsg seat - cursor set X Y` / `cursor press|release button1`
  (works because `hs-vptr` gives the seat a pointer capability; without a virtual pointer a
  headless sway seat has capability 0 and clients get no `wl_pointer`); keys = `wtype`. **Never
  `ydotool`**: it is a uinput device and acts on the user's real session. Exact click targets
  come from the client's `SSS_UI_DUMP` file (rects in window points, see `hs-drive.sh`) plus the
  window origin from `swaymsg -t get_tree`.
- **Screenshots**: `grim -o HEADLESS-n` with the rig's environment only (`hs-drive.sh shot`, or
  `hs-rig.sh run -- grim ...`). `WAYLAND_DISPLAY` must be the rig's absolute socket path; a
  bare `grim` (or one with the inherited `WAYLAND_DISPLAY=wayland-0/1`) hits the user's desktop.
- **`hs-rig.sh run -- CMD`** runs CMD with the rig's environment (`XDG_RUNTIME_DIR`,
  `WAYLAND_DISPLAY`, `SWAYSOCK`, `DBUS_SESSION_BUS_ADDRESS`).
- A fullscreen `vkcube` is the moving test pattern (mailbox present mode). An idle output
  delivers no frames to the portal (capture is damage-driven), so a capture test always needs
  the pattern (`HS_PATTERN=none` only for idle-monitor tests).
- Use `PROFILE=release` (the default) for any fps/latency judgment; debug is far too slow for
  240 fps.
- What the rig cannot do: it is sway, not the user's own desktop compositor (compositor-specific behaviour such as
  output layout events, scratchpads, DPMS is not exercised); the headless 240 Hz output is
  paced by a timer in the headless backend, not a monitor (vsync, tearing and VRR behaviour are not
  representative; throughput is: ~239 fps captured from it); the audio path is not testable (below);
  the real picker UI (`wlr-share-picker`) is not exercised.
- Portal binary: the rig runs the stock `/usr/lib/xdg-desktop-portal-wlr` (0.8.4, ext-image-copy-capture; what
  ordinary sway users run). `HS_XDPW=patched` runs a locally patched portal binary (path: `.claude/local/test-machine.md` in the main checkout)
  (wlr-screencopy only), `HS_XDPW=<path>` any binary. **Why stock**: it is what most sway users have, and it dies
  (`ext_image_copy_capture_session_v1#18: error 1: session already has a frame object`) when `pipewiresrc`'s
  start-up streaming -> paused -> streaming flap resumes while the portal's first frame is still outstanding.
  The client defuses that (#52 queue behind `pipewiresrc`, #112 a 30 ms pause between PAUSED and PLAYING,
  `capture::PORTAL_START_SETTLE`; see `features/stream/capture-pipeline.md`). **How to read the error** (#112 measured it
  across commits, 20-90 starts each, same rig, stock xdpw): that line in `xdpw.log`/`xdpw-crashed.log` does NOT by
  itself mean a regression of a recent commit. Before #112 it hit 13-20 % of starts at *every* commit from `a5433cf` (#52)
  to `HEAD` (and 4 of 5 before #52), the rate moving with whatever else ran on the machine; since #112 it is 0 of
  60 (mixed codecs, 60/120 fps). The rig emulates dbus activation for xdpw, so a crash is invisible to scripts
  (the client's single retry on a fresh portal session, #59, succeeds); `$SCRATCH/xdpw-crashed.log` counts them
  (`grep -c 'restarted by bus activation'`). `hs-drive.sh share` (the Share menu path) failing to open the modal (the old "1 in 10") was
  always this crash, not a script race.
  **The other failure of the same flap (#122)**: a share that stays STARTING with no frame ("capture started in
  the Gpu domain", then `the capture source has produced no frame in 3.01s`) and an `xdpw.log` full of
  `pipewire: out of buffers` / `unable to export buffer, dropping frame` is stock xdpw stuck after a *visible*
  pause (`no buffer to queue`, `not streaming` right after a `paused` in the log). #112's pause made it about
  as common as the crash had been (task #120 saw it in 2 of 2 starts; it was not a load flake and not the rig);
  since #122 `capture::repair_portal_start` restarts such a stream once and the client log says
  `portal stream stalled by the start-up flap (first buffer corrupted); restarting it, attempt 1` (expected in
  a large share of starts, harmless; attempt 3 or a `StartFailed` is not). To count starts that really get frames
  (a staged capture looks healthy without them) loop Share -> `golive` -> wait for `sending a keyframe` in
  `client1.log` -> `stopshare`; `ready` in the modal dump only means "capture started".
- A bare `fakesink` as the tail of a `pipewiresrc` probe cannot link (caps ANY -> node class
  `Stream/Input/Unknown` -> WirePlumber 0.5 "target not found"): use `video/x-raw ! fakesink`
  (`hs-probe.sh`).

## What does not count as verification of app behaviour (full text)

- **`--gallery`** (`simple-screensharing-client --gallery`, `client/src/widgets/gallery.rs`).
  It renders every widget helper with fake data. It is the right tool for *widget* work
  (a new row helper, a token change, a slot-overlay check with `set_debug_slots`) and a
  quick visual QC, but it never touches `AppState`, the relay, capture or decode. Do not
  report "the gallery looked right" as proof that a screen, an action or a stream works.
- **`cargo test` alone** for anything user-visible. Unit tests (`client/src/ui/tests.rs`,
  `client/src/widgets/tests.rs`, the lib's tests) are necessary, not sufficient.
- Browser/Playwright MCP tools. They drive a standalone Chrome, never the egui window.

The one legitimate reason to fall back is an environment failure: the binary cannot be built,
the rig cannot start (no GPU render node, sway or the portal packages missing). Say so
explicitly in your report ("could not run the rig because X"); never silently substitute and
imply equivalence. Do not fall back to the real desktop.

## Why these rules exist (full text)

**Why this is written down**: over one session many dispatched sub-agents defaulted to
verifying against mocks (browser + mock data) even for changes that depended on real
backend behaviour, and the project owner corrected each one individually. A first version
of this rule still carved out a "provably CSS-only" mock exception, and that exception was
itself re-litigated twice in the same session (a button-padding change verified against
mock+browser; the owner asked "why not Playwright against the real UI?" both times). A
rule with a judgment-call exception gets the exception re-argued per agent, so there is
none. After the Electron frontend was replaced by egui the same failure has a new
spelling, the `--gallery` screen: fine for widgets, not proof of the app.

**Why headless sway (user request, 2026-10-14)**: the user works at that machine. Tests on
their real desktop covered or stole their windows, grabbed the keyboard through the portal
picker (a GTK layer-shell dialog), and needed `ydotool`, a uinput device that injects into the
*real* session whatever monitor you think you target; one wrong y offset or a window on one of their outputs was always one typo away. A private headless sway has none of that: virtual outputs,
its own seat, a portal that grants a screencast without any UI. It also removes the shared
GUI lock and the multi-agent queue for the one desktop: every worktree runs its own rig in
parallel. The user also stopped audio tests (below).

## History (reasoning kept from the old page)

- **2026-10-14 (user request): everything moved into a private headless sway rig.** The user, back at their desk, asked to keep testing in headless sway and to stop audio tests. The real-desktop rules (a forbidden-output table, the desktop compositor's own tools, a GUI lock, `ydotool` driving, the real portal picker, a music player as test audio source, headed Chromium on real outputs) were dropped from the rule page; the scripts that need them are kept as legacy in `client/scripts/` with a banner. Details of the user's real desktop: `.claude/local/test-machine.md` in the main checkout.
- **Until 2026-10-14** the rule mandated a real Electron shell plus the built `src-tauri` sidecar, driven with Playwright's `_electron` driver. All of that described the deleted Electron/HTML frontend; the app is now a single egui binary.
- Full old text of every version: `git log -- superdoc/claude-instructions/testing-and-verification.md`.
