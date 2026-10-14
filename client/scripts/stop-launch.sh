#!/usr/bin/env bash
#
# stop-launch.sh — stop what launch-test-tmux.sh (or launch-test.sh) started.
#
# Usage: stop-launch.sh SCRATCH_DIR [TMUX_SESSION]
#   Kills the client process groups listed in SCRATCH_DIR/client.pids (this also
#   takes their pw-loopback children), then the relay tmux session and its
#   watchdog. The session name is TMUX_SESSION, else the one recorded in
#   SCRATCH_DIR/tmux.session by rig.sh; with neither, no tmux session is touched
#   (there is NO shared default name: a fixed name like `sssrelay` could be
#   someone else's relay). Only what that scratch dir's run created: never a
#   blanket pkill.

set -uo pipefail
SCRATCH="${1:?usage: $0 SCRATCH_DIR [TMUX_SESSION]}"
SESSION="${2:-}"
SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SELF_DIR/launch-lib.sh"
[[ -z "$SESSION" && -f "$SCRATCH/tmux.session" ]] && SESSION="$(<"$SCRATCH/tmux.session")"

# '=' = exact session name match: a bare name also prefix-matches "<name>-watchdog".
if [[ -n "$SESSION" ]]; then
  tmux kill-session -t "=$SESSION-watchdog" 2>/dev/null
  tmux kill-session -t "=$SESSION" 2>/dev/null
else
  log "no tmux session recorded in $SCRATCH/tmux.session and none given: leaving tmux alone"
fi
sss_kill_clients
log "stopped. Remaining simple-screensharing-client processes (should be none of yours):"
pgrep -af 'release/simple-screensharing-client' | cut -c1-120
exit 0
