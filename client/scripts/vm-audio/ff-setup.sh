# Runs INSIDE a test VM: generates a 440 Hz tone + page and starts Firefox on it with autoplay allowed.
# usage: ff-setup.sh [firefox-binary]  (the Arch image has no firefox package; an unpacked mozilla tarball works)
FF=${1:-firefox}
mkdir -p ~/t && cd ~/t
# 440 Hz tone, 48 kHz stereo, 20 s, loopable
ffmpeg -loglevel error -y -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=20" -ac 2 tone.wav
cat > tone.html <<'EOH'
<html><body><h1>tone</h1><audio id="a" src="tone.wav" loop autoplay controls></audio></body></html>
EOH
mkdir -p ~/ffprof
cat > ~/ffprof/user.js <<'EOP'
user_pref("media.autoplay.default", 0);
user_pref("media.autoplay.blocking_policy", 0);
user_pref("browser.shell.checkDefaultBrowser", false);
user_pref("browser.startup.homepage_override.mstone", "ignore");
user_pref("datareporting.policy.dataSubmissionEnabled", false);
user_pref("toolkit.telemetry.reportingpolicy.firstRun", false);
user_pref("browser.aboutwelcome.enabled", false);
user_pref("media.mediacontrol.testingevents.enabled", true);
EOP
export XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus MOZ_ENABLE_WAYLAND=1
nohup $FF --profile ~/ffprof --no-remote "file:///home/mo/t/tone.html" > ~/ff.log 2>&1 &
sleep 8
pw-dump | grep -c . ; pw-dump | grep -E '"node.name"' | head -20
busctl --user list | grep -i mpris
