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
# Every step is driven from the rects the clients write to SSS_UI_DUMP ($SCRATCH/ui<n>.txt,
# client/src/ui/drive_dump.rs; names in README.md), mapped to global pixels with the window origin
# from `swaymsg -t get_tree`; every value is read back and the step FAILS if it did not take.
#
# Usage: SCRATCH=<rig scratch> hs-drive.sh STEP [ARGS...]
#   share                     sharer: click "Share screen" (portal auto-grants, no picker; SHARER=3 drives client 3 instead of 1, for steps share/setup/golive/stopshare);
#                             waits for the "Set up your stream" modal
#   setup FPS CODEC PRESET [KBPS]
#                             FPS one of 15 30 60 90 120 144 165 240; CODEC h264|av1|h265;
#                             PRESET text|quality|gaming. KBPS needs keyboard input
#                             (wtype) and is the least reliable part; prefer START_KBPS at `up`.
#   golive                    sharer: "Go live in <room>" (waits until enabled; exit 1 unless
#                             the modal closes)
#   discard                   sharer: click "Discard" (leftmost footer button of the staged modal): drops
#                             the staged capture and closes its portal session (repeatable share starts)
#   modal                     print the sharer's modal dump
#   mclick NAME [DX [DY]]     click a control of the open modal by its dumped rect (DX<0: from the right
#                             edge, `L60`: 60 pt from the left edge)
#   watch [T]                 viewer (VIEWER=N, default 2): click "Watch stream" on stage tile T (default 0)
#   hover [T]                 viewer: switch "Show stats on the stream" on for tile T and park the pointer on its
#                             name tag (tooltip shows "Decoded X fps / shown Y fps")
#   sample OUTDIR N SECS      viewer: N screenshots of the tooltip region every SECS s ->
#                             OUTDIR/tip-<i>.png (+ montage.png when ImageMagick is there)
#   stopshare                 sharer: stop sharing (Stream settings, leading footer button)
#   unwatch [T]               viewer: "Stop watching" in tile T's stream menu
#   shot FILE [OUTPUT]        grim -o OUTPUT (default HEADLESS-3 = the viewer)
#   crop FILE OUTPUT X Y W H  grim -g on OUTPUT-relative coordinates
#   click N X Y | move N X Y  pointer in client N's window coordinates (debugging)
#   rect N NAME               the dumped rect NAME of client N: x0 y0 x1 y1 (points, window-relative)
#   val N KEY                 the dumped value KEY of client N
#   await N KEY VALUE [SECS]  wait until the dumped KEY is VALUE (exit 1 if not within SECS, default 5)
#   uclick N NAME [DX [DY]]   click a dumped rect of client N (DX as for mclick); a `pw.` name is in the popped-out
#   udclick N NAME [DX [DY]]  stage window. udclick double-clicks, umove only moves the pointer there
#   uscroll N NAME NOTCHES    wheel notches over a dumped rect (> 0 wheel down, < 0 up)
#   drag N X1 Y1 X2 Y2 [HOLD_MS] [STEPS]
#                             a real pointer drag in client N's window coordinates (hs-vptr drag), staying pressed
#                             at the end for HOLD_MS; run it with & to look at the drag meanwhile
#   udrag N NAME DX DY [HOLD_MS] [STEPS]
#                             the same, from the centre of a dumped rect by (DX, DY) points
#   key N KEYS...             wtype to client N (focuses it first), e.g. `key 1 -M ctrl a -m ctrl`
#   relay-restart [DOWN_SECS] kill the relay and start it again DOWN_SECS later (default 0), same port and cert
#   tree                      sway window list

set -uo pipefail
SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SELF_DIR/hs-lib.sh"

hs_load_state
hs_export_env
hs_assert_private

STEP="${1:?STEP}"; shift

UI_DUMP="${UI_DUMP:-$SCRATCH/ui${SHARER:-1}.txt}"      # the sharer's (client 1, or client $SHARER)

# --- pointer / keys -------------------------------------------------------------------------
gmove()  { swaymsg "seat - cursor set $1 $2" >/dev/null; }
gclick() { gmove "$1" "$2"; sleep 0.15; swaymsg "seat - cursor press button1" >/dev/null; sleep 0.08; swaymsg "seat - cursor release button1" >/dev/null; }

