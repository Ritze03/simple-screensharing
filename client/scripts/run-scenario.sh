#!/usr/bin/env bash
#
# LEGACY (2026-10-14): works on the user's REAL MangoWC desktop only (drive.sh + animate.sh on real monitors). Agents must not
# use it: since the headless switch all real-app tests run in the private headless sway rig,
# see hs-rig.sh and superdoc/claude-instructions/testing-and-verification.md. Headless: hs-scenario.sh.
#
# run-scenario.sh — one measured share: sender -> relay -> viewer, with the
# receiver's decoded/presented fps (tooltip screenshots) and the sender's
# per-stage buffer rates (GStreamer stats tracer) over ~30 s.
#
# Prerequisites (see launch-test-tmux.sh and drive.sh):
#   * the two-client rig is up on MONITOR, the SHARER started with the tracer:
#       CLIENT_ENV_1="GST_TRACERS=stats GST_DEBUG=GST_TRACER:7 GST_DEBUG_NO_COLOR=1 GST_DEBUG_FILE=<scratch>/gst1.log"
#   * a high-frame-rate animation is running on the captured monitor (animate.sh)
#   * the viewer shows no stale tile (drive.sh unwatch)
#
# Usage: run-scenario.sh LABEL CODEC FPS PRESET [KBPS]
#   CODEC h264|av1|h265; FPS 15..240 as in the dropdown; PRESET lowlat|balanced|quality|gaming
# Env: MONITOR (clients' monitor, default DP-2), TARGET (captured monitor, default
#      DP-1), SCRATCH (holds gst1.log), OUT (results dir, default
#      <target>/results), SAMPLES (default 7), INTERVAL (secs, default 5)
# Output: $OUT/LABEL/{montage.png,tip-*.png,rates.txt,summary.txt,notes.txt}
# Ends with the share stopped and the viewer's tile closed.

set -uo pipefail
SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SELF_DIR/launch-lib.sh"

label="${1:?LABEL}"; codec="${2:?CODEC}"; fps="${3:?FPS}"; preset="${4:?PRESET}"; kbps="${5:-}"
MONITOR="${MONITOR:-DP-2}"
TARGET="${TARGET:-DP-1}"
SCRATCH="${SCRATCH:-$CARGO_TARGET_DIR/launch-scratch/rig}"   # holds gst1.log (rig.sh up)
OUT="${OUT:-$CARGO_TARGET_DIR/results}/$label"
SAMPLES="${SAMPLES:-7}"
INTERVAL="${INTERVAL:-5}"
GSTLOG="$SCRATCH/gst1.log"
export SCRATCH                             # drive.sh reads the modal dump + pids from it
d() { "$SELF_DIR/drive.sh" "$MONITOR" "$@"; }

mkdir -p "$OUT"
{
  echo "scenario: $label  codec=$codec fps=$fps preset=$preset kbps=${kbps:-default}"
  echo "start: $(date -Is)  loadavg: $(cat /proc/loadavg)"
} >"$OUT/notes.txt"

d share "$TARGET" || exit 1
d setup "$fps" "$codec" "$preset" ${kbps:+"$kbps"} || { echo "setup FAILED" >>"$OUT/notes.txt"; exit 1; }
d golive || { echo "golive FAILED" >>"$OUT/notes.txt"; exit 1; }
sleep 5
d watch
sleep 4
d hover
sleep 6                                    # warm-up: encoder rate control, jitter buffer
echo "measuring: loadavg $(cat /proc/loadavg)" >>"$OUT/notes.txt"
d sample "$OUT" "$SAMPLES" "$INTERVAL"
echo "end: $(date -Is)  loadavg: $(cat /proc/loadavg)" >>"$OUT/notes.txt"
python3 -I "$SELF_DIR/gst-rate.py" "$GSTLOG" --tail $(( (SAMPLES - 1) * INTERVAL )) >"$OUT/rates.txt"
python3 -I "$SELF_DIR/gst-rate.py" "$GSTLOG" --tail $(( (SAMPLES - 1) * INTERVAL )) --summary >"$OUT/summary.txt"
d shot "$OUT/final.png"                    # includes the sender's own window: its live fps label
d stopshare
sleep 2
d unwatch
sleep 1
log "scenario $label done -> $OUT"
cat "$OUT/summary.txt"
