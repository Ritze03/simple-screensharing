#!/usr/bin/env bash
#
# probe-run.sh — run portal-probe.py once against DP-1 (or MONITOR) and answer
# the picker safely (picker-answer.sh).
#
# Env: SRC_PROPS = extra pipewiresrc properties (e.g. "min-buffers=8")
# Usage: probe-run.sh OUTFILE SECS "TAIL" [KEEPALIVE_MS] [MONITOR] [CURSOR_MODE 0|1|2|4]
#   TAIL = the GStreamer chain after pipewiresrc, e.g.
#     "videoconvert ! video/x-raw,format=BGRx ! fakesink sync=false"

set -uo pipefail
SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
out="${1:?OUTFILE}"
secs="${2:?SECS}"
tail_="${3:?TAIL}"
ka="${4:-0}"
cursor="${6:-0}"
mon="${5:-DP-1}"
mkdir -p "$(dirname "$out")"
python3 -I "$SELF_DIR/portal-probe.py" --secs "$secs" --tail "$tail_" --keepalive "$ka" --cursor "$cursor" --src "${SRC_PROPS:-}" --watch "${WATCH:-}" 9>&- >"$out" 2>&1 &
pp=$!
"$SELF_DIR/picker-answer.sh" "$mon" 15 || { kill "$pp" 2>/dev/null; exit 1; }
wait "$pp"
cat "$out"
