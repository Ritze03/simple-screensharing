#!/usr/bin/env bash
#
# LEGACY (2026-10-14): works on the user's REAL MangoWC desktop only (ydotool pointer moves/clicks on real outputs). Agents must not
# use it: since the headless switch all real-app tests run in the private headless sway rig,
# see hs-rig.sh and superdoc/claude-instructions/testing-and-verification.md. Headless: swaymsg seat - cursor, see hs-drive.sh.
#
# ui-click.sh — move the pointer to a GLOBAL layout coordinate with ydotool and
# optionally click. Hides this desktop's ydotool quirk (absolute y is offset by
# -1080) and refuses any point that lies inside an HDMI output's rectangle.
#
# Usage:
#   ui-click.sh move  GX GY     # just move
#   ui-click.sh click GX GY     # move, then left click (press+release)
#   ui-click.sh dbl   GX GY     # double click
# GX/GY are global compositor coordinates (a grim screenshot of monitor M at
# (mx,my) puts pixel (px,py) at global (mx+px, my+py)).
# Env: YDOTOOL_Y_OFFSET (default 1080), YDOTOOL_SOCKET (ydotool's own).

set -uo pipefail
SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SELF_DIR/launch-lib.sh"

mode="${1:?move|click|dbl}"; gx="${2:?GX}"; gy="${3:?GY}"
off="${YDOTOOL_Y_OFFSET:-1080}"

layout="$(sss_monitors)" || die "cannot read the monitor layout"
inside_ok=0
while read -r name x y w h; do
  (( gx >= x && gx < x + w && gy >= y && gy < y + h )) || continue
  [[ "$name" =~ ^[Hh][Dd][Mm][Ii] ]] && die "point ($gx,$gy) is on $name; refusing."
  inside_ok=1
done <<<"$layout"
(( inside_ok )) || die "point ($gx,$gy) is outside every monitor; refusing."

ydotool mousemove --absolute -x "$gx" -y $((gy - off)) >/dev/null
sleep 0.15
case "$mode" in
  move) ;;
  click) ydotool click 0xC0 >/dev/null ;;
  dbl) ydotool click 0xC0 >/dev/null; sleep 0.08; ydotool click 0xC0 >/dev/null ;;
  *) die "unknown mode $mode" ;;
esac
