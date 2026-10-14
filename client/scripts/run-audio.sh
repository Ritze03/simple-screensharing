#!/usr/bin/env bash
#
# LEGACY (2026-10-14): works on the user's REAL MangoWC desktop only (audio sharing with the real Spotify stream; NO audio tests are run any more). Agents must not
# use it: since the headless switch all real-app tests run in the private headless sway rig,
# see hs-rig.sh and superdoc/claude-instructions/testing-and-verification.md. Do not use.
#
# run-audio.sh — share the screen WITH Spotify's per-app audio stream and prove
# the viewer plays it. Run inside session.sh (rig + animation up, lock held):
#   session.sh run client/scripts/run-audio.sh LABEL [SPOTIFY_STREAM_VOLUME_PCT]
#
# Evidence collected into $OUT/LABEL/:
#   modal-meter-*.png   sharer's setup modal audio-level meter (source level)
#   tile-*.png          viewer's tile with hover overlay (audio pill vs "No audio shared")
#   final.png           whole viewer/sharer monitor at the end
#   audio.txt           audio-probe.py: Spotify's own stream, and every
#                       simple-screensharing playback stream (the viewer's output)
# If a volume percent is given, Spotify's stream volume is set to it for the run and
# restored to its exact original raw value afterwards (also on error).

set -uo pipefail
SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SELF_DIR/launch-lib.sh"

label="${1:?LABEL}"
pct="${2:-}"
MONITOR="${MONITOR:-DP-2}"
TARGET="${TARGET:-DP-1}"
OUT="${OUT:-$CARGO_TARGET_DIR/results}/$label"
SCRATCH="${SCRATCH:-$CARGO_TARGET_DIR/launch-scratch/rig}"
d() { "$SELF_DIR/drive.sh" "$MONITOR" "$@"; }
mkdir -p "$OUT"
: >"$OUT/audio.txt"

# Spotify's sink-input index and original raw volume (restored on exit).
SPOT="$(pactl -f json list sink-inputs | jq -r '[.[] | select(.properties["application.name"]=="Spotify")][0].index // empty')"
[[ -n "$SPOT" ]] || die "Spotify has no playback stream right now"
ORIG_RAW="$(pactl -f json list sink-inputs | jq -r --argjson i "$SPOT" '.[] | select(.index==$i) | .volume | to_entries[0].value.value')"
restore() { pactl set-sink-input-volume "$SPOT" "$ORIG_RAW"; echo "Spotify volume restored to raw $ORIG_RAW" >>"$OUT/audio.txt"; }
trap restore EXIT
echo "spotify sink-input #$SPOT original raw volume $ORIG_RAW" >>"$OUT/audio.txt"
[[ -n "$pct" ]] && pactl set-sink-input-volume "$SPOT" "${pct}%"

d share "$TARGET" || exit 1
d setup 120 h265 balanced
d spotify
for i in 1 2 3 4; do d crop "$OUT/modal-meter-$i.png" 40 430 440 40; sleep 0.6; done
d shot "$OUT/modal.png"
d golive
sleep 5
d watch
sleep 4
d hover
sleep 3
echo "== during the share, ~10 s in ==" >>"$OUT/audio.txt"
python3 -I "$SELF_DIR/audio-probe.py" Spotify --secs 3 >>"$OUT/audio.txt" 2>&1
python3 -I "$SELF_DIR/audio-probe.py" simple-screensharing --secs 3 >>"$OUT/audio.txt" 2>&1
for i in 1 2 3; do d crop "$OUT/tile-$i.png" 1255 112 405 230; sleep 1; done
d shot "$OUT/final.png"
python3 -I "$SELF_DIR/audio-probe.py" simple-screensharing --secs 3 >>"$OUT/audio.txt" 2>&1
d stopshare
sleep 2
d unwatch
cat "$OUT/audio.txt"
