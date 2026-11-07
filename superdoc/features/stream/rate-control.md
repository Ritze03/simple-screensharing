# Rate control

The history of the rate-control modes (CQP/QVBR, Dynamic/Static) and why encoding is now always Dynamic.

Part of the Stream Control reference, hub: `superdoc/features/stream-control.md`. The text moved here verbatim from that page: "above", "below" and "this page" in it may point at a section that now lives in a sibling page of this folder (the hub lists them).

### Rate control collapses from three modes to two: CQP / QVBR, new default, new Gaming preset (2026-07-30)

**Why:** the project owner's own real-world usage of all three modes: `Constant` (CBR)
"wastes bandwidth for no visible gain" on easy content (CBR pads to the target bitrate
regardless); `Adaptive` (VBR) showed visible "pop-in" artifacts — traced to
`target-percentage` never being set, so VBR's real target swings between its default 66%
and 100% of the configured ceiling as content changes; `Quality` (CQP) was "the best of the
three, but barely uses the available bandwidth" and never sharpens a static/still image over
time — expected, since CQP targets a fixed quantizer (`qpi`/`qpp`/`qpb`, left at the element
default of 26) with no bandwidth-ceiling awareness and no scene-change/refinement feature at
all (`vah264enc`'s full property list has none to reach for).

**The fix:** `RateControlMode` is now `Cqp`/`Qvbr` (wire strings `"cqp"`/`"qvbr"`) — `Constant`
and `Adaptive` are gone, not just hidden. `Cqp` keeps the old `Quality` mode's behavior
unchanged (still the "resourceful"/predictable-bandwidth choice). `Qvbr` is new: `vah264enc`'s
`rate-control=qvbr` ("quality-defined VBR") — a mode this codebase had never used before,
confirmed to exist via `gst-inspect-1.0 vah264enc` on the project's real AMD GPU. QVBR
targets quality (`qpi=20`, a deliberate step down from CQP's 26) while still treating
`bitrate` as a real ceiling and `cpb-size` (set to 1x the configured bitrate) as the VBV
buffer — quality-driven but genuinely bandwidth-bounded, which is what "adaptively use as much
of the available bandwidth as possible for good quality" actually requires given what this
specific encoder element supports. **`Qvbr` is now the default** (not `Cqp` — the owner's
explicit call: maximize quality by default, not conserve bandwidth by default).

**Superseded 2026-10-15 (#87): the "Efficient"/"Max Quality" names and the Low-Latency/Gaming = Cqp rows below were reversed/changed, see "Rate control renamed Dynamic / Static".**

The Advanced panel's raw `Rate Control` `pillSwitch` now shows two plain-language options,
"Efficient" (Cqp) and "Max Quality" (Qvbr), replacing the old three raw mode names. The four
`ENCODER_PRESETS` (now `client/src/presets.rs`) were re-assigned per-preset rather than blanket-defaulted:

| Preset | `tune` | `quality_level` | `rate_control` | GOP |
| --- | --- | --- | --- | --- |
| Low-Latency | `low_latency` | `performance` | **Efficient (Cqp)** | 2000ms |
| Balanced (default) | `best_quality` | `balanced` | **Max Quality (Qvbr)** | 2000ms |
| Quality | `best_quality` | `quality` | **Max Quality (Qvbr)** | 2000ms |
| Gaming (new) | `low_latency` | `performance` | **Efficient (Cqp)** | **750ms** |

Gaming uses Cqp over Qvbr deliberately: constant motion rarely gives Qvbr's implicit
"spend leftover bandwidth budget on a static/still frame" upside anything to work with, while
Cqp's fixed per-frame quantizer holds up more predictably under continuous motion. Gaming's
750ms GOP (vs. every other preset's 2000ms) trades a little bandwidth for faster recovery from
motion-induced artifacts.

**GOP is now preset-driven, not a single global constant.** Previously every preset shared one
hardcoded `GOP_SECONDS = 2` regardless of its other settings. `gop_millis: u32` is now threaded
end-to-end: `capture::destination::VideoParams`/`branch::EncodeSettings` →
`classify_change` (a `BranchSwap` trigger, same tier as the other three tuning knobs) →
`add_branch`'s frame-count formula → `session::DestEdit`/`AppState` →
`StagedCaptureRow` (`capture.rs`)/`edit_destination` → the UI (originally a `set_gop_millis` command of the removed Tauri layer).
Deliberately **not** persisted to `settings.toml` — it's a preset-specific tradeoff, not a
standalone remembered setting, so `stageVideoShare()` always resets it to 2000ms before
staging a fresh capture. (Superseded: `gop_millis` is saved in `start_stream_defaults` since 2026-11-04, see "Presets: Text, Quality, Gaming".)

**Left unwired, flagged rather than guessed at:** `ref-frames` — the same IPC-plumbing cost as
`gop_millis` for a knob that wasn't asked for; a real follow-up if lower ref-frame counts turn
out to matter for Gaming's decode latency.

Regression coverage: `rate_control_mode_maps_per_backend` (rewritten to assert `qpi`/
`cpb-size` are genuinely different between Cqp/Qvbr on real VAAPI, not just that the enum
value round-trips) and `tuning_knob_changes_are_tier_two` (extended to cover `gop_millis`
too), both in `capture.rs`.

### Investigated: "For QVBR, the max bitrate is entirely ignored" — confirmed real, root cause is a driver limitation, not a simple bug (2026-07-30)

**Owner's report, verbatim:** *"For QVBR, the max bitrate is entirely ignored."*

**Investigated live, not guessed at.** First ruled out the property-string theory from the bug
report: constructed the exact same `vah264enc ... rate-control=qvbr bitrate=N qpi=20
cpb-size=N` fragment via `Gst.parse_launch` with the properties in every plausible order
(`bitrate` before/after `qpi`/`cpb-size`, `rate-control` set last) and read every property back
off the live element both at construction and while `Playing` — `bitrate`, `cpb-size`, `qpi`,
`rate-control` all held exactly the configured values in every ordering, at every point. This
codebase's own pipeline string was never the problem; GStreamer's property-string parsing has no
ordering quirk here.

**Then measured actual encoded output**, real `vah264enc` on this machine's real AMD GPU
(radeonsi/Mesa, VA-API 1.24), with a pad probe summing real encoded bytes/sec at 1920×1080/30fps,
across content complexity and two very different `bitrate` ceilings (2000 vs 8000 kbps) at the
shipped `qpi=20`:

| Content | Ceiling 2000 kbps | Ceiling 8000 kbps |
| --- | --- | --- |
| Static test pattern (`smpte`) | 19034 kbps | 19207 kbps |
| Moving content (`ball`) | 75 kbps | 75 kbps |
| Worst-case incompressible noise (`snow`) | 159239 kbps | 159239 kbps |

**The ceiling has zero measurable effect on output, in every case, confirmed on the live
element.** This is not "QVBR rarely needs to hit the ceiling on real content" (the static-pattern
row is *already* 9-10x over an 8000 kbps ceiling — the opposite of a ceiling that rarely binds,
it's a ceiling that never binds at all) and it is not fixable by retuning `qpi` (no `qpi` value
changes whether `bitrate`/`cpb-size` are honored — they simply aren't, regardless).

**A/B confirmation this is `qvbr`-specific, not a general VAAPI/property bug**: the same harness
against `rate-control=cbr` and `rate-control=vbr` on identical `smpte` content genuinely scaled
with the ceiling (`cbr`: 1886 kbps @ ceiling 2000, 7922 kbps @ ceiling 8000; `vbr`: 1878 / 7922,
same numbers) — proving this machine's VA-API driver/hardware *can* and *does* respect a bitrate
ceiling, just not under `rate-control=qvbr`. `rate-control=cqp` (this project's other mode) showed
the identical "ignores everything but its own quantizer" shape as `qvbr` (18295 kbps regardless of
ceiling) — expected and correct for `cqp`, which was never supposed to respect a ceiling; the
finding is that on this driver, `qvbr` behaves like `cqp` (quantizer-only, no real ceiling) rather
than like its own documented "quality within a bandwidth ceiling" contract. `GST_DEBUG=va*:6`
shows `vah264enc` computing and logging a max/target bitrate internally (`_ensure_rate_control`:
"Max bitrate: 3030 kbps, target bitrate: 2000 kbps") and a driver warning
(`Driver does not support attribute VAConfigAttribEncQuantization`) — GStreamer believes it's
telling the driver to cap bitrate; the driver silently doesn't. This is a genuine driver/hardware
limitation of AMD's Mesa radeonsi VA-API QVBR implementation, external to this codebase, not a
property-ordering bug, not a units mismatch, and not fixable by tuning `qpi`.

**The real fix would trade one already-rejected problem for another.** `rate-control=vbr` *does*
respect the ceiling (measured: static/simple content ~19 kbps, detailed-static content scaling up
to ~5900 kbps under a 6000 kbps ceiling, worst-case noise content briefly exceeding the ceiling by
~58% under a 6s burst — ordinary encoder HRD-compliance slop, not a broken cap) — appearing to be
the "real fix." But `vbr` is exactly the mode this same file's "Rate control collapses from three
modes to two" section (immediately above, same date) documents this project deliberately moving
*away* from, because the owner's own real-world testing found `vbr`'s (then `Adaptive`'s) bitrate
swinging between its `target-percentage` and the ceiling as content changes produced visible
"pop-in" artifacts. `qvbr` was adopted specifically *because* it promised quality-driven,
ceiling-bounded encoding without that swing — a promise this investigation now shows this
hardware's driver does not keep. Switching back to `vbr` would make the ceiling real again at the
cost of reintroducing the exact swing/pop-in behavior this project already rejected once; staying
on `qvbr` keeps the smooth quality but the ceiling stays fictional. `target-percentage` retuning
(tested 50 vs 66, the only two values in its valid 50-100 range near the low end) made no
measurable difference in these tests, so it isn't a free way out of the tradeoff either — but this
was only measured as aggregate bitrate over fixed windows, not as subjective visual "pop-in", so
it doesn't rule out `vbr` with a different `target-percentage` being an acceptable middle ground.

**Not fixed — escalated per this task's own instructions.** No code change was made in
`capture.rs`: this is the "genuine `qpi`-vs-`bitrate` design tension" (in fact one level worse —
not a tuning tradeoff but a hardware capability gap) that the task explicitly said to stop and
report on with real data rather than guess at, and the two known real options (keep `qvbr`'s smooth
quality with a non-functional ceiling, or switch to `vbr` and reintroduce its previously-reported
swing/pop-in) directly re-litigate a tradeoff this project already made a considered call on once.
Whoever picks this up next should decide with the owner: accept `qvbr`'s ceiling as
aspirational/best-effort (and say so in the UI/docs), switch to `vbr` and re-test for the pop-in
artifact specifically (possibly with a lower `target-percentage` than the untuned 66% that caused
the original complaint), or investigate a non-default combination (e.g. `vbr` with a tightly-set
`cpb-size` as a hard VBV cap) not yet tried here.

### Follow-up: switched "Max Quality" to `vbr` with a tight `cpb-size` + high `target-percentage` — ceiling now genuinely works, pop-in measurably reduced (2026-07-30)

**The owner's decision, given the tradeoff above:** option (b) — try `vbr` again, but with a
tighter `cpb-size` than the naive 1x-bitrate buffer, on the hypothesis that a smaller VBV buffer
forces more frequent, finer-grained rate adjustment and might avoid the original pop-in mechanism
via a different lever than `target-percentage` alone.

**What changed in `capture.rs`:** `RateControlMode::Qvbr`'s VAAPI branch now emits
`rate-control=vbr` (was `rate-control=qvbr`), with `target-percentage=95` (`VBR_TARGET_PERCENTAGE`)
and `cpb-size = bitrate_kbps * 3 / 8` (`VBR_CPB_SIZE_NUMERATOR`/`VBR_CPB_SIZE_DENOMINATOR`) instead
of the old `qpi=20 cpb-size=bitrate_kbps` (1x buffer). The `qpi` property is dropped entirely —
it's a `qvbr`/`icq`-specific quality-factor knob, meaningless under plain `vbr`. `Cqp`/"Efficient"
is untouched. The enum variant keeps the name `Qvbr` despite no longer emitting literal
`rate-control=qvbr` — see `capture.rs`'s doc comment on `RateControlMode` for the full reasoning
(short version: the wire string `"qvbr"` is both the UI's `rateControl` value in
~15 call sites and the on-disk persisted field in `AppSettings`, so renaming it would break
existing users' saved settings for a purely cosmetic gain; the user-facing "Max Quality" label is
unaffected either way).

**Verified live, real hardware, real `vah264enc`, same AMD GPU** — not the full two-party
relay+viewer path this time (a sender's own `bitrate_kbps` (then `DestinationStatsDto`, now `Destination::live_params()`) merely
echoes the *configured* bitrate, not a measured one — see that field's doc comment —
so seeing the real achieved bitrate requires a second party's `received_bitrate_kbps`, out of scope
for this pass's effort budget), but the exact same GStreamer element and properties
`launch_fragment` constructs, driven directly via `gst-launch-1.0` against real decoded video
(not synthetic test patterns — a real 1280×720/30fps clip, plus a noise-boosted variant to force
genuinely demanding content):

- **Ceiling now genuinely works**: on demanding real content, a 2000 kbps ceiling produced
  ~1911 kbps actual output (95.6% of ceiling — matches the tuned `target-percentage=95` almost
  exactly) and an 8000 kbps ceiling produced ~7641 kbps (95.5%) — output scales cleanly with the
  ceiling, both landing right at the configured target-percentage, unlike `qvbr`'s measured
  complete non-response to the same 4x ceiling change. This matches/slightly improves on the prior
  investigation's ~1886/7922 kbps figures for plain untouched `vbr`.
- **Bitrate stability (the actual pop-in proxy)**: encoded a 19s clip that cuts from static/simple
  content to noise-boosted demanding content partway through (a real complexity change, not a
  synthetic spike), at a fixed 4000 kbps ceiling, once with this fix's tuning
  (`target-percentage=95`, `cpb-size=0.375x`) and once with the old `Adaptive`-era untuned baseline
  (`target-percentage` unset/66%, `cpb-size=0`/auto ~1x). Measured achieved bitrate in 500ms
  windows across the steady-state demanding-content portion (excluding the initial keyframe spike,
  which is identical scene-cut cost either way):

  | | tuned (this fix) | untuned baseline |
  | --- | --- | --- |
  | mean | 4443.6 kbps | 4818.9 kbps |
  | stdev | 506.9 kbps | 787.2 kbps |
  | coefficient of variation | 0.114 | 0.163 |
  | mean frame-window-to-window swing | 550.8 kbps | 862.1 kbps |
  | max swing | 1668.5 kbps | 2165.5 kbps |

  The tuned combination cuts stdev by ~36%, mean swing by ~36%, and max swing by ~23% relative to
  the untuned baseline that originally caused the reported pop-in — a real, measured improvement in
  bitrate stability, consistent with (though not itself proof of) less visible pop-in.
- **`Cqp`/"Efficient" unaffected**: confirmed via the updated `rate_control_mode_maps_per_backend`
  unit test (real `gst::init()` + real `vah264enc` element construction) — `Cqp` still emits
  `rate-control=cqp`, untouched `qpi=26`, `cpb-size=0`; only `Qvbr`'s VAAPI branch changed.
- `cargo test --workspace` passes (183 + 12 tests, 0 failed).
- The app (then the Electron shell) still launched without console errors after this change;
  driving a full two-party watch session to read a viewer's real
  `received_bitrate_kbps` was not done in this pass — flagged above as the honest verification gap.

**Why these specific values**: `target-percentage=95` sits at the high end of the property's
50-100 range so the real encode target tracks close to the configured ceiling rather than the
default 66% (the original pop-in's diagnosed cause). `cpb-size`'s 3/8 (0.375x) fraction was chosen
as a value meaningfully tighter than the old 1x-bitrate buffer per the task's guidance
(0.25x-0.5x range) — not separately swept against other fractions in this pass; the measured
result above already shows a real improvement at this value, so no further tuning was done, but a
tighter (e.g. 0.25x) or looser (0.5x) value remains unexplored if the current setting turns out to
need further adjustment after real-world (non-scripted) use.

### Rate control renamed Dynamic / Static; Low-Latency and Gaming move to Dynamic (2026-10-15, #87)

**Superseded the same day (#88): the choice was removed, only Dynamic remains. Kept for the measurements (the rationale).**

**User report (2026-10-15):** "'Efficient' works flawlessly. 'Max quality' freezes on big movements ... Efficient uses like 6 Mbit/s with just my desktop showing, and Max quality like half a Mbit/s, but it should be the other way around, right?"

**The names were backwards, the modes were not switched.** `Cqp` (shown as "Max quality") holds the
*quantizer* (quality per frame) constant, so the bitrate follows the content: tiny on a still desktop, enormous
on motion, and the configured kbps is not a cap. `Qvbr` (shown as "Efficient", emits VA `rate-control=vbr`)
holds the *bitrate* near `VBR_TARGET_PERCENTAGE`% (95) of the configured value with the configured value as
a ceiling, and lets quality vary. "Max quality" sounded like "spends the most" but is the one that spends
least when nothing moves; "Efficient" sounded frugal but spends toward its budget.

**Measured** (`vah264enc`, 1080p60, 5 s, `b-frames=0 key-int-max=120 target-usage=2`, `videotestsrc` through
`vapostproc`, same element properties `launch_fragment` builds; 8000 kbps configured):

| content | `vbr` (Dynamic) | `cqp` (Static) |
| --- | --- | --- |
| `smpte` (static, detailed) | 7484 kbps | 35591 kbps |
| `ball` (static, simple) | 131 kbps | 84 kbps |
| `snow` (noise, worst-case motion) | 9340 kbps | 561864 kbps |

So CQP on busy content produces 4-70x the configured bitrate: a link that cannot carry it drops packets and
the stream freezes (the user's "freezes on big movements"). Dynamic stays at about the budget. On a real
1080p60 vkcube in the rig, Low-Latency + Dynamic at 8000 kbps measured 7.6 Mbps at the viewer (= 95 %),
and the same stream switched live to Static 4.8 Mbps (simple moving content, under the cap; the spikes
above need more complex content).

**Why Dynamic shows ~6 Mbit/s on a still desktop:** VA `vbr` does not undershoot much on detailed
content even when it is static: with `target-percentage=95` the rate control keeps spending toward the target
(it lowers QP on the P frames and the periodic IDRs are large), see the `smpte` row. Truly flat content
(`ball`) does stay tiny (131 kbps). Text and gradients on a desktop land in between. This is the
2026-07-30 tuning working as designed (a high target tracks the budget, which is what avoids the old
"pop-in" swings), not a bug, and was deliberately left alone here. x264 (software) `Qvbr` is plain
`bitrate=` (ABR-like single pass) and was not measured.

**What changed:** UI names Efficient -> **Dynamic** (`Qvbr`), Max quality -> **Static** (`Cqp`), de
"Dynamisch" / "Statisch", with a plain-words tooltip (`ui.stream.rate_control_tip`); i18n keys renamed
`rc_dynamic` / `rc_static`. Enum names and the persisted `"qvbr"`/`"cqp"` values are unchanged (saved
settings keep working). `presets.rs`: **Low-Latency and Gaming now use `Qvbr`** (Dynamic), so all four
presets are Dynamic; Low-Latency vs Gaming still differ by GOP only. A saved Low-Latency/Gaming setup with
`Cqp` now matches no preset (shown as hand-tuned, Static selected) until a preset is picked again.
The browser mirrors it: Low-Latency/Gaming use `bitrateMode: "variable"`, the read-only rate-control row
reads "Dynamic (variable bitrate)".

### Rate control is no longer configurable: always Dynamic (2026-10-15, #88)

**User request (2026-10-15):** "It's probably best if we just make Dynamic the default and remove the ability to
configure that. So the rate control is just set and it just works, since it behaves really good and the other one
doesn't behave at all."

`Why:` Static (CQP) froze the stream on motion because it ignores the configured bitrate (measurements in the #87
section above: 4-70x the configured kbps on busy content); Dynamic held the budget. With one mode that was never
the wrong choice, the setting was only a way to break a stream, so it was deleted rather than hidden (nothing in
the UI, presets or settings reads it any more).

**What is gone:** `capture::RateControlMode` (`Cqp`/`Qvbr`) and every `rate_control` field (`VideoParams`,
`EncodeSettings`, `EncoderTuning`, `DestEdit`, `NextStream`, `AppState`, `StartStreamDefaults`, the
`launch_fragment` argument); the Advanced "Rate control" row; i18n `ui.stream.rate_control`, `rate_control_tip`,
`rc_dynamic`, `rc_static`; the browser's read-only row, `bitrateMode` on the presets and the `bitrateMode` override
on `shareScreen` (the sender always uses `"variable"`). A preset is now `tune` + `quality_level` + GOP; Low-Latency
vs Gaming is still told apart by GOP alone.

**What is fixed:** VA encoders (`vah264enc`/`vah265enc`/`vaav1enc`) always get `rate-control=vbr
target-percentage=95 cpb-size=bitrate*3/8` (`VBR_TARGET_PERCENTAGE`, `VBR_CPB_SIZE_*` in `capture.rs`; `vbr`, not the
literal `qvbr`, because of the Mesa radeonsi ceiling bug documented above); x264enc keeps its plain `bitrate=`
(`pass` unset). The only backends are Software and Vaapi: there is no Windows/MF encoder branch in this crate.

**Old settings files:** `settings.toml` written while the choice existed has `rate_control = "cqp"` or `"qvbr"` in
`[start_stream_defaults]`. `StartStreamDefaults` has no `deny_unknown_fields`, so serde ignores the key and the file
loads as before (everything behaves as Dynamic). Test: `rooms::tests::start_stream_defaults_ignores_removed_rate_control_key`.
Nothing else persisted the value (destination params are runtime-only).

The tests that used `rate_control` as one of the toggled tuning controls now use `gop_millis` (also a `BranchSwap`):
`tuning_knob_changes_are_tier_two`, `late_watch_after_all_three_tuning_controls_toggled_...`.
`rate_control_mode_maps_per_backend` became `rate_control_is_always_dynamic_per_backend` (+ `..._on_h265_and_av1`).
