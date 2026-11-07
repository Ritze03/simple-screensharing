# Audio tests in the VMs

Plain-link reference page, moved out of the force-loaded `claude-instructions/testing-and-verification.md` on 2026-11-07 (row 167).

## Audio tests in the VMs (user offer, 2026-11-03)

The no-audio rule in `claude-instructions/testing-and-verification.md` is for the **host**. The user offered two qemu VMs where real audio testing is
allowed (task #123): libvirt `qemu:///system` domains, one Arch and one Fedora (`<vm>`, shut off by default; real names, ports and login: `.claude/local/test-machine.md` in the main checkout, worktrees do not have it).
Inside them you may create audio streams, play a tone in Firefox, use `pw-cli`/`pw-link`/`pactl` and install
packages with the VM's own passwordless `sudo`. On the host nothing changes: no audio streams, never the user's music player.
Both are GNOME/Wayland desktops with PipeWire + WirePlumber, user `<vm-user>`, no sound card (only `auto_null`).

- **Start / reach:** `virsh -c qemu:///system start <vm>`. The NIC is user-mode
  (`passt`), so `virsh domifaddr` is empty; libvirt forwards one `127.0.0.1:<port>` per VM to the guest's sshd:
  `ssh -p <port> <vm-user>@127.0.0.1` (key login works). `virsh qemu-agent-command <dom>
  '{"execute":"guest-ping"}'` also works. A guest sees `/run/user/<uid>` with `wayland-0`; scripts source
  `client/scripts/vm-audio/lib.sh` for `XDG_RUNTIME_DIR`/`WAYLAND_DISPLAY`/`DBUS_SESSION_BUS_ADDRESS`. Shut down
  with `virsh -c qemu:///system shutdown <dom>` when done.
- **Build in the VM** (glibc differs per distro): `client/scripts/vm-audio/vm-sync.sh <port>` tars the worktree
  into `~/sss` (no rsync in the VMs), then in the VM `cargo test -p simple-screensharing-client --release --lib
  --no-run` and `cargo build --release -p simple-screensharing-server` (about 4 min cold, 35 s incremental).
  Arch had every dependency already; Fedora needed `gstreamer1-devel gstreamer1-plugins-base-devel
  gstreamer1-plugins-bad-free-devel` (`client/check-deps.sh` says what is missing).
- **Make it look like a desktop:** `fakehw.sh` (in the VM) adds a driver-clocked null sink
  `alsa_output.fakehw` at priority 1000 as a PipeWire drop-in. Without it the only sink is the fallback
  `auto_null`, which WirePlumber removes the moment another sink appears, and the share sink becomes the default
  one: a different situation from a desktop with a real output device. EasyEffects (`dnf install easyeffects`,
  `QT_QPA_PLATFORM=wayland easyeffects -w`) was run on the Fedora VM to match the host's setup; it changed nothing.
- **Source of sound:** `ff-setup.sh [firefox]` makes a 440 Hz `tone.wav` with ffmpeg, a page with
  `<audio loop autoplay>` and a Firefox profile with autoplay allowed, and starts Firefox on it (Arch has no
  Firefox package in the image; an unpacked `download.mozilla.org` tarball in `~/ffox` works). Pause/resume it
  without any GUI driving through MPRIS (`busctl --user call org.mpris.MediaPlayer2.firefox.instance_* /org/mpris/MediaPlayer2
  org.mpris.MediaPlayer2.Player Pause|Play`); no `ydotool` needed. Firefox's own `PlaybackStatus` property is the
  way to see who paused it.
- **Run a share:** the ignored `end_to_end_test::live_audio_share_soak` shares the node named `SSS_LIVE_APP`
  (default `Firefox`) from an in-process sharer to an in-process viewer through a real relay
  (`SSSHARE_SERVER_BIN`, `SSSHARE_SERVER_CWD`) for `SSS_LIVE_SECONDS`. The viewer's real `autoaudiosink` plays to
  the null sink `viewer-out` (`PULSE_SINK=viewer-out`), whose monitor `pw-record` writes to a WAV: that is *what
  a viewer hears*. A second `pw-record` on the share sink's monitor shows whether the graph or the app's pipeline
  is at fault. `client/scripts/vm-audio/soak.sh LABEL pause|pause1|idle` (in the VM) drives one run
  (pause/resume cycles, or a plain stop) and `host-run.sh PORT LABEL SCENARIO [SECS]` runs it from the host and
  analyzes it: `analyze.py` (per 0.5 s: level, zero-crossing frequency, largest sample step relative to the peak; a
  clean 440 Hz sine is 440 Hz / 0.058, a click or dropout is far above, a rate error moves the frequency) and
  `summary.py` (seconds until clean after each resume). `watch.py` logs every node/link change every 0.3 s, which
  is how the lost links and the re-created ports were found.
- **A source that keeps its node and ports (task #125):** Firefox in the VMs re-creates its ports on resume; the host's
  browser does not. `emu_src.py NAME` (a GStreamer `pulsesink`, SIGUSR1 pause / SIGUSR2 play) behaves like the host
  one: `EMU=1 APP=<host-browser-name> host-run.sh PORT LABEL pause|flap`. `VM_SSS=<dir in the VM>` runs a baseline build kept in
  another checkout (`VM_SSS='$HOME/sss-base'`). Make the fake hardware suspend when idle (`node.pause-on-idle = true`,
  `node.always-process = false` in the fakehw drop-in) to get the Dummy-Driver switch a real ALSA sink gives. A healthy
  share delivers ~50 packets/s to the viewer (`pps.py test.log`); fewer means the capture stream is not being scheduled.
- **Graph changes, rates, dumps (task #128):** the VMs default to `clock.allowed-rates = [48000]`, the dev host to
  `[44100, 48000]` (44.1 kHz hardware): pass `ALLOWED="44100 48000" SRATE=44100` (emulated source at
  44.1 kHz) or the VM never switches rate. `soak.sh ... graph` changes quantum and rate under a playing source,
  `... vanish` destroys and re-creates the source node (EMU only). `probe.py`/`probe-run.sh MODE forcerate|pause|quantum`
  run the share's capture chain without the app and print samples/s, PTS steps and the pipeline clock against
  `CLOCK_MONOTONIC` (`pwclock` vs `sysclock`); `probe-view.py`/`probe-view-run.sh` do the same for the viewer's playback
  tail. `soak.sh` sets `SSS_AUDIO_DUMP` (client/src/audio_dump.rs): `pktdump.py sent-*.pkt out.wav` decodes what the
  sender sent, `recv-*.pkt` what the viewer received, `play-*.log` lists its `push`/`gap` actions; `phases.py viewer.wav
  events.txt` counts bad 0.5 s windows per phase. Always compare with the share sink's own monitor (`sinkmon.wav`):
  that is the VM's noise floor (0-3 bad windows per phase). In the VM, kill with patterns that cannot match your own
  ssh command line (`pkill -f "[p]robe.py"`), or the ssh session kills itself.
- **Noise floor of a silent stretch (#129):** `VFMT=f32 host-run.sh ...` records the viewer as float (an s16 recording adds
  its own dither and hides what the viewer produced), `noise.py viewer.wav events.txt` prints the RMS of every paused stretch.
- **Desktop audio (task I7, 2026-11-04):** `client/scripts/vm-audio/desktop-audio.sh LABEL desktop|named|empty|only` (in the VM) drives desktop audio, the exclusion list (Discord added, removed, Firefox added and removed again) and audio-only shares against real streams plus silent fake sources for the groups; `desktop-run.sh PORT LABEL SCENARIO` runs it from the host and analyzes the recordings with `tones.py FILE.wav FREQ...` (per window, how loud each tone is in dBFS: 440 Hz = Firefox, 880 Hz = the excluded app; pure analysis of a file, plays nothing).
- **Also capture, before/after a change:** `pw-dump`, `pw-top -b -n 2`, `pw-metadata -n settings`, and the
  WirePlumber side (`WIREPLUMBER_DEBUG=s-linking.mpris:5`) when a player is paused unexpectedly.
- **Leave the VMs as found:** remove `~/.config/pipewire/pipewire.conf.d/50-fakehw.conf` (and restart
  `pipewire pipewire-pulse wireplumber`) if you added it; installed build dependencies may stay (say which in
  the report).

