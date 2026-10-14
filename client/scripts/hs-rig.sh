#!/usr/bin/env bash
#
# hs-rig.sh — THE way to test the native client: a private HEADLESS sway desktop with its own
# dbus session, PipeWire, xdg-desktop-portal(-wlr, no picker), a relay, a sharer client, a
# viewer client and a moving test pattern. Nothing touches the user's real outputs or input.
# Layout, env vars and the why: hs-lib.sh header and superdoc/claude-instructions/testing-and-verification.md.
#
# Usage:
#   hs-rig.sh up              build (release), bring everything up, print the join link
#   hs-rig.sh status          what is running, outputs, windows, ports
#   hs-rig.sh run -- CMD...   run CMD inside the rig's environment (grim, swaymsg, wtype, ...)
#   hs-rig.sh down [--purge]  stop everything this rig started, remove the private runtime dir
#                             (--purge also deletes the scratch dir: logs, configs, screenshots)
#
# One rig per SCRATCH dir (default <target>/hs-scratch/rig, i.e. one per worktree); set SCRATCH to
# run several. HS_PORT sets the relay UDP port (default: a random free one).

set -uo pipefail
SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SELF_DIR/hs-lib.sh"

cmd="${1:-}"; shift || true

rig_running() { [[ -f "$HS_PIDS" ]] && hs_alive sway; }

case "$cmd" in
  up)
    rig_running && die "a rig is already up for SCRATCH=$SCRATCH (hs-rig.sh down first, or use another SCRATCH)"
    for t in sway swaymsg grim jq tmux dbus-daemon pipewire wireplumber vkcube busctl wayland-scanner gcc; do
      command -v "$t" >/dev/null || die "missing tool: $t"
    done
    [[ -x /usr/lib/xdg-desktop-portal-wlr && -x /usr/lib/xdg-desktop-portal ]] || die "xdg-desktop-portal and xdg-desktop-portal-wlr (stock) are needed"
    : >"$HS_PIDS"
    RT="$(hs_rt)"
    PORT="${HS_PORT:-$(hs_free_port)}"   # launch-lib.sh pre-sets PORT=4433, so a separate variable
    RIG_OK=0
    trap 'if [[ $RIG_OK != 1 ]]; then echo "bring-up failed: tearing down (logs stay in $SCRATCH)" >&2; hs_kill_all; tmux kill-session -t "=${TMUX_SESSION:-none}" 2>/dev/null; rm -rf "$RT"; rm -f "$HS_STATE"; fi' EXIT
    hs_build
    [[ -e "$RT" ]] && rm -rf "$RT"
    mkdir -p "$RT" && chmod 700 "$RT"
    cat >"$HS_STATE" <<EOF
RT='$RT'
PORT='$PORT'
CLIENT_BIN='$CLIENT_BIN'
RELAY_BIN='$RELAY_BIN'
EOF
    hs_export_env
    hs_write_portal_conf
    # Each step dies (-> trap -> teardown) on failure.
    hs_start_sway
    hs_start_bus
    hs_start_pipewire
    hs_start_portal
    hs_start_vptr
    hs_start_pattern
    hs_start_relay
    echo "TMUX_SESSION='$TMUX_SESSION'" >>"$HS_STATE"
    echo "JOIN_STRING='$JOIN_STRING'" >>"$HS_STATE"
    # Sharer with the GStreamer stats tracer (gst-rate.py reads $SCRATCH/gst1.log), viewer plain.
    if [[ "${HS_TRACE:-1}" != 0 ]]; then
      rm -f "$SCRATCH/gst1.log"
      export CLIENT_ENV_1="${CLIENT_ENV_1:-GST_TRACERS=stats GST_DEBUG=GST_TRACER:7 GST_DEBUG_NO_COLOR=1 GST_DEBUG_FILE=$SCRATCH/gst1.log}"
    fi
    hs_start_client 1 2 "$HS_OUT_SHARER"
    hs_start_client 2 3 "$HS_OUT_VIEWER"
    RIG_OK=1
    hs_summary
    ;;

  status)
    hs_load_state
    hs_export_env
    echo "scratch: $SCRATCH   runtime dir: $RT   relay port: UDP $PORT   tmux: ${TMUX_SESSION:-?}"
    while read -r name pid; do
      if kill -0 "$pid" 2>/dev/null; then echo "  up    $name (pid $pid)"; else echo "  DEAD  $name (pid $pid)"; fi
    done <"$HS_PIDS"
    if [[ -S "$SWAYSOCK" ]]; then
      swaymsg -t get_outputs | jq -r '.[] | "  output \(.name) \(.current_mode.width)x\(.current_mode.height)@\(.current_mode.refresh/1000|floor)Hz at \(.rect.x),\(.rect.y)"'
      swaymsg -t get_tree | jq -r '.nodes[] | select(.type=="output") | .name as $o | [.. | objects | select(.type? == "con" and .pid? != null)] | .[] | "  window \(.name) (pid \(.pid)) on \($o), \(.rect.width)x\(.rect.height)"'
    else
      echo "  sway is not running"
    fi
    echo "  join link: ${JOIN_STRING:-?}"
    ;;

  run)
    [[ "${1:-}" == -- ]] && shift
    [[ $# -gt 0 ]] || die "usage: hs-rig.sh run -- CMD..."
    hs_load_state
    hs_export_env
    hs_assert_private
    exec "$@"
    ;;

  down)
    purge=0; [[ "${1:-}" == --purge ]] && purge=1
    [[ -f "$HS_STATE" ]] && source "$HS_STATE"
    RT="${RT:-$(hs_rt)}"
    log "stopping the rig (scratch $SCRATCH)"
    [[ -f "$SCRATCH/tmux.session" ]] && tmux kill-session -t "=$(<"$SCRATCH/tmux.session")" 2>/dev/null
    hs_kill_all
    # Only the private runtime dir this rig made (sockets + pipewire/wireplumber runtime files).
    [[ "$RT" == /run/user/*/sss-hs-* ]] && rm -rf "$RT"
    # A leftover guard: nothing of ours may still be running from this runtime dir.
    left="$(pgrep -af -- "$RT" 2>/dev/null | grep -v pgrep | cut -c1-140)"
    [[ -n "$left" ]] && { echo "WARNING: processes still referencing $RT:"; echo "$left"; }
    rm -f "$HS_STATE"
    [[ "$purge" == 1 ]] && rm -rf "$SCRATCH"
    log "down."
    ;;

  *) echo "usage: $0 up|status|run -- CMD...|down [--purge]" >&2; exit 2 ;;
esac
