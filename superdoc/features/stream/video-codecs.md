# Video codec selection

H.264, H.265 and AV1: the `VideoCodec` threading, which codecs a machine offers (#95 probe), output caps and codec-string derivation.

Part of the Stream Control reference, hub: `superdoc/features/stream-control.md`. The text moved here verbatim from that page: "above", "below" and "this page" in it may point at a section that now lives in a sibling page of this folder (the hub lists them).

## Video codec selection: H.264, H.265, AV1 (2026-07-30, backend/wire only — Stage 1 of 2)

**Scope of this entry:** this is the *backend + wire protocol* half of adding a codec
selector to the stream control UI (approved after a codec-comparison discussion this
session settled on offering all three codecs, not picking one). A separate Stage 2 built
the UI selector on top of the contract documented here (now the codec row in
`client/src/ui/stream_settings.rs`).

### The `VideoCodec` enum and its threading

`capture::VideoCodec` (`H264`/`H265`/`Av1`, `#[default] = H264`) is threaded through the
*exact same* per-destination settings pipeline `TuneMode`/`QualityLevel`
(and, until #88, the removed `RateControlMode`) use: `VideoParams` → `EncodeSettings` → `DestEdit` → `rooms::AppSettings::
StartStreamDefaults::codec` (snake_case serde strings `"h264"`/`"h265"`/`"av1"`; the old Tauri `dto.rs` parse functions are gone; persisted, `#[serde(default)]` so every existing user's
on-disk `settings.toml` — which predates this field — still loads cleanly, defaulting to
`H264`, same reasoning as why the rate-control strings didn't change after the VBR
fix: never break an existing config).

A codec change is unconditionally a tier-2 `BranchSwap` (`capture::destination::
classify_change`) on both backends — there is no live property that could ever make
swapping the encoder *element itself* (`vah264enc` → `vah265enc`, etc.) a tier-1 write.

Settings entry points today (this section was written when they were Tauri commands; they are `AppState`
methods now):

- `AppState::set_start_stream_defaults(StartStreamDefaults)` (`session.rs`) — the process-wide values for the
  *next* video capture started, `codec` among them (same "applied before `go_live`" shape as the other
  defaults, see "Start-stream defaults").
- `AppState::edit_destination(&mut self, destination_id: DestinationId, edit: DestEdit)` — a live destination's
  edit; `DestEdit` carries the codec next to tune / quality level / `gop_millis`.
- `AppState::supported_codecs() -> &'static [capture::VideoCodec]` — see below.

### Software-fallback gating: `capture::supported_codecs`

Only `x264enc` (H.264) is confirmed installed in software-encoder mode on this machine — no
`x265enc`/`aomenc`/`svtav1enc`/etc. is assumed present, so `EncoderBackend::Software`
**silently always produces H.264** regardless of what `VideoCodec` was requested
(`EncoderBackend::actual_codec`, the single source of truth for "what will this backend
*actually* emit" — used both by `branch::add_branch`'s output-caps selection and by the
software-backend real-hardware test `software_backend_always_produces_h264_regardless_of_
requested_codec`). This mirrors how `EncoderBackend::Vaapi` already silently ignores
`TuneMode` — a no-op, not an error.

The UI calls `AppState::supported_codecs` (wraps `capture::
supported_codecs(capture::detect_encoder_backend())`, returning `[H264]` on
`Software` and H.264 plus whichever of H.265 / AV1 the VA stack has an encoder element for on `Vaapi`, see the Why below) to hide H.265/AV1 in the
selector on a machine that would silently downgrade them, rather than let a user pick an
option that quietly does something else. `capture::StagedCaptureRow` also carries this same
list alongside the staged card's other encoder
settings, so the "Share Screen" staged-card flow doesn't need a second round trip.

`Why a probe of the encoder elements (#95, 2026-11-01; this section first shipped a static list, "all three on VAAPI"):`
`capture::supported_codecs(Vaapi)` checks `gst::ElementFactory::find` for `vah265enc` / `vaav1enc` once
(`OnceLock`), H.264 being what `detect_encoder_backend`'s `vah264enc` probe already proved. GStreamer's VA plugin
registers an encoder element only for a codec the GPU's driver advertises an encode entrypoint for, so the
factory's presence is the capability check (an older iGPU or a dGPU without AV1 encode has no `vaav1enc`). The static
list offered such a machine an AV1 button that silently downgraded the branch to x264 H.264
(`branch::add_branch_with_fallback`): a control that does nothing, which the UI now hides (user rule 2026-11-01).
The per-branch software fallback remains as the safety net.

### `vah265enc`/`vaav1enc`: confirmed via `gst-inspect-1.0` on the test machine (an AMD
GPU, GStreamer 1.28.5)

