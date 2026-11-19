#!/usr/bin/env bash
#
# hs-gamescope.sh - drive a NESTED gamescope inside the rig while it is shared directly (row 215): the "gamescope
# size change" check. gamescope runs as a window on HEADLESS-1 (--backend wayland), the sharer client shares its
# PipeWire node (SHARE_PICK=gamescope), the viewer (client 2) watches; the script resizes gamescope's window / toggles
# fullscreen and measures the longest FROZEN picture in the viewer's tile (a hash of a region of it every 0.1 s).
# Software encoding only (see testing-and-verification.md): bring the rig up with
#   CLIENT_ENV_1="LIBVA_DRIVERS_PATH=/nonexistent SSS_CAPTURE_PATH=system" CLIENT_ENV_2="<same>" hs-rig.sh up
#
# Usage: SCRATCH=<rig scratch> hs-gamescope.sh MODE ...
#   start [vkcube|still]     start gamescope (1280x720) on HEADLESS-1 and float it; vkcube = a moving picture, still = a zenity
#                            dialog (a picture that never changes by itself). Its PID is recorded in $SCRATCH/gs.pids.
#   stop                     kill only the recorded gamescope / dialog-loop PIDs
#   size WxH | full | window one change: resize gamescope's window / fullscreen on / off
#   repaint                  still scene only: close the dialog, the loop opens another (gamescope repaints)
#   run single N [SEED]      share, then N changes 3-8 s apart (a fixed list: 16:9, 4:3, fullscreen on/off, ...)
#   run burst  B [SEED]      share, then B bursts of 5-8 changes 0.2-1.5 s apart, 6 s apart
#   run stress SECS [SEED]   share, then SECS seconds of random sizes (16:9, 4:3, tiny, wide), singles and bursts mixed
#   run still  [STILL_SECS]  share a still picture, resize it, stay still STILL_SECS (default 90), repaint: must be on air and show it
#   run startstill [SECS]    a share STARTED on a still picture, left alone SECS (default 90), then a repaint
#   Each run starts its own gamescope, stops it at the end, and prints one line:
#   "RESULT <mode>: changes=.. longest_freeze=..s ... share=live errors=.. PASS|FAIL". Exit 1 on FAIL: the share is not
#   live at the end, a "share error" was logged, or the viewer's aspect does not follow the final size.
#   GS_SETTLE=SECS (default 6): how long a run keeps watching after its last change. GS_TILE="X,Y WxH": the sampled viewer region.
#   The viewer samples are kept in $SCRATCH/gs-view.log (epoch hash aspect), the changes in $SCRATCH/gs-changes.log.
# Needs a running rig (hs-rig.sh up) with gamescope, vkcube, zenity and grim installed.

set -uo pipefail
SELF_DIR="$(cd "$(dirname "$BASH_SOURCE")" && pwd)"
source "$SELF_DIR/hs-lib.sh"
hs_load_state
hs_export_env
hs_assert_private

GS_PIDS="$SCRATCH/gs.pids"; VIEW="$SCRATCH/gs-view.log"; CHANGES="$SCRATCH/gs-changes.log"
# a middle region of the viewer's first tile (client 2 on HEADLESS-3, global x 3840..)
TILE="${GS_TILE:-5000,450 200x160}"

gs_pid() { awk '$1=="gs" {p=$2} END{print p}' "$GS_PIDS" 2>/dev/null; }
gs_sway() { swaymsg "[pid=$(gs_pid)] $*" >/dev/null; }
stamp() { date +%s.%3N; }
change() { echo "$(stamp) $*" >>"$CHANGES"; echo "$(date +%T.%3N) gamescope -> $*"; }

