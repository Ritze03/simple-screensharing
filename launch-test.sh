#!/usr/bin/env bash
#
# launch-test.sh — developer convenience script for manually testing
# simple-screensharing end to end on one machine: builds the relay and the
# native egui client, starts the relay, and opens N client windows (each with
# its own scratch config, pre-joined to a fresh test room) on the monitors you
# choose. No tmux: the relay's ratatui TUI needs a real pty, so it runs in the
# FOREGROUND of this terminal as the last step. Quit it ('q' in the TUI) or
# Ctrl-C and the clients are cleaned up. launch-test-tmux.sh is the variant that
# keeps the relay in a tmux session instead.
#
# All the logic lives in client/scripts/launch-lib.sh (shared by both scripts).

set -uo pipefail

SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

usage() {
  cat <<EOF
launch-test.sh — build the relay + egui client, start the relay (foreground,
in this terminal) and open N client windows for manual testing.

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
  GUI_LOCK_FILE     if set, hold an exclusive flock on it while running (several
                    agents sharing one desktop); children never inherit it
  SSS_SKIP_MONITOR_CHECK=1
                    skip the HDMI safety check (non-MangoWC compositors only)

Logs: <SCRATCH>/client<N>.log; relay state + certificate: <SCRATCH>/relay/.
Stop everything: quit the relay TUI ('q') or Ctrl-C this script.
EOF
}

if [[ "${1:-}" == "--help" || "${1:-}" == "-h" ]]; then usage; exit 0; fi
[[ $# -eq 0 ]] || { usage >&2; exit 2; }

# shellcheck source=client/scripts/launch-lib.sh
source "$SELF_DIR/client/scripts/launch-lib.sh"

# Safety first, before anything is built or opened.
read -r -a MONS <<<"$CLIENT_MONITORS"
sss_check_monitors "${MONS[@]}" || exit 1

sss_take_gui_lock
sss_build
sss_make_scratch

cleanup_done=0
cleanup() {
  [[ "$cleanup_done" == 1 ]] && return
  cleanup_done=1
  log "stopping clients ..."
  sss_kill_clients
}
trap cleanup EXIT INT TERM

# The clients need the relay's certificate hash for their join link, and the
# relay only creates its certificate once it runs. So a background job waits
# for the certificate and then opens the clients, while the relay itself takes
# this terminal's pty in the foreground below.
(
  sss_wait_for_cert || { echo "ERROR: relay did not write its certificate in 10 s" >&2; exit 1; }
  sss_make_join_string
  sss_spawn_clients || { echo "ERROR: client launch failed" >&2; exit 1; }
  # The relay's TUI owns this terminal's screen, so the join link goes to a file.
  sss_print_summary >"$SCRATCH/launch-summary.txt" 2>&1
) 9>&- &

log "join link + summary: $SCRATCH/launch-summary.txt (the relay TUI will take over this terminal)"
log "starting the relay in THIS terminal on UDP port $PORT (press q to quit and stop the clients) ..."
( cd "$SCRATCH/relay" && exec "$RELAY_BIN" "$PORT" ) 9>&-
