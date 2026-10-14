#!/usr/bin/env bash
#
# test-watchdog.sh — run inside `session.sh run client/scripts/test-watchdog.sh`
# (rig up, lock held): kills the relay's tmux session and checks launch-test-tmux.sh's
# watchdog then stops both clients (process groups from <scratch>/client.pids).

set -uo pipefail
SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SELF_DIR/launch-lib.sh"
BIN="$CARGO_TARGET_DIR/release/simple-screensharing-client"
log "clients before: $(pgrep -fc "$BIN$")"
SESSION="$(<"${SCRATCH:-$CARGO_TARGET_DIR/launch-scratch/rig}/tmux.session")"   # this rig's own session (rig.sh up records it)
tmux kill-session -t "=$SESSION"
sleep 6
log "clients after killing the relay session (watchdog should have stopped them): $(pgrep -fc "$BIN$")"
tmux ls 2>/dev/null | grep -c "^$SESSION" || true
