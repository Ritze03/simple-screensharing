#!/usr/bin/env bash
#
# hs-resize.sh - change the size of the rig's captured output (HEADLESS-1, what the portal shares)
# while a share is live: one change, or a burst such as a window edge being dragged.
#
# Usage: SCRATCH=<rig scratch> hs-resize.sh WxH            one mode change
#        SCRATCH=<rig scratch> hs-resize.sh burst [N] [GAP_MS] [FINAL WxH]
#          N sizes (default 12) walking between 1280x720 and 1920x1080, GAP_MS apart (default 80),
#          ending on FINAL (default 1600x900). Prints each change with a ms timestamp.
# The size change reaches the client as a source size change: see features/stream-control.md
# "Source resize (2026-11-05)". Needs a running rig (hs-rig.sh up).

set -uo pipefail
SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SELF_DIR/hs-lib.sh"
hs_load_state
hs_export_env
hs_assert_private

# Window mode (HS_PATTERN=window, row 148): the shared thing is the floating vkcube window.
#   hs-resize.sh win WxH                    one window resize
#   hs-resize.sh winstress SECS [SEED]      random sizes (tiny, odd, wide, tall) in bursts of 3-8 every 60-250 ms with 1-4 s
#                                           pauses, for SECS seconds, then rests at 1000x600; one line per change
win_size() {
    echo "$(date +%T.%3N) vkcube -> $1"
    swaymsg "[title=\"vkcube\"] resize set ${1%x*} ${1#*x}" >/dev/null
}
if [ "${1:-}" = win ]; then win_size "${2:?usage: hs-resize.sh win WxH}"; exit 0; fi
if [ "${1:-}" = winstress ]; then
    secs="${2:?usage: hs-resize.sh winstress SECS [SEED]}"; RANDOM="${3:-1}"
    rnd() { echo $(( $1 + RANDOM % ($2 - $1 + 1) )); }
    end=$((SECONDS + secs)); changes=0
    while [ $SECONDS -lt $end ]; do
        for ((k = $(rnd 3 8); k > 0; k--)); do
            case $(rnd 0 3) in
                0) w=$(rnd 40 200); h=$(rnd 30 150) ;;
                1) w=$(( $(rnd 200 1800) | 1 )); h=$(( $(rnd 150 1000) | 1 )) ;;
                2) w=$(rnd 900 1800); h=$(rnd 100 300) ;;
                *) w=$(rnd 100 300); h=$(rnd 600 1000) ;;
            esac
            win_size "${w}x${h}"; changes=$((changes + 1))
            sleep "$(awk "BEGIN{print $(rnd 60 250)/1000}")"
        done
        sleep "$(rnd 1 4)"
    done
    win_size 1000x600
    echo "winstress done: $changes size changes"
    exit 0
fi

set_size() {
    echo "$(date +%T.%3N) HEADLESS-1 -> $1"
    swaymsg output HEADLESS-1 resolution "$1" >/dev/null
}

if [ "${1:-}" = burst ]; then
    n="${2:-12}"; gap="${3:-80}"; final="${4:-1600x900}"
    for ((i = 0; i < n; i++)); do
        w=$((1280 + (i * 53) % 641)); h=$((w * 9 / 16 / 2 * 2))
        set_size "${w}x${h}"
        sleep "$(awk "BEGIN{print $gap/1000}")"
    done
    set_size "$final"
else
    set_size "${1:?usage: hs-resize.sh WxH | burst [N] [GAP_MS] [FINAL]}"
fi
