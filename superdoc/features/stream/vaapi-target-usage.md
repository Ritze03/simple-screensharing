# VA-API target-usage

`target-usage` on Mesa AMD and other drivers, how it reaches the hardware, and the encoder-limit hint that came and went.

Part of the Stream Control reference, hub: `superdoc/features/stream-control.md`. The text moved here verbatim from that page: "above", "below" and "this page" in it may point at a section that now lives in a sibling page of this folder (the hub lists them).

## `target-usage` is a bitfield on Mesa AMD; Balanced = 3, Quality = 4 (2026-11-01, #92)

**2026-11-01 (#95): the UI hides what the mapping makes a twin, per running backend.** *(Superseded 2026-11-04: `EncoderProfile` and the twin hiding were deleted when the presets became Text/Quality/Gaming, see "Presets: Text, Quality, Gaming".)* On Mesa AMD,
Performance (2) and Balanced (3) are the same encode (table below), so the stream-settings modal no longer
offers a Balanced quality level there, and Low-Latency (Performance, 2000 ms) is the same encode as Balanced
(Tune is dropped on VA) so that preset is hidden too (user rule: "If a setting doesn't do anything, hide it").
`capture::EncoderProfile` derives this from `vaapi_target_usage` (`quality_effect`: Mesa AMD folds
{2,3}/{4,5}/{6,7}; Intel and x264 have no twins), so changing the mapping changes what is hidden with no UI edit.
Stored values are never rewritten. Also new here: `EncoderBackend::tune_applies` (moved from `presets`),
`EncodeBranch::backend()` (the backend a branch was *actually* built with, after a per-branch fallback to x264)
and `DestinationRuntime::actual_backend()` (recorded when a branch becomes authoritative; `params.backend` stays
the preferred one so the next swap tries VA first). `supported_codecs(Vaapi)` now probes for `vah265enc` /
`vaav1enc` instead of listing all three. UI side and rationale: `ui/APP-SHELL.md` (2026-11-01, #95).

**2026-11-01 (#94): Performance is 7 on every non-Mesa-AMD driver.** Spec-order drivers
(Intel iHD) follow the VA order, 1 = best quality ... 7 = fastest, so `vaapi_target_usage`
now gives them Performance 7 (Balanced 4 and Quality 1 unchanged). Mesa AMD keeps 2 / 3 / 4.
*Why:* Performance = 2 was chosen in #46 because 7 was the slowest band on radeonsi; since #92
radeonsi has its own mapping, so the 2 only ever protected a driver it no longer applies to.
**Untested on Intel hardware** (none available, and the headless rig is AMD, so the changed
branch cannot be exercised there); the value rests on the VA spec. Pinned by the
`vaapi_target_usage_per_driver` unit test. The #46 note below keeps its history.

User report: Balanced is very slow with AV1 on the test machine's AMD GPU, and AV1 shares lag when the
GPU is fully loaded. Balanced was `target-usage=4`, the half-speed AV1 band of the #46
table below. Cause and fix, from the Mesa 26.2.5 and GStreamer 1.28.8 sources (the installed
versions) and measurements. Mapping lives in `capture::vaapi_target_usage`, driver detection in
`capture::va_driver_is_mesa_amd`.

### How `target-usage` reaches the hardware

1. GStreamer's `vah264enc`/`vah265enc`/`vaav1enc` send `target-usage` unchanged as
   `VAEncMiscParameterBufferQualityLevel.quality_level` (`gstvabaseenc.c:1175-1186`, in
   gst-plugins-bad `sys/va`; nothing is sent for 0, the property is 1-7). They first clamp it to
   the driver's `VAConfigAttribEncQualityRange` (`gstvaav1enc.c:2613-2624`); radeonsi reports
   32 (`si_mm_screen.c:139`, `PIPE_VIDEO_CAP_ENC_QUALITY_LEVEL`), and Mesa's own comment
   (`config.c:341-347`) says the range "isn't strictly faster when higher value used".
2. Mesa's VA frontend treats the value as a bitfield, not an ordinal.
   `vlVaQualityBits` (`src/gallium/frontends/va/va_private.h:445-456`):
   `valid_setting:1` (bit 0), `preset_mode:2` (bits 1-2), `pre_encode_mode:1` (bit 3),
   `vbaq_mode:1` (bit 4). `vlVaHandleVAEncMiscParameterTypeQualityLevel` (`encode.c:32-55`):
   0 = speed preset, no pre-encode, no VBAQ; **1 = special case: balance preset + pre-encode +
   VBAQ** ("suggested setting"); anything else = the bits read as they are (the valid bit is
   not even checked). Presets (`va_private.h:74-77`): speed 0, balance 1, quality 2, high
   quality 3; the radeonsi side (`radeon_vcn_enc.c:15-60`) clamps high quality to quality below
   VCN 4.0, and takes pre-encode only on the first frame and as 4x downscale.
3. With 1..7 only bits 0-2 exist, so:

| target-usage | 1 | 2, 3 | 4, 5 | 6, 7 |
| --- | --- | --- | --- | --- |
| preset | balance | balance | quality | high quality |
| pre-encode | **on** | off | off | off |
| VBAQ | **on** | off | off | off |

   My first hypothesis (4 = bit 2 = pre-encode) was wrong: the valid bit comes first, so the
   4-5 band is the *quality preset*, and 6-7 is *high quality*. Pre-encode and VBAQ (bits 3-4,
   values >= 8) are reachable only through 1. The speed preset (0) is not reachable at all
   from GStreamer. 2 and 3 produce bit-identical streams (same byte counts below).

### Measurements (2026-11-01)

`client/scripts/enc-bench.sh` with the new `SRC=` mode (1080p NV12 file, 360 frames of a 60 fps
screen recording played 3 times, `SRC_REPEAT=3`), `rate-control=vbr bitrate=8000`, median of 3
rounds, fps of raw encoder throughput. **The machine was busy** (load average 55-60 from other
workers' builds, GPU 23 % busy from the desktop), which compresses H.265 to ~240 and AV1's fast
band to ~300; the bands, not the absolute values, are the result. The old idle-machine
`videotestsrc` numbers are kept in the #46 table below. (`videotestsrc` itself caps at ~180
fps on a loaded box, which is why the `SRC=` mode exists.)

| target-usage | 1 | 2 | 3 | 4 | 5 | 6 | 7 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `vah264enc` | 161 | 191 | 182 | 128 | 126 | 127 | 129 |
| `vah265enc` | 238 | 265 | 267 | 229 | 240 | 239 | 242 |
| `vaav1enc` | 268 | 297 | 305 | 152 | 152 | 95 | 96 |

Quality, same clip, encoded to a file and compared with ffmpeg (`psnr`, `libvmaf`), VMAF /
PSNR dB, actual bitrate in brackets. At 8 Mbit/s the clip is easy (6.3 Mbit/s used) and all
levels are within 0.1-0.5 dB; at 3 Mbit/s the rate control binds:

| 3 Mbit/s | H.264 | H.265 | AV1 |
| --- | --- | --- | --- |
| tu 1 | 77.2 / 38.2 (2465 kbps) | 88.5 / 41.1 (2371) | 89.9 / 41.3 (2399) |
| tu 2 (= 3) | 85.7 / 39.8 (2452) | 91.0 / 41.7 (2419) | 90.2 / 41.4 (2412) |
| tu 4 (= 5) | 85.7 / 39.8 (2454) | 91.1 / 41.7 (2424) | 90.8 / 41.7 (2397) |
| tu 6 (= 7) | 85.7 / 39.8 (2454) | 91.4 / 41.8 (2430) | 90.8 / 41.7 (2402) |

(8 Mbit/s AV1: VMAF 94.47 / 94.49 / 94.83 / 94.94 for tu 1 / 2 / 4 / 6.)
Single clip of game footage, objective metrics only; VBAQ is a perceptual tool that PSNR and
VMAF may undervalue.

Real app (headless rig, release build, one live AV1 share at 240 fps, 1080p vkcube, 8 Mbit/s,
sender stats tracer, 30 s window, machine load average ~25; the rig ran with
`HS_XDPW=patched` because the stock xdpw died on the start-up flap under that load, #59, on
the first try): encoder output fps

| preset (level, target-usage) | capture | into encoder | encoder output |
| --- | --- | --- | --- |
| Gaming (Performance, 2) | 238.3 | 236.0 | 232.6 |
| Balanced (3) | 238.9 | 238.9 | 235.3 |
| Quality (4) | 237.9 | 184.2 | 184.2 (encoder-limited) |

Balanced (3) holds the full 240; with the old Balanced value 4 it would have sat at the
Quality row. The log line `VA encoder kernel driver: amdgpu` confirms the detection.

### Decision

- **Performance 2, Balanced 3** on Mesa AMD: both are the balance preset. The encodes are
  identical, because nothing reachable sits between the balance and quality presets (the
  only other fast option, 1, is *worse*, below). Balanced's job here is to stay in the fast
  band (AV1 ~300 fps instead of ~150, H.264 ~185 instead of ~127), which is what the
  full-GPU-load lag needs; the cost is -0.0 to -0.6 VMAF.
- **Quality 4** (quality preset), not 1 and not 6. **Why not 1 (the old value):** pre-encode +
  VBAQ was slower than 2 on H.264 and *lower* in PSNR and VMAF on every codec at 3 Mbit/s
  (H.264 VMAF 77.2 vs 85.7), so "Quality" was the worst-looking level on this driver. **Why not
  6:** over 4 it adds nothing for H.264, +0.1 dB for H.265 and +0.01 dB for AV1 while AV1
  falls from ~152 to ~95 fps, below 120 fps streams.
- Other drivers (anything not on the `amdgpu` kernel driver, or undetectable) got
  Performance 2 / Balanced 4 / Quality 1 when this was written; since #94 they are **Performance 7 / Quality 1**
  (`capture.rs` `vaapi_target_usage`; Balanced no longer exists). Performance 2 there was unmeasured (an Intel box was
  not available; the spec's fastest is 7).
- **Detection:** the kernel driver behind `vah264enc`'s own `device-path` property
  (`/dev/dri/renderD128` -> `/sys/class/drm/renderD128/device/driver` -> `amdgpu`), read once
  (`OnceLock`) and logged as `VA encoder kernel driver: amdgpu`. **Why not the VA vendor
  string:** GStreamer has no accessor for it from Rust and libva would be a new dependency.
- The `QualityLevel` UI/saved values are unchanged; only the `target-usage` they map to moves.

## Encoder limits made visible; Performance = `target-usage=2` (2026-10-14, #46)

### `target-usage` on Mesa radeonsi is not the VA spec's order

Raw throughput, `client/scripts/enc-bench.sh`, 1080p NV12, 8 Mbit/s (CBR and VBR gave the
same numbers), AMD GPU, Mesa 26.2.4, GStreamer 1.28.8:

| target-usage | 1 | 2 | 3 | 4 | 5 | 6 | 7 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `vah264enc` | 246 | 307 | 307 | 172 | 172 | 172 | 172 |
| `vah265enc` | 482 | 536 | 536 | 476 | 476 | 476 | 476 |
| `vaav1enc` | 526 | 535 | 532 | 217 | 217 | 117 | 117 |

The driver groups the values into bands, and 7 (the spec's "fastest") was the slowest AV1
band and no faster than 4 for H.264. `QualityLevel::Performance` (Gaming and Low-latency
presets) now maps to **2**, the fastest band for all three codecs; Balanced (4) and Quality
(1) were unchanged then (moved on Mesa AMD by #92, section above, which also explains the
bands). **Why Performance is 2 (2026-11-01 update, #94):** that holds for Mesa AMD only; other
drivers get the spec's fastest, 7, since #92 gave radeonsi its own mapping (untested on Intel). **Why 2 and not 1:** 1 is slower and its rate control overshoots (H.264
wrote 65 % more bytes than at 2 for the same bitrate). Quality cost of 2 is noise (H.265
PSNR 40.83 dB at 2, 40.84 at 4, 40.90 at 7 on a `testsrc2` clip). Trade-off: a driver that
follows the VA order (Intel iHD) gets a slower encode for Performance; the hint below (removed, see below) still
tells the user when that caps the framerate. Full table and reasoning:
`EncoderBackend::launch_fragment` in `client/src/capture.rs`.

Real app, vkcube on a 280 Hz output, 240 fps target, sender stats tracer: AV1 Gaming 107.8 fps
encoder output before, 240.1 after; H.264 Gaming 239.9 after.

### The encoder-limit hint (removed)

Every 500 ms the pump compared the encoder's output fps with `min(frames offered to the branch, target fps)` and the UI
showed "Encoding N of M fps". **Removed 2026-11-04 (user, row 135):** "framerate is dynamic, so remove the warning. It
always pops up, when nothing is moving." The UI went first, the measurement (`EncoderLimit`) with row 133. The
offered-frames probe on the branch stays: `stall_verdict` uses it to tell a dead source from a wedged encoder.
The older stream-settings banner ("Your encoder reaches about N fps", "Use H.265") went on 2026-10-15 (user, #87: "it's just annoying").
