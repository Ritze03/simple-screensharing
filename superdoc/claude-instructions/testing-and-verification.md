# Testing and Verification

**Mandatory working rule for agents, force-loaded from the repo-root `CLAUDE.md`.**

Last rewritten 2026-10-14 for the headless-sway rig (the user asked for all real-app tests to
move off their desktop; earlier the same day it was rewritten for the native egui client and
before that the Electron/Tauri-sidecar rules went away; see **History** at the end). The concrete
launch recipe is the client's `run` skill, `client/.claude/skills/run/SKILL.md`; the rig's
scripts are documented in `client/scripts/README.md`. This page is the rule and the machine
facts those two rely on.

## The rule: verify against the real egui binary, inside the headless rig

Verify every change against the real running app: the compiled
`simple-screensharing-client` binary (`client/`, crate `simple-screensharing-client`), a real
relay (`server/`), real windows on a real compositor, a real xdg-desktop-portal screencast,
real GPU capture/encode/decode. This is the default, not a fallback for changes you judge
risky. It includes pure layout/CSS-style tweaks: launching the rig is barely more expensive than
any substitute, and it removes the judgment call about when a substitute is "good enough".

**The compositor is a private headless sway that you start yourself (`client/scripts/hs-rig.sh
up`). The user's real desktop is never used for tests.** Nothing in a test may touch the user's
real outputs or input: no windows, no `hyprctl dispatch`/`mmsg`, no `ydotool`, no `grim` without the rig's
environment, no real portal picker. Never touch the user's real outputs (Hyprland: DP-1, DP-2,
DP-3, HDMI-A-1). The GUI lock is not needed (the rig shares nothing visible with anyone).

What does **not** count as verification of app behaviour:

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
their MangoWC desktop covered or stole their windows, grabbed the keyboard through the portal
picker (a GTK layer-shell dialog), and needed `ydotool`, a uinput device that injects into the
*real* session whatever monitor you think you target; one wrong y offset or an HDMI-A-1 window
(their RDP) was always one typo away. A private headless sway has none of that: virtual outputs,
its own seat, a portal that grants a screencast without any UI. It also removes the shared
GUI lock and the multi-agent queue for the one desktop: every worktree runs its own rig in
parallel. The user also stopped audio tests (below).

## The rig

```bash
client/scripts/hs-rig.sh up            # build + sway + bus + PipeWire + portal + relay + pattern + 2 clients
client/scripts/hs-drive.sh share|setup|golive|watch|hover|shot ...   # or hs-scenario.sh for one measured share
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
- What the rig cannot do: it is sway, not the user's Hyprland (compositor-specific behaviour such as
  output layout events, scratchpads, DPMS is not exercised); the headless 240 Hz output is
  paced by a timer in the headless backend, not a monitor (vsync, tearing and VRR behaviour are not
  representative; throughput is: ~239 fps captured from it); the audio path is not testable (below);
  the real picker UI (`wlr-share-picker`) is not exercised.
- Portal binary: the rig runs the stock `/usr/lib/xdg-desktop-portal-wlr` (0.8.4, ext-image-copy-capture; what
  ordinary sway users run). `HS_XDPW=patched` runs the user's `~/.local/bin/xdg-desktop-portal-wlr-patched`
  (wlr-screencopy only), `HS_XDPW=<path>` any binary. **Why stock**: it used to crash the share
  (`ext_image_copy_capture_session_v1#18: error 1: session already has a frame object`) when the
  client's GPU pipeline flapped the stream at start; #52 fixed that in the client (queue behind
  `pipewiresrc`, see `features/stream-control.md`), so the default now covers the common setup. If that
  error shows up in `xdpw.log` again, the fix regressed.
- A bare `fakesink` as the tail of a `pipewiresrc` probe cannot link (caps ANY -> node class
  `Stream/Input/Unknown` -> WirePlumber 0.5 "target not found"): use `video/x-raw ! fakesink`
  (`hs-probe.sh`).

## No audio tests (hard rule, user request 2026-10-14)

