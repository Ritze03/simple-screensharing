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

**Always headless sway, never Hyprland (user rule, 2026-11-05).** No Hyprland instance of any kind in a test: not headless,
not nested, no `xdg-desktop-portal-hyprland`. The user: "ALWAYS headless sway. Never hyprland".

**The rig does not isolate the GPU (2026-11-05, row 148).** It shares the host's real amdgpu. A rig client's VA encoder under a
window-resize stress in sway (random sizes down to about 60x36, several per second) caused VCN page faults, `ring vcn_unified_1
timeout`, a failed ring reset and a full `GPU mode1 reset` with `VRAM is lost`, which aborted the user's Hyprland, kitty, Floorp and
gamescope. So **no resize stress or rapid encoder restarts on a VA encoder on this machine** unless the user okays that run; use
software encoding (x264, `CaptureDomain::System`) for that logic. At the first `mmhub page fault` or `vcn_unified` line in
`journalctl -k`, stop the run and report; do not continue.

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
- What the rig cannot do: it is sway, not the user's Hyprland (compositor-specific behaviour such as
  output layout events, scratchpads, DPMS is not exercised); the headless 240 Hz output is
  paced by a timer in the headless backend, not a monitor (vsync, tearing and VRR behaviour are not
  representative; throughput is: ~239 fps captured from it); the audio path is not testable (below);
  the real picker UI (`wlr-share-picker`) is not exercised.
