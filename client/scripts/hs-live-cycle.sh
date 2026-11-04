#!/usr/bin/env bash
#
# hs-live-cycle.sh — change Preset / bitrate / FPS / Keyframe on a LIVE share, over and over, through the real UI
# (`hs-drive.sh live`), and count what each change did. Rig must be up (hs-rig.sh up), nothing shared yet.
#
# Usage: SCRATCH=<rig scratch> hs-live-cycle.sh LABEL CODEC FPS PRESET KBPS SECS [GAP]
#   starts a share with those values, goes live, a viewer watches, then every GAP s (default 6) applies the next
#   step of the cycle until SECS have passed. Prints a summary of sender and viewer log counts for the whole run.
# Cycle (each entry is one `hs-drive.sh live` call = one modal visit, changes a few hundred ms apart):
#   preset=quality kbps=10000 | preset=gaming kbps=25700 | fps=60 gop=1000 | fps=120 gop=250 | kbps=10000 |
#   kbps=25700 | quality=performance | quality=quality | gop=1000 | gop=250 | fps=240 | fps=120
#   (env CYCLE="step|step|..." overrides it)
# Output: $OUT/LABEL/{steps.txt,c1.log,c2.log,summary.txt,final-viewer.png}; OUT defaults to <target>/hs-results.
set -uo pipefail
SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SELF_DIR/hs-lib.sh"
hs_load_state
hs_export_env
label="${1:?LABEL}"; codec="${2:?CODEC}"; fps="${3:?FPS}"; preset="${4:?PRESET}"; kbps="${5:?KBPS}"; secs="${6:?SECS}"; gap="${7:-6}"
OUT="${OUT:-$CARGO_TARGET_DIR/hs-results}/$label"; mkdir -p "$OUT"
d() { "$SELF_DIR/hs-drive.sh" "$@"; }
CYCLE_DEFAULT="preset=quality kbps=10000|preset=gaming kbps=25700|fps=60 gop=1000|fps=120 gop=250|kbps=10000|kbps=25700|quality=performance|quality=quality|gop=1000|gop=250|fps=240|fps=120"
IFS='|' read -r -a steps <<<"${CYCLE:-$CYCLE_DEFAULT}"
C1="$SCRATCH/client1.log"; C2="$SCRATCH/client2.log"
: >"$OUT/steps.txt"
start_line="$(wc -l <"$C1")"; start_line2="$(wc -l <"$C2")"   # count from before the share starts: the first deaths come right after Go live
d share || die "share failed"
d setup "$fps" "$codec" "$preset" "$kbps" || die "setup failed"
d golive || die "golive failed"
sleep 4; d watch; sleep 6
t0=$SECONDS; n=0; fails=0
while (( SECONDS - t0 < secs )); do
  s="${steps[$(( n % ${#steps[@]} ))]}"; n=$((n + 1))
  echo "$(( SECONDS - t0 ))s #$n $s (c1 line $(wc -l <"$C1"))" >>"$OUT/steps.txt"
  # shellcheck disable=SC2086
  d live $s >>"$OUT/steps.txt" 2>&1 || { fails=$((fails + 1)); echo "  FAILED" >>"$OUT/steps.txt"; }
  sleep "$gap"
done
d shot "$OUT/final-viewer.png"
tail -n +"$((start_line + 1))" "$C1" | sed 's/\x1b\[[0-9;]*m//g' >"$OUT/c1.log"
tail -n +"$((start_line2 + 1))" "$C2" | sed 's/\x1b\[[0-9;]*m//g' >"$OUT/c2.log"
cnt() { grep -c -- "$1" "$2"; }
{
  echo "== $label: $codec $fps fps $preset $kbps kbps, $secs s, gap $gap s: $n changes ($fails driver failures)"
  echo "capture source lost:       $(cnt 'capture source lost' "$OUT/c1.log")"
  echo "portal restart attempts:   $(cnt 'restart attempt' "$OUT/c1.log")"
  echo "branch flow errors:        $(cnt 'returned a flow error' "$OUT/c1.log")"
  echo "branch rebuilds (failed):  $(cnt 'rebuilding just this branch' "$OUT/c1.log")"
  echo "keyframes sent:            $(cnt 'sending a keyframe' "$OUT/c1.log")"
  echo "AV1 rate-control restarts: $(cnt 'restarting its rate control' "$OUT/c1.log")"
  echo "AV1 restart learned off:   $(cnt 'not restarting it again' "$OUT/c1.log")"
  echo "AV1 quality floors set:    $(cnt 'rebuilding it with a quality floor' "$OUT/c1.log")"
  echo "codec fallbacks:           $(cnt 'moving this destination to' "$OUT/c1.log")"
  echo "no IDR within 5s:          $(cnt 'produced no IDR' "$OUT/c1.log")"
  echo "capture saw no frame:      $(cnt 'produced no frame' "$OUT/c1.log")"
  echo "viewer keyframe never completed: $(cnt 'never completed' "$OUT/c2.log")"
} | tee "$OUT/summary.txt"
d stopshare >/dev/null 2>&1; sleep 2; d unwatch >/dev/null 2>&1
exit 0
