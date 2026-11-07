# Sharing audio

Audio as an attachment on a screen share: capture, meter, mute, pairing, the 2026-07-28 audio bugs.

Part of the Stream Control reference, hub: `superdoc/features/stream-control.md`. The text moved here verbatim from that page: "above", "below" and "this page" in it may point at a section that now lives in a sibling page of this folder (the hub lists them).

## Sharing audio: an attachment on a screen share's card

> **Superseded in part 2026-11-04 (task I7, plan D9; `features/audio/desktop-audio.md`):** audio is no longer an attachment on a screen share. The audio button of the
> control bar is independent of the screen: no "+ Add audio on a card", no "paired with video" invariant (`start_audio_share(room, None, ..)` is what the UI
> always passes; `for_capture: Some(..)` survives for a share paired with a screen, whose Stop then stops the audio too). Stopping the screen leaves the audio
> running. A viewer sees an audio-only sender as a **Listen** tile (`Watch` is for video; a sender with a screen still gets its audio with the watch,
> "Audio follows video"). Everything below is the 2026-07-28 reasoning, kept where it still explains the pipeline.

**Added 2026-07-28.** **Superseded the same day** — the section below described audio as
its own `LocalCapture` *and* its own card, started from a standalone SHARE AUDIO panel.
The capture is still its own `LocalCapture` (that part is unchanged); the **UI is not**.
See "Audio is a row on the share, not a card of its own" below for what ships now, and
"The 2026-07-28 audio bugs" for why the first pass of this never actually worked.

The **streamer's** side keeps the opt-in the project owner asked for: *"The streamer
should be able to enable audio on his share."* Audio is still its own `LocalCapture` with
its own `StreamKind::Audio` stream id on the wire, started via `list_audio_sources` →
the audio button's `AudioChoice` → `start_audio_share`. Your own outgoing audio destination still
appears in your OUTGOING sidebar list with a `volume_up` glyph — that is your own share,
not a viewer-facing artifact. (A 2026-07-28 investigation initially misread some
UI branches as the viewer-facing "separate audio stream" to delete; they were the sharer's own
outgoing rows and were left alone.)

What changed is entirely on the **watcher's** side: a viewer never sees another person's
audio as a separate stream, row, or "Listen" action. Watching someone's video
automatically subscribes and plays their audio, and stops it again when the watch stops.
The full architecture, the rejected alternatives, and the four cases that broke the
previous "latch the pairing at watch time" approach are in
`superdoc/features/video-pipeline.md`'s "Audio follows video" section — including why
audio is played in-process rather than relayed to a display layer like video was.

Note this leaves the two halves deliberately asymmetric, and that is the point: *sending*
audio is an explicit choice the sharer makes about their own machine, so it has UI;
*receiving* it is not a choice at all, so it has none (beyond a per-tile volume/mute knob
on the video tile in the View tab).

---

## The 2026-07-28 audio bugs: the sharer was broadcasting their microphone

**Added 2026-07-28.** The "bundle audio with video" pass shipped with a passing synthetic
end-to-end test and did not work at all in real use. The owner's report: the audio share
sat on **STARTING** forever, and the viewer heard **nothing** even though a volume slider
had appeared on their tile. Three separate defects, all in
`simple-screensharing-client`:

### 1. `pipewiresrc path=<node id>` silently captured the microphone

`audio.rs::build_pipeline` built `pipewiresrc path={loopback_output_node_id}`, where the
id came from `pw-dump`'s top-level `id` field.

- `path` is **deprecated** (`gst-inspect-1.0 pipewiresrc`); `target-object` replaces it.
- Both take *"the source name/serial"* — a node's **`object.serial`** or its `node.name`.
  Neither takes `pw-dump`'s registry `id`, which is a *different number*: measured live on
  the test machine, the loopback output node was registry id `333`, `object.serial`
  `18643`. The registry id therefore matched nothing.
- **A `pipewiresrc` whose target does not resolve does not fail.** `node.autoconnect`
  defaults to `true`, so the session manager links it to the **default audio source** —
  the microphone. Confirmed live with `pw-link -l`: the capture stream came up linked to
  `<audio-effects-app>_source:capture_FL`.

So the shared application's audio was never captured, and the sharer's mic was being
Opus-encoded and broadcast to the room instead. Nothing downstream could tell: the
pipeline reached `PLAYING`, packets flowed, the viewer's decoder ran.

`Why the tests missed it:` the end-to-end test drove its synthetic source at `volume=0`,
on the reasoning that "every assertion here is about pipeline liveness and subscription
state, none of which depend on amplitude". That reasoning was the bug — a test that
cannot distinguish "audio is flowing" from "audio is not flowing" cannot protect an audio
feature. It now runs at `volume=0.02` (inaudible, but *not nothing*) and asserts the
sender's real capture level and the receiver's real packet count.

**Fixes:** `target-object=<object.serial>`; `audio::wait_for_capture_link` polls `pw-dump`
after the pipeline starts and refuses the share unless a link genuinely exists between the
loopback output node and our own capture stream node (found by the `client-name` we set).
That check is not paranoia — a mis-targeted capture is a *privacy* failure, and it is the
only place that can detect one. It doubles as the real "capture started" gate:
`Element::state()` reports `Ok(StateChangeSuccess::Async)` for a live source still stuck
waiting to negotiate, which the old `if let Err(..)` check read as success.

