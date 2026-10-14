#!/usr/bin/env bash
# Scripted check for #41: the relay process must exit promptly after
# SIGINT / SIGTERM / a TUI quit ('q') and leave the terminal restored.
# Needs tmux (the TUI needs a pty). Usage:
#   server/tests/exit_check.sh <relay-binary> [port]
# Kills only the tmux session and relay PID it started itself.
set -u
BIN=$(readlink -f "$1")
PORT=${2:-47441}
LIMIT_MS=3000
fail=0

for mode in INT TERM q; do
  work=$(mktemp -d)
  sess="exitchk$$-$mode"
  tmux new-session -d -s "$sess" -x 120 -y 40 -c "$work" \
    "$BIN $PORT; echo rc=\$? > $work/rc; stty -a > $work/stty; sleep 30"
  sleep 1.5
  pid=$(pgrep -f -n -- "$BIN $PORT")
  alt_before=$(tmux display -p -t "$sess" '#{alternate_on}')
  t0=$(date +%s%N)
  if [ "$mode" = q ]; then tmux send-keys -t "$sess" q; else kill -$mode "$pid"; fi
  while kill -0 "$pid" 2>/dev/null; do
    sleep 0.02
    if [ $(( ($(date +%s%N) - t0) / 1000000 )) -gt 10000 ]; then break; fi
  done
  ms=$(( ($(date +%s%N) - t0) / 1000000 ))
  sleep 0.3
  alt_after=$(tmux display -p -t "$sess" '#{alternate_on}')
  raw=$(grep -c -- '-icanon' "$work/stty" 2>/dev/null)
  echo "mode=$mode exit_ms=$ms rc=$(cat "$work/rc" 2>/dev/null) alt_screen_before=$alt_before after=$alt_after raw_mode_left=$raw"
  { [ "$ms" -le "$LIMIT_MS" ] && [ "$alt_after" = 0 ] && [ "$raw" = 0 ]; } || { echo "FAIL $mode"; fail=1; }
  tmux kill-session -t "$sess" 2>/dev/null
  rm -rf "$work"
done
exit $fail