# client N's window content origin in global pixels: "OX OY".
client_origin() {
  local pid; pid="$(hs_pid_of "client$1")"
  [[ -n "$pid" ]] || die "no client$1 in $HS_PIDS"
  hs_win_rect "$pid" | awk '{print $1, $2}'
}
# The same for client N's popped-out stage window (app id simple-screensharing-popout; the newest one).
popout_origin() {
  local pid; pid="$(hs_pid_of "client$1")"
  swaymsg -t get_tree | jq -r --argjson p "$pid" '[.. | objects | select((.type? == "con" or .type? == "floating_con") and .pid? == $p and .app_id? == "simple-screensharing-popout")] | last // empty | "\(.rect.x + .window_rect.x) \(.rect.y + .window_rect.y)"'
}
# click/move at window-relative coordinates of client N.
wclick() { local o; o="$(client_origin "$1")"; gclick $((${o% *} + $2)) $((${o#* } + $3)); }
wmove()  { local o; o="$(client_origin "$1")"; gmove  $((${o% *} + $2)) $((${o#* } + $3)); }
# The whole output layout "W H" (hs-vptr's absolute motion is a fraction of it).
layout_size() { swaymsg -t get_outputs | jq -r '[.[] | select(.active)] | "\(map(.rect.x + .rect.width) | max) \(map(.rect.y + .rect.height) | max)"'; }
# vdrag X1 Y1 X2 Y2 [HOLD_MS] [STEPS] in global pixels.
vdrag() { "$SCRATCH/bin/hs-vptr" drag $(layout_size) "$1" "$2" "$3" "$4" "${6:-20}" "${5:-0}"; }

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
# Global pixels of a point of client N's dumped rect NAME: `gpos N NAME [DX [DY]]` prints "X Y". DX: 0 = centre, `L60` =
# 60 pt from the left edge, < 0 = from the right edge, > 0 = centre + DX; DY: centre + DY. The dump file of client N;
# a `pw.` name is in the popped-out stage window.
gpos() {
  local n="$1" name="$2" dx="${3:-0}" dy="${4:-0}" r o ppp
  UI_DUMP="$SCRATCH/ui$n.txt"
  r="$(dump_rect "$name")"
  [[ -n "$r" ]] || die "the dump has no rect '$name' ($UI_DUMP)"
  if [[ "$name" == pw.* ]]; then o="$(popout_origin "$n")"; else o="$(client_origin "$n")"; fi
  [[ -n "$o" ]] || die "no window for '$name'"
  ppp="$(dump_get ppp)"
  awk -v r="$r" -v o="$o $ppp" -v dx="$dx" -v dy="$dy" 'BEGIN {
    split(r, a, " "); split(o, b, " ");
    if (dx ~ /^L/) { dx = substr(dx, 2) + 0; x = a[1] + dx }   # "L60": 60 pt from the left edge
    else { x = (a[1] + a[3]) / 2 + dx; if (dx < 0) x = a[3] + dx }
    y = (a[2] + a[4]) / 2 + dy;
    printf "%d %d\n", b[1] + x * b[3], b[2] + y * b[3] }'
}
uclick() { local p; p="$(gpos "$@")" && gclick $p; }
udclick() {
  local p; p="$(gpos "$@")" || return 1
  gclick $p; sleep 0.04
  swaymsg "seat - cursor press button1" >/dev/null; sleep 0.04; swaymsg "seat - cursor release button1" >/dev/null
}
umove()  { local p; p="$(gpos "$@")" && gmove $p; }
mclick() { uclick "${SHARER:-1}" "$@"; }
index_of() {
  local w="$1" i=0 x; shift
  for x in "$@"; do [[ "$x" == "$w" ]] && { echo "$i"; return; }; i=$((i + 1)); done
}
modal_open() { dump_wait open 1 15 || die "the sharer's stream-settings modal is not open (dump: $UI_DUMP)"; }

VIEWER="${VIEWER:-2}"
# The name tag and its tooltip below it, for `sample` (X Y W H in the viewer's window).
tip_geom() { UI_DUMP="$SCRATCH/ui$VIEWER.txt"; dump_rect "tag.${1:-0}" | awk '{ printf "%d %d 560 260\n", $1, $2 }'; }

case "$STEP" in
  share)
    uclick "${SHARER:-1}" ctl.share
    modal_open
    ;;
  setup)
    fps="${1:?FPS}"; codec="${2:?CODEC}"; preset="${3:?PRESET}"; kbps="${4:-}"
    case "$preset" in text|quality|gaming) ;; *) die "preset $preset? (text|quality|gaming)";; esac
    modal_open
    if [[ "$(dump_get fps)" != "$fps" ]]; then
      i="$(index_of "$fps" $(dump_get fps_options))"
      [[ -n "$i" ]] || die "fps $fps not in the dropdown ($(dump_get fps_options))"
      mclick fps; sleep 0.6
      mclick "opt.$i"
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
      swaymsg "[pid=$(hs_pid_of "client${SHARER:-1}")] focus" >/dev/null
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
  discard)
    modal_open
    [[ "$(dump_get mode)" == staged ]] || die "discard: the modal is not the staged one (mode $(dump_get mode))"
    mclick footer L70
    ;;
  modal) cat "$UI_DUMP" ;;
  mclick) modal_open; mclick "$@" ;;
  watch) uclick "$VIEWER" "watch.${1:-0}" L40 ;;
  hover)
    t="${1:-0}"
    umove "$VIEWER" "tile.$t"; sleep 0.5
    if [[ "$(UI_DUMP="$SCRATCH/ui$VIEWER.txt" dump_get "stats.$t")" != 1 ]]; then
      uclick "$VIEWER" "tool.$t.more"; sleep 0.4
      uclick "$VIEWER" menu.Show_stats_on_the_stream; sleep 0.3
      "$SELF_DIR/hs-drive.sh" key "$VIEWER" -k Escape; sleep 0.3
      umove "$VIEWER" "tile.$t"; sleep 0.5
    fi
    umove "$VIEWER" "tag.$t"; sleep 0.8 ;;
  sample)
    out="${1:?OUTDIR}"; n="${2:?N}"; secs="${3:?SECS}"
    mkdir -p "$out"
    for i in $(seq 1 "$n"); do
      read -r tx ty tw th <<<"$(tip_geom)"
      grim -g "$((HS_VIEWER_X + tx)),$((HS_VIEWER_Y + ty)) ${tw}x${th}" "$out/tip-$(printf %02d "$i").png"
      [[ "$i" -lt "$n" ]] && sleep "$secs"
    done
    command -v montage >/dev/null && montage "$out"/tip-*.png -tile 3x -geometry +4+4 -background '#222' "$out/montage.png" 2>/dev/null
    ;;
  stopshare)
    uclick "${SHARER:-1}" ctl.share; modal_open; sleep 0.3
    [[ "$(dump_get mode)" == live ]] || die "stopshare: the modal is not the live one (mode $(dump_get mode))"
    mclick footer L70
    dump_wait open 0 5 || die "stopshare: the modal did not close" ;;
  unwatch)
    t="${1:-0}"
    umove "$VIEWER" "tile.$t"; sleep 0.5; uclick "$VIEWER" "tool.$t.more"; sleep 0.4; uclick "$VIEWER" menu.Stop_watching ;;
  shot)      grim -o "${2:-$HS_OUT_VIEWER}" "${1:?FILE}" ;;
  crop)      # crop FILE OUTPUT X Y W H
    o="${2:?OUTPUT}"; read -r ox oy < <(swaymsg -t get_outputs | jq -r --arg o "$o" '.[] | select(.name==$o) | "\(.rect.x) \(.rect.y)"')
    grim -g "$((ox + $3)),$((oy + $4)) ${5}x${6}" "${1:?FILE}" ;;
  rect)      UI_DUMP="$SCRATCH/ui${1:?N}.txt"; dump_rect "${2:?NAME}" ;;
  val)       UI_DUMP="$SCRATCH/ui${1:?N}.txt"; dump_get "${2:?KEY}" ;;
  await)     UI_DUMP="$SCRATCH/ui${1:?N}.txt"; dump_wait "${2:?KEY}" "${3?VALUE}" "${4:-5}" || die "await: $2 is '$(dump_get "$2")', wanted '$3'" ;;
  uclick)    uclick "$@" ;;
  udclick)   udclick "$@" ;;
  umove)     umove "$@" ;;
  uscroll)   umove "$1" "$2"; sleep 0.2; "$SCRATCH/bin/hs-vptr" scroll "${3:?NOTCHES}" ;;
  drag)
    o="$(client_origin "${1:?N}")"; ox="${o% *}"; oy="${o#* }"
    vdrag $((ox + $2)) $((oy + $3)) $((ox + $4)) $((oy + $5)) "${6:-0}" "${7:-20}" ;;
  udrag)
    p="$(gpos "${1:?N}" "${2:?NAME}")" || exit 1
    read -r x y <<<"$p"; ppp="$(dump_get ppp)"
    vdrag "$x" "$y" "$(awk -v a="$x" -v d="${3:?DX}" -v p="$ppp" 'BEGIN { printf "%d", a + d * p }')" "$(awk -v a="$y" -v d="${4:?DY}" -v p="$ppp" 'BEGIN { printf "%d", a + d * p }')" "${5:-0}" "${6:-20}" ;;
  click)     wclick "$1" "$2" "$3" ;;
  move)      wmove "$1" "$2" "$3" ;;
  key)       n="${1:?N}"; shift; swaymsg "[pid=$(hs_pid_of "client$n")] focus" >/dev/null; hs_type "$@" ;;
  relay-restart)
    tmux respawn-pane -k -t "=$TMUX_SESSION:" -c "$SCRATCH/relay" "sleep ${1:-0}; exec '$RELAY_BIN' $PORT 2>>'$SCRATCH/relay.err'" ;;
  tree)      swaymsg -t get_tree | jq -r '.nodes[] | select(.type=="output") | .name as $o | [.. | objects | select(.type? == "con" and .pid? != null)] | .[] | "\($o)\t\(.name)\tpid \(.pid)\t\(.rect)"' ;;
  *) die "unknown step $STEP" ;;
esac
