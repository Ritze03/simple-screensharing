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
