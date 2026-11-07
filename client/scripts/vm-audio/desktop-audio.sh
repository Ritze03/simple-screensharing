#!/bin/bash
# Runs INSIDE a test VM (task I7; superdoc/testing/vm-audio.md "Audio tests in the VMs"): desktop audio,
# the exclusion list, audio-only shares and the source groups against real PipeWire streams.
# usage: desktop-audio.sh LABEL SCENARIO         (Firefox must already play ~/t/tone.html: ff-setup.sh; fakehw.sh first)
# SCENARIO
#   desktop  the whole desktop: Firefox (440 Hz, real, MPRIS) + a source called Discord (880 Hz) + silent fake sources for
#            the groups (media.role=Game, SteamAppId in the environment, a bluez_input node). Exclusion list "Discord",
#            then at 12 s remove it, at 24 s add it back, at 36 s add Firefox (nothing left), at 48 s remove Firefox.
#            Firefox's PlaybackStatus is logged all along (a source that is unlinked or a share that stops must not
#            pause it: #123). Ends with the share stopping (Firefox must still be Playing).
#   named    the same, sharing the named app Floorp (emulated, 440 Hz): the old #105 behaviour must be unchanged.
#   empty    desktop audio with nothing linked: Firefox is excluded for the first 10 s (no other source exists), then
#            allowed. Shows what a share sends while the desktop is silent (packets per second, see "viewer packets").
#   only     audio-only share through AppState::start_audio_share(room, None, ..): no screen, the viewer hears it, Stop
#            leaves nothing in the graph (test live_audio_only_share_through_the_app_state), named then desktop.
# Artefacts in ~/out/LABEL: viewer.wav (what the in-process viewer played: viewer-out's monitor), sinkmon.wav (the share
# sink's monitor), status.txt (Firefox PlaybackStatus), test.log, nodes.txt.
source ~/lib.sh
LABEL=${1:-run}; SCEN=${2:-desktop}
OUT=~/out/$LABEL; rm -rf $OUT; mkdir -p $OUT ~/relaycwd
SSS=${SSS:-$HOME/sss}
pkill -f "[l]ive_audio_share_soak"; pkill -f "[l]ive_audio_only_share"; pkill -x pw-record; pkill -f "simple-screensharing-se[r]ver"
pkill -f "[e]mu_src.py"; pkill -f "[f]ake-bluez"
pactl list short sinks | grep -q viewer-out || pactl load-module module-null-sink sink_name=viewer-out sink_properties=node.description=viewer-out >/dev/null
TB=$(ls -t $SSS/target/release/deps/simple_screensharing_client-* | grep -v '\.d$' | head -1)
export SSSHARE_SERVER_BIN=$SSS/target/release/simple-screensharing-server SSSHARE_SERVER_CWD=~/relaycwd PULSE_SINK=viewer-out
export SSS_LIVE_DEBUG=1 SSS_AUDIO_DUMP=$OUT

emu() {  # NAME FREQ VOLUME PROPS [ENV=VALUE]
  # PULSE_SINK is for the viewer only: the emulated apps play to the default sink like real ones.
  ( env -u PULSE_SINK ${5:-X=1} python3 ~/emu_src.py "$1" "$2" 48000 "$3" "$4" >> $OUT/emu.log 2>&1 & ) 2>/dev/null
}
fake_bluez() {  # a Stream/Output/Audio node named like a phone connected over Bluetooth (the client groups it as a device)
  ( { echo 'create-node adapter { factory.name=support.null-audio-sink node.name=bluez_input.AA_BB_CC_DD_EE_FF.2 node.description="iPhone von Mo" media.class=Stream/Output/Audio audio.position=[ FL FR ] object.linger=false }'; sleep 3600; } | exec -a fake-bluez pw-cli > $OUT/bluez.log 2>&1 & ) 2>/dev/null
}
record() {
  ( pw-record --target viewer-out -P stream.capture.sink=true --rate 48000 --channels 1 --format s16 $OUT/viewer.wav > $OUT/rec.log 2>&1 & ) 2>/dev/null
}
record_sink() {
  SINKNAME=$(pw-link -o | grep "ssshare-sink" | grep -v -- "-cap" | head -1 | cut -d: -f1)
  [ -n "$SINKNAME" ] && ( pw-record --target "$SINKNAME" -P stream.capture.sink=true --rate 48000 --channels 1 --format s16 $OUT/sinkmon.wav > $OUT/rec2.log 2>&1 & ) 2>/dev/null
}
status_log() { while true; do echo "$(date +%s.%N) $(status)" >> $OUT/status.txt; sleep 0.5; done 2>/dev/null & ST=$!; }
cleanup() {
  pkill -x pw-record; kill $ST 2>/dev/null; pkill -f "[e]mu_src.py"; pkill -f "[f]ake-bluez"; pkill -f "[l]ive_audio_share_soak"; pkill -f "[l]ive_audio_only_share"
}

