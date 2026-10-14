#!/usr/bin/env bash
#
# session.sh — one SHORT, self-contained locked GUI session: take the shared GUI
# lock (waiting for it, never holding it while idle), bring the rig and the
# animation up, run the given steps, then ALWAYS tear everything down, restore
# focus and release the lock (also on Ctrl-C / error).
#
# Usage: session.sh scenarios LABEL:CODEC:FPS:PRESET [...]    measured shares (run-scenario.sh)
#        session.sh audio-peek                                screenshots of the sharer's setup modal + audio dropdown
#        session.sh run CMD [ARGS...]                          any command while the rig is up
# Env: LOCK (default /tmp/claude-1000/sss-gui.lock), LOCK_WAIT secs (default 1800),
#      FOCUS_BACK (monitor to refocus, default DP-3), plus everything rig.sh /
#      run-scenario.sh read. No children inherit the lock fd (9 closed).

set -uo pipefail
SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SELF_DIR/launch-lib.sh"

LOCK="${LOCK:-/tmp/claude-1000/sss-gui.lock}"
FOCUS_BACK="${FOCUS_BACK:-DP-3}"
mkdir -p "$(dirname "$LOCK")"
exec 9>"$LOCK"
log "waiting for the GUI lock ..."
flock -w "${LOCK_WAIT:-1800}" 9 || die "could not get the GUI lock in ${LOCK_WAIT:-1800}s"
log "GUI lock held ($(date +%T))"

teardown() {
  trap - EXIT INT TERM
  "$SELF_DIR/animate.sh" stop 9>&- >/dev/null 2>&1
  "$SELF_DIR/rig.sh" down 9>&- >/dev/null 2>&1
  pkill -TERM -f '/usr/bin/wlr-share-picker$' 2>/dev/null     # a forgotten picker blocks every later share
  mmsg dispatch focusmon,"$FOCUS_BACK" >/dev/null 2>&1
  log "torn down, focus -> $FOCUS_BACK, releasing the lock ($(date +%T))"
  exec 9>&-
}
trap teardown EXIT INT TERM

"$SELF_DIR/rig.sh" up 9>&- 2>&1 | grep -E "window|ERROR" || true
"$SELF_DIR/animate.sh" start "${TARGET:-DP-1}" 9>&- || exit 1
sleep 2

mode="${1:?scenarios|audio-peek|run}"; shift
case "$mode" in
  scenarios)
    for spec in "$@"; do
      IFS=: read -r label codec fps preset <<<"$spec"
      "$SELF_DIR/run-scenario.sh" "$label" "$codec" "$fps" "$preset" 9>&- || log "scenario $label FAILED"
    done
    ;;
  audio-peek)
    d() { "$SELF_DIR/drive.sh" "${MONITOR:-DP-2}" "$@" 9>&-; }
    d share "${TARGET:-DP-1}"
    d setup 120 h265 balanced
    d shot "$CARGO_TARGET_DIR/results/audio-modal.png"
    "$SELF_DIR/ui-click.sh" click $((3840 + 290)) $((1080 + 450)) 9>&-
    sleep 1
    d shot "$CARGO_TARGET_DIR/results/audio-dropdown.png"
    ;;
  run)
    "$@" 9>&-
    ;;
  *) die "unknown mode $mode" ;;
esac
