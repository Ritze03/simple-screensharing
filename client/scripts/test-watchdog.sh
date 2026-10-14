#!/usr/bin/env bash
#
# LEGACY (2026-10-14): works on the user's REAL MangoWC desktop only (smoke test of the real-desktop launch scripts). Agents must not
# use it: since the headless switch all real-app tests run in the private headless sway rig,
# see hs-rig.sh and superdoc/claude-instructions/testing-and-verification.md. 
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
