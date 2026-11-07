# Testing and Verification

**Mandatory working rule for agents, force-loaded from the repo-root `CLAUDE.md`.** Hard rules only; reference
material lives in plain-link pages under `superdoc/testing/` (read on demand): `rig-reference.md` (rig details, portal
failure history, full rationale), `gpu-safety-incidents.md`, `vm-audio.md`, `test-writing-notes.md`,
`relay-and-website-checks.md`. Launch recipe: `client/.claude/skills/run/SKILL.md`; scripts: `client/scripts/README.md`.
Real machine values (the user's desktop, the test VMs' names, ports and login) are in `.claude/local/test-machine.md`
in the **main checkout** (git-ignored; worktrees do not have it).

## The rule: verify against the real egui binary, inside the headless rig

Verify every change against the real running app: the compiled `simple-screensharing-client` binary, a real relay
(`server/`), real windows on a real compositor, a real xdg-desktop-portal screencast, real GPU capture/encode/decode.
This is the default, not a fallback for risky changes, and it includes pure layout/style tweaks: no judgment-call
exceptions.

Does **not** count as verification of app behaviour: `--gallery` (widget work only, no `AppState`/relay/capture/decode),
`cargo test` alone for anything user-visible, browser/Playwright MCP tools (they drive a standalone Chrome, never the
egui window).

The one legitimate fallback is an environment failure (binary cannot be built, rig cannot start: no GPU render node,
sway or portal packages missing). Say so explicitly ("could not run the rig because X"); never silently substitute and
imply equivalence; never fall back to the real desktop.

Why this is written down: sub-agents kept defaulting to mocks plus a browser, and the owner corrected each one; a rule
with a "provably CSS-only" exception got that exception re-argued twice, so there is none. Full text:
`testing/rig-reference.md`.

## The rig

```bash
client/scripts/hs-rig.sh up      # build + sway + bus + PipeWire + portal + relay + pattern + 2 clients
client/scripts/hs-drive.sh ...   # share|shareauto|setup|golive|watch|hover|shot (see client/scripts/README.md)
client/scripts/hs-rig.sh run -- CMD   # CMD with the rig's environment
client/scripts/hs-rig.sh down    # ALWAYS, on success, failure and early exit
```

A private headless sway (HEADLESS-1 captured, HEADLESS-2 sharer, HEADLESS-3 viewer) with its own dbus, PipeWire,
portal and relay, all under one private `XDG_RUNTIME_DIR`. One rig per worktree. Use `PROFILE=release` (the default)
for any fps/latency judgment. A capture test needs the moving `vkcube` pattern (an idle output delivers no frames).
Details, input/screenshot recipes, stock-xdpw start-up flap: `testing/rig-reference.md`.

- **Always headless sway, never Hyprland** (user rule, 2026-11-05: "ALWAYS headless sway. Never hyprland"): no
  Hyprland instance of any kind in a test, not headless, not nested, no `xdg-desktop-portal-hyprland`.
- **The user's real desktop is never used for tests.** Never touch any real output of the user: no windows, clicks,
  screenshots or captures there. Anything aimed outside the rig's environment hits the real session. No compositor
  tools of the real desktop, no real portal picker, **never `ydotool`** (uinput acts on the real session).
- Pointer input = `swaymsg seat - cursor ...` and keys = `wtype`, **only** with the rig's environment; `grim` and
  `swaymsg` likewise (`hs-drive.sh shot`, or `hs-rig.sh run -- ...`). `WAYLAND_DISPLAY` must be the rig's absolute
  socket path; a bare `grim` or an inherited `wayland-0/1` hits the user's desktop.

## GPU: the rig shares the host's real GPU

A VA encoder under window-resize stress or rapid restarts caused VCN page faults, a failed ring reset and a full
`GPU mode1 reset` that aborted the user's desktop session; even a gentle VA resize run (sizes >= 640x360, one change per
3-6 s) still faulted. So:

- **No VA resize test at all, and no rapid encoder restarts on a VA encoder**, until the capture-restart teardown is
  fixed, unless the user okays that run. For that logic use software encoding (`LIBVA_DRIVERS_PATH=/nonexistent
  SSS_CAPTURE_PATH=system` in `CLIENT_ENV_1`/`_2`; the log must say `using software encoder`).
