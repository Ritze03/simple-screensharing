# Runs INSIDE a test VM (client/scripts/vm-audio/README in superdoc testing-and-verification.md).
# usage: soak.sh LABEL SCENARIO   (SECS = how long the in-process share runs; Firefox must already play ~/t/tone.html)
# SCENARIO: pause (pause/resume x3, 15 s apart), pause1 (one cycle, graph dumps before/after), idle (30 s, then
#           the share stops: shows whether stopping the share pauses the player)
# Artefacts in ~/out/LABEL: viewer.wav (what the in-process viewer played), sinkmon.wav (the share sink's monitor),
#   events.txt, status.txt (Firefox MPRIS PlaybackStatus), watch.txt (node/link changes), test.log
source ~/lib.sh
LABEL=${1:-run}; SCEN=${2:-pause}
OUT=~/out/$LABEL; rm -rf $OUT; mkdir -p $OUT ~/relaycwd
pkill -f live_audio_share_soak; pkill -x pw-record; pkill -x simple-screensharing-server
pactl list short sinks | grep -q viewer-out || pactl load-module module-null-sink sink_name=viewer-out sink_properties=node.description=viewer-out >/dev/null
ff_play >/dev/null 2>&1; sleep 1
TB=$(ls -t ~/sss/target/release/deps/simple_screensharing_client-* | grep -v '\.d$' | head -1)
export SSSHARE_SERVER_BIN=~/sss/target/release/simple-screensharing-server SSSHARE_SERVER_CWD=~/relaycwd PULSE_SINK=viewer-out SSS_LIVE_APP=Firefox SSS_LIVE_SECONDS=${SECS:-60}
export SSS_LIVE_DEBUG=1
while true; do echo "$(date +%s.%N) $(status)" >> $OUT/status.txt; sleep 0.5; done 2>/dev/null &
ST=$!
( "$TB" --ignored live_audio_share_soak --nocapture > $OUT/test.log 2>&1 & ) 2>/dev/null
for i in $(seq 1 60); do grep -q "viewer listening" $OUT/test.log 2>/dev/null && break; sleep 0.5; done
grep -q "viewer listening" $OUT/test.log || { echo "viewer never listened"; tail -20 $OUT/test.log; exit 1; }
echo "--- graph after start"; nodes; links | grep -v Midi
( pw-record --target viewer-out -P stream.capture.sink=true --rate 48000 --channels 1 --format s16 $OUT/viewer.wav > $OUT/rec.log 2>&1 & ) 2>/dev/null
SINKNAME=$(pw-link -o | grep "ssshare-sink" | grep -v -- "-cap" | head -1 | cut -d: -f1)
( pw-record --target "$SINKNAME" -P stream.capture.sink=true --rate 48000 --channels 1 --format s16 $OUT/sinkmon.wav > $OUT/rec2.log 2>&1 & ) 2>/dev/null
T0=$(date +%s.%N)
( python3 ~/watch.py > $OUT/watch.txt 2>&1 & ) 2>/dev/null
ev() { printf "%6.1f %s\n" "$(awk "BEGIN{print $(date +%s.%N) - $T0}")" "$1" | tee -a $OUT/events.txt; }
sleep 8
case $SCEN in
 pause)
  for k in 1 2 3; do
    ev "PAUSE $k"; ff_pause >/dev/null; sleep 1; echo "paused-graph:"; nodes | grep -E "Firefox|ssshare|fakehw"; links | grep -A3 "^Firefox"; sleep 5
    ev "RESUME $k"; ff_play >/dev/null; sleep 1; nodes | grep -E "Firefox|ssshare|fakehw"; links | grep -A3 "^Firefox"; sleep 8
  done;;
 pause1)
  pw-dump > $OUT/dump-before.json; pw-top -b -n 2 | tail -12 > $OUT/top-before.txt
  ev "PAUSE 1"; ff_pause >/dev/null; sleep 5
  ev "RESUME 1"; ff_play >/dev/null; sleep 8
  pw-dump > $OUT/dump-after.json; pw-top -b -n 2 | tail -12 > $OUT/top-after.txt
  sleep 17;;
 idle) sleep 30;;
esac
echo "--- graph at end"; nodes; links | grep -v Midi; pw-metadata -n settings | grep -E "quantum|rate"
pw-top -b -n 2 | tail -14
pkill -x pw-record; pkill -f watch.py; kill $ST
sleep 6
pkill -f live_audio_share_soak
echo "--- test log tail"; grep -E "soak|audio|rebuilt|WARN|ERROR" $OUT/test.log | tail -25
ls -la $OUT
