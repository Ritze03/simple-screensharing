#!/usr/bin/env bash
#
# drive.sh — drive the two-client rig started by launch-test-tmux.sh with
# ydotool, to run a share -> watch -> measure scenario repeatably.
#
# GEOMETRY ASSUMPTION: both clients tiled side by side on DP-2 (the launch
# scripts' default: client 1 = sharer on the left, client 2 = viewer on the
# right), 1920x1080 monitor, windows 945 px wide. The UI coordinates below are
# relative to the monitor's top-left (what a `grim -o DP-2` screenshot shows) and
# were read off such screenshots; if the UI layout changes they need re-reading.
# Every click goes through ui-click.sh, which refuses points on HDMI outputs.
#
# Usage: drive.sh MONITOR STEP [ARGS...]   (MONITOR = the one the clients are on)
#   share [TARGET]            sharer: click "Share screen", answer the portal
#                             picker by typing "dp" + Enter after checking the
#                             preselected choice is TARGET (default DP-1)
#   setup FPS CODEC PRESET [KBPS]
#                             sharer's "Set up your stream" modal. FPS one of
#                             15 30 60 90 120 144 165 240; CODEC h264|av1|h265;
#                             PRESET lowlat|balanced|quality|gaming
#   golive                    sharer: "Go live in <room>"
#   watch                     viewer: click "Watch" on the sharer's card
#   hover                     viewer: park the pointer on the tile's stats bar
#                             so the "Decoded X fps / shown Y fps" tooltip shows
#   sample OUTDIR N SECS      viewer: N cropped screenshots of the tooltip, one
#                             every SECS s -> OUTDIR/tip-<i>.png, plus
#                             OUTDIR/montage.png (read that one)
#   stopshare                 sharer: stop sharing (clicks the red Stop button)
#   unwatch                   viewer: close the watched tile (x on its stats row).
#                             Needed after the sharer stops: the viewer keeps a frozen
#                             0 fps tile of the ended stream (bug), and the next
#                             share then shows up as a second card instead.
#   shot FILE                 screenshot of MONITOR (grim -o MONITOR)

set -uo pipefail
SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SELF_DIR/launch-lib.sh"

MON="${1:?MONITOR}"; STEP="${2:?STEP}"; shift 2
sss_check_monitors "$MON" || exit 1
read -r MX MY MW MH <<<"$(sss_monitor_rect "$MON")"
click() { "$SELF_DIR/ui-click.sh" click $((MX + $1)) $((MY + $2)); }
move()  { "$SELF_DIR/ui-click.sh" move  $((MX + $1)) $((MY + $2)); }

LASTCHOICE="$HOME/.local/state/wlr-share-picker/last-choice.json"

case "$STEP" in
  share)
    target="${1:-DP-1}"
    mmsg dispatch focusmon,"$MON" >/dev/null
    click 366 1040                                        # "Share screen" (sharer, left window)
    # The picker can take well over 6 s to appear (seen once: ~20 s), hence the long wait.
    "$SELF_DIR/picker-answer.sh" "$target" 45 || exit 1
    sleep 2
    ;;
  setup)
    fps="${1:?FPS}"; codec="${2:?CODEC}"; preset="${3:?PRESET}"; kbps="${4:-}"
    case "$fps" in 15) y=448;; 30) y=488;; 60) y=528;; 90) y=568;; 120) y=608;; 144) y=648;; 165) y=688;; 240) y=728;; *) die "fps $fps not in the dropdown";; esac
    click 760 410; sleep 0.6; click 670 "$y"; sleep 0.5
    case "$codec" in h264) cx=675;; av1) cx=762;; h265) cx=850;; *) die "codec $codec?";; esac
    click "$cx" 490; sleep 0.4
    case "$preset" in lowlat) px=663;; balanced) px=729;; quality) px=795;; gaming) px=861;; *) die "preset $preset?";; esac
    click "$px" 584; sleep 0.4
    if [[ -n "$kbps" ]]; then
      click 873 450; sleep 0.2
      ydotool key 29:1 30:1 30:0 29:0 >/dev/null           # Ctrl+A
      ydotool type "$kbps" >/dev/null; ydotool key 28:1 28:0 >/dev/null; sleep 0.4
    fi
    ;;
  golive)  click 816 762 ;;
  watch)   click 1360 421 ;;
  hover)
    move 1300 300; sleep 0.4; move 1330 128; sleep 0.8 ;;
  sample)
    out="${1:?OUTDIR}"; n="${2:?N}"; secs="${3:?SECS}"
    mkdir -p "$out"
    for i in $(seq 1 "$n"); do
      grim -g "$((MX + 1255)),$((MY + 112)) 340x125" "$out/tip-$(printf %02d "$i").png"
      [[ "$i" -lt "$n" ]] && sleep "$secs"
    done
    montage "$out"/tip-*.png -tile 3x -geometry +4+4 -background '#222' "$out/montage.png" 2>/dev/null
    ;;
  stopshare) click 900 1040 ;;
  unwatch)   move 1300 300; sleep 0.5; click 1638 134 ;;
  spotify)  # sharer's setup modal: Audio dropdown -> "Spotify" (2nd entry after "No audio")
    click 290 450; sleep 0.8; click 240 528; sleep 1 ;;
  crop)     # crop FILE X Y W H: monitor-relative region (never use while the picker is open)
    grim -g "$((MX + $2)),$((MY + $3)) ${4}x${5}" "${1:?FILE}" ;;
  shot)    grim -o "$MON" "${1:?FILE}" ;;
  *) die "unknown step $STEP" ;;
esac
