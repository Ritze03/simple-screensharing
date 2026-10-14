#!/usr/bin/env bash
#
# LEGACY (2026-10-14): works on the user's REAL MangoWC desktop only (puts the clients on real monitors via mmsg). Agents must not
# use it: since the headless switch all real-app tests run in the private headless sway rig,
# see hs-rig.sh and superdoc/claude-instructions/testing-and-verification.md. Headless: hs-rig.sh.
#
# rig.sh — bring the two-client measurement rig up or down: relay in tmux, a
# SHARER (client 1) started with the GStreamer stats tracer writing
# <scratch>/gst1.log (read by gst-rate.py), a VIEWER (client 2), both tiled on
# DP-2, scratch configs, pre-joined to the test room.
#
# Usage: rig.sh up [SCRATCH]     (default SCRATCH: <target>/launch-scratch/rig)
#        rig.sh down [SCRATCH]
# Thin wrapper over launch-test-tmux.sh / stop-launch.sh. Other env (PORT,
# PROFILE, CLIENT_MONITORS, ...) passes through.

set -uo pipefail
SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SELF_DIR/launch-lib.sh"
cmd="${1:?up|down}"
SCRATCH="${2:-$CARGO_TARGET_DIR/launch-scratch/rig}"

case "$cmd" in
  up)
    mkdir -p "$SCRATCH"
    rm -f "$SCRATCH/gst1.log"
    export SCRATCH
    # Own, uniquely named relay session (launch-test-tmux.sh would default to the shared `sssrelay`);
    # recorded so `down` stops exactly this one.
    TMUX_SESSION="sssrig-$(basename "$SCRATCH")-$$-$RANDOM"
    export TMUX_SESSION
    echo "$TMUX_SESSION" >"$SCRATCH/tmux.session"
    ATTACH=0 \
    CLIENT_ENV_1="GST_TRACERS=stats GST_DEBUG=GST_TRACER:7 GST_DEBUG_NO_COLOR=1 GST_DEBUG_FILE=$SCRATCH/gst1.log" \
      "$REPO_ROOT/launch-test-tmux.sh"
    ;;
  down)
    "$SELF_DIR/stop-launch.sh" "$SCRATCH"
    ;;
  *) echo "usage: $0 up|down [SCRATCH]" >&2; exit 2 ;;
esac
