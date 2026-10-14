#!/usr/bin/env bash
#
# hs-probe.sh — run portal-probe.py against the HEADLESS rig's private portal: its own
# ScreenCast session on HEADLESS-1 (the test pattern), granted with no picker, pipewiresrc rate
# into a chosen tail. Isolates "portal/compositor produces N fps" from the client's pipeline.
# Headless counterpart of probe-run.sh.
#
# Usage: SCRATCH=<rig scratch> hs-probe.sh [SECS] [TAIL] [EXTRA portal-probe.py ARGS...]
#   default TAIL: "video/x-raw ! fakesink sync=false". The caps filter matters: with a bare
#   `fakesink` pipewiresrc negotiates ANY caps, its PipeWire node gets media.class
#   Stream/Input/Unknown and WirePlumber refuses to link it to the Video/Source ("target not
#   found"). Default --src is "min-buffers=8" (see stream-control.md: the 2-buffer default pool
#   halves the rate once anything holds a buffer).
# Needs a running rig with HS_PATTERN=vkcube (an idle output delivers no frames).

set -uo pipefail
SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SELF_DIR/hs-lib.sh"
hs_load_state
hs_export_env
hs_assert_private

secs="${1:-8}"
tail_="${2:-video/x-raw ! fakesink sync=false}"
shift 2 2>/dev/null || shift $#
exec python3 -I "$SELF_DIR/portal-probe.py" --secs "$secs" --tail "$tail_" --src "${SRC_PROPS:-min-buffers=8}" "$@"