- Portal binary: the rig runs the stock `/usr/lib/xdg-desktop-portal-wlr` (0.8.4, ext-image-copy-capture; what
  ordinary sway users run). `HS_XDPW=patched` runs the user's `~/.local/bin/xdg-desktop-portal-wlr-patched`
  (wlr-screencopy only), `HS_XDPW=<path>` any binary. **Why stock**: it is what most sway users have, and it dies
  (`ext_image_copy_capture_session_v1#18: error 1: session already has a frame object`) when `pipewiresrc`'s
  start-up streaming -> paused -> streaming flap resumes while the portal's first frame is still outstanding.
  The client defuses that (#52 queue behind `pipewiresrc`, #112 a 30 ms pause between PAUSED and PLAYING,
  `capture::PORTAL_START_SETTLE`; see `features/stream-control.md`). **How to read the error** (#112 measured it
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

## No audio tests on the host (hard rule, user request 2026-10-14; the test VMs are the exception, next section)

Do not test audio sharing, level meters or tones; do not create any PipeWire *audio* stream.
Spotify keeps playing on the user's machine (their own session, not part of the rig): never
stop, pause or reconfigure it. If you need an example stream for something, Spotify is it, read
only. The rig's clients never share audio, have `PULSE_SERVER` pointed at a dead socket, play their notification sounds into
`SSS_SOUND_SINK=fakesink` (`hs_start_client` sets it, 2026-11-04) and the rig's PipeWire is private. Rig fakes that need no audio
stream: `SSS_UI_FAKE_AUDIO_SOURCES=1` (the audio menu's sources; sharing is refused), `SSS_UI_FAKE_AUDIO=1` (watched streams act as if they had audio),
`SSS_FAKE_CAMERA=1` (a moving test camera). **Browser tests too:** a test that creates an `AudioContext` or a `{ audio: true }` capture
(`docs/dev/`: `stubCapture({ audio: true })`, `audio-only.mjs`, `matrix.mjs --audio`) belongs in the VMs; the harness refuses it unless
`SSS_E2E_AUDIO=1` and mutes Chromium otherwise (`docs/dev/README.md` "No audio on the host").

**Why, and the `pipewiresink` stall warning (task #48)**: a live `audiotestsrc ! pipewiresink`
that gets stuck at state `S` (suspended with an `active` link; check `pw-top`) wedges PipeWire
link negotiation server-wide (PipeWire 1.6.9): no new link gets a `check_states`, and every new
screencast `pipewiresrc` times out after 30 s ("GPU capture path failed to start ... Element
failed to change its state", then the CPU fallback hangs on the same source). A cleanly
corked/paused stream does not trigger it. **Never create such a stream**; on the user's shared
PipeWire it also breaks their other apps. (The rig's private PipeWire is immune, one reason it is
the default; `HS_PIPEWIRE=shared` is available but you should not need it.) See
`features/stream-control.md` "Capture/encode performance".

## Audio tests in the VMs (user offer, 2026-11-03)

The no-audio rule above is for the **host**. The user offered two qemu VMs where real audio testing is
allowed (task #123): libvirt `qemu:///system` domains **`arch-ssd`** and **`fedora-ssd`** (shut off by default).
Inside them you may create audio streams, play a tone in Firefox, use `pw-cli`/`pw-link`/`pactl` and install
packages with the VM's own passwordless `sudo`. On the host nothing changes: no audio streams, never Spotify.
Both are GNOME/Wayland desktops with PipeWire + WirePlumber, user `mo`, no sound card (only `auto_null`).

- **Start / reach:** `virsh -c qemu:///system start arch-ssd` (and/or `fedora-ssd`). The NIC is user-mode
  (`passt`), so `virsh domifaddr` is empty; libvirt forwards **`127.0.0.1:2224` -> arch, `127.0.0.1:2223` ->
  fedora** to the guest's sshd: `ssh -p 2224 mo@127.0.0.1` (key login works). `virsh qemu-agent-command <dom>
  '{"execute":"guest-ping"}'` also works. A guest sees `/run/user/1000` with `wayland-0`; scripts source
  `client/scripts/vm-audio/lib.sh` for `XDG_RUNTIME_DIR`/`WAYLAND_DISPLAY`/`DBUS_SESSION_BUS_ADDRESS`. Shut down
  with `virsh -c qemu:///system shutdown <dom>` when done.
- **Build in the VM** (glibc differs per distro): `client/scripts/vm-audio/vm-sync.sh <port>` tars the worktree
  into `~/sss` (no rsync in the VMs), then in the VM `cargo test -p simple-screensharing-client --release --lib
  --no-run` and `cargo build --release -p simple-screensharing-server` (about 4 min cold, 35 s incremental).
  Arch had every dependency already; Fedora needed `gstreamer1-devel gstreamer1-plugins-base-devel
  gstreamer1-plugins-bad-free-devel` (`client/check-deps.sh` says what is missing).
- **Make it look like a desktop:** `fakehw.sh` (in the VM) adds a driver-clocked null sink
  `alsa_output.fakehw` at priority 1000 as a PipeWire drop-in. Without it the only sink is the fallback
  `auto_null`, which WirePlumber removes the moment another sink appears, and the share sink becomes the default
  one: a different situation from a desktop with a real output device. EasyEffects (`dnf install easyeffects`,
  `QT_QPA_PLATFORM=wayland easyeffects -w`) was run on the Fedora VM to match the user's host; it changed nothing.
- **Source of sound:** `ff-setup.sh [firefox]` makes a 440 Hz `tone.wav` with ffmpeg, a page with
  `<audio loop autoplay>` and a Firefox profile with autoplay allowed, and starts Firefox on it (Arch has no
  Firefox package in the image; an unpacked `download.mozilla.org` tarball in `~/ffox` works). Pause/resume it
  without any GUI driving through MPRIS (`busctl --user call org.mpris.MediaPlayer2.firefox.instance_* /org/mpris/MediaPlayer2
  org.mpris.MediaPlayer2.Player Pause|Play`); no `ydotool` needed. Firefox's own `PlaybackStatus` property is the
  way to see who paused it.
- **Run a share:** the ignored `end_to_end_test::live_audio_share_soak` shares the node named `SSS_LIVE_APP`
  (default `Firefox`) from an in-process sharer to an in-process viewer through a real relay
  (`SSSHARE_SERVER_BIN`, `SSSHARE_SERVER_CWD`) for `SSS_LIVE_SECONDS`. The viewer's real `autoaudiosink` plays to
  the null sink `viewer-out` (`PULSE_SINK=viewer-out`), whose monitor `pw-record` writes to a WAV: that is *what
  a viewer hears*. A second `pw-record` on the share sink's monitor shows whether the graph or the app's pipeline
  is at fault. `client/scripts/vm-audio/soak.sh LABEL pause|pause1|idle` (in the VM) drives one run
  (pause/resume cycles, or a plain stop) and `host-run.sh PORT LABEL SCENARIO [SECS]` runs it from the host and
  analyzes it: `analyze.py` (per 0.5 s: level, zero-crossing frequency, largest sample step relative to the peak; a
  clean 440 Hz sine is 440 Hz / 0.058, a click or dropout is far above, a rate error moves the frequency) and
  `summary.py` (seconds until clean after each resume). `watch.py` logs every node/link change every 0.3 s, which
  is how the lost links and the re-created ports were found.
- **A source that keeps its node and ports (task #125):** Firefox in the VMs re-creates its ports on resume; the host's
  Floorp does not. `emu_src.py NAME` (a GStreamer `pulsesink`, SIGUSR1 pause / SIGUSR2 play) behaves like the host
  one: `EMU=1 APP=Floorp host-run.sh PORT LABEL pause|flap`. `VM_SSS=<dir in the VM>` runs a baseline build kept in
  another checkout (`VM_SSS='$HOME/sss-base'`). Make the fake hardware suspend when idle (`node.pause-on-idle = true`,
  `node.always-process = false` in the fakehw drop-in) to get the Dummy-Driver switch a real ALSA sink gives. A healthy
  share delivers ~50 packets/s to the viewer (`pps.py test.log`); fewer means the capture stream is not being scheduled.
- **Graph changes, rates, dumps (task #128):** the VMs default to `clock.allowed-rates = [48000]`, the user's host to
  `[44100, 48000]` (its hardware runs at 44.1 kHz): pass `ALLOWED="44100 48000" SRATE=44100` (emulated source at
  44.1 kHz) or the VM never switches rate. `soak.sh ... graph` changes quantum and rate under a playing source,
  `... vanish` destroys and re-creates the source node (EMU only). `probe.py`/`probe-run.sh MODE forcerate|pause|quantum`
  run the share's capture chain without the app and print samples/s, PTS steps and the pipeline clock against
  `CLOCK_MONOTONIC` (`pwclock` vs `sysclock`); `probe-view.py`/`probe-view-run.sh` do the same for the viewer's playback
  tail. `soak.sh` sets `SSS_AUDIO_DUMP` (client/src/audio_dump.rs): `pktdump.py sent-*.pkt out.wav` decodes what the
  sender sent, `recv-*.pkt` what the viewer received, `play-*.log` lists its `push`/`gap` actions; `phases.py viewer.wav
  events.txt` counts bad 0.5 s windows per phase. Always compare with the share sink's own monitor (`sinkmon.wav`):
  that is the VM's noise floor (0-3 bad windows per phase). In the VM, kill with patterns that cannot match your own
  ssh command line (`pkill -f "[p]robe.py"`), or the ssh session kills itself.
- **Noise floor of a silent stretch (#129):** `VFMT=f32 host-run.sh ...` records the viewer as float (an s16 recording adds
  its own dither and hides what the viewer produced), `noise.py viewer.wav events.txt` prints the RMS of every paused stretch.
- **Desktop audio (task I7, 2026-11-04):** `client/scripts/vm-audio/desktop-audio.sh LABEL desktop|named|empty|only` (in the VM) drives desktop audio, the exclusion list (Discord added, removed, Firefox added and removed again) and audio-only shares against real streams plus silent fake sources for the groups; `desktop-run.sh PORT LABEL SCENARIO` runs it from the host and analyzes the recordings with `tones.py FILE.wav FREQ...` (per window, how loud each tone is in dBFS: 440 Hz = Firefox, 880 Hz = the excluded app; pure analysis of a file, plays nothing).
- **Also capture, before/after a change:** `pw-dump`, `pw-top -b -n 2`, `pw-metadata -n settings`, and the
  WirePlumber side (`WIREPLUMBER_DEBUG=s-linking.mpris:5`) when a player is paused unexpectedly.
- **Leave the VMs as found:** remove `~/.config/pipewire/pipewire.conf.d/50-fakehw.conf` (and restart
  `pipewire pipewire-pulse wireplumber`) if you added it; installed build dependencies may stay (say which in
  the report).

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

### Tests of "nothing reached the decoder"

Do not prove a negative with `sleep(150 ms)` plus "`decoded_total` did not change" (task #112): frames
already inside a GStreamer decoder keep arriving after the call you are testing, so the counter moves a
little later on a loaded machine (`left: 3, right: 2` in
`deltas_of_the_other_bitstream_family_are_held_until_a_keyframe`, 31 of 100 runs failed under a 40-thread
`stress-ng`, 0 of 200 after the fix). Assert on what the code under test did synchronously instead:
`decode::video::Worker::pushed()` (test-only) counts the access units handed to a pipeline. To hunt such a
flake, run the single test binary 100-200 times with `xargs -P 20` under `stress-ng --cpu 40` (start it yourself,
stop it afterwards).

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
