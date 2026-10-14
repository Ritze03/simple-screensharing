#!/usr/bin/env bash
#
# LEGACY (2026-10-14): works on the user's REAL MangoWC desktop only (answers the real portal picker with ydotool). Agents must not
# use it: since the headless switch all real-app tests run in the private headless sway rig,
# see hs-rig.sh and superdoc/claude-instructions/testing-and-verification.md. Headless: the rig portal has no picker.
#
# picker-answer.sh — answer the xdg-desktop-portal-wlr share picker
# (wlr-share-picker, a GTK layer-shell dialog that grabs the keyboard) safely.
#
# Usage: picker-answer.sh [TARGET_MONITOR] [WAIT_SECS]     (default DP-1, 15)
#   Waits for the picker to open, then checks the monitor it PRESELECTS
#   (~/.local/state/wlr-share-picker/last-choice.json) is TARGET_MONITOR. Only
#   then types "dp" (filters the list to DisplayPort outputs, so HDMI can never
#   match) and Enter. If the preselection is anything else the picker is
#   cancelled (SIGTERM = "cancel") rather than guessed at. Never takes a
#   screenshot (its thumbnails include HDMI). Exit 0 = answered, 1 = not.

set -uo pipefail
SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SELF_DIR/launch-lib.sh"

target="${1:-DP-1}"
wait_secs="${2:-15}"
sss_check_monitors "$target" || exit 1
LASTCHOICE="$HOME/.local/state/wlr-share-picker/last-choice.json"
PICKER='/usr/bin/wlr-share-picker$'

for _ in $(seq 1 $((wait_secs * 5))); do
  pgrep -f "$PICKER" >/dev/null && break
  sleep 0.2
done
pgrep -f "$PICKER" >/dev/null || { echo "ERROR: the portal picker did not open within ${wait_secs}s" >&2; exit 1; }
sleep 1
choice="$(jq -r .id "$LASTCHOICE" 2>/dev/null || true)"
if [[ "$choice" != "$target" ]]; then
  pkill -TERM -f "$PICKER"
  echo "ERROR: picker preselects '$choice', not $target; cancelled it instead of guessing." >&2
  exit 1
fi
ydotool type dp >/dev/null
sleep 0.4
ydotool key 28:1 28:0 >/dev/null            # Enter
for _ in $(seq 1 30); do
  pgrep -f "$PICKER" >/dev/null || break
  sleep 0.2
done
if pgrep -f "$PICKER" >/dev/null; then
  pkill -TERM -f "$PICKER"
  echo "ERROR: picker still open after Enter; cancelled it." >&2
  exit 1
fi
log "picker answered: $(jq -c '{id,time}' "$LASTCHOICE")"
