#!/usr/bin/env bash
#
# LEGACY (2026-10-14): puts client windows on the user's REAL MangoWC desktop (mmsg, real monitors).
# Agents must not use it: since the headless switch all real-app tests run in the private headless
# sway rig, see client/scripts/hs-rig.sh and superdoc/claude-instructions/testing-and-verification.md.
# Why: the user works at that machine and asked for tests off their desktop. The script refuses
# unless SSS_ALLOW_REAL_DESKTOP=1 (the user's own manual use; the legacy client/scripts/rig.sh and
# test-launch-fg.sh call it too and therefore need that variable exported as well).
# Headless replacement: client/scripts/hs-rig.sh up.
#
# launch-test-tmux.sh — tmux variant of launch-test.sh. Same stack (relay +
# N native egui clients, scratch configs, pre-joined test room, HDMI safety
# check), but the relay's TUI lives in a named tmux session you can attach to
# and detach from, and the script returns once everything is up.
#
# All the logic lives in client/scripts/launch-lib.sh (shared by both scripts).

set -uo pipefail

SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# Unique per run unless overridden: a shared default ("sssrelay") let two runs
# collide, and stop-launch.sh finds a run's session via <SCRATCH>/tmux.session.
TMUX_SESSION="${TMUX_SESSION:-sssrelay-$$-$RANDOM}"

usage() {
  cat <<EOF
launch-test-tmux.sh — build the relay + egui client, start the relay in tmux
session '$TMUX_SESSION' and open N client windows for manual testing.

Usage: $0 [--help]

Each client gets its own scratch XDG_CONFIG_HOME with the test room already
saved (auto-connect) so it joins on start; nothing touches your real config.
Client 1 is the sharer, client 2 the viewer by convention.

Env var overrides:
  PORT              relay UDP port (default: 4433)
  CLIENTS           number of client instances (default: 2)
  CLIENT_MONITORS   monitor per client, space separated, wraps around
                    (default: "DP-2 DP-2"). Refused if any is an HDMI output
                    or overlaps one (checked with mmsg; MangoWC).
  PROFILE           release (default) or debug. Use release for fps work.
  CARGO_TARGET_DIR  build dir (default: <repo>/target, on disk, never /tmp)
  SCRATCH           scratch dir for configs/logs/relay state
                    (default: a fresh dir under <CARGO_TARGET_DIR>/launch-scratch)
  START_CODEC / START_FPS / START_KBPS
                    pre-fill the stream-settings form: h264|h265|av1, fps, kbps
  RUST_LOG          client log filter (default: info)
  CLIENT_ENV_<N>    extra environment for client N only, e.g.
                    CLIENT_ENV_1="GST_TRACERS=stats GST_DEBUG=GST_TRACER:7 GST_DEBUG_FILE=/x/gst1.log"
  COPY_LINK=1       also copy the join link to the clipboard (off by default)
  TMUX_SESSION      tmux session name for the relay (default: unique per run,
                    sssrelay-<pid>-<random>; recorded in <SCRATCH>/tmux.session)
  ATTACH            1 = attach to the relay session at the end, 0 = don't
                    (default: 1 if stdin is a terminal)
  GUI_LOCK_FILE     if set, hold an exclusive flock on it until this script
                    returns (several agents sharing one desktop); the relay and
                    clients never inherit it
  SSS_SKIP_MONITOR_CHECK=1
                    skip the HDMI safety check (non-MangoWC compositors only)

This run's relay session would be '$TMUX_SESSION' (a new name each run unless
TMUX_SESSION is set); a watchdog session '$TMUX_SESSION-watchdog' stops the
clients when the relay session ends. Stop everything with:
  tmux kill-session -t =$TMUX_SESSION
or client/scripts/stop-launch.sh <SCRATCH> (reads <SCRATCH>/tmux.session).
Logs: <SCRATCH>/client<N>.log; relay state + certificate: <SCRATCH>/relay/.
EOF
}

if [[ "${1:-}" == "--help" || "${1:-}" == "-h" ]]; then usage; exit 0; fi
[[ $# -eq 0 ]] || { usage >&2; exit 2; }

# LEGACY guard (2026-10-14): refuse unless explicitly opted in; point at the headless rig instead.
if [[ "${SSS_ALLOW_REAL_DESKTOP:-}" != 1 ]]; then
  echo "$(basename "$0"): refused. This opens windows on the user's REAL desktop (legacy)." >&2
  echo "Use the private headless sway rig instead: client/scripts/hs-rig.sh up  (see client/scripts/README.md)." >&2
  echo "The user's own manual use: SSS_ALLOW_REAL_DESKTOP=1 $0" >&2
  exit 3
fi

# shellcheck source=client/scripts/launch-lib.sh
source "$SELF_DIR/client/scripts/launch-lib.sh"

command -v tmux >/dev/null 2>&1 || die "tmux is required (the relay's ratatui TUI needs a real pty)."
if tmux has-session -t "=$TMUX_SESSION" 2>/dev/null; then
  die "a tmux session named '$TMUX_SESSION' already exists; kill it (tmux kill-session -t $TMUX_SESSION) or set TMUX_SESSION."
fi

# Safety first, before anything is built or opened.
read -r -a MONS <<<"$CLIENT_MONITORS"
sss_check_monitors "${MONS[@]}" || exit 1

sss_take_gui_lock
sss_build
sss_make_scratch
echo "$TMUX_SESSION" >"$SCRATCH/tmux.session"

log "starting the relay in tmux session '$TMUX_SESSION' on UDP port $PORT ..."
# fd 9 closed so the tmux server (which outlives us) never holds the GUI lock.
tmux new-session -d -s "$TMUX_SESSION" -c "$SCRATCH/relay" "exec '$RELAY_BIN' '$PORT'" 9>&-
sss_wait_for_cert || { tmux kill-session -t "=$TMUX_SESSION" 2>/dev/null; die "relay did not write its certificate in 10 s (tmux session killed)."; }
sss_make_join_string

sss_spawn_clients || { sss_kill_clients; tmux kill-session -t "=$TMUX_SESSION" 2>/dev/null; die "client launch failed; everything stopped."; }

# Watchdog: its own detached tmux session so it survives this script. When the
# relay session ends, kill the client process groups listed in client.pids.
WATCHDOG="$TMUX_SESSION-watchdog"
tmux kill-session -t "=$WATCHDOG" 2>/dev/null || true
tmux new-session -d -s "$WATCHDOG" "while tmux has-session -t '=$TMUX_SESSION' 2>/dev/null; do sleep 2; done; for p in \$(cat '$SCRATCH/client.pids'); do kill -TERM -- -\$p 2>/dev/null; done; sleep 1; for p in \$(cat '$SCRATCH/client.pids'); do kill -KILL -- -\$p 2>/dev/null; done" 9>&-

sss_print_summary
log "tmux session: $TMUX_SESSION (recorded in $SCRATCH/tmux.session)"
log "relay TUI: tmux attach -t =$TMUX_SESSION   (Ctrl-B then D to detach)"
log "stop everything: tmux kill-session -t =$TMUX_SESSION   (the watchdog then stops the clients)"

if [[ "${ATTACH:-}" == 1 || ( -z "${ATTACH:-}" && -t 0 ) ]]; then
  exec 9>&-    # release the GUI lock (if any) before blocking in the attach
  tmux attach -t "=$TMUX_SESSION"
fi
