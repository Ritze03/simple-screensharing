# sourced in VM scripts
export XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus
mpris() { busctl --user list --no-legend | awk '/org.mpris.MediaPlayer2.firefox/{print $1; exit}'; }
status() { busctl --user get-property "$(mpris)" /org/mpris/MediaPlayer2 org.mpris.MediaPlayer2.Player PlaybackStatus; }
ff_pause() { busctl --user call "$(mpris)" /org/mpris/MediaPlayer2 org.mpris.MediaPlayer2.Player Pause; }
ff_play() { busctl --user call "$(mpris)" /org/mpris/MediaPlayer2 org.mpris.MediaPlayer2.Player Play; }
nodes() { pw-dump | python3 -c '
import json,sys
d=json.load(sys.stdin)
for o in d:
  if o["type"]=="PipeWire:Interface:Node":
    p=o["info"]["props"]; print(o["id"],o["info"]["state"],p.get("media.class"),p.get("node.name"),"drv=",p.get("node.driver-id"),"rate=",p.get("node.rate"),"lat=",p.get("node.latency"))
'; }
links() { pw-link -l; }
SINKCMD='create-node adapter { factory.name=support.null-audio-sink node.name=ssshare-test media.class=Audio/Sink audio.rate=48000 audio.channels=2 audio.position=[ FL FR ] node.virtual=true node.driver=true node.always-process=true node.pause-on-idle=false session.suspend-timeout-seconds=0 priority.session=0 object.linger=false }'
