#!/usr/bin/env bash
#
# LEGACY (2026-10-14): works on the user's REAL MangoWC desktop only (smoke test of the real-desktop launch-test.sh). Agents must not
# use it: since the headless switch all real-app tests run in the private headless sway rig,
# see hs-rig.sh and superdoc/claude-instructions/testing-and-verification.md. 
#
# test-launch-fg.sh — smoke test of the repo-root launch-test.sh (the non-tmux
# variant): runs it inside a throwaway tmux session (its relay TUI needs a pty),
# checks both client windows land on DP-2, quits the relay with 'q' and checks the
# clients are gone. Takes the GUI lock for the few seconds it runs.
#
# Usage: test-launch-fg.sh

set -uo pipefail
SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SELF_DIR/launch-lib.sh"
LOCK="${LOCK:-/tmp/claude-1000/sss-gui.lock}"
exec 9>"$LOCK"
flock -w 1800 9 || die "no GUI lock"
S="$CARGO_TARGET_DIR/launch-scratch/fgtest"
rm -rf "$S"
log "starting launch-test.sh in tmux session sss-fgtest"
tmux new-session -d -s sss-fgtest -c "$REPO_ROOT" "SCRATCH='$S' CLIENT_MONITORS='DP-2 DP-2' '$REPO_ROOT/launch-test.sh' 9>&-; sleep 5" 9>&-
tmux set-option -t "=sss-fgtest:" remain-on-exit on     # "=name:" = exact session, its current window
for _ in $(seq 1 60); do
  [[ "$(pgrep -fc "$CARGO_TARGET_DIR/release/simple-screensharing-client$")" -ge 2 ]] && break
  sleep 1
done
sleep 4
log "clients up: $(pgrep -fc "$CARGO_TARGET_DIR/release/simple-screensharing-client$")"
mmsg get all-clients | jq -c '.clients[] | select(.appid=="simple-screensharing") | {id,monitor}'
cat "$S/client.pids" 2>/dev/null
tmux capture-pane -p -t "=sss-fgtest:" | tail -12
tmux send-keys -t "=sss-fgtest:" q
sleep 5
log "after quitting the relay: clients left: $(pgrep -fc "$CARGO_TARGET_DIR/release/simple-screensharing-client$")"
tmux kill-session -t =sss-fgtest 2>/dev/null
mmsg dispatch focusmon,DP-3 >/dev/null
log "relay processes left: $(pgrep -fc "$CARGO_TARGET_DIR/release/simple-screensharing-server$")"
