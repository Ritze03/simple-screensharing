# Module map (`client/src/ui/`)

Which file of the egui UI owns what. Split out of `APP-SHELL.md` on 2026-11-07 (table verbatim); the screen contract (data in, actions out) stays in `APP-SHELL.md`.

| File | Owns |
| --- | --- |
| `mod.rs` | `App` (eframe app: polls the session, lays out the panels, runs modals/toasts, applies actions); `View` (`Welcome` / `Watching` / `Room(RoomKey)`); `Modal`, `PopoverKind`/`Popover`; `Nav { view, modal, popover }`; `Shell` (staged shares, toasts, create/join results, level taps); `UiAction` (every cross-screen action); `Cx`; selectors (`room_entries`, `ShareState`, `same_room`, ...); the shared `tr` / `tr_with` i18n fallback helpers |
| `apply.rs` | `App::apply`: the **only** place a `UiAction` changes state. Thin calls into the lib (`session.rs`) plus nav/shell bookkeeping |
| `video.rs` | `VideoViews`: one GPU `VideoView` per watched stream, keyed by `StreamKey = (RoomHandle, ConnId, StreamId)`, synced once per frame; `popped` flag + `close_requested` for pop-outs |
| `stage/` | the stage (`mod.rs`: `StageState`, `StageSettings`, `StageView`/`StageLayout`, `Host`, `ItemId`, items, input, painting, `show_main` / `show_windows`; `geom.rs`: pure grid / bar / floating / webcam-in-picture maths; `pip.rs`: one webcam in picture, drawn and dragged; `tile.rs`: video, idle, audio-only and own tiles, the name tag with stats, the hover buttons, the viewers dropdown, the stream menu's volume row; `bar.rs`: the control bar and its pure `arrange`; `strip.rs`: people strip and hover card; `menus.rs`: bar dropdowns and the stream menu; `chrome.rs`: the fullscreen auto-hide rule) |
| `webcam_settings.rs` | the webcam modal (task I8): Source (camera, mode), Compression (codec, bitrate), "Encoder: Performance · Keyframe 2000 ms", footer `[Start / Stop webcam][Done]` |
| `audio_settings.rs` | the audio settings menu (task I7): Source, Volume (Boost), Desktop audio (exclusion list and its Add popover), footer `[Stop sharing audio / Share desktop audio][Done]` |
| `drive_dump.rs` | the `SSS_UI_DUMP` hook of the rig's scripts (control rects and values; names in `client/scripts/README.md`) |
| `menu.rs` | the app's menu style beyond `pop_item`: `menu::select` (a dropdown field), `switch_row`, the popover frame |
| `share.rs` | the bar's Share screen and Share audio dropdown bodies, level-meter taps, "Also send to / Move to" lists (`RoomTargets`), pills, formatting, popover rows shared with the stream-settings modal (this was `action_bar.rs`) |
| `sidebar.rs` | brand band, body (Watched streams well, ROOMS heading, rooms well with sharer sub-rows, Create-or-join), user band (avatar, "Live in N rooms" / "Not sharing" -> Outgoing popover, settings gear) |
| `room.rs` | `show_header` (title, pill, meta; `⋯` for rooms with no stage), `show_stage` (the stage region), `show_body` (welcome, disconnected, connecting, reconnecting: banner, empty states) |
| `stream_settings.rs` | the stream-settings modal (staged "Set up your stream", live "Stream settings"), width 1120. Video card: Preset, FPS, **Encoding** (Performance / Quality segments) and **Keyframe** (select 250 / 500 / 1000 / 2000 ms), caption; see "Video card: Encoding and Keyframe" in `SHARE-FLOW.md` |
| `modals.rs` | create/join, app settings, leave / forget confirm, rename, choose-your-name |
| `popout.rs` | pop-out viewer windows: one deferred viewport per popped stream |
| `tests.rs` | session-model lookups, bandwidth wiring, shell geometry (`shell_regions_match_the_spec_sizes`), nav/action behaviour |
