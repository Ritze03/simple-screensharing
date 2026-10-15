# Own-tile live preview (the sharer sees their own share)

> **Status (2026-10-15, task #78): live again, in a different shape.** The old always-on 320x180
> self-preview branch was removed (see "History"); since #78 the sharer's own tile in the room's
> "Sharing in this room" grid shows a live, downscaled picture of what is being captured, **only while
> the app window is focused** (native) or the page is visible and focused (browser). Otherwise the tile
> is the hatched placeholder and the preview costs nothing.
>
> *Why (user request 2026-10-15):* "For the sharer, it should show a stream preview, while he is tabbed
> into the program. Otherwise, its fine to hide it."

## Native (egui)

- **Where:** `capture.rs` `live_branch` (appended to both capture descriptions) and `LivePreview`;
  `CaptureHandle::set_live_preview(bool)`, `live_preview_frame()` (newest RGBA frame, never blocks),
  `live_preview_frames()` (buffers that reached the sink, for verification). UI: `ui/mod.rs`
  `App::poll_live_previews` + `Shell::live_previews`, own tile `ui/room.rs` `my_card`.
- **Branch:** `tee ! valve(drop, forward-sticky-events) + RateCap probe (30 fps) ! queue(leaky, 1)
  ! vapostproc | videoscale ! videoconvert ! capsfilter(RGBA, fitted size, 30/1) ! appsink`. Size: the source
  fitted into 640x360, aspect kept, never upscaled, even sides (`fit_live_size`), written into the capsfilter
  when the branch opens. The branch is the same in both [`CaptureDomain`]s except for the scaler: in the GPU
  domain `vapostproc` scales on the GPU and downloads only the small RGBA picture (~28 MB/s at 640x360x30,
  nothing like the 240 fps full-size stream); in the System domain it is `videoscale ! videoconvert`. One
  branch per capture, shown in every room's own tile of that capture; the encode branches are not touched.
- **Focus gating, so unfocused costs nothing:** the own tile pushes `UiAction::LivePreviewSeen(capture)` on
  every pass it is painted with the window focused and not minimized (`viewport().focused` /
  `.minimized`). `poll_live_previews` opens the valve while that was within 0.6 s (`LIVE_PREVIEW_GRACE`),
  closes it otherwise (unfocused, minimized, other room or view selected, tile scrolled out of the pass),
  drops the texture, and while it runs repaints every 33 ms. A closed valve sits directly on the tee: a
  frame costs one pad call and an unref; no scale, no download, no upload, no egui repaint. Refocus: the
  focus event repaints, the tile is painted, the valve opens one frame later; the first picture arrives
  about a frame period after that.
- **Why a valve and not add/remove of the branch:** no pad surgery on the live tee (where upstream
  `RECONFIGURE`s once killed real portal capture, `isolate_source_from_branch_renegotiation`), and reopening
  is instant.
- **Why the appsink is `async=false` (found the hard way, 2026-10-15):** an appsink wants a preroll
  buffer before the pipeline may finish PAUSED -> PLAYING, and a closed valve never gives it one. The whole
  capture then stalled (5 s to "capture started", every encode branch "produced no IDR"). Reproduced without
  any portal: `videotestsrc ! vapostproc ! tee` with this branch behind a closed valve gave 0 fps on the
  other branch, 240 with `async=false`. The unit test `live_preview_branch_is_gated_by_its_valve` also
  checks that the room-preview branch keeps flowing while the live one is closed.
- **Texture:** one per capture, in `Shell::live_previews` (UI state, not `LocalCapture`: that struct is
  built in a dozen test sites), `TextureOptions::LINEAR`, drawn cover-fit like other tiles' thumbnails
  (`paint_cover`), a `PREVIEW` chip in the top-right corner and the usual `LIVE`/`STARTING` chip left.
  Audio-only shares have no video handle and no preview.
- **Not the room-preview thumbnails:** those are the ~30 s JPEGs other members see (`preview.rs`); a
  separate fixed 1 fps branch feeds them and is unchanged (verified at 1.0 fps behind its `videorate` while the source ran 239 fps, #79: `stream-control.md`).

Measured numbers (headless rig, H.265 240 fps): `superdoc/features/stream-control.md`
"Own-tile live preview".

## Browser

`docs/js/ui/capture.js` `previewFor(capture)`: the sharer already holds the `MediaStream`, so the own tile
holds a muted `<video class="live-preview">` playing `new MediaStream([videoTrack])` (video only, never
audio). Attached while `document.visibilityState === "visible" && document.hasFocus()`; on
`visibilitychange` / `focus` / `blur` it is paused and `srcObject = null`, the element `hidden`, so the
placeholder shows and nothing renders (the track itself keeps feeding the encoder). *Why a persistent
element per capture:* `render()` rebuilds the tree on every state change; `myCard()` re-inserts the same
element, so playback never restarts (same idea as the fullscreen stage in `popout.js`). CSS (`app.css`
`.thumb.mine`) uses `:has()` to swap the "Your screen" label for the `PREVIEW` chip. Audio-only captures
(`c.video == null`) get no element. E2E: `docs/dev/e2e/live-preview.mjs` (see `features/website.md`).

## History

- **Until 2026-10-14:** a third `tee` branch (12 fps, 320x180, RGB) fed `CaptureHandle::self_preview_frame()`
  and the share card showed "Your preview". The UI half was dropped in the egui rebuild (and before that the
  Electron build removed the same preview on purpose, commit `a4be944` in the old client repo), and the branch
  was later deleted too (it leaked its size upstream through the tee's CAPS query and starved the runtime
  with a per-pixel loop on `capture_loop`'s task; the `tee` CAPS-query guard it motivated stays). The reasoning
  that still holds for #78: one appsink per distinct consumer; a size cap on any branch that reaches a
  CPU-side loop; a preview shows what *this machine captures*, identical for every room, so it is per
  capture, not per `StreamId`.

## Related docs

- `superdoc/features/stream-control.md`: capture pipeline, capture domains, "Own-tile live preview".
- `superdoc/ui/APP-SHELL.md`: the own card.
- `superdoc/features/website.md`: browser side.
