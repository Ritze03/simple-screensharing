# PipeWire share sink and node lifecycle

The private share sink, linking nodes by name, and surviving a source stream vanishing or being switched (#105, #117, #118, #121). Hub: `../audio-sharing.md`.

## 2026-11-03: the share captures a private, self-clocked null sink instead of a `pw-loopback` (task #121)

> **Corrected by #128 (above):** the loopback's missing clock was not why the share broke; the null sink stays, but
> for its own merits.

User report after #117: muting and unmuting works now, but *pausing* the video and playing it again is
"broken the same way" (garbled audio until the source is selected again).

**Evidence (user's read-only `pw-dump` filtered to `ssshare|<browser>`, `{id,state}`):** working and broken-after-pause
are identical: the same four node ids (271, 277, 379, 314), all `running`. So on pause Firefox neither destroys its
stream node (the #117 path) nor does it go `idle`/`suspended`; the first idea of this task (treat a linked node
that goes idle as a gap) was written, then dropped on this data. Graph settings read at the same time:
`clock.rate 48000`, `allowed-rates [44100 48000]`, `quantum 1024`, `min-quantum 1024`, `max-quantum 2048`.
Something upstream changes on pause/resume while every node keeps running (the working guess: the stream's
latency/rate request, hence the quantum or rate the group runs at), and a `pw-loopback`'s capture side, a stream
with no rate, no latency and no clock of its own (`dsp`/F32P, `resample.disable=true`), did not survive it. That
is still a hypothesis: it was not measured (no audio tests may be run), only the consequences are known.

**Change (`audio.rs`): own the clock.** `spawn_sink` replaces `spawn_loopback`. A `pw-cli` child is kept alive on a
stdin pipe and told `create-node adapter { factory.name=support.null-audio-sink ... }` (`sink_create_command`):
`media.class=Audio/Sink`, `audio.rate=48000`, stereo `[ FL FR ]`, `node.driver=true` (its own timer clock),
`node.always-process=true` (scheduled even with nothing linked), `node.pause-on-idle=false` and
`session.suspend-timeout-seconds=0` (never suspended while the source is quiet), `node.virtual=true` and
`priority.session=0` (never the default sink next to real devices at 1000+), `object.linger=false`. The app's
output ports are `pw-link`ed into the sink's *input* ports exactly as they were into the loopback input
(`link_poll_loop`, `sync_links`, `gap_decision`, `rebuild_chain` are unchanged apart from spawning the sink), and
the capture pipeline reads the sink's *monitor*: `pipewiresrc target-object=<sink serial>
stream-properties="props,stream.capture.sink=true"` (`sink_monitor_source`; the way a PulseAudio client reads
`<sink>.monitor`; `wait_for_capture_link` still proves the capture stream is linked from the sink node, so the
microphone trap stays shut). Whatever the source then does (pause, vanish, a different latency or rate), PipeWire
adapts each input to the sink, and the capture side sees one steady 48 kHz stereo stream from a node that does not
depend on the source being alive.

*Why `pw-cli` on a pipe:* the node must die with the app, including a crash. A node created by a `pw-cli`
connection with `object.linger=false` is destroyed when the connection ends; `pw-cli` has no tty check
(`main`: interactive iff no command arguments; it reads stdin lines and quits on EOF), so keeping its stdin pipe
open keeps the sink; `PR_SET_PDEATHSIG` (`die_with_parent`) and the pipe closing cover SIGKILL. The alternative,
`pactl load-module module-null-sink`, persists in the daemon until unloaded and would need a stale-sink sweep at
every start. *Why not stay with a tuned `pw-loopback`:* its capture side is a stream whose clock is someone
else's; the properties that would help (`node.always-process`, `node.passive`) cannot be validated without
creating an audio stream, whereas the null-sink factory is the documented way to get a node with its own clock.

Kept as the safety net: the #117 gap/rebuild (it now rebuilds a sink + pipeline). Kept: #118 mute.

**Side effects to know:** the sink is a real `Audio/Sink` named `ssshare-sink-<stream>-<uuid>` (description
`simple-screensharing-audio-capture`), so it is listed by `pw-link`/`pw-dump` and by pavucontrol while a share
runs. It is not hideable (the monitor needs the class); it never becomes the default sink and disappears when the
share stops or the app dies. The client now needs `pw-cli` instead of `pw-loopback` (`check-deps.sh`, AppRun,
packaging notes updated).

**Verified live in the VMs later (task #123, see the block above); the sentence below is the state at the time of #121.** **NOT verified live (hard rule: no audio nodes):** the `create-node` line, that the sink gets its input and
monitor ports, that `stream.capture.sink=true` links the monitor to the capture stream, and above all that
pause/resume no longer garbles. Unit tests cover the property set (`the_sink_create_command_describes_a_self_clocked_owned_virtual_sink`),
the source description and that it parses into a `pipewiresrc` carrying the property
(`the_capture_source_reads_the_sinks_monitor`), and the fixture-based sink lookup / readiness / capture-link
checks on an `Audio/Sink` node. If it still breaks, run *while paused* and again after resuming:
`pw-dump | jq '.[] | select((.info.props["node.name"]? // "") | test("ssshare|<browser>"; "i")) | {id, type, state: .info.state, name: .info.props["node.name"], driver: .info.props["node.driver-id"], lat: .info.props["node.latency"]}'`,
`pw-link -l`, `pw-top -b -n 2`, and `pw-metadata -n settings`.

## 2026-11-03: audio broken after the source's stream disappears and comes back (task #117)

> **Corrected by #128 (above):** the mechanism was the GStreamer pipeline clock (`pipewiresrc`'s), not the graph; the
> rebuild described here is removed, a re-appearing stream is simply linked.

User report: sharing the Firefox-based browser (YouTube) audio, muting the tab for a moment and unmuting (also: closing and
reopening the browser; sometimes the first selection) gives *"really broken audio"* until the source is selected
again. Firefox destroys its PipeWire stream node when nothing plays and creates a new one (new id, same name) on
resume, so mute/unmute is the #105 node-replacement path.

**What was ruled out (read-only `pw-dump`, `pw-link -l`, `pw-top` on the user's live share, plus the user's own
`pw-link -l` while broken):** the links are correct (one the Firefox-based browser node, FL->FL and FR->FR into
`input.ssshare-loopback-*`, the loopback output 1:1 into the `-cap` stream, no stale or duplicate link). So it is
not the link diff, not channel mapping and not a stale `linked` id.

**Cause: not proven.** What differs between the broken state and the working one (a manual re-select) is only that
the long-running loopback and `pipewiresrc` stream sat through a stretch with *no input* and then got a new stream
linked into the already-running graph; a re-select spawns a fresh loopback, links the source *before* the capture
stream connects, and starts a fresh pipeline. Graph facts that make the gap suspect: the loopback nodes are
`dsp`/`F32P` streams with no rate and `resample.disable=true`, with no `node.latency` of their own; with nothing
linked upstream the loopback output + `-cap` group has no real driver (it falls to `Dummy-Driver`) and only rejoins
the sink's driver (`alsa_output...`, via `easyeffects_sink`) when the new the Firefox-based browser node is linked. A driver, clock or
quantum change under a pipeline that was started against the old one is the likely mechanism; it could not be
measured (no audio tests). Loopback settings (`node.always-process`, `node.passive`, a fixed `node.latency`) were
not tried: they cannot be validated without creating an audio stream.

**Fix (`audio.rs`): do automatically what the re-select does.** `link_poll_loop` tracks a sticky `gap`
(`gap_decision`): set when nothing is linked into the loopback (also at start with the app absent). The next time a
stream with the name has ports and is ready to be linked, `rebuild_chain` spawns a fresh `pw-loopback`, links the
streams into it, sets the pipeline to Null, retargets `pipewiresrc name=src` (`target-object`, `client-name`),
starts it again, re-anchors the wire PTS offset (running time restarts at 0; the offset is now an
`Arc<AtomicU64>` shared with `pump_loop`), proves the capture link (`wait_for_capture_link`, so the microphone
trap cannot come back) and only then kills the old loopback (`AudioCaptureHandle::loopback_child` is an
`Arc<Mutex<Child>>`; a `ChildGuard` kills a half-built one if `stop()` aborts the task). Encoder, appsink, pump and
the relay announcement are untouched: viewers see a short silence, not a new stream. A failed rebuild keeps the gap
flag and retries every second. One of several streams closing does *not* rebuild (the loopback never went empty).
*Why not just tune the loopback:* the symptom is a property of a graph state we cannot observe without creating
audio streams, and the rebuild reproduces exactly the sequence the user showed to work. Cost: the first ~0.5 s of
the resumed sound can be lost while the chain rebuilds.

**Tests (no audio):** `a_stream_reappearing_after_a_gap_rebuilds_the_loopback_once` (mute/unmute, two-tab churn,
start with the app absent), `the_pipeline_delivers_again_after_a_null_playing_restart` (the appsink delivers after
Null -> Playing with a changed source property; running time restarts). **Not verified:** the rebuild against a
real PipeWire and whether it cures the broken sound. If it still happens: with the share broken, send
`pw-top -b -n 2`, `pw-dump` filtered to the `ssshare`/the Firefox-based browser nodes (state, `node.driver-id`) and the client log
(`audio share ... rebuilt the loopback`).
**First selection sometimes not taking:** not explained by this; the start path links the source before the
pipeline connects. Candidates left: the picked name had no stream with ports at that instant (now covered, the poll
rebuilds when it shows up) and a half-linked node (FL linked, FR `pw-link` failed, retried forever on the duplicate
FL; still open, see #105 "Not covered").
## 2026-11-03: switching the shared audio source kept un-muting (task #118)

User report: *"Selecting a new audio stream as the sharer unmutes it for viewers. Fix that. It doesn't reset
the set volume, which is good."* Two mutes were lost on a source switch, both for the same reason (a switch
replaces the whole share / watch):

- **Sharer mute** (the card's Mute, `volume name=mute`): `UiAction::ShareAudio` (`ui/apply.rs`) stops the old
  audio capture and starts a new one with a new pipeline, which starts unmuted. Now the old share's
  `is_audio_share_muted` is read first and passed down (`start_audio_share(.., muted)` ->
  `start_audio_share_with` -> `audio::start_audio_sharing(.., muted)` -> `build_pipeline*(.., muted)`), where the
  element's `mute` is set **before PLAYING**: the replacement never sends one unmuted buffer. The reconnect path
  carries it the same way (`SuspendedShare::Audio { muted }`, read in `suspend_destination`). The #114 boost
  needed no such care: it is an `AppState` setting, not per-share. A staged share (no Go Live yet) has no mute
  to carry, so `GoLive` starts unmuted. The browser sharer has no source switch (one `getDisplayMedia` track per
  capture) and already re-applies `c.audioMuted` to every new destination (`capture.js`).
- **Viewer mute**: `RoomSession::audio_volume_by_owner` remembered only the slider value, so a viewer who had
  muted a sender heard audio resume when that sender switched source (new `StreamId`, fresh `AudioWatch`, the
  old code comment called it a "known, narrow gap"). `audio_muted_by_owner` (a `HashSet<ConnId>`) now does for
  mute what the map does for volume; `start_audio_watch` restores both.
- Test: `audio::tests::a_pipeline_can_be_built_muted`. The UI/session wiring and the PipeWire path are not
  unit-tested (no audio tests are allowed here, see `testing-and-verification.md`).


## 2026-11-03: the shared audio is a name; all streams with that name are mixed (task #105)

> **Superseded in part by I7 and #121/#128:** the shared audio is no longer picked as a name through `AppState::selected_audio_source` / `StagedShare::audio` (both gone): a share carries an `AudioChoice` (Desktop audio, or one named app) and the set of node ids comes from `wanted_ids` (`audio.rs`, replacing `node_ids_named`). The loopback is gone: the share captures a private null sink (`spawn_sink`, #121), and `sync_links` also unlinks nodes that no longer match. The "name, mixed by name" rule and the one-second poll stand.

User request: *"Concise audio streams into streams with the same name only. So we are essentially
only selecting a string in the UI. Check every second for new streams with the same name, if they
have the same name, include them in the share (Combining multiple streams)."*

- **Dropdown:** `audio::list_audio_sources` deduplicates by display name (`dedup_by_name`, first
  node wins, listing order). The `AudioSource` the UI carries is therefore "a name plus a
  representative node"; only `name` matters to the share. The level meter taps the representative
  node only (so a name whose first stream is silent but a later one plays shows a quiet meter; the
  share itself is unaffected).
- **Start:** `start_audio_sharing` links every node named `source.name` (`node_ids_named`, which
  uses the same name derivation and the same self-exclusion as the list) into the loopback input.
  The loopback already sums what is linked into its input, so mixing costs nothing extra.
- **Every second:** `link_poll_loop` (a tokio task owned by `AudioCaptureHandle`, aborted on
  stop/drop; `pw-dump` + `pw-link` run on the blocking pool) calls `sync_links`: link matching
  nodes not yet linked, forget linked ids that vanished (PipeWire removes their links itself). A
  node that cannot be linked yet (no ports) is simply retried next tick. Nothing but nodes with
  the chosen name is ever linked.
- **Zero matches:** the share stays up and silent, and picks the app up when it appears (also
  after a reconnect restores the share by name). Before, a vanished source failed the start.
  Safe because the capture pipeline targets only the loopback's output serial and
  `wait_for_capture_link` still proves that link, so silence can never turn into the microphone
  (the 2026-07-28 trap in `stream-control.md`).
- **Why a name and not ids:** apps (browsers, games, music players, Discord) come and go and often hold
  several streams under one name; a node id is gone with the track or tab.
- **Not covered:** a half-linked node (FL linked, FR `pw-link` failed) is retried and then fails
  forever on the duplicate FL link; rare, logged at debug level. Two *different* apps that share a
  display name are mixed too (that is the requested definition of "same name").
- **Persistence:** none to migrate. The selection lives in `AppState::selected_audio_source` /
  `StagedShare::audio` and was never written to a settings file.
- **Tests:** unit only (the no-audio rule): `audio::tests::dropdown_lists_each_name_once_*`,
  `a_name_matches_all_its_output_streams_and_nothing_else`, `link_diff_adds_new_matches_*`. The
  real mixing (several live streams, a stream appearing/vanishing during a share) was not tested
  here and has to be checked by the user on a real PipeWire.
