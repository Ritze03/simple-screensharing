#!/usr/bin/env bash
#
# hs-drive.sh — drive the two clients of the HEADLESS rig (hs-rig.sh up) through a share ->
# watch -> measure scenario. Headless counterpart of drive.sh: same steps, but pointer input
# goes through sway's own seat (`swaymsg seat - cursor set X Y` / `cursor press|release button1`,
# which only works because hs-vptr gave the seat a pointer capability), keys through wtype, and
# screenshots through grim -o HEADLESS-n. No ydotool (a uinput device would inject into the
# user's REAL session), no mmsg, no picker (xdpw runs with chooser_type=none).
#
# Layout (hs-lib.sh): sharer = client 1 fullscreen on HEADLESS-2 (global x 1920..3839), viewer =
# client 2 fullscreen on HEADLESS-3 (x 3840..5759), the captured test pattern on HEADLESS-1.
#
# The stream-settings modal (setup, golive) is driven from the rects the sharer writes to
# SSS_UI_DUMP ($SCRATCH/ui1.txt, stream_settings.rs `drive_dump`), mapped to global pixels with
# the window origin from `swaymsg -t get_tree`; every value is read back and the step FAILS if
# it did not take. The other steps (share, watch, hover, stopshare, unwatch) use fixed window
# coordinates read off screenshots of the 1920x1080 fullscreen windows (below).
#
# Usage: SCRATCH=<rig scratch> hs-drive.sh STEP [ARGS...]
#   share                     sharer: click "Share screen" (portal auto-grants, no picker);
#                             waits for the "Set up your stream" modal
#   setup FPS CODEC PRESET [KBPS]
#                             FPS one of 15 30 60 90 120 144 165 240; CODEC h264|av1|h265;
#                             PRESET lowlat|balanced|quality|gaming. KBPS needs keyboard input
#                             (wtype) and is the least reliable part; prefer START_KBPS at `up`.
#   golive                    sharer: "Go live in <room>" (waits until enabled; exit 1 unless
#                             the modal closes)
#   modal                     print the sharer's modal dump
#   mclick NAME [DX [DY]]     click a control of the open modal by its dumped rect
#   watch                     viewer: click "Watch" on the sharer's card
#   hover                     viewer: park the pointer on the tile's stats bar (tooltip shows
#                             "Decoded X fps / shown Y fps")
#   sample OUTDIR N SECS      viewer: N screenshots of the tooltip region every SECS s ->
#                             OUTDIR/tip-<i>.png (+ montage.png when ImageMagick is there)
#   stopshare                 sharer: stop sharing
#   unwatch                   viewer: close the watched tile
#   shot FILE [OUTPUT]        grim -o OUTPUT (default HEADLESS-3 = the viewer)
#   crop FILE OUTPUT X Y W H  grim -g on OUTPUT-relative coordinates
#   click N X Y | move N X Y  pointer in client N's window coordinates (debugging)
#   key N KEYS...             wtype to client N (focuses it first), e.g. `key 1 -M ctrl a -m ctrl`
#   tree                      sway window list

set -uo pipefail
SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SELF_DIR/hs-lib.sh"

hs_load_state
hs_export_env
hs_assert_private

STEP="${1:?STEP}"; shift

UI_DUMP="${UI_DUMP:-$SCRATCH/ui1.txt}"      # the sharer's (client 1)

# --- pointer / keys -------------------------------------------------------------------------
gmove()  { swaymsg "seat - cursor set $1 $2" >/dev/null; }
gclick() { gmove "$1" "$2"; sleep 0.15; swaymsg "seat - cursor press button1" >/dev/null; sleep 0.08; swaymsg "seat - cursor release button1" >/dev/null; }

