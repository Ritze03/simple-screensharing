#!/usr/bin/env bash
# usage: native-session.sh <join-link> <tag> <script-file>
# One short locked GUI session: native app on DP-2 with a scratch config seeded with the room (auto-connect), then run
# the step script (bash, sourced) which can use: shot <name>, click X Y (absolute global coords), key..., sleep.
LINK=$1; TAG=$2; STEPS=$3
case "$LINK" in @*) LINK=$(cat "${LINK#@}");; esac
exec 9>/tmp/claude-1000/sss-gui.lock && flock 9
set -u
OUT=${E2E_OUT:-/tmp/sss-e2e}
MON=${NATIVE_MON:-DP-2}
MOTION_MON=
if [ "${SHARE_TEST:-}" = 1 ]; then
  # the portal picker preselects its last choice: put the moving content on that monitor and the native app on the other one
  LCID=$(python3 -c 'import json,os; print(json.load(open(os.path.expanduser("~/.local/state/wlr-share-picker/last-choice.json")))["id"])')
  case "$LCID" in DP-1) MON=DP-2; MOTION_MON=DP-1;; DP-2) MON=DP-1; MOTION_MON=DP-2;; *) echo "ABORT: picker last choice is $LCID"; exit 1;; esac
fi
case "$MON" in DP-1) OX=1920;; DP-2) OX=3840;; *) echo "bad monitor"; exit 1;; esac
OY=1080
CFG=$OUT/cfg-$TAG
rm -rf $CFG; mkdir -p $CFG/simple-screensharing
[ -n "${NOROOM:-}" ] || cat > $CFG/simple-screensharing/rooms.toml <<EOF
[[rooms]]
name = "E2E"
join_string = "$LINK"
auto_connect = true
EOF
[ -n "${EXTRA_SETTINGS:-}" ] && printf '%s\n' "$EXTRA_SETTINGS" > $CFG/simple-screensharing/settings.toml
export XDG_CONFIG_HOME=$CFG
monitors=$(mmsg get all-monitors | python3 -c 'import sys,json; print([ (m["name"],m["x"],m["y"]) for m in json.load(sys.stdin)["monitors"]])')
echo "monitors: $monitors"
PREV=$(mmsg get focusing-client | python3 -c 'import sys,json; print(json.load(sys.stdin).get("id"))')
mmsg dispatch focusmon,$MON >/dev/null
RUST_LOG=${RUST_LOG:-info} ${NATIVE_BIN:?set NATIVE_BIN to the client binary} >$OUT/native-$TAG.log 2>&1 9>&- &
PID=$!
T0=$(date +%s)
kill_my_loopbacks() {
  # pw-loopback taps the native app spawns (ssshare-loopback-*) outlive a killed client and would keep fd 9 (the GUI lock)
  # open; kill only orphans (ppid 1) that appeared during this session
  for p in $(pgrep -f 'ssshare-loopback' 2>/dev/null); do
    pp=$(ps -o ppid= -p $p | tr -d ' '); st=$(ps -o etimes= -p $p | tr -d ' ')
    [ "$pp" = 1 ] && [ -n "$st" ] && [ "$st" -le $(( $(date +%s) - T0 + 5 )) ] && kill $p 2>/dev/null
  done
}
cleanup() {
  # polite close first (window close -> the app may send Leave / close the connection); then SIGTERM
  CID=$(mmsg get all-clients | python3 -c 'import sys,json; print(next((c["id"] for c in json.load(sys.stdin)["clients"] if c["title"]=="Simple Screensharing" and c["pid"]=='$PID'),""))' 2>/dev/null)
  [ -n "$CID" ] && mmsg dispatch killclient client,$CID >/dev/null 2>&1 && sleep 2
  kill $PID 2>/dev/null; [ -n "${EXTRA_PIDS:-}" ] && kill $EXTRA_PIDS 2>/dev/null; sleep 1; kill -9 $PID 2>/dev/null; [ -n "${EXTRA_PIDS:-}" ] && kill -9 $EXTRA_PIDS 2>/dev/null; rm -rf "${EXTRA_DIRS:-/nonexistent-dir}"
  sleep 1; kill_my_loopbacks
  pkill -f [w]lr-share-picker 2>/dev/null
  mmsg dispatch focusmon,DP-3 >/dev/null
  [ -n "$PREV" ] && [ "$PREV" != None ] && mmsg dispatch focusclient,$PREV >/dev/null 2>&1
  echo "cleanup done; leftover: $(pgrep -f native-bin | wc -l) native, $(pgrep -f [w]lr-share-picker | wc -l) picker"
}
trap cleanup EXIT
sleep 7
# DP-2 origin: x=3840 y=1080 (screenshot-local = global - origin)
shot() { grim -o $MON "$OUT/$TAG-$1.png"; }
# click at screenshot-local coordinates (lx, ly) on DP-2; ydotool absolute y is offset by -1080 on this setup
click() {
  local gx=$((OX + $1)) gy=$((OY + $2))
  ydotool mousemove --absolute -x $gx -y $((gy - 1080)); sleep 0.15
  ydotool click 0xC0; sleep 0.4
}
move() {
  local gx=$((OX + $1)) gy=$((OY + $2))
  ydotool mousemove --absolute -x $gx -y $((gy - 1080)); sleep 0.3
}
source "$STEPS"
