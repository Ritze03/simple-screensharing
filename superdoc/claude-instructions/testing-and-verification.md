# Testing and Verification

**Mandatory working rule for agents, force-loaded from the repo-root `CLAUDE.md`.**

Last rewritten 2026-10-14 for the native egui client (the Electron/Tauri-sidecar era rules
are gone; see **History** at the end). The concrete launch recipe is the client's `run`
skill, `client/.claude/skills/run/SKILL.md`; this page is the rule and the machine facts
that skill relies on.

## The rule: verify against the real egui binary, by default

Verify every change against the real running app: the compiled
`simple-screensharing-client` binary (`client/`, crate `simple-screensharing-client`), a
real relay (`server/`), real windows on a real compositor. This is the default, not a
fallback for changes you judge risky. It includes pure layout/CSS-style tweaks: once the
binary is built, launching it is barely more expensive than any substitute, and it removes
the judgment call about when a substitute is "good enough".

What does **not** count as verification of app behaviour:

- **`--gallery`** (`simple-screensharing-client --gallery`, `client/src/widgets/gallery.rs`).
  It renders every widget helper with fake data. It is the right tool for *widget* work
  (a new row helper, a token change, a slot-overlay check with `set_debug_slots`) and a
  quick visual QC, but it never touches `AppState`, the relay, capture or decode. Do not
  report "the gallery looked right" as proof that a screen, an action or a stream works.
