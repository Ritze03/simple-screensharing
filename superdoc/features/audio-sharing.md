# Audio sharing: A/V sync (viewer-side audio delay)

Audio capture/encode/playback is described where it grew up: `features/stream-control.md`
("Sharing audio ...", "The 2026-07-28 audio bugs"), `features/video-pipeline.md` ("Audio follows
video") and the module docs of `client/src/audio.rs` and `client/src/decode.rs`. This page covers
**keeping the audio aligned with the picture** (task #91, 2026-11-01), the **shared audio is a name** (#105), the viewer volume (#96), the **sender-side boost** (#114, capped at 200 % and calibration-free since #119) and the **viewer-side loudness levelling** (#119); for the share sink's lifecycle (WirePlumber pauses a player when its sink goes away, a resuming stream loses its links) see the #123 block. **Why shared audio broke after a pause/resume** (the capture ran on `pipewiresrc`'s clock; the native viewer's jitter buffer guessed the frame size from PTS steps) is the #128 block, which also corrects the explanations of #117, #121, #123 and #125.

## 2026-11-03: the real cause of "broken after a pause/resume": two clocks (task #128)

User report on master `eec2124` (with #117, #121, #123, #125): pausing and playing a YouTube video in Floorp still
leaves viewers with "really broken" audio until the sharer re-selects the source. Two observations ruled out the
graph-state explanations of the earlier blocks: a host monitor over 2-3 pause/play cycles showed the Floorp node
`running` the whole time with no node or link change at all (no trigger of #117/#123/#125 fired, still broken), and
in the client log an automatic #125 rebuild did not cure it while a manual re-select 3 s later did. What a re-select
renews that a rebuild does not: a new stream end to end, i.e. a **new viewer-side decoder**. Both ends turned out to
be broken, by the same kind of bug: a clock or frame size taken from the wrong place.

**The host's graph** (read-only `pw-top`/`pw-dump`, same session): the hardware sink
`alsa_output.pci-0000_00_1f.3.analog-stereo` drives at **44100 Hz**, quantum 512 (`clock.allowed-rates = [44100,
48000]`; Spotify, the game and gamescope run 44.1 kHz streams), while our share sink, alone, is its own driver at
48000 Hz. Every time the shared app is linked in, pauses, resumes, or another stream of it appears, the share's
capture stream changes driver, quantum and **rate**. The VMs run `allowed-rates = [48000]` only, so no earlier VM
test ever saw a rate switch (the harness can now set it, `ALLOWED`).

### 1. Sender: the capture pipeline ran on `pipewiresrc`'s clock (`audio.rs` `use_monotonic_clock`)

`pipewiresrc` provides a `GstPipeWireClock` and the pipeline picks it. Its time is the graph driver's sample position
(`pw_stream_get_time_n`: ticks / rate, plus interpolation), while the buffer PTS are `CLOCK_MONOTONIC` cycle times
(`b->time - delay`). `pipewiresrc` is a pseudo-live `GstBaseSrc` (it implements `get_times`), so basesrc fixes a
`ts_offset` at the first buffer and then **waits on the pipeline clock until each buffer's PTS** (source:
`gstpipewiresrc.c` / `gstpipewireclock.c` of PipeWire 1.6.10, `gstbasesrc.c` `gst_base_src_do_sync` of GStreamer
1.28). On a driver change the stream keeps its tick count continuous but loses the switch gap; on a rate change the
accumulated ticks are re-read at the new rate. Either way the clock **steps against the PTS**, and after a step back
the source is held back on every buffer, runs out of buffers and drops cycles, **for good**. Only a pipeline restart
(Null -> Playing: new clock, new `ts_offset`) clears it, which is all a re-select or the #117/#123/#125 rebuild did.
`pipewiresrc` even says so in its source ("We may see a driver/quantum/clock rate change on switching audio sources
... Application must handle the clock lost message ... by pausing the pipeline and then setting it back to
playing"), but posts that message only when the stream goes PAUSED, not on a driver/rate/quantum change.

Measured with `client/scripts/vm-audio/probe.py` (the exact sink + capture chain, instrumented; `probe-run.sh`),
samples per second leaving `pipewiresrc` (48000 is healthy):

| change under a running capture | `pipewireclock0` (until #128) | system clock (#128) |
| --- | --- | --- |
| quantum 1024 -> 256 (same driver, same rate) | clock -0.4 ms, **~28 600/s for good** | 48 000 |
| quantum 256 -> 2048 | clock +36 ms, back to 48 000 | 48 000 |
| graph rate 48000 -> 44100 | clock +542 ms, 47 900 | 47 900 |
| graph rate 44100 -> 48000 | clock -766 ms, **~6 100/s for good** | 48 000 |
| 44.1 kHz source paused/resumed 3x (allowed 44.1/48 kHz) | clock -20 ms per switch; 48k for one cycle, then 20-37k | 48 000 throughout |

**Fix:** `use_monotonic_clock(&pipeline)` = `pipeline.use_clock(Some(&gst::SystemClock::obtain()))` in
`build_pipeline_from_source` (the share) and in `start_level_tap` (the dropdown meter, same bug). `CLOCK_MONOTONIC`
is the axis of the buffer PTS, so basesrc's waits are now consistent whatever PipeWire does; PipeWire rate-matches the
stream to its driver itself. Nothing else needs the PipeWire clock (the appsink does not sync). Side effect: the
audio wire PTS are now on a monotonic running time, the same axis as video's (`decode/avsync.rs`), instead of a
device clock that drifts by its ppm. *Why not `provide-clock=false`:* the property exists in 1.6.10, but a fixed
pipeline clock does not depend on what a given `pipewiresrc` version offers. *Why not handle `clock-lost`:* it is not
posted for these changes, and restarting the pipeline loses audio each time.

### 2. Native viewer: one short PTS step poisoned the jitter buffer for good (`decode.rs` `opus_packet_duration_us`)

`AudioJitterBuffer` inferred the Opus frame duration as the **minimum of the last 16 PTS steps** and treated a packet
as contiguous if it came within 1.5 frames. A sender re-times its PTS by a few ms when its graph changes (the fixed
sender too: a 5.1 ms step at a quantum change and a 0.9 ms step at a rate change were measured with `SSS_AUDIO_DUMP`).
From that one step on, the "frame" was 5.1 ms, every normal 20 ms packet looked like 3 lost frames, so each one was
held 40 ms and preceded by 2 `GAP`s of PLC (5 after the 0.9 ms step, the 2.5 ms clamp); concealed steps are never
recorded, so the 16-step window **never recovered**. Measured in one 50 s run: **6797 `GAP` events**, 100-250 a
second, the viewer heard **428 Hz instead of 440 (2.7 % low) with clicks**, while the packets it received decoded
back to back were clean. A re-select gives the viewer a new `AudioDecoder` (that cured it); the #125 rebuild kept
the viewer's decoder (that did not). This is also #123's "about 2.7 % low, steady clicks", which was blamed on
re-linking into the running chain.

**Fix:** the packet says how long it is. `opus_packet_duration_us` reads the TOC byte (RFC 6716 3.1: config ->
frame size, code -> frame count; `None` for empty/malformed, then 20 ms), each held packet carries its duration, and
`drain` judges a packet against where the last released one **ends** (`pts <= end + slot/2` is contiguous, which
also accepts a packet that starts a few ms early; a hole is `(pts - end) / slot` missing slots of the last packet's
duration, still capped at 5). The estimator (`recent_deltas`, `frame_duration_us`, the bootstrap hold) is gone; a
40 ms-frame sender is on its grid from its first packet.
**The browser viewer** (`docs/js/core/audio.js`) has no such estimator (it schedules decoded frames by PTS): it had
only cause 1.

### 3. The rebuilds are gone (`audio.rs` `link_poll_loop`, `sync_links`, `lost_links`)

With both causes fixed, linking into the running chain is clean, so the poll just links: new streams of the name
(#105, #117's re-created node) and streams we linked that lost their links (`lost_links`: Firefox re-creates its
ports on pause/resume, #123; `link_source_into_loopback` is idempotent). Removed: `rebuild_chain`, `Chain`,
`ChildGuard`, `gap_decision`, `IdleWatch`, `poll_decision`, `node_is_running` (and `PwInfo::state`), and their tests;
the sink handle and the PTS offset are plain values again. A resume no longer loses its first ~1 s. Unchanged: #118
mute carry-over, the microphone guard (`wait_for_capture_link` at start), #123's unlink-before-removal, #121's sink.

### 4. Debug dumps (`audio_dump.rs`, `SSS_AUDIO_DUMP=<dir>`)

Off unless set. The sender writes every Opus packet it sends (`sent-<stream>-<ms>.pkt`), the native viewer every
packet it receives (`recv-*.pkt`) and what its jitter buffer/timeline did (`play-*.log`: `push pts= mapped= now=`,
`gap pts= dur=`). `client/scripts/vm-audio/pktdump.py FILE.pkt [OUT.wav]` prints PTS steps per second and decodes
the packets back to back into a WAV: "was it sent broken or played broken" in one step. This is what found cause 2.

### Measured (Fedora VM, PipeWire 1.6.2, in-process sharer and viewer through a real relay, `soak.sh`)

Bad 0.5 s windows per phase (`phases.py`, `analyze.py`'s criteria: within 15 Hz of 440 and a sample step under
0.12 of the peak). The share sink's own monitor, recorded in parallel, is the VM's noise floor (0-3 per phase).
"Packets/s" is what the viewer received (`pps.py`, 50 is healthy).

| scenario | master `eec2124` | #128 |
| --- | --- | --- |
| `graph` (source plays on; quantum 256 / back / rate 44.1k / back) | 9/15, 2/15, 7/15, **21/23** | 0/15, 1/15, 2/15, 3/23; 1 `GAP` |
| same, sender fix only (viewer estimator still in) | - | 7/15, 7/15, 6/15, 21/23; 6797 `GAP`s |
| `pause`, emulated 44.1 kHz Floorp, allowed 44.1/48k (3 resumes) | 3/17, 2/17, **13/17**; 24-44 pkt/s while paused | 2/17, 3/17, 4/17; 48-51 pkt/s |
| same, EasyEffects as default sink | 3/17, **7/17, 6/17**; 22-43 pkt/s while paused | 2/17, 1/17, 3/17; 48-51 pkt/s |
| `pause`, real Firefox (re-creates its ports; #128 re-links) | 2/17, 3/17, 1/17, a rebuild + ~1 s lost per resume | 1/17, 2/17, 1/18, no rebuild, nothing lost |
| `vanish` (node destroyed and re-created, #117) | 3/15, 4/15, 2/15 (rebuilds) | 0/15, 3/15, 2/15 (plain link) |
| `flap` 10 x 2.2 s at 44.1k, then 20 s | - | last 20 s 5/43 (sink monitor 4/43), 0 `GAP` |

Sent audio (`pktdump.py` of `sent-*.pkt`, decoded back to back) with #128: at the sink monitor's level in every
scenario (0-3 per phase; the last phase also holds the end of the stream).

**Tests (no audio):** `audio::tests::the_capture_pipeline_runs_on_the_monotonic_system_clock` (the real `pipewiresrc`
description in Null: fixed clock, `GstSystemClock`, monotonic; and the running chain keeps it),
`a_source_that_lost_its_links_into_the_sink_is_noticed` (now `lost_links`), `decode::tests::jitter_is_not_poisoned_by_a_short_pts_step`,
`jitter_reads_a_frame_duration_other_than_20ms_from_the_packets`, `opus_packet_duration_reads_the_toc_byte` (RFC table
and real `opusenc` packets), `audio_dump::tests::packets_are_written_in_the_documented_layout`; the other jitter tests
use a stub with a real 20 ms TOC byte (`0xF8`).

### What the earlier blocks got wrong (kept below as history)

- **#117** ("a driver, clock or quantum change under a pipeline that was started against the old one is the likely
  mechanism"): right instinct, wrong clock. It was the *GStreamer* pipeline clock (`pipewiresrc`'s), not the graph;
  the rebuild helped only because it restarted that clock.
- **#121** ("a `pw-loopback`'s capture side has no clock of its own"): not the cause. The null sink is kept (it is a
  fine design and #123's unlink-before-removal builds on it), but it never fixed anything about this.
- **#123** ("re-linking into the running chain garbled, about 2.7 % low, steady clicks; PipeWire merges the driver
  groups non-deterministically"): the silence/garble was cause 1, the 2.7 % and the clicks were cause 2 (the viewer).
  Re-linking is exactly what the poll does now, measured clean.
- **#125** ("the driver of the group must not change under a running capture stream"): PipeWire handles a driver
  change fine; the GStreamer clock did not. Its `priority.driver=3000` experiment worked because with our sink always
  the driver the capture's clock never stepped. The idle -> running rebuild is removed.

**Not verified:** the user's own host (real ALSA at 44.1 kHz, EasyEffects, Floorp, a viewer elsewhere); the Arch VM;
the browser viewer against the fixed sender. **Same pattern elsewhere, not changed:** the video capture pipelines
(`capture.rs`) also run on their `pipewiresrc`'s clock; a screencast's driver is the portal node and does not switch
rate, but if a video stream ever starves after a portal renegotiation, try the same one-liner first (whether #122's
"out of buffers" has anything to do with it was not checked).

## 2026-11-03: garbled audio after a pause/resume that keeps the node and its links (task #125)

> **Corrected by #128 (above):** the cause was not the driver change of the share group but the capture pipeline
> running on `pipewiresrc`'s clock (plus the viewer's frame-size estimator); the idle -> running rebuild described
> here is removed. The measurements stay valid as measurements.

User report on master `d39e70d` (with #123): pausing and resuming a YouTube video in Floorp still garbles the shared
audio until the source is re-selected. **Host evidence** (read-only `pw-dump` every 0.3 s over 4 pause/resume cycles,
then a re-select): the share sink `ssshare-sink-5-...` (sink 296 + capture stream 340), Floorp node 365 (0 in / 2 out
ports), `easyeffects_sink` 317 running, the ALSA sink 88 running as driver. On every pause Floorp goes
`running` -> `idle`, on resume `idle` -> `running`, **same node id, same ports, the links into our sink stay `active`**
(no link loss, so #123's lost-links trigger never fired and the log shows no rebuild). Re-selecting (new sink,
linked first, capture started after) fixes it. Separately a destroyed/re-created Floorp node (365 -> 342) triggered the
#117 rebuild correctly.

### 1. Fix: an idle -> running source rebuilds the chain (`audio.rs`, `IdleWatch`, `poll_decision`, `node_is_running`)

The poll now also reads each node's `info.state` (`PwInfo::state`) and counts the streams carrying the shared name and
how many are `running`. **Every stream of the name not running for 2 polls in a row** (`IDLE_TICKS`) sets the sticky
`IdleWatch::idled` (the chain is stale, exactly like a vanished stream, #117); **as soon as one runs again** the
decision is `rebuild_chain` (same path as a re-select: new sink, links, then capture pipeline, old sink unlinked and
removed), logged as `audio share '<name>': source resumed after idle; rebuilding the loopback`. It goes through
`gap_decision` (the `linkable` input is `new_linkable || lost || resumed`), so #117, #123 (lost links) and the
mute carry-over are unchanged.
- *Several linked sources:* one idle while another runs is not "every stream idle": no rebuild.
- *Debounce / no loops:* 2 idle polls before a pause counts (a new stream is `idle` for a moment before it runs, an
  app that buffers blips); after any rebuild `REBUILD_COOLDOWN_TICKS` (5 polls) must pass before the next
  idle-triggered one. A resume inside the cooldown is remembered and rebuilt for when it ends (the flag is sticky),
  so a source that flaps costs at most one ~1 s rebuild per 5 s (measured: 10 flaps of 2.2 s -> 9 rebuilds, then clean).
- A node without a reported state, or not in the snapshot, counts as running (no conclusion drawn).
- Start with the source already paused: the share starts stale, and the resume rebuilds (same reasoning).
*Why a rebuild and not just re-linking:* see "Root cause" and the #123 block (anything done to the running capture
chain is non-deterministic, a fresh chain built in the order sink, links, capture stream is clean).

### 2. Root cause (reproduced and measured in the Fedora VM, PipeWire 1.6.2, EasyEffects)

**Topology:** EasyEffects running and set as default sink (`easyeffects_sink` -> fake ALSA sink `alsa_output.fakehw`,
the #123 drop-in), the source linked into both `easyeffects_sink` and our share sink. **Source that keeps its node
and ports:** Firefox in the VMs re-creates its ports on resume (already handled by #123), the host Floorp does not, so
the VM harness got `client/scripts/vm-audio/emu_src.py`: a GStreamer `pulsesink` (pulse client of pipewire-pulse like
Firefox's cubeb) that is set PAUSED/PLAYING; the pulse sink corks the stream, which is exactly the host picture
(`running` -> `idle`, links `active` -> `paused`, same node and ports). Run it as app `Floorp`
(`EMU=1 APP=Floorp host-run.sh PORT LABEL pause`).

**Result:** reproduced on the unchanged `d39e70d` code, no rebuild in the log, the share garbled for the rest of each
cycle: viewer 31 / 48 / 47 bad 0.1 s windows of 60 (3 s after each resume to the next pause; a clean run is 0-7 of 60,
the VM's own noise), clicks (`step/pk` 0.5-1.0) and a flat tone. The **sink's own monitor** recorded in parallel is
clean (0-3 of 60), so the damage is on the capture side of the chain. `watch.py` shows why: while the source runs, the
share group (sink + capture stream) is driven by the hardware sink (`drv=32`); on pause the links go `paused`, the
group is left alone and **the driver changes** (the sink becomes its own driver `drv=118`, or the Dummy-Driver `30` when
the hardware sink suspends when idle: fakehw with `node.pause-on-idle=true`, like a real ALSA device); on resume the
groups merge again (`drv=59`, then `32`) **under the running capture stream**. The packets/s the in-process viewer
receives fall from 50 to 15-38 from the first driver change on (`pps.py`): the capture stream is no longer scheduled
regularly, hence the missing samples, the clicks and the wrong pitch.

**Proof that the driver change is the cause:** the sink created with `node.driver=true priority.driver=3000` (our sink is
always the driver, the driver never changes) stays at 50 packets/s and is clean without any rebuild (viewer 1/1/1 of 60).
Also tested, no help: dropping `node.driver` (`node.driver=false`) is clean only while the hardware sink never goes idle
(fakehw `always-process`: 5/6/3 and 0/1/1 of 60), and garbled (53/47/26 of 60, `drv` 32 -> 30 -> 32) as soon as the hardware
suspends when idle, i.e. the realistic case. So `node.driver` is not the lever; **the driver of the group must not change
under a running capture stream.**

**Not shipped: `priority.driver` high.** It would make the *user's* playback (the source, EasyEffects, the ALSA device)
follow our sink's timer clock for as long as the share exists (the ALSA sink becomes a resampled follower), a change to
their primary audio path that a VM without real hardware cannot judge. The rebuild fixes the share without touching their
playback; the cost is ~1 s of silence after each resume. If that is ever too much, `priority.driver` is the lever to
evaluate on real hardware. Not tried: `node.group`/`node.link-group` (the group of the source is not known when the sink is
created), `node.lock-quantum`, `node.force-rate`: none addresses a driver change.

### 3. Measured with the fix (same VM, same harness, 3 cycles of 5 s paused / 8 s playing, 0.1 s windows 3 s after the resume)

| | before (`d39e70d`) | after (#125) |
| --- | --- | --- |
| emulated Floorp, EE, always-on fake hw | viewer 31 / 48 / 47 bad of 60 | 4 / 5 / 2 |
| emulated Floorp, EE, hw that suspends when idle | 43 / 46 / 47 | 7 / 4 / 6 (another run 2 / 1 / 33, see below) |
| real Firefox, EE, hw that suspends when idle | (#123 rebuild on lost links) | 0 / 0 / 0, exactly one rebuild per resume |
| 10 flaps of 2.2 s | - | 9 rebuilds, final state clean (1 bad of 187) |
Viewer packets/s after the rebuild: 49-51 (before: 15-38). One of three fix runs had a bad stretch of ~5 s after the third
resume in the sink's own monitor too (so in the source/sink part, not the capture chain); not reproduced in two repeats,
treated as VM noise but not explained. The first ~1 s of the resumed sound is lost (poll interval + rebuild).

**Tests (no audio):** `node_is_running_reads_the_pw_dump_state`, `a_source_that_goes_idle_and_runs_again_rebuilds_the_chain_once`,
`only_all_sources_idle_for_a_while_counts_as_a_pause` (one of two idle, a single idle poll, no streams),
`a_flapping_source_rebuilds_a_bounded_number_of_times` (also the deferred rebuild after the cooldown),
`the_idle_rule_leaves_the_other_triggers_alone`. **Harness additions:** `emu_src.py`, `soak.sh ... flap`, `VM_SSS`
(binary dir, to compare a baseline build), `EMU`/`APP` passthrough in `host-run.sh`, `pps.py` (packets/s the viewer received, from the soak log; 50/s is healthy), and a
"steady state" line in `summary.py`. **Not done:** the Arch VM (only Fedora/EasyEffects was used), a real hardware sink.

## 2026-11-03: stopping or switching a share paused the player, and a pause/resume left the chain dead (task #123)

> **Corrected by #128 (above):** part 1 (WirePlumber pausing the player) stands. In part 2 the lost links are real,
> but "re-linking into the running chain garbles" was wrong: that garble was the `pipewiresrc` clock and the
> "2.7 % low, steady clicks" the native viewer's jitter buffer. The poll now re-links instead of rebuilding.

Two user reports on master with #121: *"selecting my browser as an audio source immediately pauses the video"*
and *"pausing and unpausing still breaks"* the shared audio (viewers hear broken audio until the source is
re-selected). Both were reproduced and measured for real, in the two test VMs (Arch: PipeWire 1.6.10 /
WirePlumber 0.5.18; Fedora 44: PipeWire 1.6.2 / WirePlumber 0.5.13; both GNOME/Wayland, Firefox 159, EasyEffects
8.2.8 on the Fedora one), see `claude-instructions/testing-and-verification.md` "Audio tests in the VMs".

### 1. The video paused because WirePlumber pauses players when a sink goes away

**Cause (proven).** WirePlumber's `linking/mpris-pause.lua` (setting `linking.pause-playback`, on by default;
present in 0.5.13 and 0.5.18) pauses the MPRIS media players whose streams are linked to a sink at the moment
that sink is removed, the way a browser stops when headphones are unplugged. Since #121 the share sink is a real
`Audio/Sink` with the shared app linked into it, so every removal of it paused the app being shared. (The old
`pw-loopback` was two stream nodes, not a sink, so the script never saw it.) Evidence: a D-Bus monitor on the
VM's session bus caught `org.mpris.MediaPlayer2.Player.Pause` sent **by `wireplumber`** (bus name resolved to its
pid) to Firefox at the instant a sink disappeared; and with the real client sharing Firefox, Firefox's
`PlaybackStatus` went `Playing` -> `Paused` exactly when the share stopped, on both distros. Nothing else is
needed to trigger it: no EasyEffects, no default-sink change.

**Fix (`audio.rs`, `kill_and_wait` / `unlink_sink_inputs`).** Every removal of the sink now first deletes the
links that feed it (`pw-link -d <link id>` for every link whose `link.input.node` is the sink; the monitor link
to our own capture stream stays) and waits 250 ms (`UNLINK_SETTLE`) so WirePlumber has handled the link removals
before the node removal; the script reads the links that exist when the sink goes, finds no player, and does
nothing. The sink's owner became `SinkProc { child, name }` (derefs to the `pw-cli` `Child`) so the removal
knows which sink to unlink; `kill_and_wait(&mut SinkProc)` is used by `stop()`, `Drop`, the rebuild's old sink
and every failed-start path. After: Firefox stays `Playing` through the stop (both distros).

*Why unlink first and not change the sink:* the script keys on "an `Audio/Sink` session item is removed", which
the capture by `pipewiresrc ... stream.capture.sink=true` needs the sink class for; unlinking first keeps the
#121 design (own clock) and costs 250 ms once per removal. *Which path paused it "immediately on selecting"* is
not proven: picking a source while one is already shared stops the old share first (`ShareAudio` in
`ui/apply.rs` -> `stop_share`), and a failed start kills its sink too; both are covered. If the report persists
in a form that is neither, run `journalctl --user -u wireplumber | grep mpris` (or `WIREPLUMBER_DEBUG=s-linking.mpris:5`)
while selecting: it logs `node N removed: pausing linked media player ...` with the node that went away.

### 2. After a pause/resume the share is silent (or garbled) for good

**Cause (proven).** On resume Firefox **re-creates the ports of its stream node** (same node id, new port ids;
`pw-dump` watched at 0.3 s: the node goes `running` -> `idle` on pause, and on resume its old
`output_FL`/`output_FR` ports vanish and new ones appear). PipeWire removes every link on the old ports, so the
links into our sink are gone while the node, its id and its `running` state look unchanged (the user's
`pw-dump` showed exactly that: the same four ids, all running). The poll tracked linked streams by node id, so
it saw nothing to do, and the share carried silence until the user re-selected the source. That is the
"silent" outcome measured on both distros (`viewer.wav` after the resume: digital silence for the rest of the
run). Re-creating just the links was tried first (`pw-link` the missing pairs into the running chain) and is
worse: the sink monitor stayed perfectly clean, but the capture side came out **garbled** on the viewer (about
2.7 % low, steady clicks; in one run `pw-top` showed the share's group with no cycles at all and the capture was silent),
while the sender-side PCM and PTS were clean in some runs and garbled in others. Linking a source into a sink
whose capture stream is already running makes PipeWire merge the source's driver group (the hardware sink) into
the share's own group (sink + capture stream) under a running pipeline, and the result is not deterministic. A
chain built in the order sink, links, then capture stream (what a re-select does, and what #117's rebuild does)
was clean in every run. That is also the likely source of the garbled sound the user hears.

**Fix (`audio.rs`, `link_poll_loop`, `missing_links`, `gap_decision`).** A linked stream that has lost links
(`missing_links(objects, node, sink) > 0`: ports exist on both sides, a channel pair has no link) makes the chain
stale exactly like a vanished stream does (#117): sticky `gap`, and as soon as the stream has ports again
`rebuild_chain` builds a fresh sink, links first, restarts the capture pipeline, proves the capture link and only
then removes the old sink (now unlinking it first, part 1). Viewers get about 1 s of silence (the poll interval
plus the rebuild) and then clean audio; encoder, appsink, pump and the relay announcement are untouched.
`link_source_into_loopback` became idempotent (`ports_are_linked`: a pair that is already linked is skipped), which
also lets a half-linked stream (FL linked, FR's `pw-link` failed) complete on the next tick instead of failing on
"File exists" forever (the open #105/#117 note).

### Measured (VMs; 440 Hz tone in Firefox, in-process sharer + viewer through a real relay, the viewer's output recorded)

| | Arch (before) | Arch (after) | Fedora (before) | Fedora (after) |
| --- | --- | --- | --- | --- |
| audio after a pause/resume | silent for the rest of the run | clean after 0.9-1.4 s, 0 bad 0.1 s windows after that, 3 of 3 cycles | silent for the rest of the run | clean after 0.8-1.8 s, 0 bad windows after that, 3 of 3 cycles |
| Firefox when the share stops | `Playing` -> `Paused` | stays `Playing` | `Playing` -> `Paused` | stays `Playing` |

(Windows judged by zero-crossing frequency within 60 Hz of 440 Hz and a maximum sample step under 0.12 of the
peak; a clean 440 Hz sine has 0.058. Occasional single bad windows occur in the VM even before any pause
(starting transients of the in-process viewer on 4 vCPUs); they are not related to this task.) Also verified:
Firefox's `PlaybackStatus` shows only the test's own pauses during the cycles, so the rebuild's removal of the
old sink does not pause it either.

**Tests (no audio):** `a_source_that_lost_its_links_into_the_sink_is_noticed` (`missing_links` pairs channels,
crossed links do not count, the stale/rebuild decision), `only_the_links_feeding_the_sink_are_unlinked_before_it_goes`
(`links_into_node` ignores the monitor link). The live soak itself is the ignored
`end_to_end_test::live_audio_share_soak` (VM only).

**Still open / not covered:** the first ~1 s of the resumed sound is lost (poll interval + rebuild); polling faster
costs a `pw-dump` per tick. Not reproduced: the user's EasyEffects-on-the-host setup (the Fedora VM runs EasyEffects
and shows the same behaviour as without it). The 250 ms settle also runs in `Drop` (the abrupt-disconnect backstop)
on whatever thread drops the handle. A different WirePlumber without `mpris-pause.lua` is unaffected by part 1 and
only pays the 250 ms.

## 2026-11-03: viewer-side loudness levelling, calibration removed, boost capped at 200 % (task #119)

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
  adjust the volume. Still one global setting, not per stream. Details: `ui/APP-SHELL.md`.

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

## 2026-11-03: the share captures a private, self-clocked null sink instead of a `pw-loopback` (task #121)

> **Corrected by #128 (above):** the loopback's missing clock was not why the share broke; the null sink stays, but
> for its own merits.

User report after #117: muting and unmuting works now, but *pausing* the video and playing it again is
"broken the same way" (garbled audio until the source is selected again).

**Evidence (user's read-only `pw-dump` filtered to `ssshare|Floorp`, `{id,state}`):** working and broken-after-pause
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
`pw-dump | jq '.[] | select((.info.props["node.name"]? // "") | test("ssshare|Floorp"; "i")) | {id, type, state: .info.state, name: .info.props["node.name"], driver: .info.props["node.driver-id"], lat: .info.props["node.latency"]}'`,
`pw-link -l`, `pw-top -b -n 2`, and `pw-metadata -n settings`.

## 2026-11-03: audio broken after the source's stream disappears and comes back (task #117)

> **Corrected by #128 (above):** the mechanism was the GStreamer pipeline clock (`pipewiresrc`'s), not the graph; the
> rebuild described here is removed, a re-appearing stream is simply linked.

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