Do not test audio sharing, level meters or tones; do not create any PipeWire *audio* stream.
Spotify keeps playing on the user's machine (their own session, not part of the rig): never
stop, pause or reconfigure it. If you need an example stream for something, Spotify is it, read
only. The rig's clients never share audio, have `PULSE_SERVER` pointed at a dead socket, and the
rig's PipeWire is private.

**Why, and the `pipewiresink` stall warning (task #48)**: a live `audiotestsrc ! pipewiresink`
that gets stuck at state `S` (suspended with an `active` link; check `pw-top`) wedges PipeWire
link negotiation server-wide (PipeWire 1.6.9): no new link gets a `check_states`, and every new
screencast `pipewiresrc` times out after 30 s ("GPU capture path failed to start ... Element
failed to change its state", then the CPU fallback hangs on the same source). A cleanly
corked/paused stream does not trigger it. **Never create such a stream**; on the user's shared
PipeWire it also breaks their other apps. (The rig's private PipeWire is immune, one reason it is
the default; `HS_PIPEWIRE=shared` is available but you should not need it.) See
`features/stream-control.md` "Capture/encode performance".

## Machine safety

- **Never touch the user's real outputs** (Hyprland, layout read 2026-10-14: DP-1 at 0,0
  1920x1080@280 Hz, DP-2 at 1920,0, DP-3 at -1920,0, HDMI-A-1 at 0,-1080 above DP-1; the last one
  holds their WinBoat RDP window). No window, click, screenshot or capture there. Re-check: if
  a command of yours has no rig environment it is aimed at the real session.
- **PipeWire**: the rig's own instance is private. If you ever use the shared one, screencast
  nodes are just more nodes; do not restart or reconfigure it.
- **Never broad-kill.** No `pkill -x simple-screensh` / `pkill -f simple-screensharing`: every
  worker's clients, relays and sways share those names. Kill only PIDs you recorded or the
  process groups the rig recorded in `$SCRATCH/hs.pids` (`hs-rig.sh down` does exactly that).
- **sudo**: **workers never run sudo.** If something needs it, report the exact command; only the
  lead runs it, via its `claude-sudo` tmux session.
- `/tmp` is a 16 GB RAM tmpfs: see the target-dir rule below.

### Prove the binary you launch is your build

- A shared `target/` at the repo root can hold a binary another worktree built more
  recently, and cargo's freshness check accepts it. One earlier run executed other
  workers' code without noticing. So **build what you launch in your own worktree's
  target dir**: `CARGO_TARGET_DIR=<your worktree>/target cargo build --release -p simple-screensharing-client -p simple-screensharing-server`
  (`hs-rig.sh up` does this: `launch-lib.sh` `sss_build`, target `<worktree>/target`). The
  first build takes minutes; it is worth it.
- Before trusting a binary, check it contains a string you just added:
  `grep -c '<new string>' <binary>`.
- **Never put a Cargo target dir under `/tmp`** (including the scratchpad). `/tmp` is a
  16 GB RAM tmpfs; a 7.7 GB target filled it and broke builds with "Disk quota exceeded".
  Copy only small binaries to the scratchpad and delete large artifacts when done. The rig's
  scratch dir (`<target>/hs-scratch`) is on disk; the sharer's GStreamer tracer log can reach
  ~100 MB per minute there (`HS_TRACE=0` to skip, `hs-rig.sh down --purge` to delete).
- Debug builds panic on icon names missing from `theme::ICONS` (`unknown icon "..."`);
  report it. A release build only shows a blank glyph.

### Tests and the global locale

`rust_i18n`'s locale is process-global and `cargo test` runs tests in parallel. A test that calls
`rust_i18n::set_locale("de")` makes unrelated tests fail at random (`left: "Bildschirmfreigabe", right: "Screen share"`,
task #78). Tests that need another locale either pass it explicitly (`t!(key, locale = "de")`, as
`pill_slots_fit_every_label_in_every_locale` does) or run in a child process with their own locale
(`ui::stream_settings::tests::every_locale_fits_two_columns_at_full_modal_width` re-runs itself per locale with
`SSS_TEST_LOCALE`). Never switch the global locale in-process in a test.

### Scratch config

The rig launches every client with its own `XDG_CONFIG_HOME`/`XDG_DATA_HOME` in the scratch
dir; the user's saved rooms never auto-connect. If you launch a client outside the rig
(for example `--gallery`) set `XDG_CONFIG_HOME=<scratch dir>` yourself; eframe may also create
`~/.local/share/<app-id>/` storage dirs: delete any you created.

### The relay

`cargo build -p simple-screensharing-server` (own target dir, see above). Its ratatui TUI
needs a pty: the rig runs it inside `tmux new-session -d` (uniquely named, killed by `down`).
Default port 4433; the rig picks a random free UDP port. Cert state lives in `./state/`
relative to the working directory, so **start it in a scratch cwd**. For e2e tests point
them at your own binary with `SSSHARE_SERVER_BIN=<path>` (and `SSSHARE_SERVER_CWD=<dir>`);
copy the binary somewhere private if others may rebuild it meanwhile. Never run the two
`real_portal` e2e tests unattended (`--skip real_portal`); they open a portal dialog on the
real desktop. `docs/dev/relay.sh` starts a throwaway relay in tmux and prints its certificate
hash.

### Website (browser viewer) checks

Use `/usr/bin/chromium`, **headless only** (the harness is `docs/dev/`, see
`docs/dev/README.md`). Never open a headed browser window on the user's desktop; if a check
truly needs headed Chromium (H.265 decode only works on the hardware path, not headless),
run it inside the rig's sway (untested: `hs-rig.sh run -- chromium --ozone-platform=wayland ...`, window
on HEADLESS-2/3) and say so in your report.

