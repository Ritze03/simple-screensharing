#!/bin/bash
# HOST side (task I7): run one desktop-audio scenario in a test VM and analyze the recordings (analysis only reads files).
# usage: desktop-run.sh PORT LABEL SCENARIO    PORT: 2223 = fedora-ssd, 2224 = arch-ssd; SCENARIO: desktop | named | only
# Needs the source in the VM (vm-sync.sh) built there (`cargo test -p simple-screensharing-client --release --lib --no-run`
# and `cargo build --release -p simple-screensharing-server`), Firefox playing the tone page (fakehw.sh, ff-setup.sh).
port=$1; label=$2; scen=${3:-desktop}
D=$(cd "$(dirname "$0")" && pwd); O=${OUT_DIR:-$PWD/.tl-scratch/vm-audio}/$label
mkdir -p "$O"
scp -q -P "$port" "$D/desktop-audio.sh" "$D/lib.sh" "$D/emu_src.py" mo@127.0.0.1:
ssh -o BatchMode=yes -p "$port" mo@127.0.0.1 "SSS=${VM_SSS:-\$HOME/sss} bash ~/desktop-audio.sh $label $scen" > "$O/stdout.txt" 2>&1
for f in viewer.wav sinkmon.wav status.txt test.log test-desktop.log nodes.txt nodes-end.txt emu.log result.txt; do
  scp -q -P "$port" "mo@127.0.0.1:out/$label/$f" "$O/$f" 2>/dev/null
done
cat "$O/stdout.txt"
echo "=== viewer.wav, per second (what the viewer heard)"
[ -f "$O/viewer.wav" ] && python3 "$D/tones.py" "$O/viewer.wav" 440 880
echo "=== sinkmon.wav (the share sink's own monitor)"
[ -f "$O/sinkmon.wav" ] && python3 "$D/tones.py" "$O/sinkmon.wav" 440 880
echo "=== Firefox PlaybackStatus changes (none expected)"
awk '{s=$2$3; if (s!=p) print $1, $2, $3; p=s}' "$O/status.txt"
