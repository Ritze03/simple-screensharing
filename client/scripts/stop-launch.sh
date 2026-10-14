#!/usr/bin/env bash
#
# stop-launch.sh — stop what launch-test-tmux.sh (or launch-test.sh) started.
#
# Usage: stop-launch.sh SCRATCH_DIR [TMUX_SESSION]
#   Kills the client process groups listed in SCRATCH_DIR/client.pids (this also
#   takes their pw-loopback children), then the relay tmux session and its
#   watchdog (default session name: sssrelay). Only what that scratch dir's run
#   created: never a blanket pkill.

set -uo pipefail
SCRATCH="${1:?usage: $0 SCRATCH_DIR [TMUX_SESSION]}"
SESSION="${2:-sssrelay}"
SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SELF_DIR/launch-lib.sh"

# '=' = exact session name match: a bare name also prefix-matches "<name>-watchdog".
tmux kill-session -t "=$SESSION-watchdog" 2>/dev/null
tmux kill-session -t "=$SESSION" 2>/dev/null
sss_kill_clients
log "stopped. Remaining simple-screensharing-client processes (should be none of yours):"
pgrep -af 'release/simple-screensharing-client' | cut -c1-120
exit 0
