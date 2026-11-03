# Runs INSIDE a test VM (client/scripts/vm-audio/README in superdoc testing-and-verification.md).
# usage: soak.sh LABEL SCENARIO   (SECS = how long the in-process share runs; Firefox must already play ~/t/tone.html)
# SCENARIO: pause (pause/resume x3, 15 s apart), pause1 (one cycle, graph dumps before/after), idle (30 s, then
#           the share stops: shows whether stopping the share pauses the player), flap (10 short cycles),
#           vanish (#128, EMU only: the source node is destroyed and re-created 3x, the #117 situation),
#           graph (#128: the source keeps playing, only the graph changes: quantum 256 -> back, rate 44100 -> back)
# Env (#128): EMU=1 runs the emulated source (emu_src.py, started here if it is not running) as APP at SRATE
#   (default 48000); ALLOWED="44100 48000" sets clock.allowed-rates for the run (the host's setting; the VMs'
#   default is only 48000, so a rate switch never happens without it).
# Artefacts in ~/out/LABEL: viewer.wav (what the in-process viewer played), sinkmon.wav (the share sink's monitor),
#   events.txt, status.txt (Firefox MPRIS PlaybackStatus), watch.txt (node/link changes), test.log
source ~/lib.sh
LABEL=${1:-run}; SCEN=${2:-pause}
OUT=~/out/$LABEL; rm -rf $OUT; mkdir -p $OUT ~/relaycwd
pkill -f "[l]ive_audio_share_soak"; pkill -x pw-record; pkill -f "simple-screensharing-se[r]ver"
pactl list short sinks | grep -q viewer-out || pactl load-module module-null-sink sink_name=viewer-out sink_properties=node.description=viewer-out >/dev/null
if [ -n "$ALLOWED" ]; then pw-metadata -n settings 0 clock.allowed-rates "[ $ALLOWED ]" >/dev/null; fi
if [ -n "$EMU" ] && ! pgrep -f "[e]mu_src.py" >/dev/null; then
  python3 ~/emu_src.py "${APP:-Floorp}" 440 "${SRATE:-48000}" > $OUT/emu.log 2>&1 &
  sleep 2
fi
ff_play >/dev/null 2>&1; sleep 1
TB=$(ls -t ${SSS:-$HOME/sss}/target/release/deps/simple_screensharing_client-* | grep -v '\.d$' | head -1)
export SSSHARE_SERVER_BIN=${SSS:-$HOME/sss}/target/release/simple-screensharing-server SSSHARE_SERVER_CWD=~/relaycwd PULSE_SINK=viewer-out SSS_LIVE_APP=${APP:-Firefox} SSS_LIVE_SECONDS=${SECS:-60}
export SSS_LIVE_DEBUG=1
export SSS_AUDIO_DUMP=$OUT  # #128: sent-*.pkt / recv-*.pkt / play-*.log (pktdump.py)
while true; do echo "$(date +%s.%N) $(status)" >> $OUT/status.txt; sleep 0.5; done 2>/dev/null &
ST=$!
( "$TB" --ignored live_audio_share_soak --nocapture > $OUT/test.log 2>&1 & ) 2>/dev/null
for i in $(seq 1 60); do grep -q "viewer listening" $OUT/test.log 2>/dev/null && break; sleep 0.5; done
grep -q "viewer listening" $OUT/test.log || { echo "viewer never listened"; tail -20 $OUT/test.log; exit 1; }
echo "--- graph after start"; nodes; links | grep -v Midi
( pw-record --target viewer-out -P stream.capture.sink=true --rate 48000 --channels 1 --format ${VFMT:-s16} $OUT/viewer.wav > $OUT/rec.log 2>&1 & ) 2>/dev/null
SINKNAME=$(pw-link -o | grep "ssshare-sink" | grep -v -- "-cap" | head -1 | cut -d: -f1)
( pw-record --target "$SINKNAME" -P stream.capture.sink=true --rate 48000 --channels 1 --format s16 $OUT/sinkmon.wav > $OUT/rec2.log 2>&1 & ) 2>/dev/null
T0=$(date +%s.%N)
( python3 ~/watch.py > $OUT/watch.txt 2>&1 & ) 2>/dev/null
ev() { printf "%6.1f %s\n" "$(awk "BEGIN{print $(date +%s.%N) - $T0}")" "$1" | tee -a $OUT/events.txt; }
sleep 8
case $SCEN in
 pause)
  for k in 1 2 3; do
    ev "PAUSE $k"; ff_pause >/dev/null; sleep 1; echo "paused-graph:"; nodes | grep -E "${APP:-Firefox}|ssshare|fakehw"; links | grep -A3 "^${APP:-Firefox}"; sleep 5
    ev "RESUME $k"; ff_play >/dev/null; sleep 1; nodes | grep -E "${APP:-Firefox}|ssshare|fakehw"; links | grep -A3 "^${APP:-Firefox}"; sleep 8
  done;;
 pause1)
  pw-dump > $OUT/dump-before.json; pw-top -b -n 2 | tail -12 > $OUT/top-before.txt
  ev "PAUSE 1"; ff_pause >/dev/null; sleep 5
  ev "RESUME 1"; ff_play >/dev/null; sleep 8
  pw-dump > $OUT/dump-after.json; pw-top -b -n 2 | tail -12 > $OUT/top-after.txt
  sleep 17;;
 flap)  # a source that flaps: 10 x (2.2 s paused, 2.2 s playing), then 20 s playing (rebuild count must stay bounded)
  for k in 1 2 3 4 5 6 7 8 9 10; do ev "PAUSE $k"; ff_pause >/dev/null; sleep 2.2; ev "RESUME $k"; ff_play >/dev/null; sleep 2.2; done
  sleep 20;;
 vanish)  # #117: the source's node is destroyed and a new one appears (Firefox does this on tab mute/unmute); EMU only
  for k in 1 2 3; do
    ev "PAUSE $k"; pkill -f "[e]mu_src.py"; sleep 4
    ev "RESUME $k"; python3 ~/emu_src.py "${APP:-Floorp}" 440 "${SRATE:-48000}" >> $OUT/emu.log 2>&1 & sleep 8
  done;;
 graph)
  ev "QUANTUM 256"; pw-metadata -n settings 0 clock.force-quantum 256 >/dev/null; sleep 8
  ev "QUANTUM default"; pw-metadata -n settings 0 clock.force-quantum 0 >/dev/null; sleep 8
  ev "RATE 44100"; pw-metadata -n settings 0 clock.force-rate 44100 >/dev/null; sleep 8
  ev "RATE default"; pw-metadata -n settings 0 clock.force-rate 0 >/dev/null; sleep 12;;
 idle) sleep 30;;
esac
echo "--- graph at end"; nodes; links | grep -v Midi; pw-metadata -n settings | grep -E "quantum|rate"
pw-top -b -n 2 | tail -14
pkill -x pw-record; pkill -f "[w]atch.py"; kill $ST
pw-metadata -n settings 0 clock.force-quantum 0 >/dev/null; pw-metadata -n settings 0 clock.force-rate 0 >/dev/null
[ -n "$ALLOWED" ] && pw-metadata -n settings 0 clock.allowed-rates "[ 48000 ]" >/dev/null
[ -n "$EMU" ] && pkill -f "[e]mu_src.py"
sleep 6
pkill -f "[l]ive_audio_share_soak"
echo "--- test log tail"; grep -E "soak|audio|rebuilt|WARN|ERROR" $OUT/test.log | tail -25
ls -la $OUT