# client N's window content origin in global pixels: "OX OY".
client_origin() {
  local pid; pid="$(hs_pid_of "client$1")"
  [[ -n "$pid" ]] || die "no client$1 in $HS_PIDS"
  hs_win_rect "$pid" | awk '{print $1, $2}'
}
# click/move at window-relative coordinates of client N.
wclick() { local o; o="$(client_origin "$1")"; gclick $((${o% *} + $2)) $((${o#* } + $3)); }
wmove()  { local o; o="$(client_origin "$1")"; gmove  $((${o% *} + $2)) $((${o#* } + $3)); }

# --- modal dump (same helpers as drive.sh) --------------------------------------------------
dump_get()  { awk -v k="$1" '$1==k { $1=""; sub(/^ /, ""); print; exit }' "$UI_DUMP" 2>/dev/null; }
dump_rect() { awk -v n="$1" '$1=="rect" && $2==n { print $3, $4, $5, $6; exit }' "$UI_DUMP" 2>/dev/null; }
dump_wait() {
  local i
  for i in $(seq 1 $(( $3 * 10 ))); do
    [[ "$(dump_get "$1")" == "$2" ]] && return 0
    sleep 0.1
  done
  return 1
}
# "OX OY PPP" of the sharer window (points -> global pixels).
sharer_origin() {
  local o; o="$(client_origin 1)" || return 1
  echo "$o $(dump_get ppp)"
}
mclick() {
  local r o gx gy
  r="$(dump_rect "$1")"
  [[ -n "$r" ]] || die "the modal dump has no rect '$1' ($UI_DUMP)"
  o="$(sharer_origin)"
  read -r gx gy < <(awk -v r="$r" -v o="$o" -v dx="${2:-0}" -v dy="${3:-0}" 'BEGIN {
    split(r, a, " "); split(o, b, " ");
    x = (a[1] + a[3]) / 2 + dx; if (dx < 0) x = a[3] + dx;
    y = (a[2] + a[4]) / 2 + dy;
    printf "%d %d\n", b[1] + x * b[3], b[2] + y * b[3] }')
  gclick "$gx" "$gy"
}
index_of() {
  local w="$1" i=0 x; shift
  for x in "$@"; do [[ "$x" == "$w" ]] && { echo "$i"; return; }; i=$((i + 1)); done
}
modal_open() { dump_wait open 1 15 || die "the sharer's stream-settings modal is not open (dump: $UI_DUMP)"; }

# Fixed window coordinates (1920x1080 fullscreen clients, ppp 1), read off screenshots:
# (measured 2026-10-14 on the 1920x1080 fullscreen windows; override with the env vars if the UI moves)
SHARE_BTN="${SHARE_BTN:-354 1052}"     # sharer: "Share screen" in the bottom bar
STOP_BTN="${STOP_BTN:-1838 1052}"      # sharer: red "Stop sharing" in the bottom bar
WATCH_BTN="${WATCH_BTN:-377 371}"      # viewer: "Watch" on the sharer's card
HOVER_XY="${HOVER_XY:-433 123}"        # viewer: the tile's "client-1 1920x1080 235fps 8.0 Mbps" pill
UNWATCH_XY="${UNWATCH_XY:-1148 122}"   # viewer: the x on the tile
TIP_GEOM="${TIP_GEOM:-280 90 560 260}" # viewer: region holding the pill and its tooltip (X Y W H)

case "$STEP" in
  share)
    wclick 1 $SHARE_BTN
    modal_open
    ;;
  setup)
    fps="${1:?FPS}"; codec="${2:?CODEC}"; preset="${3:?PRESET}"; kbps="${4:-}"
    case "$preset" in lowlat) preset=low_latency;; balanced|quality|gaming) ;; *) die "preset $preset?";; esac
    modal_open
    if [[ "$(dump_get fps)" != "$fps" ]]; then
      i="$(index_of "$fps" $(dump_get fps_options))"
      [[ -n "$i" ]] || die "fps $fps not in the dropdown ($(dump_get fps_options))"
      # The list opens under the select: first row centre 22 pt below the select's bottom edge,
      # then one row per 40 pt (32 pt row + 8 spacing).
      read -r _ y0 _ y1 <<<"$(dump_rect fps)"
      mclick fps; sleep 0.6
      mclick fps 1 $(( y1 + 22 + 40 * i - (y0 + y1) / 2 ))
      dump_wait fps "$fps" 3 || die "setup: framerate is $(dump_get fps), wanted $fps"
    fi
    if [[ "$(dump_get codec)" != "$codec" ]]; then
      i="$(index_of "$codec" $(dump_get codec_order))"
      [[ -n "$i" ]] || die "codec $codec? ($(dump_get codec_order))"
      mclick "codec.$i"
      dump_wait codec "$codec" 3 || die "setup: codec is $(dump_get codec), wanted $codec (greyed out = this machine can't encode it)"
    fi
    if [[ "$(dump_get preset)" != "$preset" ]]; then
      i="$(index_of "$preset" $(dump_get preset_order))"
      [[ -n "$i" ]] || die "preset $preset? ($(dump_get preset_order))"
      mclick "preset.$i"
      dump_wait preset "$preset" 3 || die "setup: preset is $(dump_get preset), wanted $preset"
    fi
    if [[ -n "$kbps" && "$(dump_get bitrate)" != "$kbps" ]]; then
      [[ -n "$(dump_get bitrate)" ]] || die "setup: no bitrate slider (absolute bandwidth mode?)"
      mclick bitrate -36; sleep 0.2                         # the row's 72 pt value box
      swaymsg "[pid=$(hs_pid_of client1)] focus" >/dev/null
      hs_type -M ctrl a -m ctrl "$kbps" -k Return
      dump_wait bitrate "$kbps" 3 || die "setup: bitrate is $(dump_get bitrate), wanted $kbps (wtype: keyboard capability/keymap issue? use START_KBPS at rig up)"
    fi
    log "setup: $(dump_get fps) fps, $(dump_get codec), $(dump_get preset), $(dump_get bitrate) kbps"
    ;;
  golive)
    modal_open
    [[ "$(dump_get mode)" == staged ]] || die "golive: the modal is not the staged one (mode $(dump_get mode))"
    dump_wait ready 1 30 || die "golive: Go live never became enabled (the capture is not active)"
    mclick footer -48                                      # rightmost footer button = Go live
    dump_wait open 0 5 || die "golive: the modal did not close (click missed?)"
    ;;
  modal) cat "$UI_DUMP" ;;
  mclick) modal_open; mclick "$@" ;;
  watch) wclick 2 $WATCH_BTN ;;
  hover)
    wmove 2 900 500; sleep 0.4; wmove 2 $HOVER_XY; sleep 0.8 ;;
  sample)
    out="${1:?OUTDIR}"; n="${2:?N}"; secs="${3:?SECS}"
    mkdir -p "$out"
    for i in $(seq 1 "$n"); do
      read -r tx ty tw th <<<"$TIP_GEOM"
      grim -g "$((HS_VIEWER_X + tx)),$((HS_VIEWER_Y + ty)) ${tw}x${th}" "$out/tip-$(printf %02d "$i").png"
      [[ "$i" -lt "$n" ]] && sleep "$secs"
    done
    command -v montage >/dev/null && montage "$out"/tip-*.png -tile 3x -geometry +4+4 -background '#222' "$out/montage.png" 2>/dev/null
    ;;
  stopshare) wclick 1 $STOP_BTN ;;
  unwatch)   wmove 2 900 500; sleep 0.5; wclick 2 $UNWATCH_XY ;;
  shot)      grim -o "${2:-$HS_OUT_VIEWER}" "${1:?FILE}" ;;
  crop)      # crop FILE OUTPUT X Y W H
    o="${2:?OUTPUT}"; read -r ox oy < <(swaymsg -t get_outputs | jq -r --arg o "$o" '.[] | select(.name==$o) | "\(.rect.x) \(.rect.y)"')
    grim -g "$((ox + $3)),$((oy + $4)) ${5}x${6}" "${1:?FILE}" ;;
  click)     wclick "$1" "$2" "$3" ;;
  move)      wmove "$1" "$2" "$3" ;;
  key)       n="${1:?N}"; shift; swaymsg "[pid=$(hs_pid_of "client$n")] focus" >/dev/null; hs_type "$@" ;;
  tree)      swaymsg -t get_tree | jq -r '.nodes[] | select(.type=="output") | .name as $o | [.. | objects | select(.type? == "con" and .pid? != null)] | .[] | "\($o)\t\(.name)\tpid \(.pid)\t\(.rect)"' ;;
  *) die "unknown step $STEP" ;;
esac
