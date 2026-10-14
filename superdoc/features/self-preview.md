# Self-Preview (removed from the UI; library part still present)

> **Status (2026-10-14): history, plus an unused library remnant.** The sender-side "Your
> preview" thumbnail is **not shown by the app any more**. The UI half was dropped when the
> client was rebuilt as one egui binary (I11); this matches the Electron build, which removed the
> same preview on purpose (commit `a4be944` in the old client repo, per the project owner's
> brief; that commit is not in this repo, so it could not be re-read). What remains is the
> capture-side plumbing below, which nothing consumes.

## What it was

While sharing, the share card showed a small live preview of the sharer's own captured video
("this is what viewers see"), sender-side only, fed by a third `tee` branch, at 12 fps and a
320x180 cap. It was unrelated to the 30 s JPEG room-preview thumbnails other members see
(`client/src/preview.rs`), which are still live.

## What still exists in the code (verified 2026-10-14)

| Piece | Where | State |
| --- | --- | --- |
| `self_preview_sink` tee branch | `client/src/capture.rs`: `SELF_PREVIEW_BRANCH_FRAMERATE = 12`, `SELF_PREVIEW_MAX_WIDTH/HEIGHT = 320x180`, `SELF_PREVIEW_SINK_NAME`; pipeline text in `build_pipeline` (both capture domains) | **Still built and running** for every capture |
| `SelfPreviewBridge`, `sample_to_color_image` | `capture.rs` | Still converts each sample to an `egui::ColorImage` in `pump_loop` |
| `CaptureHandle::self_preview_frame()` | `capture.rs` | Public take-and-clear poll; **no callers** |
| `LocalCapture::self_preview_texture` | `client/src/session.rs` | Field kept (always `None`, set to `None` in constructors and tests); **nothing writes it** |
| `update_self_preview_textures` | formerly `ui::App` | **Gone** (not ported to `ui/`) |
| locale key `share.self_preview_label` | `client/locales/{en,de}.yml` | Orphaned string, unused |

So today a 12 fps RGB branch is decoded to `ColorImage` and thrown away. Decide before relying
on it: either delete the branch, the bridge, `self_preview_frame`, the `LocalCapture` field and the
locale keys (cheapest; also removes a `vapostproc`/`videoconvert` chain per capture), or re-add a
UI that consumes `self_preview_frame()`. This page's owner did not remove the code (docs-only task);
it is flagged in the report for the lead.

## Why the branch looks the way it does (still true if you ever revive it)

- **A dedicated third branch rather than reusing `preview_sink`.** `preview_sink` runs at ~1 fps
  because the JPEG thumbnail sampler only reads it every 30 s. Raising it to a live-view rate would
  pay the conversion cost for the thumbnail path too, and coupling the cadences means a change to one
  silently affects the other. One appsink per distinct consumer is this pipeline's pattern.
- **The 320x180 cap is load-bearing, not an optimisation.** Uncapped, the branch carried the full
  source resolution to `sample_to_color_image`, a synchronous per-pixel Rust loop that ran on
  `capture_loop`'s own task, the same runtime every destination's branch-swap machinery shares. At
  12 fps that starved the runtime badly enough to look like a permanent freeze (a live `gdb`
  backtrace caught it inside the loop; commit `c0c04ed`; detail in `stream-control.md`, "A
  follow-up freeze after the burst fix above"). Keep the cap whatever the framerate.
- **GPU capture domain.** There the scale is a `vapostproc` that also downloads to BGRx; see
  `stream-control.md` "Capture/encode performance". `videorate` is `drop-only` on this branch since
  2026-10-14.
- **Texture ownership (if re-added):** one texture on the `LocalCapture`, not a `stream_id`-keyed map,
  because a preview shows what *this machine is capturing* (identical for every room), and a protocol
  `StreamId` is unique only within one connection.

## Related docs

- `superdoc/features/stream-control.md`: capture pipeline, the freeze fix, capture domains.
- `superdoc/architecture/overview.md`: module map entries for `capture.rs` / `session.rs`.
