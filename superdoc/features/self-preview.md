# Self-Preview

What the sender-side "self-preview" is, the pipeline branch it's fed from, and why it's
a separate branch from the existing JPEG-thumbnail preview. Lives in
`simple-screensharing-client`.

## What it does

While a user is sharing their screen, the share panel shows a small, live-updating
preview of that same user's own captured video — a "this is what viewers actually see"
confirmation, rendered next to the "Active shares" row for that video share
(`ui::App::ui_stream_control_tab`'s `share.active_header` card — it lived on the old
one-time "connected screen" until the 2026-07-26 app-shell pass moved the sharing
controls onto the Stream Control nav tab; see `superdoc/ui/APP-SHELL.md`). This is
**sender-side only**: it is never a thumbnail of another room member's stream (that's the
unrelated stream-picker preview thumbnail on the View tab, fed by the existing 30s
JPEG-thumbnail mechanism — see below).

It appears once a video share reaches `ShareStatus::Active` (a `CaptureHandle` — and
therefore a frame source — only exists from that point on) and disappears when the
share stops.

## The tee branch

`capture.rs`'s capture pipeline already forks its raw video (post-`videoconvert`, off a
`tee`) into two branches: the encoder branch, and a low-rate RGB8 branch feeding the
existing 30s JPEG-thumbnail sender (`preview.rs`). Self-preview adds a **third** branch,
`appsink name=self_preview_sink` (`queue leaky=downstream max-size-buffers=1 ! videorate
! videoscale ! video/x-raw,format=RGB,width=...,height=... ! videoconvert ! appsink ...
max-buffers=1 drop=true`), capped to its own framerate, `capture::SELF_PREVIEW_BRANCH_
FRAMERATE` (12fps), **and** its own resolution, `capture::SELF_PREVIEW_MAX_WIDTH`/
`SELF_PREVIEW_MAX_HEIGHT` (320×180 — added 2026-07-28; see `stream-control.md`'s
"A follow-up freeze after the burst fix above" entry for the real-hardware hang this
closes: an uncapped full-source-resolution self-preview branch, converted with a
synchronous per-pixel Rust loop on the same task/runtime every destination's branch-swap
machinery shares, was starving that runtime badly enough to look like a permanent freeze).
Before that fix this branch was structured identically to the JPEG-thumbnail branch, minus
the resolution cap — that similarity no longer holds for the resolution half.

See `capture.rs`'s module doc comment for the full pipeline diagram with all three
branches.

### Why a third branch instead of reusing `preview_sink`

`preview_sink` (the existing JPEG-thumbnail branch) is capped to ~1fps purely because
`preview::spawn_preview_task` only ever samples the latest frame every 30s — there was
never a reason for that branch to run faster. Making self-preview a *consumer* of that
same branch would mean either:

- Raising `preview_sink`'s own framerate to something smooth enough for a live view,
  which pays `videoconvert`/appsink cost at that higher rate for the JPEG-thumbnail path
  too, even though it still only needs one frame every 30s; or
- Coupling the two features' cadences together, so a future change to one (e.g. a faster
  JPEG-thumbnail interval, or a different self-preview rate) risks silently affecting the
  other.

