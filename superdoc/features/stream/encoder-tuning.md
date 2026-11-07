# Encoder tuning

Presets, tuning controls and their per-backend mapping, the start-stream defaults saved before a capture begins.

Part of the Stream Control reference, hub: `superdoc/features/stream-control.md`. The text moved here verbatim from that page: "above", "below" and "this page" in it may point at a section that now lives in a sibling page of this folder (the hub lists them).

## Presets: Text, Quality, Gaming; the keyframe interval is saved (2026-11-04, plan decision D8)

The four presets (Low-Latency, Balanced, Quality, Gaming) and the Advanced tuning rows are gone. The
user's table (`client/src/presets.rs`, `EncoderPreset`):

| preset | FPS (set once) | encoding (`QualityLevel`) | keyframe interval | tune (hidden) |
| --- | --- | --- | --- | --- |
| Text | 15 | Quality | 250 ms | best quality |
| Quality | 60 | Quality | 1000 ms | best quality |
| Gaming | 120 | Performance | 250 ms | low latency |

*Why (user's table, plan decision D8):* three presets people can tell apart replace four that were
partly twins; only Performance and Quality survive as "encoding" modes (the VA `target-usage` mapping
below is unchanged, Quality = 4 on Mesa AMD, Performance = 2; x264 Quality is `medium`, see the `Why:` below). Text is for
documents and code: sharp at a low frame rate, a keyframe is cheap at 15 fps so 250 ms costs little.

- **A preset sets the FPS once.** `apply_preset_to_start_stream` / `DestEdit::apply_preset` write the
  preset's frame rate, quality level, tune and keyframe interval; the FPS dropdown afterwards changes
  only the rate. "Which preset is lit" is derived from **FPS + quality level + keyframe interval**, all
  exact (`EncoderPreset::detect(framerate, tuning)`, 2026-11-05; user: "it should only show a preset, if
  the settings match it perfectly"), so a hand-changed FPS unlights it and the matching FPS relights it,
  and a stale stored tune does not unlight it (the tune only matters on x264 and is no longer shown). A combination that matches no preset
  (an older settings file) lights no button; the Video card's caption always reads the real values
  (`60 FPS · Quality encoding · Keyframe 1000 ms`) and never says "custom".
- **The keyframe interval is saved** in `settings.toml` (`start_stream_defaults.gop_millis`, written at
  "Go live" with the rest) and restored by `apply_start_stream_defaults`. It used to be reset to the
  default on every start because only Gaming moved it; now every preset sets one, so forgetting it would
  show a preset's name with another preset's interval. `session::save_start_stream_defaults` now takes the
  whole `StartStreamDefaults`.
- **Old files keep loading.** `StartStreamDefaults.quality_level` loads `"balanced"` (the retired middle
  level) as `quality` (`#[serde(alias = "balanced")]` on `QualityLevel::Quality`); `tune = "low_latency"` is kept as stored (hidden, only x264
  reads it); a missing `gop_millis` becomes 1000 ms (`DEFAULT_GOP_MILLIS`, the Quality preset's), a missing
  `quality_level` Quality. An old `performance` stays Performance at 1000 ms, which is no preset until the
  user picks Gaming (the old Gaming's 750 ms was never saved, and old Low-Latency and Gaming are
  indistinguishable in the file, so they cannot both map to Quality as the old preset names would
  suggest). Stored values are not rewritten until the next save. `QualityLevel::Balanced` is removed from the
  enum: `quality_level = "balanced"` is a `serde(alias)` of `Quality`, so nothing can produce it any more
  (a file written by this build never contains it; an old one is not rewritten until its next save).
  `EncoderPreset::from_value("balanced" | "low_latency")` is Quality, for symmetry with the browser, which
  stores preset names (`docs/js/core/video-tx.js` `ENCODER_PRESETS`).
- **Fresh app default = the Quality preset's fields** (`AppState::default`, `StartStreamDefaults::default`)
  at the old 30 fps / 8000 kbps; 30 fps with Quality encoding and 1000 ms lights no preset button since 2026-11-05 (`detect` matches fps + quality + gop exactly, `presets.rs`; see above). `capture::DEFAULT_GOP_MILLIS` is 1000 now (was the protocol spec's 2 s; the spec
  figure is only a self-heal ceiling, a shorter interval stays conformant, it costs bitrate and buys a
  faster join/recovery). The webcam (task I8) is fixed to Performance and 2000 ms (`EncodeChoice::webcam`, see
  "Webcam capture" above).
- **Encoding and Keyframe are editable again (2026-11-04):** the modal shows Encoding (quality level) and Keyframe (`gop_millis`) as editable fields next to the Preset; reasoning in `ui/SHARE-FLOW.md` "Video card: Encoding and Keyframe".
- **Removed:** `EncoderProfile` (and `EncoderBackend::tune_applies`, `quality_effect`,
  `visible_quality_levels`, `display_quality_level`, `EncoderPreset::visible_for/detect_for`): the twin hiding
  they drove (#95) is moot with three presets that are distinct on every backend. The "hide a setting that
  does nothing" rule is kept by deleting the Tune, Quality-level and Keyframe rows rather than hiding them.
- **UI** (`ui/stream_settings.rs`, replica `superdoc/ui/replica/index.html`): right column **Compression**
  (Codec, Bitrate) then **Video** (Preset, FPS dropdown, Encoding, Keyframe, caption). The Source card's **Change** button is
  there staged and live. While live it ends the stream first (`Discard` + `StageShare`: there is no source
  swap under a running encoder; it also stops the capture's other rooms), and the modal reopens staged.
- **Keyframe interval at 250 ms: nothing broke.** `key-int-max` = `capture::gop_frames` (rounded frames:
  Text is 4 frames = 267 ms at 15 fps). The sender-redundancy bursts (#97: +300 ms / +1 s repeats, one per
  `KEYFRAME_BURST_MIN_GAP` = 2 s) only follow forced IDRs and are harmless next to a natural IDR every
  250 ms; `IdrControl`'s "big" test is by size (> 1/4 s of bitrate), independent of the interval, and a
  natural IDR that large at 250 ms would eat the whole interval's budget, which the VBR does not produce on
  these contents. Not stress-tested with a big-IDR scene at 250 ms (the rig's vkcube IDRs are 30-60 kB).
  **It was not harmless for the picture:** with a big-IDR scene every keyframe is a pacing freeze, 4 per second at
  250 ms; see "The lagspike on every keyframe" at the top of this page (row 129), which fixes it.
- **Measured** (headless rig, release build, H.264 VA, 1080p vkcube, 8000 kbps, GStreamer tracer on the
  encoder output pad, `DELTA_UNIT` clear = keyframe; the client log's `sending a keyframe` line only covers
  keyframes of 24+ datagrams, vkcube's are smaller at 60/120 fps, so it cannot be used for this):

  | preset | encoder output | keyframes | median gap (p10 / p90) |
  | --- | --- | --- | --- |
  | Text | 15.0 fps | 282 in ~75 s | 267 ms (263 / 271) |
  | Quality | 60.0 fps | 27 in ~24 s | 1001 ms (697 / 1003; the 697 is the transition) |
  | Gaming | 119.7 fps | 241 in ~60 s | 250 ms (246 / 254) |

  The native viewer decoded Text (sidebar "16 fps", 5.6 Mbps) and Gaming ("121 fps"). `settings.toml` after
  "Go live" with Text: `framerate = 15`, `tune = "best_quality"`, `quality_level = "quality"`,
  `gop_millis = 250`; the next staged card opened with Text / 250 ms / 15 fps. Presets were switched live
  Text -> Quality -> Gaming (a branch rebuild each, the stream kept running).
- **Why x264 Quality is `medium`, not `veryslow` (2026-11-04):** Quality became the default (and is the Text
  preset), and software x264 must not get heavier than the old default, Balanced = `medium`. Windows encodes
  with `x264enc` only (`features/windows.md`), so `veryslow` at 1080p would have been the default there.
  `capture::EncoderBackend::launch_fragment` maps `Performance` to `ultrafast` and `Quality` to `medium`
  (pinned by `quality_level_maps_to_speed_preset_and_target_usage`). VA is unchanged (Quality = 4 on Mesa AMD,
  1 elsewhere). The `windows.md` row that measured x264 at `veryslow` (~line 210) is historic and was left as
  written. Not measured on x264 since the change (it is the same `speed-preset` the old default used).

## Encoder tuning controls

**Superseded in the UI 2026-11-04** (Tune, Quality-level and Keyframe rows and the four old presets are gone, see "Presets: Text, Quality, Gaming"); the backend mapping below is still how the fields are encoded.

**Added 2026-07-28.** Three more per-destination live-editable settings, alongside
resolution/framerate/bitrate: **Tune** (Low-Latency / Best Quality — `BestQuality` default),
**Quality Level** (Performance / Balanced / Quality — `Balanced` default), and **Rate
Control** (Constant / Adaptive / Quality — `Constant` default). Defined as `TuneMode`,
`QualityLevel`, `RateControlMode` (removed in #88) in `capture.rs`, exposed via `edit_destination`'s
`tune`/`quality_level` params (snake_case strings, same convention as every
other `edit_destination` param) and as controls in the destination settings
(`client/src/ui/stream_settings.rs`).

`Why the defaults are `BestQuality`/`Balanced`, not either speed/quality extreme` — the
original product requirement (from this feature's owner) was explicit: "Low latency should
be an option, but quality also has to be achievable. By default, it should be a good mix."
The pipeline's original hardcoded behavior was `tune=zerolatency` + `speed-preset=ultrafast`
— the fast/low-quality *extreme*, not a middle ground, and an audit (2026-07-28) confirmed
this as a real product bug against that requirement. The fix: `TuneMode::BestQuality` (no
`tune=zerolatency`; trades a little latency for meaningfully better compression) and a new
`QualityLevel::Balanced` variant — `speed-preset=medium`/`target-usage=4`, the actual
midpoint between `Performance`'s `ultrafast`/`7` and `Quality`'s `veryslow`/`1` (`medium` is
also x264's own upstream default preset) — are now the `#[default]`s, with `LowLatency` and
`Performance` demoted to explicit opt-ins for whichever extreme a user actually needs.
`RateControlMode::Constant` (removed in #88) (CBR) stays the default untouched: CBR-by-default is the correct
choice for live-streaming bandwidth predictability on its own merits, not an extreme that
needed fixing. Regression coverage for the new mapping:
`capture::tests::quality_level_maps_to_speed_preset_and_target_usage` (asserts `Balanced` →
`medium`/`4` on both backends; since #92 the VA half compares against `vaapi_target_usage`, 3 on Mesa AMD, and `vaapi_target_usage_per_driver` pins the table) and `capture::destination::tests::tuning_knob_changes_are_tier_two`.

### Presets (simple) over the raw tuning fields (advanced)

*Why two tiers:* the project requirement is "easy for regular users and powerful for advanced
users", and showing Tune/Quality/Rate Control jargon (Rate Control has since been removed, #88) on every card failed the first half. A
**preset** is a convenience over the same three raw fields, never a fourth field: picking one sets
`tune`/`quality_level`/`rate_control` together through the same `edit_destination` call a single
raw control uses. The table lives in `client/src/presets.rs` (ported from the former Electron
frontend's `ENCODER_PRESETS`; it later gained a fourth preset, Gaming, see "Rate control collapses
..." below):

| Preset | `tune` | `quality_level` | `rate_control` |
| --- | --- | --- | --- |
| Low-Latency | `low_latency` | `performance` | `constant` |
| Balanced (default) | `best_quality` | `balanced` | `constant` |
| Quality | `best_quality` | `quality` | `quality` |

The active preset is computed from the destination's current three fields (whichever preset
matches exactly); a hand-tuned combination selects none, an intentional unvalidated state. The raw
fields sit behind an "Advanced" disclosure that must keep its open state across re-renders.
Framerate is a regular-user control (always visible); the raw knobs are not.


**Why these are tier 2 (`BranchSwap`), not tier 1 (a live property write) — including for
`RateControlMode::Adaptive` (removed in #88), which turns out to be a no-op on one backend:** none of
`x264enc`/`vah264enc`'s tuning properties (`speed-preset`/`target-usage`,
`tune`, `rate-control`/`pass`) are safely writable on an already-`Playing` element the way
`bitrate` is — `EncoderBackend::launch_fragment` bakes them into the element's
*construction* string. So "change one of these" is definitionally "build a new encoder,"
which is exactly what a branch swap already exists to do safely (make-before-break, wait
for the new branch's first IDR, flip authority, drop the old one — see "Editing a live
destination" above). Routing these three through the *same* `classify_change`/
`swap_to_new_branch` machinery every other tier-2 edit uses means they inherit the
readiness gate and the `KeptOld`-on-failure safety net for free, with zero new
state-machine code.

### Backend mapping — real, and asymmetric between backends

**Since 2026-11-04 the Balanced level is gone and x264 `Quality` is `medium` (see "Presets: Text, Quality, Gaming"); the table below is the older mapping.**

| Control | `x264enc` (Software) | `vah264enc` (Vaapi) |
| --- | --- | --- |
| Tune | `tune=zerolatency` (LowLatency) / unset (**BestQuality, default** — x264enc has no explicit "best quality" value, only the *absence* of the low-latency one) | **No equivalent control.** The field is still accepted and stored, but has no effect while the branch is VA-API, so the stream-settings modal does not show the Tune row there (#90, `ui/APP-SHELL.md`) |
| Quality Level | `speed-preset`: `ultrafast` (Performance) / `medium` (**Balanced, default** — x264's own upstream default preset) / `veryslow` (Quality) | `target-usage`, VA spec order (Intel): `7` (Performance; `2` between #46 and #94, 2026-11-01; untested on Intel) / `4` (**Balanced, default**) / `1` (Quality). **Mesa AMD (radeonsi), since #92:** `2` / `3` (**Balanced, default**) / `4` — `target-usage` is a bitfield there, see "`target-usage` is a bitfield on Mesa AMD" |
| Rate Control | No native enum — `Constant` (default)/`Adaptive` are both plain `bitrate=` (**`Adaptive` is a deliberate no-op relative to `Constant`** — x264enc's CBR is already frame-adaptive within its budget, so there's no meaningfully different x264enc setting to reach for); `Quality` switches to `pass=qual quantizer=21` (bitrate-target `bitrate=` is then ignored) | `rate-control=cbr`/`vbr`/`cqp` — a genuine three-way native enum, no asymmetry |

`Why the `Adaptive`/`Software` no-op is documented rather than faked with some invented
x264enc setting:` a real asymmetry between backends is more honest recorded plainly than
silently pretended symmetric — a destination on the software backend showing "Adaptive"
selected and behaving identically to "Constant" is the accurate picture of what's actually
happening, not a bug to paper over.

**Superseded 2026-07-30 — see the section below.** The `Rate Control` row above (three-way
`Constant`/`Adaptive`/`Quality`) describes the *original* design; it has since collapsed to a
two-way `Cqp`/`Qvbr` choice. Left in place rather than rewritten because the "why" reasoning
above (the `Adaptive`/software no-op asymmetry, CBR-by-default for predictability) is still
accurate history for the design that predated the collapse.

### Interaction with viewers: codec-string re-derivation

Toggling any of these three controls on a *live* destination triggers a real `BranchSwap`
— a new encoder with (potentially) a different H.264 profile/level. `x264enc` in
particular picks its AVC profile/level automatically from `speed-preset` with no explicit
`profile=` set anywhere in this pipeline, so a Performance→Quality toggle can genuinely
change the bitstream's profile out from under an already-connected WebCodecs watcher. This
was a real, real-hardware-verified bug (a watcher connecting *after* all three controls had
been toggled got stuck at 0fps) — the fix lived entirely on the viewer's delivery side (the
codec string is re-derived from every keyframe's bitstream, never cached once). The native decoder
has the same property by construction: `decode/codec.rs` sniffs the codec from the bitstream
(`superdoc/meta/TERMINOLOGY.md` "Sniffing"). Regression coverage:
`end_to_end_test::late_watch_after_all_three_tuning_controls_toggled_gets_a_real_keyframe_every_time`.
Nothing about `capture.rs`'s own tuning-control mechanism needed to change for that fix.

---

## Start-stream defaults: process-wide settings applied before a capture starts

*Why this shape:* `edit_destination` needs an existing `destination_id`, and a stream that has not
started has none. Rather than invent a third settings mechanism, a not-yet-started stream uses the
existing pattern: **process-wide settings on `AppState`** (`framerate`, `bitrate_kbps`, `tune`,
`quality_level`, `gop_millis`, `codec`, with setters that clamp), read by
`go_live`/`start_video_share` when they build the first destination's `VideoParams`.

**Persistence:** `rooms::StartStreamDefaults` (nested as `AppSettings::start_stream_defaults`) uses
the same local-TOML load-modify-save mechanism as `save_bandwidth_settings`
(`session::save_start_stream_defaults` never clobbers sibling settings). `TuneMode`/`QualityLevel`/
`VideoCodec` derive `Serialize`/`Deserialize` with `snake_case` (`RateControlMode` was removed in #88), so the stored
strings equal the wire strings; every new field is `#[serde(default)]` so an older `settings.toml`
still loads. *Why separate from the live `AppState` settings view:* the live `framerate`/
`bitrate_kbps` reset to defaults every launch ("every launch starts fresh"), which is a different
question from "what did the user last save"; tune/quality only exist per destination.
First-run fallback (since 2026-11-04) is the Quality preset's encoding and keyframe at the old 30 fps default (`DEFAULT_FRAMERATE`; the Quality preset itself is 60 fps): 30 fps / 8000 kbps / `best_quality` /
`quality` / 1000 ms keyframe interval (saved too now) / H.264. The `[start_stream_defaults]` block is written to `settings.toml`
in the app's config dir; check a persistence change with a scratch `XDG_CONFIG_HOME` and the `run`
skill's `drive.sh`.

---