Both elements' `rate-control` (`cbr`/`vbr`/`cqp`/`qvbr`), `bitrate`, `key-int-max`, and
`target-usage` properties have the **exact same names, ranges, and enum values** as
`vah264enc`'s — re-verified directly, not assumed — so `EncoderBackend::launch_fragment`
only needs to swap the element name per codec, not remap any property. Their `mbbrc`
property (mentioned as a possible per-codec wrinkle before this stage started) defaults to
`"auto"`/`"disabled"` on both and was left untouched — nothing in this pipeline's existing
`RateControlMode` (removed in #88)/`QualityLevel` mapping needs it.

Output caps differ by codec, not just by element (`branch::add_branch`'s caps immediately
downstream of the encoder):

| Codec | Encoder element | Output caps |
| --- | --- | --- |
| H.264 | `vah264enc` / `x264enc` | `video/x-h264,stream-format=byte-stream,alignment=au` |
| H.265 | `vah265enc` | `video/x-h265,stream-format=byte-stream,alignment=au` |
| AV1 | `vaav1enc` | `video/x-av1,stream-format=obu-stream,alignment=tu` (no Annex-B start codes at all — a genuinely different framing, not just a different media type) |

### The `CodecInfo` wire chunk: generalized to all three codecs, still purely bitstream-derived

The former sidecar's `video_server.rs` `CodecInfo` chunk (the in-band WebCodecs codec string
every watch connection gets first; removed in the egui rework, the native equivalent is
`decode/codec.rs` sniffing and the browser's is `docs/js/core/codec.js`) derived
`avc1.PPCCLL` / `hev1.<profile>.<compat>.<tier><level>[.<constraints>]` /
`av01.<profile>.<level><tier>.<bitdepth>` strings, one parser per codec
(`parse_avc_codec_string`/`parse_hevc_codec_string`/`parse_av1_codec_string`), dispatched
by `derive_codec_string`.

**The dispatch mechanism — and the one genuine ambiguity in this stage's design:**
`protocol.rs`'s `AnnounceStream`/`StreamInfo` carry no codec field (confirmed at this
stage's start, deliberately left that way — see "Explicitly NOT in scope" below), so
`video_server.rs` has no wire-level hint telling it which codec a given watched stream is
even using. Two different answers were possible here:

1. **Derive the codec purely from the bitstream's own structure** (what was built).
   `capture.rs` deterministically builds H.264/H.265 branches with `stream-format=
   byte-stream` (Annex-B, every access unit starts with a `00 00 01`/`00 00 00 01` start
   code) and the AV1 branch with `stream-format=obu-stream` (no start codes, ever) — so
   "does this access unit begin with an Annex-B start code" is a **provably reliable**
   AV1-vs-{H.264,H.265} discriminator, not a guess. Disambiguating H.264 from H.265 within
   the Annex-B branch is *not* as clean: there is no structural guarantee, only a
   **heuristic** (`looks_like_hevc_param_set_nal`: re-interpret each NAL's header as a
   H.265 SPS and check whether the resulting `nal_unit_type` lands in the real SPS value —
   hand-verified against this machine's actual `vah264enc`/`vah265enc` output not to
   collide either way, but not proven safe for every possible encoder's output in general).
2. **Add a codec field to `protocol.rs`'s wire types** — would remove the ambiguity
   entirely, but is a real cross-cutting change to the network protocol shared with the
   (not-yet-built) relay server and browser-viewer sibling repos, explicitly out of scope
   for this stage's brief.

Option 1 was chosen (matches this module's existing design philosophy of deriving
everything from the bitstream rather than trusting a caller-declared value that could go
stale — see the module's own "Codec-string discovery" doc section), but the H.264/H.265
heuristic is a real, flagged judgment call, not a closed question — documented in full,
including the exact byte-level reasoning, in `video_server.rs`'s own "H.265/AV1
codec-string derivation" section doc comment.

Regression coverage: `capture::tests::vaapi_h265_pipeline_links_and_produces_encoded_
output`/`vaapi_av1_pipeline_links_and_produces_encoded_output` (real hardware, actually
launch the `vah265enc`/`vaav1enc` pipelines and assert encoded output), `capture::tests::
quality_level_maps_to_target_usage_on_h265_and_av1`/`rate_control_mode_maps_on_h265_and_
av1` (real hardware property-mapping), and `video_server.rs`'s
`parse_hevc_codec_string_reads_real_vah265enc_sps`/`parse_av1_codec_string_reads_real_
vaav1enc_sequence_header`/`derive_codec_string_dispatches_by_bitstream_shape` — the latter
three against byte-for-byte real `vah265enc`/`vaav1enc` capture output (not synthesized),
cross-checked against an independent decoder (`ffprobe -v trace`) for the AV1 case where
hand-derived bit-tracing first produced a wrong expected value.

### Explicitly NOT in scope for this stage

No UI changes (that was Stage 2). No change to
`protocol.rs`'s `AnnounceStream`/`StreamInfo` (see the wire-chunk section above for why,
and the ambiguity that choice leaves open). No live per-codec VAAPI availability probing
beyond the existing `vah264enc`-only one (added later, #95: see the Why above).

---
