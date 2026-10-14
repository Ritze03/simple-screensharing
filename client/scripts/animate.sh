#!/usr/bin/env bash
#
# animate.sh — run a fullscreen high-frame-rate animation on one monitor so a
# portal screen capture of that monitor has content changing at the refresh
# rate. An idle monitor delivers no frames to the portal (capture is
# damage-driven), so you cannot measure 240 fps capture without this.
#
# Usage:
#   animate.sh start [MONITOR]   # default DP-1; runs detached, prints the window
#   animate.sh stop              # kills the animation this script started
#
# Refuses HDMI outputs. Default animation: `vkcube` (GPU, Wayland WSI, MAILBOX
# present mode = renders unthrottled, the compositor shows the newest frame at
# every refresh, so the captured output has new content at the monitor's full
# refresh rate), made fullscreen with mmsg. ANIM_KIND=gst uses gst-launch
# videotestsrc pattern=ball -> waylandsink instead, but that is CPU-bound at only
# ~90 fps for 1080p (fpsdisplaysink measured 90 fps on this machine), so it
# cannot exercise 120/240 fps capture; it is kept as a fallback for machines
# without vulkan.
# Env: ANIM_KIND (vkcube|gst), ANIM_W/ANIM_H (1920/1080), ANIM_PIDFILE.

set -uo pipefail
SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SELF_DIR/launch-lib.sh"

PIDFILE="${ANIM_PIDFILE:-${XDG_RUNTIME_DIR:-/tmp}/sss-animate.pid}"
W="${ANIM_W:-1920}"
H="${ANIM_H:-1080}"
KIND="${ANIM_KIND:-vkcube}"

case "${1:-start}" in
  start)
    mon="${2:-DP-1}"
    sss_check_monitors "$mon" || exit 1
    prev="$(sss_focused_monitor)"
    mmsg dispatch focusmon,"$mon" >/dev/null
    sleep 0.3
    if [[ "$KIND" == vkcube ]]; then
      setsid vkcube --wsi wayland --present_mode 1 --width "$W" --height "$H" --suppress_popups 9>&- >"${PIDFILE%.pid}.log" 2>&1 &
    else
      setsid gst-launch-1.0 videotestsrc pattern=ball \
        ! "video/x-raw,width=$W,height=$H,framerate=300/1" \
        ! waylandsink fullscreen=true sync=false 9>&- >"${PIDFILE%.pid}.log" 2>&1 &
    fi
    echo $! >"$PIDFILE"
    sleep 2
    id="$(sss_client_id_for_pid "$(cat "$PIDFILE")")"
    if [[ "$KIND" == vkcube ]]; then
      [[ -n "$id" ]] || die "vkcube window did not appear"
      if [[ "$(sss_client_monitor "$id")" != "$mon" ]]; then
        kill -TERM -- "-$(cat "$PIDFILE")"
        die "vkcube opened on $(sss_client_monitor "$id"), not $mon; killed it."
      fi
      mmsg dispatch togglefullscreen client,"$id" >/dev/null
      sleep 1
    fi
    [[ -n "$prev" ]] && mmsg dispatch focusmon,"$prev" >/dev/null
    echo "animation pid $(cat "$PIDFILE") (log ${PIDFILE%.pid}.log); window:"
    mmsg get all-clients | jq -c --argjson p "$(cat "$PIDFILE")" '.clients[] | select(.pid==$p) | {id,monitor,is_fullscreen,x,y,width,height}'
    ;;
  stop)
    [[ -f "$PIDFILE" ]] && kill -TERM -- "-$(cat "$PIDFILE")" 2>/dev/null
    rm -f "$PIDFILE"
    ;;
  *) echo "usage: $0 start [MONITOR] | stop" >&2; exit 2 ;;
esac
