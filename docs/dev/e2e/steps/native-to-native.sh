# LEGACY (2026-10-14): real-desktop step (mmsg, ydotool, real portal picker, shared GUI lock). Sourced only by
# native-session.sh, which refuses without SSS_ALLOW_REAL_DESKTOP=1. Agents: do the native side in the headless rig
# (client/scripts/hs-rig.sh + hs-drive.sh, see docs/dev/README.md "Native side in the rig").
# Native shares DP-1 (showing moving test content in a headed Chromium) through the real portal picker.
MOTION=file://$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/motion.html
export EXTRA_DIRS=$OUT/chrome-motion
mmsg dispatch focusmon,$MOTION_MON >/dev/null
/usr/bin/chromium --user-data-dir=$EXTRA_DIRS --no-first-run --no-default-browser-check --disable-gpu-vsync --disable-frame-rate-limit --app=$MOTION >$OUT/motion.log 2>&1 9>&- &
EXTRA_PIDS=$!
sleep 4
mmsg get all-clients | python3 -c 'import sys,json; [print("client",c["id"],c["appid"],c["monitor"],c["x"],c["y"],c["width"],c["height"]) for c in json.load(sys.stdin)["clients"]]'
mmsg dispatch focusmon,$MON >/dev/null
echo "last-choice before: $(cat ~/.local/state/wlr-share-picker/last-choice.json)"
click 366 1040     # Share screen
for i in $(seq 1 40); do pgrep -f '[w]lr-share-picker' >/dev/null && break; sleep 0.25; done
echo "picker open: $(pgrep -f '[w]lr-share-picker' | wc -l)"
sleep 1.5
ydotool type dp; sleep 0.8
LC=$(cat ~/.local/state/wlr-share-picker/last-choice.json)
echo "last-choice (preselect): $LC"
ydotool key 28:1 28:0
for i in $(seq 1 40); do pgrep -f '[w]lr-share-picker' >/dev/null || break; sleep 0.25; done
echo "picker still open: $(pgrep -f '[w]lr-share-picker' | wc -l)"
echo "last-choice after: $(cat ~/.local/state/wlr-share-picker/last-choice.json)"
sleep 2.5

# --- stream setup modal: audio Spotify, fps, codec, go live ---
click 710 450; sleep 0.8; click 603 529; sleep 0.8
case "${FPS:-60}" in 60) click 1295 410; sleep 0.8; click 1145 529;; 120) click 1295 410; sleep 0.8; click 1147 609;; esac
sleep 0.6
case "${CODEC:-h264}" in h264) click 1171 490;; av1) click 1296 490;; h265) click 1422 490;; esac
sleep 0.6
shot setup-${LABEL:-x}
grim -o $MOTION_MON $OUT/motion-ref-${LABEL:-x}.png
click 1431 762     # Go live
sleep 4
# second native app as a viewer, on the monitor that shows the moving content
CFGB=$OUT/cfg-$TAG-B; rm -rf $CFGB; mkdir -p $CFGB/simple-screensharing; cp $CFG/simple-screensharing/rooms.toml $CFGB/simple-screensharing/
mmsg dispatch focusmon,$MOTION_MON >/dev/null
XDG_CONFIG_HOME=$CFGB ${NATIVE_BIN:?set NATIVE_BIN to the client binary} >$OUT/native-$TAG-B.log 2>&1 9>&- &
BPID=$!
EXTRA_PIDS="$EXTRA_PIDS $BPID"
sleep 9
mmsg get all-clients | python3 -c 'import sys,json; [print("client",c["id"],c["title"][:20],c["monitor"],c["x"],c["y"],c["width"],c["height"]) for c in json.load(sys.stdin)["clients"] if c["monitor"] in ("DP-1","DP-2")]'
case "$MOTION_MON" in DP-1) OXB=1920;; DP-2) OXB=3840;; esac
ydotool mousemove --absolute -x $((OXB + 1360)) -y $((1080 + 422 - 1080)); sleep 0.2; ydotool click 0xC0; sleep 8
move 0 0
grim -o $MOTION_MON $OUT/$TAG-B-watch.png
