# LEGACY (2026-10-14): real-desktop step (mmsg, ydotool, real portal picker, shared GUI lock). Sourced only by
# native-session.sh, which refuses without SSS_ALLOW_REAL_DESKTOP=1. Agents: do the native side in the headless rig
# (client/scripts/hs-rig.sh + hs-drive.sh, see docs/dev/README.md "Native side in the rig").
# Part A: join the browser-created link from the native Join modal (link pasted through the clipboard), then copy the invite link back.
wl-paste -n > $OUT/clip.saved 2>/dev/null || : > $OUT/clip.saved
cat $OUT/links.A | tr -d '\n' | wl-copy
click 147 162                 # Create or join a room
sleep 1.2
click 1023 538                # link field
sleep 0.3
ydotool key 29:1 47:1 47:0 29:0   # ctrl+v
sleep 0.5
click 1023 578                # room name
ydotool type FromBrowser
sleep 0.4
shot filled
click 1151 655                # Join
sleep 8
shot joined
# Copy invite link (header button)
echo -n "PLACEHOLDER" | wl-copy
click 1691 40
sleep 1
wl-paste -n > $OUT/links.nativeA-copied
echo "copied from native: $(cat $OUT/links.nativeA-copied)"
echo "browser link:       $(cat $OUT/links.A)"
cmp $OUT/links.nativeA-copied $OUT/links.A && echo "IDENTICAL (native Copy invite link == browser-minted link, cert hash included)"
sleep 2
cat $OUT/links.A.members; echo
# restore the user's clipboard (text)
if [ -s $OUT/clip.saved ]; then wl-copy < $OUT/clip.saved; else wl-copy --clear; fi