- Do **not** pass `--use-fake-ui-for-media-stream` for `getDisplayMedia` tests: it picks a
  `screen:0:0` source that delivers no frames (found by a past run). Use a
  `canvas.captureStream()` source or the harness's own runner.

### Clean up

`hs-rig.sh down` (it kills the recorded process groups, the tmux session, removes the private
runtime dir `/run/user/<uid>/sss-hs-<hash>` and warns about processes that still reference it).
Remove scratch config/storage dirs you created outside the rig, kill any other process you
started, and say in your report what you started and that you stopped it.

## History

- **2026-10-14: the user switched their desktop from MangoWC to Hyprland 0.56.2** (same output
  names; layout in "Machine safety"). The repo-root `launch-test*.sh` (via
  `client/scripts/launch-lib.sh`) gained `hyprctl` support next to `mmsg`; the HDMI refusal and
  overlap check work on both. Rules above are unchanged: agents still never run them.
- **2026-10-14 (user request): everything moved into a private headless sway rig.** The user,
  back at their desk, asked to keep testing in headless sway and to stop audio tests. The
  real-desktop rules (monitor table with HDMI-A-1 forbidden zone, `mmsg`, GUI lock, `ydotool`
  driving, the real portal picker with `wlr-share-picker`, Spotify as test audio source, headed
  Chromium on DP-1/DP-2) are gone from this page; the scripts that need them are kept as
  legacy in `client/scripts/` with a banner. The old monitor layout was DP-3 (0,1080) the
  user's terminal, DP-1 (1920,1080) 279.75 Hz, DP-2 (3840,1080), HDMI-A-1 (1920,0) above DP-1.
  Full old text: `git log -- superdoc/claude-instructions/testing-and-verification.md`.
- **Until 2026-10-14** this page mandated a real Electron shell plus the built `src-tauri`
  sidecar, driven with Playwright's `_electron` driver, and gave the HDMI output as
  starting at x=5760 (contiguous DP-1..DP-3). All of that described the deleted
  Electron/HTML frontend and an older monitor layout; the layout is now different (HDMI is
  *above* DP-1) and the app is a single egui binary. The reasoning that survived is the
  "Why this is written down" paragraph above. Full old text: `git log -- superdoc/claude-instructions/testing-and-verification.md`.
