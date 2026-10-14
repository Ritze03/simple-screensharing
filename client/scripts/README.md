# client/scripts

Dev tooling for running and measuring the real native stack (relay + egui client) on a
MangoWC desktop. Entry points are the repo-root `launch-test.sh` / `launch-test-tmux.sh`;
everything here is sourced or called by them. All of it refuses HDMI outputs (name and
geometry, `launch-lib.sh: sss_check_monitors`). Facts about the machine:
`superdoc/claude-instructions/testing-and-verification.md`.

| Script | Purpose |
| --- | --- |
| `launch-lib.sh` | shared functions: monitor safety check, build (own `target/`), scratch dir, relay cert hash, join string, per-client scratch config (`rooms.toml` auto-connect + `settings.toml`), spawn + place windows, kill by pid file |
| `rig.sh up\|down` | two-client rig via `launch-test-tmux.sh`, sharer started with the GStreamer stats tracer (`<scratch>/gst1.log`) |
| `session.sh scenarios\|audio-peek\|run` | one short locked GUI session: take the GUI lock (wait), rig + animation up, steps, always tear down + refocus DP-3 + release |
| `animate.sh start\|stop [MON]` | fullscreen `vkcube` (mailbox) on a monitor so capture has content at the refresh rate |
| `drive.sh MON STEP` | ydotool driver for the two-client rig (share, setup, go live, watch, hover, sample, stop, ...). Coordinates assume both clients tiled on DP-2 |
| `ui-click.sh` | move/click at a global coordinate; hides ydotool's -1080 y offset; refuses HDMI points |
| `picker-answer.sh` | answer the portal picker safely (checks the preselection, types `dp`, Enter) |
| `run-scenario.sh` | one measured share: receiver tooltip screenshots over ~30 s + sender per-stage rates |
| `run-audio.sh` | share with Spotify's per-app stream; restores Spotify's volume; `audio-probe.py` records the viewer's playback stream |
| `gst-rate.py` | buffers/s per pad from the tracer log (`--summary`: capture / encode-branch input / encoder output) |
| `portal-probe.py`, `probe-run.sh` | own ScreenCast portal session: pipewiresrc rate into a chosen tail (isolates portal vs pipeline) |
| `wlr-capture-rate.sh` | compositor's own wlr-screencopy rate for an output (wf-recorder) |
| `enc-bench.sh` | raw `vah264enc`/`vah265enc`/`vaav1enc` throughput at 1080p |
| `audio-probe.py` | peak/RMS of a given app's playback stream |
| `stop-launch.sh` | stop what a scratch dir's launch started (client process groups, tmux sessions) |
| `test-launch-fg.sh` | smoke test of `launch-test.sh` (non-tmux variant) |

Rules baked in: never `pkill` by name (it matches other workers' processes); tmux targets use
`=name` (a bare name prefix-matches `name-watchdog`); every child is started with fd 9 closed
so it can never keep the GUI lock.