start() {
    local scene="$1"; : >"$GS_PIDS"; rm -f "$SCRATCH/gs-loop.pid"
    local child=(vkcube --present_mode 2)
    # still: a loop showing one zenity dialog at a time; closing the dialog (repaint) opens the next one with new text
    [[ "$scene" == still ]] && child=(bash -c 'echo $$ >"$0"; n=0; while :; do n=$((n+1)); GDK_BACKEND=x11 zenity --info --text="still $n" --width 300 || sleep 1; done' "$SCRATCH/gs-loop.pid")
    # gamescope may unmap and remap its window (a still scene's dialog closing): a remapped window must come back floating at
    # its own size, not tiled (that would be a size change of its own)
    swaymsg 'for_window [app_id="gamescope"] floating enable, move position 100 100' >/dev/null
    setsid gamescope --backend wayland -W 1280 -H 720 -w 1280 -h 720 -r 60 -- "${child[@]}" >"$SCRATCH/gamescope.log" 2>&1 </dev/null &
    echo "gs $!" >>"$GS_PIDS"
    local i
    for i in $(seq 1 100); do swaymsg -t get_tree | jq -e --argjson p "$(gs_pid)" '[.. | objects | select(.pid? == $p)] | length > 0' >/dev/null && break; sleep 0.2; done
    gs_sway "move output $HS_OUT_CAP, floating enable, move position 100 100, resize set 1280 720"
    sleep 3
    echo "gamescope up (pid $(gs_pid), scene $scene)"
}
# end_share: stop the share while gamescope is still there (otherwise the client keeps restarting a capture of a node that is gone)
end_share() {
    local i
    for i in 1 2 3; do
        [[ "$(share_state)" == idle ]] && return 0
        SHARER=1 "$SELF_DIR/hs-drive.sh" stopshare >>"$SCRATCH/gs-end.log" 2>&1 || sleep 2
    done
}
stop() {
    local p
    for p in $(awk '{print $2}' "$GS_PIDS" 2>/dev/null) $(cat "$SCRATCH/gs-loop.pid" 2>/dev/null); do kill -TERM -- "-$p" 2>/dev/null || kill -TERM "$p" 2>/dev/null || true; done
    sleep 1; : >"$GS_PIDS"; rm -f "$SCRATCH/gs-loop.pid"
}
repaint() { local l z; l="$(cat "$SCRATCH/gs-loop.pid")"; z="$(pgrep -P "$l" zenity | head -1)"; [[ -n "$z" ]] && kill "$z"; change repaint; }
# do_size full | window | WxH; sets CUR to the size gamescope should now have
CUR=1280x720
do_size() {
    case "$1" in
        full) change fullscreen; gs_sway "fullscreen enable"; CUR=1920x1080 ;;
        window) change window; gs_sway "fullscreen disable"; CUR=${LAST_WIN:-1280x720} ;;
        *x*) change "$1"; gs_sway "fullscreen disable, resize set ${1%x*} ${1#*x}"; CUR="$1"; LAST_WIN="$1" ;;
    esac
}

# sampler: epoch hash aspect, every 0.1 s, until $SCRATCH/gs-view.stop appears
sampler() {
    rm -f "$SCRATCH/gs-view.stop"; : >"$VIEW"
    while [[ ! -e "$SCRATCH/gs-view.stop" ]]; do
        local h ar
        h="$(grim -g "$TILE" -t ppm - 2>/dev/null | md5sum | cut -c1-8)"
        ar="$(awk '$1=="tileinfo" && $2==0 {for(i=1;i<=NF;i++) if($i=="ar") {print $(i+1); exit}}' "$SCRATCH/ui2.txt" 2>/dev/null)"
        echo "$(stamp) $h ${ar:-?}" >>"$VIEW"; sleep 0.1
    done
}
# the longest run of identical hashes after FROM_EPOCH (a moving scene): "SECONDS START_EPOCH"
freeze_report() { awk -v from="$1" '$1>=from { if ($2==h) { if (n==0) s=$1; if ($1-s>m) { m=$1-s; at=s } n=1 } else { n=0 } h=$2 } END{ printf "%.1f %.0f\n", m+0.1, at }' "$VIEW"; }
share_state() { awk '$1=="share" {print $2; exit}' "$SCRATCH/ui1.txt" 2>/dev/null; }
errors_since() { tail -n +"$1" "$SCRATCH/client1.log" | grep -cE "share error|gave up|giving up"; }

begin_share() {
    LOG0="$(wc -l <"$SCRATCH/client1.log")"
    SHARE_PICK=gamescope "$SELF_DIR/hs-drive.sh" shareauto
    sleep 4; : >"$CHANGES"; T0="$(stamp)"
}
viewer_moving() { # within $1 s the viewer tile changes at least once
    local i a b; a="$(grim -g "$TILE" -t ppm - | md5sum)"
    for i in $(seq 1 $(( $1 * 5 ))); do b="$(grim -g "$TILE" -t ppm - | md5sum)"; [[ "$a" != "$b" ]] && return 0; sleep 0.2; done; return 1
}
# finish MODE: let things settle, stop the sampler, print the RESULT line; the viewer must be on air at CUR's aspect
finish() {
    local mode="$1" ar frz st err ok=0 W="${CUR%x*}" H="${CUR#*x}"
    sleep "${GS_SETTLE:-6}"; touch "$SCRATCH/gs-view.stop"; sleep 0.5
    local lastc lf; lastc="$(grep -v repaint "$CHANGES" | awk 'END{print $1+0}')"; lf="$(freeze_report "$lastc")"
    frz="$(freeze_report "$T0")"; [[ "$mode" == still* || "$mode" == startstill* ]] && { frz="n/a 0"; lf="n/a 0"; }
    ar="$(tail -1 "$VIEW" | awk '{print $3}')"; st="$(share_state)"; err="$(errors_since "$LOG0")"
    [[ "$st" == live && "$err" == 0 ]] || ok=1
    awk -v a="$ar" -v w="$W" -v h="$H" 'BEGIN{ if (a=="?" || a=="") exit 1; d=a-w/h; if (d<0) d=-d; exit (d < 0.06*w/h+0.02) ? 0 : 1 }' || ok=1
    echo "RESULT $mode: changes=$(grep -vc repaint "$CHANGES") longest_freeze=${frz% *}s (from epoch ${frz#* }) freeze_after_last_change=${lf% *}s final_viewer_aspect=$ar expected=$CUR share=${st:-?} errors=$err $([[ $ok == 0 ]] && echo PASS || echo FAIL)"
    return $ok
}

