# Relay and website checks

Moved out of the force-loaded `claude-instructions/testing-and-verification.md` on 2026-11-07 (row 167).

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

