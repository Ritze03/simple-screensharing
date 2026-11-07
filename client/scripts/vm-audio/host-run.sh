#!/bin/bash
# HOST side: run one soak in a test VM and analyze it.
# usage: host-run.sh PORT LABEL SCENARIO [SECS]     PORT: the VM's forwarded SSH port (real values: .claude/local/test-machine.md)
# Needs the source in the VM (vm-sync.sh) built there, Firefox playing the tone page (ff-setup.sh), see
# superdoc/testing/vm-audio.md "Audio tests in the VMs".
port=$1; label=$2; scen=$3; secs=${4:-60}
D=$(cd "$(dirname "$0")" && pwd); O=${OUT_DIR:-$PWD/.tl-scratch/vm-audio}/$label
mkdir -p "$O"
scp -q -P "$port" "$D/soak.sh" "$D/watch.py" "$D/lib.sh" "$D/emu_src.py" mo@127.0.0.1:
ssh -o BatchMode=yes -p "$port" mo@127.0.0.1 "SSS=${VM_SSS:-\$HOME/sss} EMU=${EMU:-} SRATE=${SRATE:-} ALLOWED=\"${ALLOWED:-}\" VFMT=${VFMT:-} APP=${APP:-Firefox} SECS=$secs bash ~/soak.sh $label $scen" > "$O/stdout.txt" 2>&1
scp -q -P "$port" "mo@127.0.0.1:out/$label/sent-*.pkt" "mo@127.0.0.1:out/$label/recv-*.pkt" "mo@127.0.0.1:out/$label/play-*.log" "$O/" 2>/dev/null
for f in viewer.wav sinkmon.wav emu.log events.txt status.txt watch.txt test.log dump-before.json dump-after.json; do
  scp -q -P "$port" "mo@127.0.0.1:out/$label/$f" "$O/$f" 2>/dev/null
done
for f in viewer.wav sinkmon.wav; do
  echo "=== $f (non-silent windows; BAD = pitch off 440 Hz or a jump)"
  [ -f "$O/$f" ] && python3 "$D/analyze.py" "$O/$f" "$O/events.txt" | awk '$2>-60 || /PAUSE|RESUME/'
done
echo "=== after each RESUME: seconds until 3 s of clean tone"
python3 "$D/summary.py" "$O/viewer.wav" "$O/events.txt" | grep -v '^    '
echo "=== Firefox PlaybackStatus changes (only your own pause/resume should show)"
awk '{s=$2$3; if (s!=p) print $1, $2, $3; p=s}' "$O/status.txt"
echo "=== client log"
sed 's/\x1b\[[0-9;]*m//g' "$O/test.log" | grep -E "INFO|WARN|ERROR" | cut -c1-200