# the fixed single-change list: 16:9, 4:3, small, big, fullscreen on and off
SINGLES=(1600x900 960x540 800x600 full 1280x720 1920x1080 640x480 window 1000x700 full window 1400x1000 1280x720)
# rnd VAR LO HI: VAR = a seeded random number (no $(...): a subshell would not advance RANDOM)
rnd() { printf -v "$1" %d $(( $2 + RANDOM % ($3 - $2 + 1) )); }
sleep_ms() { sleep "$(awk "BEGIN{print $1/1000}")"; }

run() {
    local mode="$1"; shift; local rc=0 sp i k a b
    case "$mode" in
        single|burst|stress)
            local arg="${1:?N, bursts or SECS}"; RANDOM="${2:-1}"
            start vkcube; begin_share; viewer_moving 10 || { echo "the viewer shows no moving picture"; return 1; }
            sampler & sp=$!
            if [[ "$mode" == single ]]; then
                for ((i = 0; i < arg; i++)); do do_size "${SINGLES[$((i % ${#SINGLES[@]}))]}"; rnd a 3 8; sleep "$a"; done
            elif [[ "$mode" == burst ]]; then
                for ((i = 0; i < arg; i++)); do
                    rnd a 5 8
                    for ((k = a; k > 0; k--)); do rnd a 600 1900; rnd b 400 1000; do_size "${a}x$b"; rnd a 200 1500; sleep_ms "$a"; done
                    sleep 6
                done
            else
                local end=$((SECONDS + arg)) w
                while [[ $SECONDS -lt $end ]]; do
                    rnd a 0 5
                    case $a in
                        0) do_size full ;; 1) do_size window ;;
                        2) rnd a 800 1800; rnd b 450 1000; do_size "${a}x$b" ;;
                        3) rnd w 500 1700; do_size "${w}x$((w * 3 / 4))" ;;                  # 4:3
                        4) rnd a 200 500; rnd b 150 450; do_size "${a}x$b" ;;                # tiny
                        *) rnd a 900 1900; rnd b 200 400; do_size "${a}x$b" ;;               # wide
                    esac
                    if (( RANDOM % 3 == 0 )); then rnd a 200 1500; sleep_ms "$a"; else rnd a 3 7; sleep "$a"; fi
                done
                do_size 1000x600
            fi
            finish "$mode $arg" || rc=1; wait "$sp" 2>/dev/null; return $rc ;;
        still|startstill)
            local secs="${1:-90}"
            start still; begin_share; sampler & sp=$!; sleep 3
            if [[ "$mode" == still ]]; then do_size 1000x700; sleep 4; do_size 800x600; fi
            echo "still for $secs s ..."; sleep "$secs"
            echo "before the repaint: share=$(share_state) errors=$(errors_since "$LOG0")"
            local t; t="$(stamp)"; repaint; sleep 6
            local moved; moved="$(awk -v from="$t" '$1>=from && $2!=h {n++} {h=$2} END{print n+0}' "$VIEW")"
            echo "viewer picture changes after the repaint: $moved"
            finish "$mode ${secs}s" || rc=1; [[ "$moved" -ge 1 ]] || { echo "the viewer did not show the repainted picture"; rc=1; }
            wait "$sp" 2>/dev/null; return $rc ;;
        *) echo "usage: hs-gamescope.sh run single N [SEED] | burst B [SEED] | stress SECS [SEED] | still [SECS] | startstill [SECS]" >&2; return 2 ;;
    esac
}

case "${1:-}" in
    start) start "${2:-vkcube}" ;;
    stop) stop ;;
    size) do_size "${2:?WxH}" ;;
    full) do_size full ;;
    window) do_size window ;;
    repaint) repaint ;;
    run) shift; trap 'end_share; stop' EXIT; run "$@"; exit $? ;;
    *) sed -n '3,25p' "$BASH_SOURCE"; exit 2 ;;
esac