case $SCEN in
 desktop|named)
  ff_play >/dev/null 2>&1; sleep 1
  emu Discord 880 0.5 "media.role=Communication"
  if [ "$SCEN" = desktop ]; then
    # quiet but real streams (volume 0 makes the stream disappear): only their grouping is looked at
    emu FakeGame 600 0.001 "media.role=Game"
    emu SteamThing 700 0.001 "media.role=Production" SteamAppId=480
    emu PlainApp 800 0.001 "media.role=Production"
    fake_bluez
    # another instance of this client (#117 task, 2026-11-04): must never be linked nor listed, whatever the list says
    emu simple-screensharing-client 1000 0.5 "media.role=Production"
  else
    emu Floorp 440 0.5 "media.role=Music"
  fi
  sleep 3
  nodes > $OUT/nodes.txt
  if [ "$SCEN" = desktop ]; then
    export SSS_LIVE_CHOICE=desktop SSS_LIVE_EXCLUDE=Discord SSS_LIVE_EXCL_AT="12:-Discord,24:+Discord,36:+Firefox,48:-Firefox"
    export SSS_LIVE_SECONDS=60
  else
    export SSS_LIVE_APP=Floorp SSS_LIVE_SECONDS=30 SSS_LIVE_EXCLUDE=Discord SSS_LIVE_EXCL_AT="12:-Discord,20:+Discord"
  fi
  status_log
  ( "$TB" --ignored live_audio_share_soak --nocapture > $OUT/test.log 2>&1 & ) 2>/dev/null
  for i in $(seq 1 60); do grep -q "viewer listening" $OUT/test.log 2>/dev/null && break; sleep 0.5; done
  grep -q "viewer listening" $OUT/test.log || { echo "viewer never listened"; tail -20 $OUT/test.log; cleanup; exit 1; }
  record; sleep 1; record_sink
  echo "--- graph after start"; nodes; links | grep -v Midi
  sleep $((SSS_LIVE_SECONDS + 4))
  sleep 3; echo "$(date +%s.%N) $(status) (after the share stopped)" >> $OUT/status.txt
  nodes > $OUT/nodes-end.txt
  cleanup;;
 empty)
  ff_play >/dev/null 2>&1; sleep 1
  nodes > $OUT/nodes.txt
  export SSS_LIVE_CHOICE=desktop SSS_LIVE_EXCLUDE=Firefox SSS_LIVE_EXCL_AT="10:-Firefox" SSS_LIVE_SECONDS=20
  status_log
  ( "$TB" --ignored live_audio_share_soak --nocapture > $OUT/test.log 2>&1 & ) 2>/dev/null
  for i in $(seq 1 60); do grep -q "viewer listening" $OUT/test.log 2>/dev/null && break; sleep 0.5; done
  record; sleep $((SSS_LIVE_SECONDS + 4))
  cleanup;;
 only)
  ff_play >/dev/null 2>&1; sleep 1
  emu Floorp 440 0.5 "media.role=Music"; sleep 3
  record
  status_log
  export SSS_LIVE_APP=Floorp
  "$TB" --ignored live_audio_only_share_through_the_app_state --nocapture > $OUT/test.log 2>&1; echo "named rc=$?" | tee -a $OUT/result.txt
  sleep 2
  export SSS_LIVE_CHOICE=desktop SSS_LIVE_EXCLUDE=Discord
  "$TB" --ignored live_audio_only_share_through_the_app_state --nocapture > $OUT/test-desktop.log 2>&1; echo "desktop rc=$?" | tee -a $OUT/result.txt
  sleep 1; echo "$(date +%s.%N) $(status) (after both shares stopped)" >> $OUT/status.txt
  cleanup;;
esac
echo "--- test log"; sed 's/\x1b\[[0-9;]*m//g' $OUT/test.log | grep -E "soak|audio-only|PASS|panicked|test result" | cut -c1-260 | head -150
[ -f $OUT/test-desktop.log ] && { echo "--- desktop test log"; sed 's/\x1b\[[0-9;]*m//g' $OUT/test-desktop.log | grep -E "audio-only|PASS|panicked|test result" | cut -c1-260; }
ls -la $OUT
