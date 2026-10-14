# Viewer: watches, pop-out windows

> **Status (2026-10-14).** The viewer is the native egui client. "Pop-out windows" and "A watch's
> lifecycle" below are current; the short **History** section at the end condenses the removed
> Electron pop-out window (kept for the *why* behind the rules the egui port copied). Display
> internals: `superdoc/features/video-pipeline.md` "Native display".

## Pop-out windows (`client/src/ui/popout.rs`)

Each watched stream tile has a pop-out action; a pop-out is one **deferred egui viewport per
stream** (`StreamKey = (RoomHandle, ConnId, StreamId)`), re-declared every frame it should exist
(egui's viewport contract). Any number of streams can be popped out at once.

- **Shared view, no second decode.** The viewport paints the same `Arc<Mutex<VideoView>>` as the tile
  (`paint(ui, ui.max_rect(), Fit::Contain)`), so GPU textures and the decoder are shared and the tile
  shows a placeholder while popped (`VideoViews`' `popped` flag, `ui/video.rs`).
- **Hover chrome** (the former Electron `body:hover` rule, no idle timeout): while the pointer is anywhere in
  the window a vignette, a top row `[statbar][x]` and a bottom audio pill (`[mute][slider 0-200 %][%]`,
  or "No audio shared") appear. Statbar = `[dot][name . room][WxH][decoded/presented fps][bitrate]`
  in fixed tabular-width fields. A "waiting"/"stalled" chip is always shown (not hover-gated). The
  chrome data is refreshed by the main window at most 4x a second into a `Shared` struct, and
  volume changes go back through `Shared::pending_volume`: `Why:` the viewport closure must be
  `Send + Sync + 'static` and cannot hold `Cx`, and screens never mutate `AppState` themselves. When
  not hovered the closure only locks the view and paints (no allocation, no `request_repaint`).
- **F11** toggles fullscreen; **Esc** only *leaves* fullscreen and never closes (the former Electron rule,
  kept on purpose: a stray Esc must not kill someone's stream window).
- **Closing brings the stream back.** The x button or an OS close sets `close_requested`, which the
  next main-window frame turns into `UiAction::BringBack`: the stream returns to its tile.
- **Performance:** pop-outs hold 240/240 fps (decoded/presented) when the presentation mode is
  Mailbox. With Fifo a pop-out starves the main window; see `superdoc/features/presentation-modes.md`.
- Window title "sharer . room"; first size from the stream's aspect ratio; app id
  `simple-screensharing-popout`.

## A watch's lifecycle (viewer side, `client/src/session.rs`)

Full detail in `superdoc/architecture/implementation-notes.md` (tasks #36 and #40); the rules:

- **A watch ends when the sharer stops (#40).** On the relay's `StreamStopped`
  `RoomSession::end_watch_of_stopped` removes the watch (tile, decoder and GPU view go away
  instead of freezing on the last frame) and remembers it for `RESUME_AFTER_STOP` = **30 s**. If the
  same sharer announces a stream of the same kind and label in that window, it takes the dead tile's
  place with no click. `Why 30 s and not the 60 s reconnect window:` this was a deliberate stop, so a
  surprise subscribe minutes later would cost bandwidth unasked; but stop, pick another monitor in
  the portal, go live takes a few seconds.
- **Auto-reconnect re-matches watches (#36).** After a dropped connection the room is *suspended*
  (see `superdoc/meta/TERMINOLOGY.md`), and what the user was watching is kept as `WantedStream`s identified by
  **owner display name + kind + label** (never `ConnId`/`StreamId`, which change). Streams not yet
  re-announced are waited for `REWATCH_GRACE` = **60 s**, matching the browser client's resume list.
- **AV1 is cropped to the announced size (#40).** VCN4 (RX 7900 XTX) encodes 1080p as 1082 lines and
  nothing in the bitstream names the padding; the decoder crops to the size from `StreamInfo`
  (`StreamDecoder::set_display_size`). Detail: `video-pipeline.md` "Visible size, not coded size".


## History: the Electron pop-out window (removed)

The pop-out used to be an Electron `BrowserWindow` per stream, fed by the Rust sidecar over a local
HTTP/SSE bridge. That whole stack (`electron/`, the HTML frontend, the Tauri sidecar) was deleted
in the egui rework; the old ~700-line log of it (hover-gate reversal, overlay-parity bugs,
window-creation consolidation, "Stream error" ghost label, grid-fit work, verification via
Playwright `_electron`) is in git: `git log -- superdoc/features/viewer-window.md`. What still explains today's behaviour:

- **Hover chrome shows when the pointer is anywhere in the window, not only over the tile.** The
  owner asked twice for *all* overlay elements (statbar, close x, audio pill) to disappear when
  the window is not hovered, including when popped out. In CSS the x button was a sibling of the
  tile grid, so the trigger became "pointer anywhere in the window"; egui copies that (pointer
  inside the viewport shows the chrome).
- **Esc leaves fullscreen but never closes**, F11 toggles: a stray Esc must not kill a stream window.
- **Closing a pop-out returns the stream to its tile.**
- **Three window mechanisms became one** (2026-07-28): a Tauri webview window, a borderless
  display-takeover window and the Electron pop-out were collapsed into a normal, normally
  positioned window, because a window claiming exact display bounds fights tiling WMs. Native
  Wayland has no absolute-positioning protocol, so a pop-out's default position is the
  compositor's choice; no display picker was built for that reason.
- **The pop-out must honour the jitter-buffer setting.** Its own JS context once fell back to a
  hard-coded 2000 ms instead of the user's `max_jitter_buffer_ms`; the egui pop-out shares the
  tile's `VideoView`, so this class of bug cannot recur.
- **Watch grid fits tiles to the window shape**, not `ceil(sqrt(n))`: for every column count the
  per-tile 16:9 size is computed and the shape with the largest tile area wins (tall windows
  stack, wide ones go side by side). *Why:* the owner asked for "stack them, if that maximises
  the used viewing area". It lives on as `grid_shape` in `client/src/ui/room.rs`, ported from the
  former Electron frontend's `computeGridShape`.
- **Verification lesson:** the Electron-era checks drove the real shell with synthetic mouse
  events and fabricated stream keys; the egui equivalent is the `run` skill plus the headless rig (`hs-rig.sh`, `hs-drive.sh`)
  (`client/.claude/skills/run/SKILL.md`, `client/scripts/`).