`channels=2` is now pinned with a caps filter too: without it PipeWire negotiated **mono**
and linked only `output_FL`, so shared music silently lost its right channel.

### 2. "STARTING" forever

`Destination::state()` derives everything from `runtime`, the *video* encode branch's
shared state, and maps `runtime: None` to `Starting`. An audio destination never has a
runtime (there is no per-destination audio geometry or stall machinery), so for video
`Starting` meant "no IDR yet" and cleared within a beat, while for audio it was permanent.

**Fix:** `LocalCapture::destination_state(dest)` folds the capture's own `ShareStatus` in
for audio captures (`Starting` while `start_audio_sharing` is in flight, `Live` once it
returns `Ok` — which now means *verified* capture, per the link check above).
`dto::destination_rows`, `driver.rs`'s state-change diff and `ui.rs` all go through it.

### 3. The volume slider had nothing to control

Not a separate bug — with no application audio ever captured, there was nothing to hear
and nothing for the slider to change. It works as designed once (1) is fixed.

---

## Audio is a row on the share, not a card of its own (backend facts)

> **Note 2026-11-04 (I7):** the UI presentation described here (an extra row on the share) is gone; the backend facts (meter, mute, boost, stop) hold. Boost is set from the audio settings menu now (not Settings > Shared audio), the "Pairing" bullet is only the paired case (`for_capture: Some`), and "No stranded audio" is moot: an audio share is listed in Outgoing on its own.

**Owner's request (2026-07-28):** attach audio to the screen share instead of making it its own
card: a meter, the captured application's name, Mute and Stop on one extra row of the share.
(Audio is still its own `LocalCapture` and its own `StreamKind::Audio` stream id on the wire; only
its *presentation* is bundled.)

- **Meter:** a `level` element in the capture pipeline, folded into `audio::AudioLevel` (an
  `AtomicU32` of `f32::to_bits`: one writer, no history, lock-free for a GStreamer streaming
  thread) and read by the UI from `AudioCaptureHandle::level()`.
- **Mute:** `AudioCaptureHandle::set_muted`, a `volume name=mute` element placed *before* `level`,
  so muting flattens the meter too (the meter answers "is my audio going out", and a bouncing bar
  while muted would answer wrongly). *Why mute with `volume` and not by pausing the send:* a
  continuous Opus stream keeps the viewer's jitter buffer from seeing a mute as a multi-second
  dropout.
- **Boost (#114):** `volume name=gain` + `rglimiter name=limiter` in F32 *before* the mute and the meter
  (`audio::GAIN_STAGE`), set live by `AudioCaptureHandle::set_gain` from the audio settings menu (Settings > Shared audio until 2026-11-04); an
  `audioconvert` after the meter takes it back to S16 for `opusenc`. `features/audio/levelling-and-volume.md` (2026-11-03,
  task #114): why sender-side, why the limiter is bypassed at 100 %. The calibration was removed and the boost capped at 200 % in #119; viewers level loudness themselves (`leveller.rs`).
- **Stop:** plain `stop_destination(<audio destination id>)`; an audio capture always has exactly
  one destination, so it stops that capture only and the screen share is untouched.
- **Pairing:** `LocalCapture::audio_for_capture: Option<CaptureSourceId>`, set from
  `start_audio_share(room_handle, for_capture)` (the UI reads it off the `LocalCapture`).
  *Why an explicit link and not "same room":* one capture can fan out to several rooms and someone
  sharing two monitors into one room has two shares but possibly one audio share; "same room" would
  make both claim it and Stop on either would kill audio for both.
- **No stranded audio:** an audio destination with no matching video card (a share torn down under
  it) must still be listed and stoppable so it can never be stranded invisibly.
- **Source list freshness:** `audio::list_audio_sources` shells out to `pw-dump` on every call (no
  cache) and filters to real `PipeWire:Interface:Node` objects (PipeWire `Client` objects carry
  lookalike `media.class` props); the UI re-lists on open and while the picker stays open. Per-option
  level meters use real per-candidate taps (`audio::start_level_tap`, built by `session::build_audio_level_tap`, held in `AppState::insert_audio_level_tap`, read with `audio_level_tap_level`, ended with `stop_audio_level_tap`).
- **The choice is a name; same-name streams are mixed (#105, 2026-11-03).** `list_audio_sources`
  returns one entry per display name (first node, used for the meter); `start_audio_sharing` links
  *every* `Stream/Output/Audio` node with that name into the one loopback input and
  `audio::link_poll_loop` re-checks every second (links new matches, forgets vanished ones; pure
  logic in `link_diff`). One loopback, one pipeline, one encoder: nothing restarts. Zero matching
  streams = a silent share (the capture only ever reads the loopback output, so never the
  microphone). The choice is not persisted anywhere (the `AudioChoice` is
  in-memory), so there is no settings back-compat. Details and *Why*: `features/audio/desktop-audio.md`.
- **Real playback measurement** (viewer volume 0/100/200 % gave +0.0/+2.7/+6.5 dB at the speakers)
  is recorded with its decorrelated-noise rationale in `superdoc/features/video-pipeline.md`
  ("Audio follows video").

**Tests must separate "audio flowing" from "audio absent"** (lesson of the audio bugs above, kept from the removed Electron UI's history).