- At the first `mmhub page fault` or `vcn_unified` line in `journalctl -k`, **stop the run and report**; do not continue.

Measurements: `testing/gpu-safety-incidents.md`, `features/stream/capture-sources.md` "Resize recovery verified".

## No audio tests on the host (hard rule, user request 2026-10-14; the test VMs are the exception)

Do not test audio sharing, level meters or tones; create no PipeWire *audio* stream. The user's music player keeps
playing: never stop, pause or reconfigure it, never touch their audio session. Rig fakes that need no audio stream:
`SSS_UI_FAKE_AUDIO_SOURCES=1`, `SSS_UI_FAKE_AUDIO=1`, `SSS_FAKE_CAMERA=1`. **Browser tests too:** anything creating an
`AudioContext` or a `{ audio: true }` capture (`docs/dev/`: `stubCapture({ audio: true })`, `audio-only.mjs`,
`matrix.mjs --audio`) belongs in the VMs; the harness refuses it unless `SSS_E2E_AUDIO=1`.

Why: a live `audiotestsrc ! pipewiresink` stuck at state `S` (suspended with an `active` link, see `pw-top`) wedges
PipeWire link negotiation server-wide, so every new screencast `pipewiresrc` times out after 30 s, and on the shared
PipeWire it breaks the user's other apps. **Never create such a stream.** The rig's PipeWire is private and immune
(`HS_PIPEWIRE=shared` exists; you should not need it).

**Exception, the test VMs** (user offer, 2026-11-03; task #123): two libvirt VMs where real audio tests are allowed.
Procedure, scripts and names: `testing/vm-audio.md`. On the host nothing changes.

## Machine safety

- **Never broad-kill.** No `pkill -x simple-screensh` / `pkill -f simple-screensharing`: every worker's clients, relays
  and sways share those names. Kill only PIDs you recorded or what `hs-rig.sh down` kills.
- **Workers never run sudo.** Report the exact command; only the lead runs it.
- **Shared PipeWire**: do not restart or reconfigure it; the rig's own instance is private.
- **Legacy (real-desktop) scripts** (`launch-test*.sh`, `docs/dev/e2e/native-session.sh`) are never run by agents.
- **Never run the two `real_portal` e2e tests unattended** (`--skip real_portal`): they open a portal dialog on the real
  desktop.
- **Website checks: `/usr/bin/chromium`, headless only**; never a headed browser on the user's desktop (if one is
  truly needed, run it inside the rig's sway and say so). No `--use-fake-ui-for-media-stream` for `getDisplayMedia`.
  Harness: `docs/dev/README.md`; details: `testing/relay-and-website-checks.md`.
- `/tmp` is a 16 GB RAM tmpfs.

### Prove the binary you launch is your build

- A shared `target/` can hold a binary another worktree built, and cargo's freshness check accepts it. **Build what you
  launch in your own worktree's target dir**: `CARGO_TARGET_DIR=<your worktree>/target cargo build --release -p
  simple-screensharing-client -p simple-screensharing-server` (`hs-rig.sh up` does this).
- Before trusting a binary, check it contains a string you just added: `grep -c '<new string>' <binary>`.
- **Never put a Cargo target dir under `/tmp`** (including the scratchpad): a 7.7 GB target filled the tmpfs and broke
  builds ("Disk quota exceeded"). Copy only small binaries there. The rig's scratch dir (`<target>/hs-scratch`) is on
  disk; its tracer log can reach ~100 MB per minute (`HS_TRACE=0`, `hs-rig.sh down --purge`).
- Debug builds panic on icon names missing from `theme::ICONS` (`unknown icon "..."`); report it.

Test-writing traps (the global `rust_i18n` locale, proving "nothing reached the decoder", scratch config, the relay's
cwd): `testing/test-writing-notes.md`, `testing/relay-and-website-checks.md`.

### Clean up

`hs-rig.sh down` (kills the recorded process groups and the tmux session, removes the private runtime dir). Remove
scratch config/storage dirs you created outside the rig, kill any other process you started, and say in your report what
you started and that you stopped it.