A dedicated third tee branch keeps the JPEG-thumbnail path's 1fps and the live
self-preview's 12fps fully independent of each other, at the cost of one extra
`queue`/`videorate`/`videoconvert`/`appsink` chain off the tee — cheap next to the
encoder branch, and consistent with this pipeline's existing pattern of one dedicated
appsink per distinct consumer (the encoder branch and the JPEG-thumbnail branch already
don't share a sink either).

## Framerate: 12fps, resolution: 320×180

`capture::SELF_PREVIEW_BRANCH_FRAMERATE = 12`. A self-preview thumbnail exists purely to
reassure the sharer about content/framing, not to be a faithful full-rate mirror of what
viewers see — so it doesn't need to track the encoder branch's configured framerate (up
to `capture::MAX_FRAMERATE`'s 240). 12fps is comfortably in the "reads as live, not a
slideshow" range for a small preview panel while staying far cheaper than the encoder
branch's `videoconvert`/encode cost.

`capture::SELF_PREVIEW_MAX_WIDTH`/`SELF_PREVIEW_MAX_HEIGHT = 320×180`. Both the framerate
cap and the resolution cap matter for the same reason — cost — but the resolution cap is
the one that's actually load-bearing for correctness, not just efficiency: see
`stream-control.md`'s 2026-07-28 "follow-up freeze" entry for the real hang an *uncapped*
resolution caused (a synchronous per-pixel conversion of a full ~1080p frame, up to 12
times a second, on the same task/runtime every destination's branch-swap machinery
shares). Keep this cap in place even if the framerate ever changes.

## Threading / bridging

Reuses the exact same bridging pattern `capture.rs` already uses for the encoder
branch's `appsink`: `gst_app::AppSink::stream()` turns appsink callbacks into a
`futures::Stream`, awaited inside `pump_loop`'s `tokio::select!` as a third arm
alongside the encoded-output and bus streams (bundled into one parameter,
`capture::SelfPreviewBridge`, to avoid growing `pump_loop`'s already-large argument
list by two instead of one). This keeps the self-preview branch on the same task/
lifecycle as the rest of `pump_loop` — no extra spawned task, no separate stop
signal — `CaptureHandle::stop`'s existing EOS-then-join sequence tears it down for free.

Each sample is converted from RGB8 to an `egui::ColorImage` (`capture::sample_to_color_image`,
stride-corrected via `gst_video::VideoFrameRef` — the same pattern
`capture::sample_to_raw_frame` already uses for the JPEG-thumbnail branch, and the same
`egui::ColorImage::from_rgba_unmultiplied` constructor `decode.rs`'s `pull_color_image`
uses on the viewer side, with an RGB→RGBA widen step since this branch negotiates RGB8,
not RGBA) and written into a `Mutex<Option<ColorImage>>` slot on `CaptureHandle`.
`CaptureHandle::self_preview_frame()` is a non-blocking take-and-clear poll of that slot,
mirroring `decode::StreamDecoder::latest_frame`'s shape on the viewer side exactly.

## UI

`ui::App::update_self_preview_textures` polls `self_preview_frame()` for every `Active`
video capture once per repaint (called from `App::update` alongside
`RoomSession::update_watch_textures`) and uploads into a lazily-created
`egui::TextureHandle` held **on the `LocalCapture` itself**
(`LocalCapture::self_preview_texture`), following the exact create-once/`set`-in-place
pattern `RoomSession::preview_textures` already uses for the "Streams" list thumbnail.

`Why:` (one texture on the capture, not a `stream_id`-keyed map) a self-preview shows
what *this machine is capturing*, which is a property of the capture and identical for
every room it is sent to. This was a `Connected::self_preview_textures:
HashMap<StreamId, _>` while the client could only ever hold one connection; a protocol
`StreamId` is unique only *within* one connection, so that keying collides the moment
two rooms exist. Moving the texture onto the `LocalCapture` removes the collision rather
than re-keying around it, and it also removes the old map-cleanup pass — the texture now
dies exactly when its capture does. See `superdoc/architecture/overview.md`'s "Client
session model".

The share panel's "Active shares" card
(`share.active_header`) then paints that texture (scaled to a fixed preview width,
`theme::FIELD`-colored placeholder box until the first frame arrives) under a
`theme::section_label` reading "Your preview".

No extra continuous repaint request was added for this: 12fps is well within what
`update()`'s existing idle-fallback repaint interval (50ms / ~20Hz while connected)
already keeps up with, unlike the up-to-240fps watched-stream case that needs its own
`ctx.request_repaint()`.

## Related docs

- `superdoc/ui/STYLING-GUIDE.md` — the preview widget (placeholder box, section label)
  follows this guide's card/token conventions.
- `superdoc/architecture/overview.md` — module map entries for `capture.rs`/`ui.rs`.
