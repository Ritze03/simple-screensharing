# Audio sharing: A/V sync (viewer-side audio delay)

Audio capture/encode/playback is described where it grew up: `features/stream-control.md`
("Sharing audio ...", "The 2026-07-28 audio bugs"), `features/video-pipeline.md` ("Audio follows
video") and the module docs of `client/src/audio.rs` and `client/src/decode.rs`. This page covers
**keeping the audio aligned with the picture** (task #91, 2026-11-01), the **shared audio is a name** (#105), the viewer volume (#96) and the **sender-side boost** (#114).

## 2026-11-03: audio broken after the source's stream disappears and comes back (task #117)

User report: sharing Floorp (YouTube) audio, muting the tab for a moment and unmuting (also: closing and
reopening the browser; sometimes the first selection) gives *"really broken audio"* until the source is selected
again. Firefox destroys its PipeWire stream node when nothing plays and creates a new one (new id, same name) on
resume, so mute/unmute is the #105 node-replacement path.

**What was ruled out (read-only `pw-dump`, `pw-link -l`, `pw-top` on the user's live share, plus the user's own
`pw-link -l` while broken):** the links are correct (one Floorp node, FL->FL and FR->FR into
`input.ssshare-loopback-*`, the loopback output 1:1 into the `-cap` stream, no stale or duplicate link). So it is
not the link diff, not channel mapping and not a stale `linked` id.

**Cause: not proven.** What differs between the broken state and the working one (a manual re-select) is only that
the long-running loopback and `pipewiresrc` stream sat through a stretch with *no input* and then got a new stream
linked into the already-running graph; a re-select spawns a fresh loopback, links the source *before* the capture
stream connects, and starts a fresh pipeline. Graph facts that make the gap suspect: the loopback nodes are
`dsp`/`F32P` streams with no rate and `resample.disable=true`, with no `node.latency` of their own; with nothing
linked upstream the loopback output + `-cap` group has no real driver (it falls to `Dummy-Driver`) and only rejoins
the sink's driver (`alsa_output...`, via `easyeffects_sink`) when the new Floorp node is linked. A driver, clock or
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
`pw-top -b -n 2`, `pw-dump` filtered to the `ssshare`/Floorp nodes (state, `node.driver-id`) and the client log
(`audio share ... rebuilt the loopback`).
**First selection sometimes not taking:** not explained by this; the start path links the source before the
pipeline connects. Candidates left: the picked name had no stream with ports at that instant (now covered, the poll
rebuilds when it shows up) and a half-linked node (FL linked, FR `pw-link` failed, retried forever on the duplicate
FL; still open, see #105 "Not covered").

## 2026-11-03: sender-side audio boost and calibration (task #114)

User request: *"Also on the like client settings like the general settings there should be something like
a volume boost because for example on my system it's really quiet for some reason so others barely hear
what I'm streaming and there I should be able to yeah just set a volume multiplier and you could even
include something like a calibration for it so the user can play something on the desktop it then you hit
like calibrate and it just listens to it for 10 seconds and according to that it adjusts the multiplier so
it's on a proper volume that would be really nice"*

**What it is.** Settings > **Shared audio** (a card between Playback and Diagnostics): a **Boost** slider
(50 % to 800 %, default 100 %, persisted as `AppSettings::audio_gain`, a float multiplier, e.g. `audio_gain = 3.2`)
and a **Calibrate** row (an app picker plus a button). The boost is applied to the audio this client *shares*,
before it is encoded; it changes the picture of the sound every viewer gets, not what any one viewer's slider
does.

**Why sender-side, with a limiter (and not more viewer boost).** A viewer can already boost a sharer to 500 %
(#96, below), but that gain runs on the viewer's decoded 16-bit samples, so a source that is quiet *because of
its level at the sharer* ends up hitting full scale on its peaks long before it sounds loud: above about 300 %
it only clips. Every viewer would also have to find and set it per stream. Fixing the level at the source,
before Opus, fixes it once for everyone, and the boost can be done in float where nothing clips until the
limiter says so. A boost without a limiter would just move the clipping to the sender.

**Pipeline** (`audio::build_pipeline`, `audio::GAIN_STAGE`):

```
pipewiresrc ! audio/x-raw,channels=2 ! audioconvert ! audioresample
  ! audio/x-raw,format=F32LE ! volume name=gain ! rglimiter name=limiter
  ! volume name=mute ! level ! audioconvert ! opusenc ! appsink
```

- **F32LE** because `rglimiter` only takes 32-bit float and a boost on 16-bit samples would clip before the
  limiter could see it. `opusenc` (1.28 here) only takes **S16LE**, hence the `audioconvert` after the meter
  (found when the first test pipeline failed to link: `could not link level to opusenc0`).
- **`rglimiter`** (gst-plugins-good, plugin `replaygain`) is a zero-latency waveshaper: soft knee from -6 dBFS,
  output never above full scale. `audiodynamic` was the alternative (also gst-plugins-good, F32 ok); not used:
  it is a per-sample compressor whose ratio-0 setting is a hard clip and whose soft knee only softens the
  ratio, not the ceiling. A real look-ahead limiter would need a plugin the project does not ship; for peaks
  that the calibration keeps within +3 dB of full scale a waveshaper is enough.
- **The limiter is bypassed at 100 % or below** (`rglimiter enabled=false`, `audio_gain::limiter_needed`): it
  would otherwise colour even an unboosted loud signal from -6 dBFS up, and the default must stay exactly what
  sharing audio was before this setting. Boost at or below 1x cannot exceed full scale, so nothing is lost.
- **Live change:** `AudioCaptureHandle::set_gain` sets `volume`'s property and the limiter's `enabled`, no
  restart; `AppState::set_audio_gain` pushes it to every running audio share, saves it, and the next share
  (also a reconnect-restored one) starts with it. The mute `volume` sits after the gain, so mute still wins.
  The VU meter sits after the gain too: it shows what is sent.
- **No `rglimiter` on the system** (the `replaygain` plugin is a separate file): `gain_stage_description`
  leaves the limiter out and logs a warning, so the share still works, just without clip protection. The
  AppImage bundle lists the plugin (`client/packaging/gst-plugins.txt`), `check-deps.sh` warns.

**The slider is in dB, shown as a percent.** The range is 4 octaves; on a linear percent track 100 % would be
7 % from the left end and everything useful (100-300 %) a handful of pixels. In dB each doubling is the same
distance (`audio_gain::gain_db_range`, knob steps of 0.5 dB, the box reads e.g. `223%`). The value is pushed
when the pointer is released, like the other sliders (each setter writes `settings.toml`).

**Calibration** (`audio_gain.rs` pure logic; `session::AppState::start_audio_calibration`, `poll_audio_calibration`):

1. The user plays something, opens Settings, picks the app in the **Calibrate** row (the list is the #105
   deduplicated-by-name source list, re-listed every 2 s while Settings is open; default = the app being shared
   right now, else the first one) and presses Calibrate.
2. `session::build_calibration_tap` re-lists the sources, takes the one with that name and calls
   `audio::start_calibration_tap`: the existing private **level tap** (`pipewiresrc target-object=<serial> !
   audioconvert ! level ! fakesink`, never anything that leaves the process) with a unique `client-name`, then
   `wait_for_capture_link` proves the PipeWire graph really links that node to the tap. *Why the extra proof:* a
   mistargeted meter is a wrong bar, a mistargeted calibration writes a wrong setting (and `pipewiresrc` falls
   back to the microphone when its target misses, `build_pipeline`'s doc). If it never links, the row says
   "Couldn't listen to ...".
3. For **10 s** the tap's bus pump feeds every 100 ms `level` message (loudest channel's RMS and peak, dBFS)
   into a `LoudnessMeter`: a sum of audible windows' power, the peak and counters (O(1) state). The row shows
   "Listening to Spotify... 7 s" and a Cancel button; closing Settings cancels too.
4. **Result:** windows below -60 dBFS are *silence and ignored* (not averaged in), loud windows count by power;
   fewer than 10 audible windows (1 s) is "nothing audible": the row says so and **the boost is left alone**.
   Otherwise `gain_for`: the boost that puts the RMS at **-18 dBFS**, limited so the **peaks stay under +3 dBFS**
   after the boost (`min` of the two in dB), clamped to 50-800 %. It is absolute, not relative to the current
   setting, because the tap measures the source before the gain. It writes the slider (and the file) and the
   row says "Boost set to 398 %. The sound was at -30 dBFS."; if the clamp hit the maximum, or the source was
   already louder than the target and went down to the minimum, it says so.
   *Why -18 / +3:* -18 dBFS RMS is the usual "loud enough with headroom" level; music peaks 10-14 dB above its
   RMS so they land around -4..-8 dBFS where the limiter only rounds them. The peak guard stops a very peaky
   source (speech with the odd loud pop, -35 dBFS RMS with peaks at -10) from being driven by its quiet RMS into a
   wall of limiting: it gets +13 dB (peaks at +3) rather than +17 dB.
5. Mixed shares (one name, several streams, #105): the tap measures the representative node only, like the
   source meter, so the number is that stream's loudness.

**Browser sharer: out of scope.** The browser client shares tab/screen audio through `getDisplayMedia`, which has
no such setting; a boost there would be a separate task (`GainNode` + `DynamicsCompressorNode` in
`docs/js/core/capture.js`).

**Tests** (unit only, the no-audio rule; nothing creates a PipeWire stream):
`audio_gain::tests::*` (clamp/dB/range, silence and blip handling, silent stretches ignored, power vs dB
averaging, gain for quiet / peaky / very quiet / loud sources and the limit flags, the whole calibration flow
with synthetic levels and a fake clock incl. "heard nothing" and "tap failed"), `audio::tests::the_limiter_keeps_a_boosted_full_scale_sine_under_full_scale`
(an `audiotestsrc` full-scale sine at +18 dB peaks at exactly 1.0 through the real gain stage into an
`appsink`; `without_the_limiter_...` is the control, peak 7.9), `no_boost_in_the_range_clips_past_full_scale`,
`a_boost_below_the_knee_is_a_plain_multiplication`, `the_default_gain_leaves_a_full_scale_signal_untouched`,
`the_gain_changes_live_without_restarting_the_pipeline`, `the_sender_pipeline_boosts_then_limits_before_mute_meter_and_encoder`
(element order in the real description), `the_sender_chain_negotiates_and_encodes_opus` (the whole sender chain
with `audiotestsrc` for `pipewiresrc`), `level_messages_feed_the_loudness_meter` (real `level` messages from an
`audiotestsrc ! level` bus), `rooms::tests::audio_gain_defaults_to_no_boost_and_is_clamped_on_load`,
`ui::tests::audio_boost::*` (persistence, calibration via `Calibration::listening` with a synthetic meter, cancel,
the modal drawing every state). Seen in the headless rig: the Settings card in English and German (fits), a click on
the slider writes `audio_gain = 2.23` to `settings.toml` and the box reads `223%`.
**Not verified:** a real calibration against a real playing app, the live gain change on a real share, and how it
sounds; those need the user's PipeWire (steps in the task report).

## 2026-11-03: the viewer volume slider scrolls with the mouse wheel (task #113)

User request: *"make sure to make the volume slider scrollable (5% steps, above 200% it should be 10%."*
Every viewer volume slider (native tile pill, pop-out / fullscreen pill, browser tile pill) steps with the wheel.

- **Rule.** One notch = one step, wheel up/forward = louder. 5 % while the value is below 200 %, 10 % from 200 %
  up; clamped 0-500 %. The step is the one of the segment being *entered*, so the boundary is symmetric:
  195 -> 200 -> 210 going up, 210 -> 200 -> 195 going down; an off-grid value (205 from a drag) lands on the
  next multiple of the step in the direction of travel (205 -> 210 up, 205 -> 200 down). Native `ui::wheel_step`
  (pure, `ui/tests.rs`), browser `ui/dom.js` `wheelStepVolumePct` (`docs/dev/tests.js`).
- **Native wiring.** `ui::volume_slider` (the one function both pills call) asks `widgets::wheel_notches` for whole
  notches over its `Response` and applies `wheel_step` per notch; it marks the response `changed`, so the tile and
  pop-out code paths that already handle a click (`changed && !dragged` -> `SetVolume`) need no change.
  Notches come from `raw_scroll_delta` (40 points = one notch, egui's line speed; touchpad pixel deltas accumulate).
  **The wheel is consumed:** egui's `ScrollArea` reads `smooth_scroll_delta` in its `end()`, i.e. after the content
  ran, so `wheel_notches` zeroes `smooth_scroll_delta.y` while the pointer is on the slider and the room panel under it
  does not scroll. (`ui::tests::wheel_over_the_volume_slider...` runs real `MouseWheel` events through a
  `ScrollArea` and fails without that line.)
- **Browser wiring.** `volumeWheel(slider)` (`ui/dom.js`, called from `audioPill`): a non-passive `wheel` listener that
  `preventDefault()`s (no page scroll), sums `deltaY` (lines x 33), steps once it reaches half a notch
  (|deltaY| ~100 = one notch; a big flick = several), sets `slider.value` and dispatches an `input` event.
- **Detent vs wheel.** The 100 % detent is for pointing (drag/click). A wheel step goes through 100 normally
  (95 -> 100 -> 105). Native: the wheel path simply does not call the snap. Browser: the dispatched `input` event carries
  `fromWheel = true` and the handler skips `snapVolumePct` for it. **Why** a flag: without it 100 -> 105 snapped
  straight back to 100 (|105 - 100| <= 10) and the wheel got stuck at 100 %.
- **Persistence unchanged** (`RoomSession::set_volume`, `WantedStream::volume_percent`, `w.vol`).
- **Verified.** Unit tests (native `wheel_step_is_5_up_to_200...`, `wheel_over_the_volume_slider...`; browser two
  `volume: wheel ...` tests in `docs/dev/tests.js`, 53/53 in headless Chromium). In the headless rig, in the widget
  gallery's "Viewer volume" card, with real wheel events (new `hs-vptr scroll N`, a virtual-pointer axis event through
  sway -> winit): 100 -> 105 (1 notch), 105 -> 210 (20 notches), 210 -> 200 -> 195 (down 2); the gallery column did not scroll
  while the pointer was on the slider and scrolled 120 px for 3 notches beside it. **Not verified:** the real tile and
  pop-out pills and the browser pill in the real site (a pill only exists for a sharer with audio; no audio tests).

## 2026-11-03: the bar's audio meter stayed flat (task #115)

User report: *"in the bottom bar when streaming audio, it doesn't do the visualizer thingy"*.
No audio test was possible (hard rule), so the whole path was audited and pinned with tests that
need no PipeWire:

- **Path (all intact):** `level` element in `audio::build_pipeline` (after `volume name=mute`) ->
  bus -> `pump_loop` -> `handle_bus_message` -> `AudioLevel` (atomic) ->
  `AudioCaptureHandle::level` -> `AppState::audio_share_level` (only while the audio capture is
  `Active`) -> `action_bar::live` (`level_lit`) -> `AudioSlot` -> `widgets::meter`, repainted every
  100 ms (`request_repaint_after` in `live`; the app repaints every 50 ms anyway). The chain has not
  changed since the July audio fix (`af5b8a6`); #105 did not touch it (it only changed which nodes
  are linked and which node the *dropdown* taps).
- **Tests:** `audio::tests::capture_chain_level_messages_reach_the_level_value` runs the real
  capture chain (`build_pipeline_from_source`, `audiotestsrc` for `pipewiresrc`, bus and appsink
  streams created late as in `start_audio_sharing`) and asserts the level moves; 
  `ui::action_bar::tests::the_bars_audio_slot_paints_as_many_lit_bars_as_the_level_says` renders
  the slot and counts lit bars.
- **One real defect found: the bar could show the dying share.** `ui::paired_audio` took the first
  audio capture pointing at the video capture. After switching the audio source (or a reconnect
  restart) the old one sits in `Stopping` next to the new one until its teardown lands, so the bar
  showed the old name with a flat meter (a stopping share has no level) and Mute hit the wrong
  capture. It now prefers any non-`Stopping` capture
  (`ui::tests::the_bar_follows_the_live_audio_share_not_the_stopping_one`).
- **Logging:** `pump_loop` logs when the appsink ends (the meter and the audio stop together; copy
  from Settings > Diagnostics > Logs).
- **Not proven:** a flat meter while audio really flows cannot come from this code; if it still
  happens on a real PipeWire, the logs above plus `pw-link -l` (is the app linked into the
  `ssshare-loopback-*` input?) are the next thing to look at.

## 2026-11-03: the shared audio is a name; all streams with that name are mixed (task #105)

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
- **Why a name and not ids:** apps (browsers, games, Spotify, Discord) come and go and often hold
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

## 2026-11-02: viewer volume goes up to 500 % (task #96)

User request: *"Let users adjust up to 500% volume when watching."* The per-tile volume control
(native tile pill and pop-out pill, browser tile pill) was 0-200 %; it is now **0-500 %**, default
still 100 %.

- **Native.** `session::MAX_VOLUME_PERCENT = 500` (clamped in `RoomSession::set_volume` and
  `effective_gain`), `decode::MAX_AUDIO_GAIN = 5.0` (clamp in `AudioDecoder::set_volume`; a test
  pins the two equal). The decode chain's GStreamer `volume` element already takes 0-10, so the
  only change is the cap: gain stays `percent / 100`, mute is still gain 0.
- **Browser.** `core/audio.js` `MAX_VOLUME = 5`. Playback already runs through a WebAudio
  `GainNode` (`HTMLMediaElement.volume` would have capped at 1), so again just the cap.
- **Persistence unchanged.** `RoomSession::audio_volume_by_owner` (survives the sender restarting
  its audio, `volume_survives_sender_audio_restart_or_switch`) and `WantedStream::volume_percent`
  (survives a reconnect) store the percent as before; browser: `w.vol` and the 60 s resume list.
  Nothing is written to disk, so there is no migration; any 0-200 value is still valid.
- **Clipping: left as a hard clip, on purpose.** Above 100 % a loud source runs into full scale.
  Native: the `volume` element works on the decoded 16-bit samples and *saturates* (no integer
  wrap; `decode::tests::volume_boost_saturates_instead_of_wrapping` feeds 10000/-10000/20000
  through `volume=5.0` and expects 5 x / full scale). Browser: the gain is a float, the device
  clips at +-1.0. `Why` no limiter or soft clip: the feature exists to lift *quiet* sharers; a
  limiter adds a stage, latency and a threshold to tune for a level the viewer picked on purpose.
  If clipped loud sources become a complaint, the place for a limiter is a `GainNode` ->
  `DynamicsCompressorNode` (browser) / `audiodynamic` or `rglimiter` after `volume` (native).
  (A quiet *sharer* is fixed at the source instead, by the sender-side boost with a limiter, #114, above.)
- **Slider: 100 % detent.** The 110 px pill slider covers 500 % in about 96 px of travel (~5 %/px),
  so hitting 100 % exactly would need pixel precision. The slider draws a 1.5 px tick at 100 % and
  snaps anything within +-10 % onto it while dragging or clicking (native
  `widgets::slider_detent` + `ui::volume_slider`, `VOLUME_DETENT_SNAP`; browser `ui/dom.js`
  `snapVolumePct` + a `.vol` track gradient in `app.css`). Consequence: 91-109 % are not
  selectable; 90 and 110 are. Everything else keeps the 5 % step. Shown as `NNN%` right of the slider
  (the 34 px slot fits `500%`).
- **Verified.** Unit tests (`effective_gain...`, `slider_detent_snaps_only_within_its_distance`,
  `volume_boost_saturates...`, browser `volume: ...` x2); the slider itself in the widget gallery
  inside the headless rig (new "Viewer volume" card: 40 % / 100 % / 500 %, a click at ~107 % lands on 100 %).
  The real tile pill could not be shown: it only exists for a sharer with audio and the rig's
  clients share none (no audio tests, `claude-instructions/testing-and-verification.md`).

## 2026-11-01: audio is delayed to match the video, dynamically

User request: *"Make sure that the audio is delayed, to stay in sync with the video (dynamic as
well)."* Context: the user keeps x264's "Best quality" tune, whose lookahead adds about 0.7 s
(medium, `rc-lookahead` 40) to about 1 s (veryslow, 60 frames at 60 fps) of video latency; GPU
encode adds little. Audio had no delay, so a viewer heard sound up to a second before the
matching picture.

### What was wrong

1. **No comparable timestamps.** Both media kinds carry an 8-byte capture `pts` inside the AEAD
   (`protocol-spec.md` 6.4), but on a native sender they counted from different zeros: video from
   its destination's creation (`capture.rs` `DestinationRuntime::pts_epoch`), audio from the audio
   pipeline's own running time. A viewer could not tell which packet belongs to which frame.
   (The browser sender already stamped both from one `MediaClock`.)
2. **Independent playout.** Native: the video appsink is `sync=false` (frames show when decoded,
   no pacing); audio played at `now + 100 ms` from its own re-based timeline. Browser: video and
   audio each anchored themselves on their own first packet. Nothing connected the two.

### Design

**One sender clock.** Every wire PTS a process sends is *capture time* in microseconds since one
process-wide epoch, `decode::sender_clock_epoch()` (`decode/avsync.rs`). Video: that epoch is now
the `pts_epoch` of every `DestinationRuntime` (one line in `capture.rs::spawn_destination`).
Audio: `audio.rs` maps its pipeline running time onto the same axis with an offset read at
pipeline start (`sender_clock_offset_us`, `audio_wire_pts_us`). No wire-format change: still a
`u64` of microseconds; only the meaning is now "same clock for all streams of one sender".
Spec note: `protocol-spec.md` 6.4.

**Viewer: match latencies, not clocks.** The viewer does not know the sender clock's offset to
its own and does not need it. Per stream it tracks a *latency floor*, the windowed minimum of
`arrival - pts` (3 s window). The unknown offset is identical in both floors:

```
delta       = floor(video) - floor(audio)           // how much later than audio the picture arrives
extra delay = clamp(delta - AUDIO_LEAD (100 ms), 0, 2 s)
```

The video floor includes the encoder lookahead because `pts` is the *capture* time, not the
encode-output time. That is the only way the viewer can see a sender-side delay.

**Native** (`decode/avsync.rs`, wired in `decode.rs`): `StreamDecoder::feed` and
`AudioDecoder::feed` find each other through `AvSync::for_peer(conn_id)`, a registry of `Weak`
handles keyed by the sender's `ConnId`; `session.rs` needed no change. `AudioDecoder::feed`
sends the current target with each packet; the audio thread smooths it with a `DelayController`
and applies it in `AudioTimeline`: the delay is part of the lead (`AUDIO_TARGET_LEAD_US + delay`),
and a change of the applied delay re-bases the timeline once.

**Browser** (`docs/js/core/avsync.js`, wired in `video-rx.js`, `audio.js`, `room-client.js`):
same maths. The video side reports when its jitter buffer will release each frame
(`JitterBuffer.lastDecodeAtMs`), so the video's own jitter buffering is matched too. The audio
receiver re-anchors its WebAudio schedule when the applied delay changes; on a decrease it stops
queued sources that would overlap.

### Behaviour, in one table

| Situation | Audio delay |
| --- | --- |
| Only audio watched (Listen, or no video decoder for that sender) | none (0) |
| Video + audio, healthy | `delta - 100 ms`, in `[0, 2 s]` |
| Video latency rises (settings change, encoder rebuild, lookahead) | follows within about 3 s (window), one step |
| Video latency falls | follows at once (a minimum), one step |
| Estimate wobbles by < 60 ms | ignored (deadband) |
| Two steps close together | rate-limited to one per 500 ms |
| Video stalls or the screen is idle (no frames) | **last delay is held**: no information is not a reason to move audio |
| Stalled-capture keepalive frames (the sender re-sends a cached IDR, `stalled` flag) | ignored for the floor: they carry no encoder latency |
| Video watch ends | back to 0 |
| `delta` outside [-1 s, +3 s] | treated as "not one clock" (older sender), no delay |
| Latency above 2 s | capped at 2 s: audio then leads the picture |

### Why

- **Why delay audio instead of speeding video up?** Video is already shown as soon as it is
  decoded (latency is the project's goal); the only knob that does not cost latency elsewhere is
  holding the cheap stream, audio, back. The price is that audio latency equals video latency.
- **Why a minimum filter?** The floor is "the path with nothing queued"; jitter only adds. A mean
  or median would wobble with network noise and turn into audible re-timing.
- **Why steps with a deadband instead of a smooth slew?** `opusdec` (a `GstAudioDecoder`) ignores
  input timestamp shifts below its 40 ms tolerance anyway, so sub-40 ms nudges would do nothing,
  and a continuous time-stretch needs an extra element (`pitch`/`scaletempo`) that is not
  guaranteed to be installed. A re-time is rare (a settings change), so one clean step (a short
  silence when the delay grows, a short skip when it shrinks) beats constant processing.
- **Why a process-wide epoch and not the destination epoch?** Audio and video are separate
  captures with separate destinations; the only thing they share is the process.
- **Why match the first 100 ms lead?** The audio path's own playout lead (jitter headroom) is
  already a delay audio has and video does not; adding the full `delta` on top would overshoot.

### Known limits and risks

- **Real audio playback was not tested** (hard project rule: no audio tests, never create a
  PipeWire audio stream, never touch the user's Spotify; see
  `claude-instructions/testing-and-verification.md`). The estimator, controller and timeline are
  covered by unit tests with synthetic timestamps (`avsync::tests`, `decode::tests::timeline_*`,
  `audio::tests::audio_wire_pts_*`, `docs/js/core/avsync.test.mjs`). How the real sink behaves at
  a re-time step (silence on increase, drop on decrease) is reasoned from GStreamer's
  `audiobasesink`/`basesink` behaviour, not measured.
- **Startup step.** Audio starts with delay 0 and steps once the first video samples exist
  (typically within a frame of the first audio packet, since the video's pre-keyframe frames
  count). If the first audio packet beats the first video frame, the first second can contain one
  re-time. Closing this fully needs `session.rs` to tell the audio decoder "this is paired with
  video" at creation.
- **Older senders** (before this change) stamp audio on its own epoch. The plausibility window
  rejects most of that, but if audio was started up to a second *before* the share the delay can
  be off by up to that much. A sender that signals "shared clock" (an announce flag) would remove
  the guess; not added, it would touch the protocol crate.
- **Video decode and sink latency are assumed equal** (VA decode is a few ms; the audio sink adds
  its own buffer, not queried). Expect a residual of a few tens of ms.
- **Video floor uses arrival, not presentation, on native** (the renderer pulls frames its own
  way). Browser uses the jitter buffer's release time.
- **Capturing sources that stamp unusual PTS** are unaffected: only `buffer pts + offset` reaches
  the wire, as before.
