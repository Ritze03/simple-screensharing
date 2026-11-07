# Levelling, boost and volume

Viewer loudness levelling, sender boost, volume sliders (500 %, wheel, live drag) and the share meter (#96, #113, #114, #115, #119, #135). Hub: `../audio-sharing.md`.

The text moved here verbatim from the Audio sharing page: "above", "below", "this page" and "the #128 block" in it may point at a section that now lives in a sibling page of this folder (the hub lists them).

## 2026-11-03: volume sliders apply while dragging, not on release (task #135)

User request: *"make sure that when the volume slider is moved it instantly sets the volume even while moving and not
only when the user actually lifts his mouse button."* **Why:** the user wants to hear the level while choosing it.
Audit of every volume-type slider:

- **Native tile pill** (`ui/room.rs` `tile_volume_slider`) was the bug: it pushed `UiAction::SetVolume` only on
  `drag_stopped()` (or a change that was not a drag), so the decoder's `volume` element moved on release only. It now pushes
  on every `changed()` frame. `SetVolume` writes only the in-memory `AudioWatch` and the decoder's `volume` property
  (nothing on disk), so one per frame is cheap. `RoomState::vol_drag` still keeps the shown value one frame ahead of the session.
- **Native Boost slider** (Settings > Shared audio, `modals.rs` `boost_action`) was the bug: pushed on pointer release because
  `set_audio_gain` saves `settings.toml`. Split into `AppState::apply_audio_gain` (live to every running audio share, no write)
  behind the new `UiAction::PreviewAudioGain` on every changed frame, and `set_audio_gain` (= apply + save) behind
  `SetAudioGain` once on release. The jitter and catch-up sliders are not volume and keep push-on-release.
- **Native pop-out / fullscreen pill** (`ui/popout.rs` `audio_pill`) was already live: `.changed()` of the same `volume_slider`
  fires on every dragged frame and goes through `Shared::pending_volume` (pop-out) or straight out (fullscreen).
- **Browser viewer slider** (`docs/js/ui/app.js` `audioPill`; since I6 the volume row of the stream menu, `docs/js/ui/menus.js`) was already live: `input` listener calling `setWatchVolume`
  (the `change` event is not used). The browser has no Boost slider; the other `sliderCtl` sliders (bitrate, jitter, ...) are not volume.

Tests: `ui::tests::the_tile_volume_slider_pushes_set_volume_while_the_button_is_still_down` (real egui press + drag events,
no release; fails on the old code), `the_boost_slider_previews_while_the_pointer_is_down_and_saves_on_release`,
`audio_boost::previewing_the_boost_applies_it_without_saving_until_set`; browser `volume: sliders apply on the input event`.
Not verified in the rig: it has no audio (host audio tests are banned), so the audible effect is covered by the unit tests only.


## 2026-11-03: viewer-side loudness levelling, calibration removed, boost capped at 200 % (task #119)

> **Moved in I7:** the sender Boost is now in the audio settings menu, not Settings > Playback, and "Level loudness" is a button on the stage's control bar (and next to each volume slider), no longer a Settings checkbox. The calibration removal and the 200 % cap stand.

User request: *"Do the loudness levelling. Remove the calibration, cap volume boost to 200%. The auto levelling
should happen on the viewers side. Make sure that it is protected against sudden audio increases."* Context: the
viewer slider is a linear amplitude (500 % = +14 dB, loud sources clip above ~200-300 %), and the 10 s calibration
(#114) was disliked.

**What changed.**
- **Removed (sender, #114):** the Calibrate row and its banner, `LoudnessMeter`, `gain_for`, `Calibration`,
  `audio::start_calibration_tap` and the loudness path of the level tap, `AppState::audio_calibration*`, the
  `CalibrateAudio` / `CancelAudioCalibration` actions and their strings and tests. The level tap itself stays (the
  source dropdown meters). `audio_gain.rs` is now only the range and conversions.
- **Boost capped at 200 %** (`audio_gain::MAX_GAIN = 2.0`, slider 50-200 %, still in dB so 100 % is mid-track).
  `AppSettings::audio_gain()` clamps a bigger stored value (the old range went to 800 %) to 2.0 on load. The
  `volume name=gain ! rglimiter` sender stage is unchanged (limiter bypassed at <= 1x). *Why 200 %:* with the viewer
  levelling, the sender boost is only a manual trim; more only lifts the noise floor.
- **New: viewer-side "Level loudness"** (Settings > Playback, default **on**, persisted as
  `AppSettings::level_loudness`, live, native and browser). The viewer's volume slider and mute act after it.
  *Follow-up #126 (2026-11-03):* the same global switch also has a small toggle button right after every volume slider
  (native tile / pop-out / fullscreen pill, browser tile pill), so the viewer sees the state and can change it where they
  adjust the volume. Still one global setting, not per stream. Details: `ui/SETTINGS.md`.

**Why viewer-side.** Every viewer has a different output chain and listening level, and watches several sharers
with different levels at once: the level that sounds right is a property of the *listener*, so it is the listener's
client that has to even it out. A sender-side normaliser would also change what every other viewer gets, and cannot
protect a viewer from a sudden loud passage on *their* device.

**Why no `audioloudnorm` / lookahead normaliser.** gst-plugins-rs `audioloudnorm` is an EBU R128 normaliser that
works with a ~3 s lookahead buffer: it would add that much delay to a live stream with a picture (and break A/V
sync, #91), and it is not installed everywhere. `rglimiter` is a soft-knee waveshaper from -6 dBFS (colours
everything loud, no hard ceiling for large overshoots) and `audiodynamic` is a static compressor with no target
loudness: neither levels. So the leveller is ~60 lines of DSP we own (`client/src/leveller.rs`, no new dependency).

**Where (native).** `decode::run_audio_pipeline`:
`appsrc ! opusdec ! audioconvert ! audioresample ! capsfilter(F32LE) ! volume name=vol ! audioconvert ! sink`, with
a **buffer pad probe on the capsfilter's src pad** (`leveller::install`) that edits the float samples in place. No
element, no queue, no lookahead: **zero added latency** (it processes the buffer that is passing). The trailing
`audioconvert` is a pass-through for an F32 sink and a safety net otherwise. The switch is the process-wide
`leveller::ENABLED` (an `AtomicBool` read per buffer, like `diag_upload::set_enabled`), so toggling applies to every
running stream at once; switching off resets the DSP state.

**The DSP (`leveller::Leveller`, per stream, any chunking gives the same result).**
1. *AGC*, feed-forward (detectors see the input, not the output). Mean power over the channels feeds a **slow**
   detector (one pole, 400 ms) and a **fast** one (5 ms attack / 80 ms release, a peak-ish RMS without ripple on
   low tones). Gain is recomputed once per 1 ms block and ramped linearly across it.
   - *Target* -18 dBFS short-term RMS (about -16 LUFS for typical stereo content), gain clamped to **-20 .. +12 dB**.
   - *Slow up:* rises at most **4 dB/s** (tapering into the target), and **not at all while the slow level is below
     -50 dBFS** (the gate: room noise, the gap between two tracks, silence stay a floor).
   - *Down:* falls at up to 12 dB/s while the slow level is above target; and **fast down:** when the fast detector
     says the output would be more than **4 dB above the target** (headroom: a drum hit or plosive passes without
     pumping) the gain is pulled to the level that fixes it with a **5 ms** time constant.
   - It starts at 0 dB, so a stream is never boosted before it has been heard.
2. *Peak limiter*, per sample, **zero attack**: the sample's gain is `min(released memory, ceiling / |peak|)`; the
   memory releases over 100 ms. Ceiling **-1 dBFS**. The output therefore **cannot** exceed the ceiling, even for the
   first sample of a sudden transient, before the AGC reacted (a final clamp removes float rounding; NaN/Inf input
   is zeroed). Cost: for the few ms the AGC needs, a big transient is flattened (the intended trade for the
   viewer's ears).

Measured on synthetic signals (unit tests + a trace): -40 dBFS sine for 6 s: gain 3.6 dB after 1 s, 7.6 dB after
2 s, 11.9 dB after 5 s (4 dB/s, stops at +12). A 0 dBFS step on top of that (gain +12 dB): output peak never
above -1 dBFS from the first sample, gain -7.8 dB after 10 ms, -13 dB after 50 ms, output settles near -17 dBFS RMS.

**Browser (`docs/js/core/leveller.js`).** A line-for-line port of the native DSP, applied to each decoded 20 ms
Opus frame **before it is scheduled** (`AudioReceiver.#play`: planar f32 planes -> `Leveller.process` ->
`copyToChannel`); the viewer `GainNode` after it is unchanged. *Why not WebAudio nodes (an `AnalyserNode`-driven
gain + `DynamicsCompressorNode`):* the compressor has a ~6 ms lookahead and no hard ceiling (a guarantee is what
was asked for); a `setInterval`-driven AGC is throttled to 1 Hz in a background tab while the decoder callback is
not; and the port is exactly testable with plain arrays, with the same scenarios and thresholds as the Rust tests.
Same switch ("Level loudness", `S.settings.levelLoudness`, default on, Settings > Playback; `RoomClient.setLevelLoudness`
applies live to all running audio receivers; i18n en + de).

**Tests (no audio streams):** native `leveller::tests::*` (a -40 -> 0 dBFS step never exceeds the ceiling, also in the
first 2 ms, and settles within 3 dB of the target; a quiet signal is raised <= 4 dB/s and stops at +12; silence and a
-65 dBFS floor are not boosted (gain exactly 0); loud material is pulled down within 100 ms; random bursts at
0.001..40 amplitude plus NaN/Inf stay under the ceiling; chunking-independent; mono; the real pad probe on an
`audiotestsrc ! F32LE ! identity ! appsink` pipeline levels with the switch on and leaves the buffers byte-identical
off), `rooms::tests::level_loudness_defaults_to_on`, `audio_gain_*` (clamp to 2.0, old 3.2 -> 2.0),
`ui::tests::audio_boost::*`; browser (`docs/dev/tests.js`, 75/75) five `leveller:` tests incl. the real
`AudioReceiver` driven through a stubbed `AudioDecoder` with a real `OfflineAudioContext`.
**Verified in the headless rig:** Settings shows "Level loudness" (checked by default) in Playback and a Shared audio
card with only the Boost slider (100 % mid-track, the end reads 199-200 %, no Calibrate); clicking the checkbox wrote
`level_loudness = false` to `settings.toml`; headless Chromium: the browser checkbox defaults on and toggles + persists.
**Not verified:** how it sounds, and the real `autoaudiosink` negotiation of the `F32LE` + `audioconvert` tail (no audio
tests). If a sink refuses the chain the symptom is a pipeline error in the log (`audio decode pipeline`); the switch
(Level loudness off) does not remove the F32 stage, only the processing.
**Tuning knobs if it sounds wrong** (all constants at the top of `leveller.rs` / `leveller.js`, keep them in step):
`TARGET_DB`, `RISE_DB_PER_S`, `GATE_DB`, `HEADROOM_DB`, `CEILING_DB`.


## 2026-11-03: sender-side audio boost and calibration (task #114)

> **Superseded in part by #119 (above):** the calibration described here, the 800 % maximum and the "Calibrate" row
> are gone; the boost now ends at 200 %. The pipeline and limiter sections still hold. Kept as history.

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
distance (`audio_gain::gain_db_range`, knob steps of 0.5 dB, the box reads e.g. `223%`). The value is applied
live while the thumb moves and saved on release (#135, below); the other Settings sliders still push on release.

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
   "Listening to <app>... 7 s" and a Cancel button; closing Settings cancels too.
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
`docs/js/ui/capture.js`).

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
- **Browser wiring.** `volumeWheel(slider)` (`ui/dom.js`, called from the stream menu's volume row in `menus.js`; from `audioPill` before I6): a non-passive `wheel` listener that
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

> **Removed in I3 (the stage):** `action_bar::live`, `AudioSlot` and the test `ui::action_bar::tests::the_bars_audio_slot_paints_as_many_lit_bars_as_the_level_says` no longer exist; the stage's control bar replaced them (task I3). The level path (`capture_chain_level_messages_reach_the_level_value`) and the `paired_audio` finding below are history.

User report: *"in the bottom bar when streaming audio, it doesn't do the visualizer thingy"*.
No audio test was possible (hard rule), so the whole path was audited and pinned with tests that
need no PipeWire:

- **Path (all intact):** `level` element in `audio::build_pipeline` (after `volume name=mute`) ->
  bus -> `pump_loop` -> `handle_bus_message` -> `AudioLevel` (atomic) ->
  `AudioCaptureHandle::level` -> `AppState::audio_share_level` (only while the audio capture is
  `Active`) -> (until task I3, 2026-11-04) `action_bar::live` (`level_lit`) -> `AudioSlot` -> `widgets::meter`; the control bar has no share meter any more, the stream-settings modal's Audio row still reads it, repainted every
  100 ms (`request_repaint_after` in `live`; the app repaints every 50 ms anyway). The chain has not
  changed since the July audio fix (`af5b8a6`); #105 did not touch it (it only changed which nodes
  are linked and which node the *dropdown* taps).
- **Tests:** `audio::tests::capture_chain_level_messages_reach_the_level_value` runs the real
  capture chain (`build_pipeline_from_source`, `audiotestsrc` for `pipewiresrc`, bus and appsink
  streams created late as in `start_audio_sharing`) and asserts the level moves; 
  `ui::action_bar::tests::the_bars_audio_slot_paints_as_many_lit_bars_as_the_level_says` rendered
  the slot and counted lit bars (removed with the slot in I3).
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
