#!/usr/bin/env bash
#
# hs-extra-client.sh N NAME WS — start one more client (client N, display name NAME) in the running
# headless rig (hs-rig.sh up), on HEADLESS-3 workspace WS, auto-connected to the rig's room. For tests
# that need more than the rig's two clients (e.g. several viewers of one share, task #116):
#
#   hs-extra-client.sh 3 Alice 4        # workspace numbers 1-3 belong to the rig, use 4, 5, ...
#   hs-drive.sh click 3 377 371         # client 3 clicks Watch (WATCH_BTN of hs-drive.sh)
#   hs-rig.sh run -- swaymsg "workspace number 3"   # show the rig's own viewer again
#
# The new window covers the viewer's workspace until you switch back. `hs-rig.sh down` stops it (hs_spawn
# records it in $HS_PIDS like the others). Run with the same SCRATCH as the rig.
set -uo pipefail
SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SELF_DIR/hs-lib.sh"
hs_load_state
hs_export_env
hs_assert_private
n="${1:?N}"; name="${2:?NAME}"; ws="${3:?WS}"
swaymsg "focus output $HS_OUT_VIEWER" >/dev/null
export "HS_NAME_$n=$name"
hs_start_client "$n" "$ws" "$HS_OUT_VIEWER"
