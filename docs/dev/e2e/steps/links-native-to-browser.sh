# LEGACY (2026-10-14): real-desktop step (mmsg, ydotool, real portal picker, shared GUI lock). Sourced only by
# native-session.sh, which refuses without SSS_ALLOW_REAL_DESKTOP=1. Agents: do the native side in the headless rig
# (client/scripts/hs-rig.sh + hs-drive.sh, see docs/dev/README.md "Native side in the rig").
wl-paste -n > $OUT/clip.saved 2>/dev/null || : > $OUT/clip.saved
echo -n "127.0.0.1:14433" | wl-copy
click 147 162
sleep 1.2
click 1078 481
sleep 0.8
click 1023 538
sleep 0.3
ydotool key 29:1 47:1 47:0 29:0
sleep 0.4
click 1023 578
ydotool type FromNative
sleep 0.4
click 1151 655                # Create
sleep 2
echo -n "EMPTY" | wl-copy
click 1150 594                # Copy (minted link)
sleep 0.8
wl-paste -n > $OUT/links.native1
echo "minted by native Create: $(cat $OUT/links.native1)"
click 1145 671                # Join now
sleep 7
shot joined
echo -n "EMPTY" | wl-copy
click 1691 40                 # Copy invite link (live room)
sleep 0.8
wl-paste -n > $OUT/links.native2
echo "native 'Copy invite link': $(cat $OUT/links.native2)"
for i in $(seq 1 40); do [ -f $OUT/links.done ] && break; sleep 1; done
echo "browser side finished: $(ls $OUT/links.done 2>/dev/null)"
shot end
if [ -s $OUT/clip.saved ]; then wl-copy < $OUT/clip.saved; else wl-copy --clear; fi
