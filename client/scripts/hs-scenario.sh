#!/usr/bin/env bash
#
# hs-scenario.sh — one measured share on the HEADLESS rig (hs-rig.sh up): sharer -> relay ->
# viewer, with the viewer's decoded/presented fps (tooltip screenshots) and the sender's
# per-stage buffer rates (GStreamer stats tracer, gst-rate.py). Headless counterpart of
# run-scenario.sh.
#
# Prerequisites: hs-rig.sh up (it starts the sharer with the tracer writing $SCRATCH/gst1.log),
# no stale tile on the viewer.
#
# Usage: SCRATCH=<rig scratch> hs-scenario.sh LABEL CODEC FPS PRESET [KBPS]
#   CODEC h264|av1|h265; FPS 15..240 as in the dropdown; PRESET lowlat|balanced|quality|gaming
# Env: OUT (results dir, default <target>/hs-results), SAMPLES (default 7), INTERVAL (secs, 5)
# Output: $OUT/LABEL/{montage.png,tip-*.png,final-viewer.png,final-sharer.png,rates.txt,
#         summary.txt,notes.txt}. Ends with the share stopped and the viewer's tile closed.

set -uo pipefail
SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SELF_DIR/hs-lib.sh"

label="${1:?LABEL}"; codec="${2:?CODEC}"; fps="${3:?FPS}"; preset="${4:?PRESET}"; kbps="${5:-}"
OUT="${OUT:-$CARGO_TARGET_DIR/hs-results}/$label"
SAMPLES="${SAMPLES:-7}"
INTERVAL="${INTERVAL:-5}"
GSTLOG="$SCRATCH/gst1.log"
d() { "$SELF_DIR/hs-drive.sh" "$@"; }

mkdir -p "$OUT"
{
  echo "scenario: $label  codec=$codec fps=$fps preset=$preset kbps=${kbps:-default}"
  echo "start: $(date -Is)  loadavg: $(cat /proc/loadavg)"
} >"$OUT/notes.txt"

d share || { echo "share FAILED" >>"$OUT/notes.txt"; exit 1; }
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
if [[ -f "$GSTLOG" ]]; then
  python3 -I "$SELF_DIR/gst-rate.py" "$GSTLOG" --tail $(( (SAMPLES - 1) * INTERVAL )) >"$OUT/rates.txt"
  python3 -I "$SELF_DIR/gst-rate.py" "$GSTLOG" --tail $(( (SAMPLES - 1) * INTERVAL )) --summary >"$OUT/summary.txt"
fi
d shot "$OUT/final-viewer.png"
d shot "$OUT/final-sharer.png" "$HS_OUT_SHARER"   # the sender's own live fps label
d stopshare
sleep 2
d unwatch
sleep 1
log "scenario $label done -> $OUT"
[[ -f "$OUT/summary.txt" ]] && cat "$OUT/summary.txt"
exit 0
