#!/usr/bin/env bash
#
# LEGACY (2026-10-14): works on the user's REAL MangoWC desktop only (mmsg + ydotool (a uinput device that injects into the real session) + grim on real monitors). Agents must not
# use it: since the headless switch all real-app tests run in the private headless sway rig,
# see hs-rig.sh and superdoc/claude-instructions/testing-and-verification.md. Headless: hs-drive.sh.
#
# drive.sh — drive the two-client rig started by launch-test-tmux.sh with
# ydotool, to run a share -> watch -> measure scenario repeatably.
#
# The stream-settings modal (setup, golive) is driven from the app's own
# layout: every client the launch scripts start writes SSS_UI_DUMP
# (<scratch>/ui<n>.txt, see stream_settings.rs `drive_dump`) with the modal's
# control rects in window points and the values they hold. drive.sh maps them
# to global pixels with the window's position from `mmsg get all-clients`,
# clicks, then reads the value back and FAILS if it did not take (#47: the old
# fixed coordinates silently left 30 fps once the modal grew).
#
# The other steps (share, watch, hover, sample, stopshare, unwatch, spotify)
# still use fixed coordinates: both clients tiled side by side on DP-2 (the
# launch scripts' default: client 1 = sharer on the left, client 2 = viewer on
# the right), 1920x1080 monitor, windows 945 px wide, relative to the monitor's
# top-left (what a `grim -o DP-2` screenshot shows), read off such screenshots.
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
#                             Reads every value back; exits 1 if one did not take.
#   golive                    sharer: "Go live in <room>" (waits until it is
#                             enabled; exits 1 unless the modal closes)
#   modal                     print the sharer's modal dump (debugging)
#   mclick NAME [DX [DY]]     click a control of the sharer's open modal by its
#                             dumped rect (e.g. `mclick advanced`)
#   watch                     viewer: click "Watch" on the sharer's card
#   hover                     viewer: park the pointer on the tile's stats bar
#                             so the "Decoded X fps / shown Y fps" tooltip shows
#   sample OUTDIR N SECS      viewer: N cropped screenshots of the tooltip, one
#                             every SECS s -> OUTDIR/tip-<i>.png, plus
#                             OUTDIR/montage.png (read that one)
#   stopshare                 sharer: stop sharing (clicks the red Stop button)
#   unwatch                   viewer: close the watched tile (x on its stats row).
#                             (The viewer's watch ends by itself on StreamStopped, so
#                             this is only for closing a live tile.)
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
SCRATCH="${SCRATCH:-$CARGO_TARGET_DIR/launch-scratch/rig}"
UI_DUMP="${UI_DUMP:-$SCRATCH/ui1.txt}"                       # the sharer's (client 1)

# dump_get KEY: the value(s) after KEY in the sharer's modal dump.
dump_get() { awk -v k="$1" '$1==k { $1=""; sub(/^ /, ""); print; exit }' "$UI_DUMP" 2>/dev/null; }
# dump_rect NAME: "x0 y0 x1 y1" of the dumped rect NAME (window points).
dump_rect() { awk -v n="$1" '$1=="rect" && $2==n { print $3, $4, $5, $6; exit }' "$UI_DUMP" 2>/dev/null; }
# dump_wait KEY VALUE SECS: until the dump says KEY VALUE (fails after SECS).
dump_wait() {
  local i
  for i in $(seq 1 $(( $3 * 10 ))); do
    [[ "$(dump_get "$1")" == "$2" ]] && return 0
    sleep 0.1
  done
  return 1
}
# The sharer window's content origin in global pixels + its scale: "OX OY PPP".
# The window is found by the pid launch-lib.sh recorded first; a border (mmsg
# size minus the content size egui reports) is split evenly around the content.
sharer_origin() {
  local pid cl
  pid="$(head -1 "$SCRATCH/client.pids" 2>/dev/null)"
  [[ -n "$pid" ]] || die "no sharer pid in $SCRATCH/client.pids"
  cl="$(mmsg get all-clients | jq -r --argjson p "$pid" '.clients[] | select(.pid==$p) | "\(.x) \(.y) \(.width) \(.height)"' | head -1)"
  [[ -n "$cl" ]] || die "no window for sharer pid $pid"
  awk -v cl="$cl" -v sc="$(dump_get screen)" -v ppp="$(dump_get ppp)" 'BEGIN {
    split(cl, c, " "); split(sc, s, " ");
    printf "%d %d %s\n", c[1] + (c[3] - s[1] * ppp) / 2, c[2] + (c[4] - s[2] * ppp) / 2, ppp }'
}
# mclick NAME [DX [DY]]: click the centre of the dumped rect NAME, offset by
# DX/DY points (a negative DX counts from the rect's right edge instead).
mclick() {
  local r o gx gy
  r="$(dump_rect "$1")"
  [[ -n "$r" ]] || die "the modal dump has no rect '$1' ($UI_DUMP)"
  o="$(sharer_origin)" || exit 1
  read -r gx gy < <(awk -v r="$r" -v o="$o" -v dx="${2:-0}" -v dy="${3:-0}" 'BEGIN {
    split(r, a, " "); split(o, b, " ");
    x = (a[1] + a[3]) / 2 + dx; if (dx < 0) x = a[3] + dx;
    y = (a[2] + a[4]) / 2 + dy;
    printf "%d %d\n", b[1] + x * b[3], b[2] + y * b[3] }')
  "$SELF_DIR/ui-click.sh" click "$gx" "$gy"
}
# index_of WORD LIST...: 0-based position of WORD in LIST (empty if absent).
index_of() {
  local w="$1" i=0 x; shift
  for x in "$@"; do [[ "$x" == "$w" ]] && { echo "$i"; return; }; i=$((i + 1)); done
}
modal_open() { dump_wait open 1 10 || die "the sharer's stream-settings modal is not open (dump: $UI_DUMP)"; }

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
    case "$preset" in lowlat) preset=low_latency;; balanced|quality|gaming) ;; *) die "preset $preset?";; esac
    modal_open
    if [[ "$(dump_get fps)" != "$fps" ]]; then
      i="$(index_of "$fps" $(dump_get fps_options))"
      [[ -n "$i" ]] || die "fps $fps not in the dropdown ($(dump_get fps_options))"
      # The list opens under the select: first row centre 22 pt below the
      # select's bottom edge, then one row per 40 pt (32 pt row + 8 spacing).
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
      ydotool key 29:1 30:1 30:0 29:0 >/dev/null           # Ctrl+A
      ydotool type "$kbps" >/dev/null; ydotool key 28:1 28:0 >/dev/null
      dump_wait bitrate "$kbps" 3 || die "setup: bitrate is $(dump_get bitrate), wanted $kbps"
    fi
    log "setup: $(dump_get fps) fps, $(dump_get codec), $(dump_get preset), $(dump_get bitrate) kbps"
    ;;
  golive)
    modal_open
    [[ "$(dump_get mode)" == staged ]] || die "golive: the modal is not the staged one (mode $(dump_get mode))"
    dump_wait ready 1 20 || die "golive: Go live never became enabled (the capture is not active)"
    mclick footer -48                                      # rightmost footer button = Go live
    dump_wait open 0 5 || die "golive: the modal did not close (click missed?)"
    ;;
  modal) cat "$UI_DUMP" ;;
  mclick) modal_open; mclick "$@" ;;
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
