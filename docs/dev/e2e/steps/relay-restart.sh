# LEGACY (2026-10-14): real-desktop step (mmsg, ydotool, real portal picker, shared GUI lock). Sourced only by
# native-session.sh, which refuses without SSS_ALLOW_REAL_DESKTOP=1. Agents: do the native side in the headless rig
# (client/scripts/hs-rig.sh + hs-drive.sh, see docs/dev/README.md "Native side in the rig").
click 387 380      # Watch
sleep 6
touch $OUT/sharer.restart
sleep 5
shot dropped
click 1013 219     # Connect (manual reconnect)
sleep 6
shot reconnected
click 387 380      # Watch again
sleep 5
move 495 134
sleep 2
shot rewatching
