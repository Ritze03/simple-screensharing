#!/bin/bash
# Runs INSIDE a test VM (task #128): probe.py against the emulated source while the graph changes under it.
# usage: probe-run.sh MODE SCENARIO [SRC_RATE]
#   MODE      pwclock | sysclock (see probe.py)
#   SCENARIO  forcerate: graph rate 48000 -> 44100 -> 48000 under a running capture (clock.force-rate)
#             pause:     allowed rates 44100+48000, the source corks/uncorks 3x (5 s each): the share group splits
#                        from the hardware group and merges again (with a 44.1 kHz source: the rate flips too)
#             quantum:   the graph quantum 1024 -> 256 -> 2048 -> default under a running capture
#             steady:    nothing happens (control)
source ~/lib.sh
MODE=${1:-pwclock}; SCEN=${2:-forcerate}; SRATE=${3:-48000}
pkill -f "[e]mu_src.py"; pkill -f "[p]robe.py"; pkill -x pw-cli; sleep 0.5
pw-metadata -n settings 0 clock.force-rate 0 >/dev/null
pw-metadata -n settings 0 clock.allowed-rates "[ 44100 48000 ]" >/dev/null
python3 ~/emu_src.py Floorp 440 "$SRATE" > ~/emu.log 2>&1 &
EMU=$!
sleep 2
python3 ~/probe.py Floorp 40 "$MODE" &
PR=$!
T0=$(date +%s.%N)
ev() { printf "EVENT t=%5.1f %s | %s\n" "$(awk "BEGIN{print $(date +%s.%N) - $T0}")" "$1" "$(pw-top -b -n 2 2>/dev/null | grep -E "fakehw|ssshare-sink-probe$|Floorp" | tail -3 | awk "{print \$2, \$3, \$4, \$NF}" | tr "\n" ";")"; }
sleep 6
case $SCEN in
 forcerate)
  ev "force-rate 44100"; pw-metadata -n settings 0 clock.force-rate 44100 >/dev/null; sleep 10
  ev "force-rate 48000"; pw-metadata -n settings 0 clock.force-rate 48000 >/dev/null; sleep 10
  ev "force-rate 0"; pw-metadata -n settings 0 clock.force-rate 0 >/dev/null; sleep 10;;
 pause)
  for k in 1 2 3; do ev "PAUSE $k"; kill -USR1 $EMU; sleep 5; ev "RESUME $k"; kill -USR2 $EMU; sleep 6; done;;
 quantum)  # quantum changes only (same rate, same driver): what a source's latency request does
  ev "force-quantum 256"; pw-metadata -n settings 0 clock.force-quantum 256 >/dev/null; sleep 10
  ev "force-quantum 2048"; pw-metadata -n settings 0 clock.force-quantum 2048 >/dev/null; sleep 10
  ev "force-quantum 0"; pw-metadata -n settings 0 clock.force-quantum 0 >/dev/null; sleep 10;;
 steady) sleep 30;;
esac
wait $PR
kill $EMU
pw-metadata -n settings 0 clock.force-rate 0 >/dev/null
pw-metadata -n settings 0 clock.force-quantum 0 >/dev/null
pw-metadata -n settings 0 clock.allowed-rates "[ 48000 ]" >/dev/null