- **`cargo test` alone** for anything user-visible. Unit tests (`client/src/ui/tests.rs`,
  `client/src/widgets/tests.rs`, the lib's tests) are necessary, not sufficient.
- Browser/Playwright MCP tools. They drive a standalone Chrome, never the egui window.

The one legitimate reason to fall back is an environment failure: no display reachable,
the binary cannot be built, no GPU/Wayland session. Say so explicitly in your report
("could not launch the real app because X"); never silently substitute and imply
equivalence.

**Why this is written down**: over one session many dispatched sub-agents defaulted to
verifying against mocks (browser + mock data) even for changes that depended on real
backend behaviour, and the project owner corrected each one individually. A first version
of this rule still carved out a "provably CSS-only" mock exception, and that exception was
itself re-litigated twice in the same session (a button-padding change verified against
mock+browser; the owner asked "why not Playwright against the real UI?" both times). A
rule with a judgment-call exception gets the exception re-argued per agent, so there is
none. After the Electron frontend was replaced by egui the same failure has a new
spelling, the `--gallery` screen: fine for widgets, not proof of the app.

## Machine safety (hard constraints)

### Monitors: never touch HDMI-A-1

The compositor is **MangoWC** (`XDG_CURRENT_DESKTOP=mango`, wlroots-based Wayland), not
Hyprland: `hyprctl` and `xrandr` tell you nothing useful. Its IPC is `mmsg`. **Re-check
the layout with `mmsg get all-monitors` before every launch**, every time; layouts change.
Layout as of 2026-10-14:

| Output | Origin | Notes |
| --- | --- | --- |
| DP-3 | x=0 y=1080 | 119.93 Hz. **The user's terminal (ghostty).** Don't cover or close it. |
| DP-1 | x=1920 y=1080 | 279.75 Hz. Use for 240 fps tests. The user's Spotify lives here as a minimised scratchpad (test audio source): never close or kill it; check `mmsg get all-clients` before placing windows. |
| DP-2 | x=3840 y=1080 | 119.93 Hz. Empty. |
| **HDMI-A-1** | x=1920 y=0, above DP-1 | **FORBIDDEN.** The user's WinBoat RDP window (`xfreerdp`) is there. Never place a window, click, screenshot or capture there. |

Safe region for windows and capture: x 1920-5759, y >= 1080 (DP-1 and DP-2). HDMI-A-1
occupies x 1920-3839, y 0-1079, so a window with a too-small Y lands on it.

- New windows open on the focused monitor. Run `mmsg dispatch focusmon,DP-1` (or `DP-2`)
  before launching, then confirm where the window actually landed with
  `mmsg get all-clients`. "Launched on the safe monitor" and "confirmed where it is" are two
  separate steps. Afterwards restore focus (`mmsg dispatch focusmon,DP-3`, then the user's
  ghostty).
- `mmsg dispatch <func>,<args> client,<id>` places windows: `togglefloating`,
  `movewin,X,Y` (**absolute** global coordinates, not offsets), `resizewin,W,H`.
  `mmsg -h` lists the rest; find other functions by experiment and restore focus after.
- Screenshots: `grim -o DP-1 file.png` (or `-o DP-2`). Never `-o HDMI-A-1` and never a
  full-desktop `grim` without `-o`.
- Input: `ydotool` (user service `ydotoold` is running; socket `$YDOTOOL_SOCKET`,
  `~/.YDOTOOL-SOCKET`). `ydotool mousemove --absolute` maps Y with a **-1080 offset** on
  this setup: to hit global y=1500 pass `-y 420`. Verify with a screenshot before
  clicking, and double-check targets fall inside DP-1/DP-2.

### Screen-share portal picker

Starting a share opens the portal picker (`xdg-desktop-portal-wlr-patched` ->
`wlr-share-picker`, a GTK layer-shell dialog that grabs the keyboard).

- Type `dp` (filters to sources containing "dp", so HDMI-A-1 cannot match) and press
  Enter. It preselects the last choice from
  `~/.local/state/wlr-share-picker/last-choice.json`; check that file names DP-1 or DP-2
  before pressing Enter.
- **Never take a full-monitor screenshot while the picker is open**: its thumbnails
  include HDMI. To capture the picker use `grim -g <region>` only.
- Before releasing the GUI lock make sure no picker is left: `pgrep -af wlr-share-picker`.
  SIGTERM makes it answer "cancel". A forgotten picker blocks every later share request.

### GUI lock: several agents share one desktop

Hold the lock around every interactive GUI session (launching windows, focus changes,
`ydotool`, `grim`):

```bash
exec 9>/tmp/claude-1000/sss-gui.lock && flock 9   # at the top of your script; released on exit
```

Keep a locked session to a few minutes: launch, screenshot, click, kill, release. Think
and edit code outside the lock. Release only after your windows are closed and focus is
restored. (The lock file is a few bytes; `/tmp` is fine for that, see the target-dir rule
below for what is not.)

### Prove the binary you launch is your build

- A shared `target/` at the repo root can hold a binary another worktree built more
  recently, and cargo's freshness check accepts it. One earlier run executed other
  workers' code without noticing. So **build what you launch in your own worktree's
  target dir**: `CARGO_TARGET_DIR=<your worktree>/target cargo build -p simple-screensharing-client`
  (same for `-p simple-screensharing-server`). The first build takes minutes; it is worth it.
  For plain `cargo build`/`cargo test` *checks* that you won't run, the shared dir is fine.
- Before trusting a binary, check it contains a string you just added:
  `grep -c '<new string>' <binary>`.
- **Never put a Cargo target dir under `/tmp`** (including the scratchpad). `/tmp` is a
  16 GB RAM tmpfs; a 7.7 GB target filled it and broke builds with "Disk quota exceeded".
  Copy only small binaries to the scratchpad and delete large artifacts when done.
- Debug builds panic on icon names missing from `theme::ICONS` (`unknown icon "..."`);
  report it. A release build only shows a blank glyph.

### Always launch with a scratch config

Launch the app with `XDG_CONFIG_HOME=<scratch dir>`. Otherwise it loads the user's saved
rooms and auto-connects them. eframe may also create `~/.local/share/<app-id>/` storage
dirs: delete any you created.

### The relay

`cargo build -p simple-screensharing-server` (own target dir, see above). Its ratatui TUI
needs a pty: run it inside `tmux new-session -d`. Default port 4433. Cert state lives in
`./state/` relative to the working directory, so **start it in a scratch cwd**. For e2e
tests point them at your own binary with `SSSHARE_SERVER_BIN=<path>` (and
`SSSHARE_SERVER_CWD=<dir>`); copy the binary somewhere private if others may rebuild it
meanwhile. Never run the two `real_portal` e2e tests unattended (`--skip real_portal`);
they open a portal dialog. `docs/dev/relay.sh` starts a throwaway relay in tmux and prints
its certificate hash.

### Website (browser viewer) checks

Use `/usr/bin/chromium`. Headless for automated checks (the harness is `docs/dev/`, see
`docs/dev/README.md`); headed only on DP-1/DP-2 and only while holding the GUI lock.

- Do **not** pass `--use-fake-ui-for-media-stream` for `getDisplayMedia` tests: it picks a
  `screen:0:0` source that delivers no frames (found by a past run). Use a
  `canvas.captureStream()` source or the harness's own runner.
- H.265 decode only works on the hardware path, so it needs a headed run; it fails
  headless.

### Audio

Spotify plays on purpose as the real audio source: pick its stream in the per-app audio
picker for audio-share tests. Never stop it.

### sudo

**Workers never run sudo.** If something needs it, report the exact command; only the
lead runs it, via its `claude-sudo` tmux session.

### Clean up

Kill every process you started (relay, client, chromium, static servers, tmux sessions),
remove scratch config/storage dirs you created, and say so in your report.

## History

- **Until 2026-10-14** this page mandated a real Electron shell plus the built `src-tauri`
  sidecar, driven with Playwright's `_electron` driver, and gave the HDMI output as
  starting at x=5760 (contiguous DP-1..DP-3). All of that described the deleted
  Electron/HTML frontend and an older monitor layout; the layout is now different (HDMI is
  *above* DP-1) and the app is a single egui binary. The reasoning that survived is the
  "Why this is written down" paragraph above. Full old text: `git log -- superdoc/claude-instructions/testing-and-verification.md`.
