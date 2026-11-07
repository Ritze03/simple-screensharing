# Audio sync, clocks and pause/resume

Keeping audio aligned with video (viewer-side delay) and why shared audio broke after a pause/resume (two clocks #128, dither #129, #125, #123). Hub: `../audio-sharing.md`.

The text moved here verbatim from the Audio sharing page: "above", "below", "this page" and "the #128 block" in it may point at a section that now lives in a sibling page of this folder (the hub lists them).

## 2026-11-03: "static when nothing is playing" was the sender's dither (task #129)

User report after #128 ("Nice now it's really fixed", but "there's some static when nothing is playing"). Measured
in the Arch VM (PipeWire 1.6.10, `allowed-rates [44100 48000]`, emulated 44.1 kHz the Firefox-based browser paused for 6 s three times,
the viewer's `viewer-out` monitor recorded as **float** so the viewer's own conversion adds no noise;
`VFMT=f32 ... host-run.sh`, `noise.py`).

**Cause: `audioconvert` before `opusenc` dithers.** The sender's tail is `... ! audioconvert ! opusenc`; `opusenc` only
takes S16, and the F32 -> S16 `audioconvert` defaults to **TPDF dither**. A paused source gives the share sink's monitor
digital zero (`sinkmon.wav`: -99 dB, nothing), but the dither turned it into +-2 LSB white noise (-97.2 dBFS RMS, peak
2 LSB; the decoded `sent-*.pkt`) which Opus then coded for real: **161-byte packets (64.4 kbps) of noise during a
silence**, exactly as large as with a tone. The viewer played it as hiss.

| paused stretch (viewer output, float) | before | #129 |
| --- | --- | --- |
| sender packet size while paused | 161 B (64 kbps), PTS fine, 0 `GAP` | **3 B** (silence frames, ~1.2 kbps) |
| sent audio decoded (dBFS RMS / peak) | -97.2 / 2 LSB | exact zero |
| viewer output, 1.5 s after the pause to the resume | **-93.5 dBFS RMS, peak 3.8 LSB** | **exactly 0 (-inf)**, all three pauses |

**Secondary (not changed): the leveller lifts that noise by up to ~+5 dB.** In the "before" run the viewer's noise
*rose* over 4 s after the tone stopped (-101.5, -97.6, -93.6, -91.7 dBFS): the 400 ms slow detector decays through
-50...-18 dBFS in about 3 s, the gate (`GATE_DB`) only looks at it, so the gain climbs at <= 4 dB/s during that tail
(-9 dB -> about +5.5 dB here) and then holds (a held gain never decays). With digital silence in, the gain is
irrelevant (0 x gain = 0), so #129 does not touch `leveller.rs`/`leveller.js`; but a real source with its own
noise floor still gets that lift after any sound ends. If that turns out audible, gate the rise on the *fast* detector
too (it falls below the gate within ~0.6 s of a stop) in both ports.

**Ruled out** (the brief's suspects): (c) gaps/PLC on SystemClock: 49-52 packets/s through the pause, 0 `GAP`
in `play-*.log`; (d) the jitter buffer: same; (b) `opusenc`/DTX: no DTX is needed, Opus codes an all-zero frame as a
3-byte silence frame by itself, it only coded 161 bytes because the input was not zero.

**Fix:** `audioconvert dithering=none` on the last `audioconvert` (`pipeline_description`). *Why not keep the dither:*
dither exists to decorrelate quantisation error of 16-bit *output*; this is an intermediate step into a lossy codec
(Opus' own noise is far above 1 LSB), and the dithered LSB noise is what the encoder was spending 64 kbps on. Side
effect: a quiet or silent share costs ~1 kbps instead of 64. The level tap's `audioconvert` only feeds `level`
(not audible) and is unchanged; the browser sender encodes float directly and never dithered.
**Test:** `audio::tests::a_silent_source_encodes_to_tiny_silence_packets` (a -140 dBFS sine through the real chain:
161-byte packets with the dither, 3-byte without; `wave=silence`/`volume=0` do not reproduce it, those buffers are
GAP-flagged and `audioconvert` passes them undithered).
Harness additions: `VFMT=f32` (soak.sh/host-run.sh) records the viewer as float, `vm-audio/noise.py FILE.wav [events.txt]`
prints per-second RMS/peak and the RMS of every paused stretch.

## 2026-11-03: the real cause of "broken after a pause/resume": two clocks (task #128)

User report on master `eec2124` (with #117, #121, #123, #125): pausing and playing a YouTube video in the Firefox-based browser still
leaves viewers with "really broken" audio until the sharer re-selects the source. Two observations ruled out the
graph-state explanations of the earlier blocks: a host monitor over 2-3 pause/play cycles showed the the Firefox-based browser node
`running` the whole time with no node or link change at all (no trigger of #117/#123/#125 fired, still broken), and
in the client log an automatic #125 rebuild did not cure it while a manual re-select 3 s later did. What a re-select
renews that a rebuild does not: a new stream end to end, i.e. a **new viewer-side decoder**. Both ends turned out to
be broken, by the same kind of bug: a clock or frame size taken from the wrong place.

**The host's graph** (read-only `pw-top`/`pw-dump`, same session): the hardware sink
drives at **44100 Hz**, quantum 512 (`clock.allowed-rates = [44100,
48000]`; the music player, the game and gamescope run 44.1 kHz streams), while our share sink, alone, is its own driver at
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
| `pause`, emulated 44.1 kHz the Firefox-based browser, allowed 44.1/48k (3 resumes) | 3/17, 2/17, **13/17**; 24-44 pkt/s while paused | 2/17, 3/17, 4/17; 48-51 pkt/s |
| same, EasyEffects as default sink | 3/17, **7/17, 6/17**; 22-43 pkt/s while paused | 2/17, 1/17, 3/17; 48-51 pkt/s |
| `pause`, real Firefox (re-creates its ports; #128 re-links) | 2/17, 3/17, 1/17, a rebuild + ~1 s lost per resume | 1/17, 2/17, 1/18, no rebuild, nothing lost |
| `vanish` (node destroyed and re-created, #117) | 3/15, 4/15, 2/15 (rebuilds) | 0/15, 3/15, 2/15 (plain link) |
| `flap` 10 x 2.2 s at 44.1k, then 20 s | - | last 20 s 5/43 (sink monitor 4/43), 0 `GAP` |

**Arch VM, PipeWire 1.6.10 / GStreamer 1.28.8 (exactly the host's versions):** `probe.py` with `pipewireclock0`:
quantum 1024 -> 256 drops the capture to ~24 000 samples/s for good, rate 44100 -> 48000 to ~6 100/s for good; with
the system clock 47 800-48 100 throughout. The #128 build: `graph` 1/15, 1/15, 1/15, 1/23 (3 `GAP`s), `pause` at
44.1 kHz 2/17, 1/17, 1/17 (0 `GAP`s), 48-52 pkt/s; the sink monitor 0-1 per phase.

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

**Not verified:** the user's own host (real ALSA at 44.1 kHz, EasyEffects, the Firefox-based browser, a viewer elsewhere); the browser
viewer against the fixed sender. If it still breaks there: run sharer and viewer with `SSS_AUDIO_DUMP=<dir>` and
compare `pktdump.py sent-*.pkt` (what was sent), `recv-*.pkt` and `play-*.log` (how many `gap` lines). **Same pattern elsewhere, not changed:** the video capture pipelines
(`capture.rs`) also run on their `pipewiresrc`'s clock; a screencast's driver is the portal node and does not switch
rate, but if a video stream ever starves after a portal renegotiation, try the same one-liner first (whether #122's
"out of buffers" has anything to do with it was not checked).

## 2026-11-03: garbled audio after a pause/resume that keeps the node and its links (task #125)

> **Corrected by #128 (above):** the cause was not the driver change of the share group but the capture pipeline
> running on `pipewiresrc`'s clock (plus the viewer's frame-size estimator); the idle -> running rebuild described
> here is removed. The measurements stay valid as measurements.

User report on master `d39e70d` (with #123): pausing and resuming a YouTube video in the Firefox-based browser still garbles the shared
audio until the source is re-selected. **Host evidence** (read-only `pw-dump` every 0.3 s over 4 pause/resume cycles,
then a re-select): the share sink `ssshare-sink-5-...` (sink 296 + capture stream 340), the Firefox-based browser node 365 (0 in / 2 out
ports), `easyeffects_sink` 317 running, the ALSA sink 88 running as driver. On every pause the Firefox-based browser goes
`running` -> `idle`, on resume `idle` -> `running`, **same node id, same ports, the links into our sink stay `active`**
(no link loss, so #123's lost-links trigger never fired and the log shows no rebuild). Re-selecting (new sink,
linked first, capture started after) fixes it. Separately a destroyed/re-created the Firefox-based browser node (365 -> 342) triggered the
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
and ports:** Firefox in the VMs re-creates its ports on resume (already handled by #123), the host browser does not, so
the VM harness got `client/scripts/vm-audio/emu_src.py`: a GStreamer `pulsesink` (pulse client of pipewire-pulse like
Firefox's cubeb) that is set PAUSED/PLAYING; the pulse sink corks the stream, which is exactly the host picture
(`running` -> `idle`, links `active` -> `paused`, same node and ports). Run it as app `<browser>`
(`EMU=1 APP=<browser> host-run.sh PORT LABEL pause`).

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
| emulated the Firefox-based browser, EE, always-on fake hw | viewer 31 / 48 / 47 bad of 60 | 4 / 5 / 2 |
| emulated the Firefox-based browser, EE, hw that suspends when idle | 43 / 46 / 47 | 7 / 4 / 6 (another run 2 / 1 / 33, see below) |
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
8.2.8 on the Fedora one), see `testing/vm-audio.md` "Audio tests in the VMs".

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
  PipeWire audio stream, never touch the user's music player; see
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
- **Video floor uses arrival plus the playout buffer's delay on native** (row 136, 2026-11-04): the native viewer
  holds encoded frames (`decode/playout.rs`), `feed` reports `now + Shared::playout_us` to `AvSync::note_video`, so the audio
  follows the buffer as it grows and shrinks (still the 60 ms deadband and 500 ms step limit). The renderer's own
  pull latency is not added. Browser uses the jitter buffer's release time.
- **Capturing sources that stamp unusual PTS** are unaffected: only `buffer pts + offset` reaches
  the wire, as before.
