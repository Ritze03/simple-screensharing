#!/bin/bash
# Runs INSIDE a test VM (task #128): probe-view.py playing into the null sink viewer-out while the graph rate
# and quantum change; the sink's monitor is recorded to ~/out/view-MODE.wav (analyze.py it on the host).
# usage: probe-view-run.sh MODE
source ~/lib.sh
MODE=${1:-default}; OUT=~/out/view-$MODE; rm -rf $OUT; mkdir -p $OUT
pkill -f "[p]robe-view.py"; pkill -x pw-record
pactl list short sinks | grep -q viewer-out || pactl load-module module-null-sink sink_name=viewer-out sink_properties=node.description=viewer-out >/dev/null
pw-metadata -n settings 0 clock.allowed-rates "[ 44100 48000 ]" >/dev/null
PULSE_SINK=viewer-out python3 ~/probe-view.py 44 "$MODE" > $OUT/probe.txt 2>&1 &
PV=$!
sleep 2
( pw-record --target viewer-out -P stream.capture.sink=true --rate 48000 --channels 1 --format s16 $OUT/viewer.wav > /dev/null 2>&1 & )
T0=$(date +%s.%N)
ev() { printf "%6.1f %s\n" "$(awk "BEGIN{print $(date +%s.%N) - $T0}")" "$1" >> $OUT/events.txt; }
sleep 6
ev "QUANTUM 256"; pw-metadata -n settings 0 clock.force-quantum 256 >/dev/null; sleep 8
ev "QUANTUM default"; pw-metadata -n settings 0 clock.force-quantum 0 >/dev/null; sleep 8
ev "RATE 44100"; pw-metadata -n settings 0 clock.force-rate 44100 >/dev/null; sleep 8
ev "RATE default"; pw-metadata -n settings 0 clock.force-rate 0 >/dev/null; sleep 10
pkill -x pw-record
wait $PV
pw-metadata -n settings 0 clock.allowed-rates "[ 48000 ]" >/dev/null
cat $OUT/probe.txt
