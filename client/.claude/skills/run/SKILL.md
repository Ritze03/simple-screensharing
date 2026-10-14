---
name: run
description: Launch and drive the real simple-screensharing egui client (plus a relay) on this machine for verification. Use whenever asked to run, start, or screenshot this app, or to confirm a change works for real. The widget gallery alone is not proof of app behaviour.
---

# Run: simple-screensharing-client (egui)

The tested procedure for launching the **real** native app: the egui binary
`simple-screensharing-client`, talking to a real relay (`simple-screensharing-server`),
on this machine's MangoWC desktop. The rule and the full machine facts (monitors,
GUI lock, portal picker, sudo) live in
`superdoc/claude-instructions/testing-and-verification.md` (force-loaded via the root
`CLAUDE.md`); read it first. This skill is only the *how*.

## Safety, every time

- `mmsg get all-monitors` first; re-check, don't trust remembered numbers. Layout as of
  2026-10-14: DP-3 (0,1080) the user's terminal, DP-1 (1920,1080) 279.75 Hz, DP-2
  (3840,1080), **HDMI-A-1 (1920,0) FORBIDDEN** (user's RDP window). Windows only at x
  1920-5759, y >= 1080.
- Hold the GUI lock for the whole interactive part: `exec 9>/tmp/claude-1000/sss-gui.lock && flock 9`.
- Scratch `XDG_CONFIG_HOME` always (else the user's saved rooms auto-connect).
- Screenshots: `grim -o DP-1 f.png` or `-o DP-2`. Never HDMI-A-1, never `grim` without `-o`.
- No sudo, ever. No Cargo target dir under `/tmp`.

## 1. Build both binaries from your own worktree

Build what you launch in your own on-disk target dir (a shared target can contain another
worktree's newer binary, which cargo happily accepts):

```bash
export CARGO_TARGET_DIR="$PWD/target"        # from your worktree root, NOT /tmp
cargo build -p simple-screensharing-client -p simple-screensharing-server
CLIENT=$CARGO_TARGET_DIR/debug/simple-screensharing-client
RELAY=$CARGO_TARGET_DIR/debug/simple-screensharing-server
grep -c '<a string you just added>' "$CLIENT"   # prove it is your build
```

Use `--release` for frame-rate / latency checks (a debug build is too slow to judge 240 fps).

## 2. Widget gallery (widget/token work only)

```bash
"$CLIENT" --gallery      # every widget helper with fake data
```

Good for a new row helper or a token tweak; `widgets::set_debug_slots` draws the slot
overlay. It does not exercise the app: don't report it as app verification.

## 3. Relay in tmux, scratch cwd

The relay's TUI needs a pty and it keeps cert state in `./state/` of its cwd.

```bash
S=$(mktemp -d -p "$HOME/.cache" sss-run.XXXX)       # on disk, not /tmp
mkdir -p "$S/relay" "$S/cfg"
tmux new-session -d -s sss-relay -c "$S/relay" "$RELAY"     # default port 4433
```

`docs/dev/relay.sh start <relay-bin> <scratch-dir> [port]` is an alternative that also
prints `PORT=… HASH=<certhash>`. The join link is `server|room-id|key|certhash`; the
Create modal in the app mints one and "Copy invite link" yields the full form.

## 4. Client with a scratch config on DP-1 / DP-2

```bash
exec 9>/tmp/claude-1000/sss-gui.lock && flock 9
mmsg get all-monitors                        # confirm layout, HDMI-A-1 at 1920,0
mmsg dispatch focusmon,DP-1                  # new windows open on the focused monitor
XDG_CONFIG_HOME="$S/cfg" "$CLIENT" &  CPID=$!
sleep 3
mmsg get all-clients                         # find the window; confirm it is on DP-1/DP-2
# if needed (absolute coordinates; Y >= 1080):
#   mmsg dispatch togglefloating client,<id>
#   mmsg dispatch movewin,2000,1200 client,<id>
#   mmsg dispatch resizewin,1280,800 client,<id>
grim -o DP-1 "$S/shot.png"                   # then Read the PNG
```

Drive it with `ydotool` (`mousemove --absolute -x X -y <global_y - 1080>`, `click 0xC0`,
`type`). Verify with a screenshot before clicking; the Y offset is the usual mistake.

Typical flow: Create/Join modal -> join the relay -> a second client (second scratch
config, same lock session) or the browser viewer to watch -> "Share screen". The portal
picker then appears: type `dp`, check `~/.local/state/wlr-share-picker/last-choice.json`
names DP-1/DP-2, press Enter. Never `grim` a full monitor while the picker is open (its
thumbnails include HDMI); use `grim -g <region>`. Afterwards `pgrep -af wlr-share-picker`
must be empty.

For video checks the lead keeps a scratch x264 test-pattern sender built on the lib API
(ask the lead for its current path); `docs/dev/` has the browser-side sender/viewer
harness (`docs/dev/README.md`).

## 5. Tear down (always)

```bash
kill "$CPID"; tmux kill-session -t sss-relay
mmsg dispatch focusmon,DP-3                  # restore focus to the user's terminal
rm -rf "$S"; rm -rf ~/.local/share/simple-screensharing* 2>/dev/null   # only if YOU created them
```

Release the GUI lock only after windows are closed and focus is restored. Say in your
report which processes you started and that you stopped them.

## Tests that need no window

`cargo test -p simple-screensharing-client` (UI shell tests in `client/src/ui/tests.rs`,
widget tests, the `every_icon_name_used_in_src_is_in_icons` test) and
`python3 -I client/locales/check_keys.py` (i18n keys). Run these in addition to, never
instead of, the real launch for anything user-visible.
