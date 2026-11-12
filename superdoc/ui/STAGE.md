# Stage (native egui client)

The room view of the desktop client: the black stage with its tiles, layouts, people strip, control bar, menus, webcam in picture, pop-outs, fullscreen and the Layout settings that are saved. Split out of `APP-SHELL.md` on 2026-11-07 (text moved verbatim); the shell around it (sidebar, header, modals, rules) is `APP-SHELL.md`, the module list is `MODULE-MAP.md`.

## The stage

- **The stage** (task I1, 2026-11-04; the replica's Stage, `ui/replica/index.html`, user-approved in the design
  sandbox, see `REPLICA.md`). The centre of a **joined room** and the whole **Watched streams**
  view are one black stage (`STAGE_BG`) that replaces the old "On screen" tiles and "Sharing in this room" cards;
  `room::show_stage` hands it the whole central region (no scroll area; every other state keeps the scrolling body,
  `room::shows_stage`). Code: `client/src/ui/stage/` (`mod.rs` state, items, input, painting; `geom.rs` the pure
  maths with tests; `tile.rs` what a tile shows). Layouts, per view (each room and Watched keep their own):
  **Split** every picture whole in a grid, **Crop** the same cells filled (`Fit::Cover`: the wgpu quad is bigger
  than the cell and the painter's clip crops it), **Floating** one window per stream. The grid is the row count
  that shows the most picture area (`geom::best_rows`), rows filled evenly from the top. *Why one grid for Split and
  Crop, and no "even grid" setting:* the user's design call; resizing is by the bars, so an even grid stays one
  double-click away on a bar.
  - **What is on it:** other people's video streams (watched = video; not watched = a dimmed preview, "X is
    sharing Y" and **Watch stream** / **Pop out**), audio-only senders (**Listen**), then your own live share.
    Watched shows what you watch, nothing else. A popped-out stream is absent (the pop-out window is its place; closing it
    brings it back, so does its owner's avatar in the strip, below). Everything that used to be a card button is on its
    tile: Watch / Pop out / Listen on the idle tile, Stream settings and the viewers chip (below) on the own tile; the rest of
    a stream's controls are its `⋯` menu.
  - **A person is a connection** (2026-11-04, #127): inside a live session people and whose-is-this go by room + `ConnId`
    (`Item::conn`, `strip::Person { room, conn }`), never by display name: strip, hover card and its keep zone, avatars,
    emoji (`Cx::emoji_of(room, conn)`), stream counts, the viewers dropdown, the stream menu head, the popped badge, webcam
    in picture. "You" is your own connection, so a second client of yours under your name is someone else and stays on
    your stage with Hide my own streams. In Watched (2026-11-04, user: "the list of participants should be filtered/condensed, so every person is only shown once")
    the strip merges: connections of one name in DIFFERENT rooms are one person (`strip.rs` `owners`: one avatar, the count of
    all their streams, one hover card with a row per stream that names its room, ring and LIVE if any of theirs is; each
    row's actions use the item's own room + conn; a click brings every popped stream back; the emoji is the first
    connection's, ids key on `Person::key`, the first connection); yours would be one "You". Two connections of one name in
    the SAME room stay two. *Why:* the name is the only identity across rooms, but inside one room two connections of a name
    are provably two (the #127 report), so a person never takes a second connection of a room it already has. A single
    room's strip is unchanged.
    *Why:* the user, from two clients of their own in one room under one name: "When i hover "You", it doesnt show
    anything and hovering the name shows both at once. (Both are technically me, but that should be properly handled!)".
    By name the two shared one egui id (one hover, one card), claimed each other's streams, and the first member of that
    name lent its emoji to the other (the roster's order is the relay's, so that could be the viewer's own emoji: #128's
    "a changed emoji doesn't reach others").
  - **Keys:** a tile is identified by `stage::ItemId` = room handle + owner name + label (+ audio flag, n-th of
    identical ones), your own share is `room/me`; **not** by `StreamKey`, whose `ConnId`/`StreamId` change with every
    reconnect, so a reconnect keeps order, grid and window rects (`WantedStream`'s identity). `ItemId` only names a place
    in the layout. Identical ids are numbered in `(ConnId, StreamId)` order (`number_identical`), so two same-named owners
    sharing one label are two tiles and the number does not depend on announcement order. **A reconnect:** `ConnId`s are
    random and new, so those two may swap numbers and with them their places, sizes and a pop-out; nothing is lost, there is
    no panic, the two ids stay the same two. *Why accepted:* nothing that survives a reconnect tells them apart
    (`session::match_wanted` has the same limit). If one of the two stops sharing, the other takes number 0 and so the
    first one's place. Watch, pop-out, volume
    and stats belong to the stream (`StreamKey`) and are shared by every view; the layout state is per view
    (`StageState::views`, UI-private, per session, **not persisted**). It is never pruned, so a stream that comes
    back keeps its place. The layout choice and the two options sit in `Shell::stage` (`StageSettings`) because the
    Layout menu is another screen: `UiAction::SetStageLayout { view, layout }`, `SetHideOwnStreams`,
    `SetAlwaysShowNames`, `SetShowStats`; `Cx::stage_view()` / `Cx::stage_layout()` read them.
  - **Resize bars (Split, Crop):** with the pointer between streams, **including the black space around a
    picture**, every bar shows dark grey (`BORDER`); over a stream they hide; the bar under the pointer is blue
    (`ACCENT`) and stays blue for the whole drag, the others hide. A bar sits **midway between the two visible
    pictures**, never on the hidden cell edge (`geom::place`). A dragged bar stays under the pointer: dragging
    60 px moves it 60 px and it stops only at the limit (12 % of the pair). *Why a search and not "move the edge by
    the pointer's distance":* in Split a picture only grows while its cell limits it, so the bar (which follows the
    pictures) moves slower than the cell edge; `geom::drag_bar` bisects the split that puts the bar under the
    pointer. The drag is applied *before* the frame is laid out so nothing lags. Double-click a bar: even grid.
  - **Floating windows** keep their stream's aspect ratio always, also when they come from Crop or after a
    frame size change (`geom::fit_float` re-applies it every frame, width wins): no black bars. They start where
    Split showed the picture. The corner handle (bottom right) moves the corner to the nearest point of the
    window's diagonal (`geom::resize_float`), so width and height both count, growing and shrinking; moving only
    left or only up shrinks it too. Press raises a window; windows stop at the stage edge.
  - **Drag to reorder** (Split, Crop): drag a tile onto another, they swap on release (accent ring on the target).
  - **Pan in Crop (2026-11-11, row 177):** hold **Shift** and drag a watched stream's picture with the left button to
    move it inside its cell (cursor: grab, grabbing while dragging); the picture follows the pointer 1:1, like
    grabbing the image. The offset is stored per stream (`ViewGeom::pan`, keyed by `ItemId` like the PIP spots, per
    session, **not persisted**, as grid sizes and window rects are not) and **normalised**: -1..1 per axis as a
    fraction of the overflow (0 = centred), so it survives cell and window resizes (`geom::pan_shift`, `pan_drag`,
    tested). It is clamped so the picture always covers the cell, and an axis with no overflow does not move.
    "Reset sizes and positions" clears it. A plain drag still reorders; a webcam in picture under the pointer keeps
    priority (its drag starts first). Only Crop: Split and Floating show the whole picture, and fullscreen / pop-out
    are not Crop stages, so no pan there (the offset is kept for when you come back). The webcam in picture keeps
    its spot relative to the centred picture, it does not follow the pan. Painted by `VideoView::paint_panned` (the
    quad shifted, the clip stays the cell). The Crop item's menu subtitle says "Shift+drag to pan".
    *Why:* user request 2026-11-11 ("hold Shift+LeftMouseButton, to move around the image in the cropped area").
    **Own tiles too (2026-11-12, row 186):** your own webcam and screen tiles pan the same way, while their live preview
    is painted (the placeholder has nothing to pan, so no pan starts); same `ViewGeom::pan` by `ItemId`, same clamp and
    reset. The own tile paints through `tile::paint_cover_panned` (the preview texture's UV window shifted; it was
    already Cover-fit). *Why:* user request 2026-11-12 ("I should be able to do the crop move on my own camera aswell").
  - **Paint order:** egui paints a wgpu callback in the order shapes are added, so a floating window above
    another covers its video correctly; the rounded-corner masks (the video cannot be clipped to a rounded rect)
    are only used outside Floating, where windows overlap and a mask would paint over the one underneath.
  - **Streams must not jump:** there is no chrome that fades on the stage itself; the hover overlay is painted
    over the tile and takes no layout space.
  - **Tests and measurements:** `ui::stage::geom::tests::*` (rows, midway bars, 60 px drag, limits, aspect
    keeping, corner resize), `decode::render::tests::fit_cover_*`, `ui::tests::the_stage_layout_is_per_view_*`,
    `watched_streams_is_a_stage_*`. Rig (2026-11-04, 1920x1080 window, headless sway): a 60 px drag moved a
    horizontal bar 60 px (rows 519-521 -> 579-581, also with letterboxed pictures), -120 px moved it 120; a
    floating window resized by (-100, 0) went 688 -> 612 px wide (formula: 612.0) with its ratio kept, (0, -80)
    578 px, (+80, +80) 673 px; moving a window by (120, 90) moved it exactly that; relay restart: the
    re-created watch came back in its dragged window position. Mouse drags need real motion events during
    the press: `swaymsg seat - cursor set/move` sends none while a button is down (sway's implicit grab), so the
    rig's scripted drags need a virtual pointer's `motion` (press, N relative motions, release in one process;
    a scratch helper over `hs-vptr`'s protocol did it, it is not in `client/scripts/` yet).
- **Tile overlay and stream menu** (task I3, D1): a watched tile shows nothing over its picture until the pointer is on it.
  Then: the **name tag** at the top left (`[icon][name][· label]`, the icon is the audio status: `volume_up` audible,
  `volume_off` muted or none) and three round buttons at the top right, **Pop out**, **Fullscreen** and `⋯`.
  **"Always show names"** (Layout menu) keeps the tag without hovering; the buttons and the stats never stay. **Show
  stats** (Layout menu, ONE switch for every watched stream, `StageSettings::show_stats`, off until switched on, saved,
  `UiAction::SetShowStats`; dump rect `menu.show_stats`) adds `res · fps · rate` to the tag **while hovered**
  (it does not ride on "Always show names"), in slots reserved
  for `9999×9999`, `999 fps` and `99.9 Mbps` (`tile::RESERVE`), so the pill is exactly as wide as its content and never
  jitters as a number gains a digit; its tooltip keeps what the old statbar's had (decoded vs shown fps, codec and decoder,
  dropped). A narrow tile drops the resolution, then the bitrate (`tile::stats_fields`). A green ring on the picture
  means the stream's shared audio is audible. Double-click a watched tile = fullscreen (below). *Why no audio pill and
  no Stop button on the tile any more:* the replica puts volume and Stop watching in the stream
  menu (level loudness too until 2026-11-04, now a bar button), so the picture stays clean (user, D1: "a stream's buttons and stats show only while it is hovered").
  *Why Show stats moved from the stream menu to the Layout menu (2026-11-04):* the user: ""Show stats on the stream"
  should be inside of the "LAYOUT" dropdown and not per individual stream." The tag's numbers are a view choice like
  names and own streams, not a property of one stream; the per-stream set (`StageState::stats`) is gone.
- **The stream menu** (`stage/menus.rs::tile_menu`, `PopoverKind::Tile(ItemId)`): the `⋯` button or a **right click on any
  tile** opens it (anchored to the button or the pointer). Header (avatar, who, `label · size`, or `label · room` in
  Watched); for a watched stream with audio a Volume row `[mute][slider 0-500 %][pct]` (no Level loudness switch any
  more: it is global, a bar button now), else "No audio in this stream"; Pop out,
  Fullscreen, Stop watching (no stats row: it is the Layout menu's). An unwatched tile's menu is Watch stream / Pop out; an audio-only sender's Listen and,
  while listening, the volume row; your own tile's Stream settings… / Stop sharing. **Volume acts during the drag**
  (`tile_volume_slider` pushes `SetVolume` on every change, #135; D1) and **scrolling on the slider** moves it 5 % a notch
  up to 200 %, 10 % above (`ui::volume_slider`, `wheel_step`, #113), here and in the pop-out. *Why the exception to "local
  edit state is pushed on `drag_stopped`" (below):* `SetVolume` only sets the decoder's volume element and the in-memory
  value, one per changed frame is cheap, and the user wants to hear the level while choosing it; the Boost slider in
  the audio settings menu does the same (`PreviewAudioGain` while dragging, `SetAudioGain` on release).
- **Own-tile live preview** (#78, user request 2026-10-15): your own tile shows a live <= 1920x1080 / <= 30 fps picture
  of what is captured **while the window is focused** (name tag `You · label` with a green or amber `cast` icon
  and the state, `PREVIEW` chip on hover); unfocused, minimized or with the tile not painted it is the hatched
  "Your screen · LIVE/STARTING" placeholder and the capture's preview branch is closed. *Why:* "show a stream
  preview while tabbed into the program; otherwise fine to hide it". It replaces the earlier deliberate "no
  self-preview". Code: `stage::tile::mine_tile`, `App::poll_live_previews`, `UiAction::LivePreviewSeen`; design in
  `superdoc/features/self-preview.md`.
- **Viewers chip and hover dropdown on the own tile** (#116, 2026-11-03; moved from the old own card to the stage's
  own tile, shown on hover next to the Stream settings button): an eye chip with the number of people
  watching that share in that room (`[PREVIEW][viewers][tune]`, `stage::tile::viewers_dropdown`). Hovering it
  opens a dropdown (not a text tooltip): a `WATCHING NOW · N` heading, then one row per viewer, the same
  initial-letter avatar the people strip uses (`ui::paint_person`, a 22 px disc)
  and the display name, sorted case-insensitively. It stays open while the pointer is on the chip,
  the dropdown or the gap between them, so the pointer can travel into it, and closes when it leaves;
  more than 8 viewers collapse into a `+N` row (a room holds 8 members, so this never shows today).
  0 viewers: the chip shows `0` and a plain tooltip says nobody watches yet; no dropdown. **On a relay
  that does not report viewers (an older relay) the chip is not drawn at all**, since a `0` would be
  wrong. Per room: a share in two rooms has two tiles with their own lists. Data: the relay's
  `ViewersList` datagrams (`protocol-spec.md` §9.1) -> `ClientEvent::Viewers` ->
  `RoomSession::viewers` (pruned after `VIEWERS_STALE`), names from the roster. *Why the own tile and
  not the bottom bar:* the tile is per room and per stream (the bar shows one share and rolls rooms
  into an "N rooms" popover) and a hover list needs room that
  the bar's one-line status does not have. *Why a dropdown with avatars:* the user's request ("a
  viewer number ... when he hovers over it, a drop-down thingy ... the profile picture and the
  username"); "profile picture" is the app's existing avatar. Browser twin: `docs/js/ui/stage.js`
  `viewersChip`, `features/website/viewer-features.md` "Who watches my share".

**The control bar** (task I3, `stage/bar.rs`; the replica's `.ctlrow`): `[hide sidebar][Copy invite link]` left (a right click opens a small menu: Copy join link, Copy browser direct link; `BarMenu::Invite`), two button
groups and Leave in the middle, `[pop out][fullscreen]` right (Watched has no Invite, Room settings or Leave and shows
Share and audio only while you have a share to act on, in the room you share in). **Share screen, Share audio, Webcam,
Layout and Room settings are split buttons that always show their dropdown arrow**, cut off from the button by a separator
line (`STAGE_BTN_SEP`); grouped buttons sit on a backdrop (`STAGE_BTN` in a `STAGE_GROUP` pill), the freestanding ones at the
sides have none; a live Share/Audio button is green, a staged one accent (`ready`); **Leave is 64x50, as tall as a group**.
The people button shows and hides the strip; the **Level loudness** button (`graphic_eq`, `LEVEL_W`, rig rect `ctl.level`, row 134, `SETTINGS.md`) sits left of it in the second group; there is no people list and no arrow above the avatars. *Share* click: idle
starts the picker and, once something is picked, **goes live by itself with the remembered settings, no modal** (2026-11-05,
`UiAction::ShareNow` -> `StagedShare::auto`, `App::poll_staged`; a dismissed picker leaves nothing staged; or opens its menu
when your share is live in another room, so add / move here stay reachable), staged
or live opens the Stream settings modal. The menu's own start entry ("Share your screen…" / "Share a different screen" /
"Change source") keeps the old flow: picker, then the full modal, then Go live (`UiAction::StageShare`). *Why:* user,
2026-11-05: "When clicking the screenshare button in the bottom bar, it should just open the picker and once something is
selected, it should automatically start the screenshare, without opening the full settings. When it is started from the
context menu, it should open up the full settings of it." **Remembered settings:** every stream setting (FPS, Preset's
Encoding and Keyframe, Codec, Bitrate) is saved as `start_stream_defaults` at Go live **and on every change of a live
share** (`AppState::apply_destination_edit`), user: "It should also save the most recent settings, that the user has used
(not just the profile, like even if a custom framerate and keyframe interval)". The saved value is the user's edit, not the
destination's live params, so an automatic codec fallback (AV1 -> H.265 -> H.264) is never remembered. Staged edits that
never went live stay unsaved. Its arrow opens the **Share menu** (Off / Ready / Live header with pill, size,
codec and rate; Preset (with its caption) and then FPS for the live destination or the next stream (2026-11-05: Preset above FPS, a preset lit only on an exact FPS + encoding + keyframe match); Change source, All stream settings…, Go live
in / Stop sharing; a recovering or encoder-bound note). *Share audio* (I7) is a share of its own, **no screen needed** (it was "next to a screen only" until 2026-11-04): one click turns **Desktop audio** on (the icon goes `music_note`, green), the next turns it off; its arrow opens the **audio menu** (`share::audio_menu`: heading "Audio source", No audio, Desktop audio with the sub-line "Everything you hear" / "... minus N excluded", then every app under its **source group** heading (Games, Browsers, Media, Voice chat, Devices, Other) with a live level meter, a Mute shared audio switch while sharing, and "All audio settings…"). Picking a row starts, switches or stops the share in the room the bar acts on at once. "All audio settings…" opens the **audio settings menu** (`audio_settings.rs`, `Modal::AudioSettings`): Source card (the Audio dropdown, `menu::select`), Volume card (**Boost** 50-200 %, moved here from Settings > Shared audio), Desktop audio card (the **exclusion list**: Add opens a popover, `PopoverKind::AudioExclude`, with the sources detected now and an "Or enter a name" field; every entry has Remove), footer `[Stop sharing audio | Share desktop audio][fill][Done]`. Why and the audio side: `features/audio/desktop-audio.md` "desktop audio ... (task I7)"; Linux only. *Webcam* (I8) is a share of its own, no screen needed: click starts it (the saved
camera in its saved mode, into the room the bar acts on; from another room it **moves**) and the next click stops it; its
arrow opens **only** the camera list (name and `/dev/video0` under it, the current one ticked; a new camera resets the
mode) and "All webcam settings…" (`share::webcam_menu`, `BarMenu::Webcam`). It is disabled with the reason as its tooltip
when there is no `v4l2src` plugin, no camera ("Looking for cameras…" while the first listing runs) or off Linux, and a
running webcam lights it green. Layout menu: Split / Crop / Floating (two-line items), **Webcam in picture** (per view, on by default),
Always show names, **Show stats** (numbers on every stream's name tag while hovered), Hide my own streams (removes your screen and webcam, in-picture webcam included, from your stage only,
others still see them), **Hide when unfocused** (on by default: your own screen's live preview stops while the window is unfocused; off keeps it running; your webcam never stops; `features/self-preview.md`, user request 2026-11-11), Reset sizes and positions (also forgets the PIP spots and sizes). Room settings menu: header, Auto-connect, Rename, Copy invite link, Copy browser direct link (the web client's address with the invite link in its `#join=` fragment, `ui/direct_link.rs`; `architecture/protocol-spec.md` §11.4; browser twin: `features/website/direct-link.md`), Leave room, Forget room (the modals
the old header used). Bar layout and narrow windows: see "Narrow windows" in `APP-SHELL.md`.

**Right click on a sidebar room row** (2026-11-05) opens this same Room settings menu at the pointer, for the clicked room (open or not, connected or not: Connect instead of Leave); a left click still selects, a right click never changes the open room. Native: `PopoverKind::RoomMenu(RoomKey)`, set by `sidebar.rs` `paint_lines` (`secondary_clicked`), drawn by `stage/menus.rs` `room_key_popover` at the top of `sidebar::show_body` (before the rows, so a right click on another row queues its open after the old menu's click-outside close); body is the shared `room_menu`. *Why:* user, "on desktop, make sure that right clicking a room in the left view opens the same context menu, as the bottom button bar has".

**The people strip and hover card** (task I3, `stage/strip.rs`): one 64 px cell per person (you first); ring, avatar, LIVE
badge and name hang off **one centre line** (the pixel-snapped avatar rect's) with and without audio, and the green ring is
one 2 px circle stroke 3 px outside the avatar, so it is even all round. The ring means audible shared audio (no voice chat);
several streams of one person are one avatar with a count; a popped-out stream shows only as a dashed outline and a pop badge
on its owner's avatar and a click on the avatar brings all of them back. Hovering a person opens **the same card for everyone**:
avatar, name, "Sharing N streams", and per stream a row `label · Audio / No audio`, the numbers (`res · fps · codec · rate`,
only the announced size for an unwatched stream), the state (`On the stage`, `Popped out` with a **Bring back** button, `Not
watching`, `Your stream · N watching`, + the room in Watched). The card stays open over the avatar, the gap and itself. Audio
is per owner on the wire (`sibling_audio_stream`), so two streams of one person both say Audio when the person shares audio.
**Avatars are drawn by one function, `ui::paint_person(ui, rect, name, emoji, presence)`** (2026-11-04, task #114): `emoji`
("" = none) goes to `widgets::paint_avatar_emoji`: a bundled emoji replaces the initial, on a background from its strongest
colour (neutral slate for a colourless one), with a thin black outline and its visible-pixel crop centred; "" or an emoji
the client has no picture of (a peer's, outside the picker's 56) draws the initial on the name's gradient. Call sites: the strip, the
hover card (head), the stream menu (head), the idle tile's big avatar, the viewers dropdown and the sidebar's user band.
*Where the emoji comes from:* `Cx::emoji_of(room, conn)` (`RoomSession::emoji_of(conn_id)`; my own entry is my setting, so a
change shows at once); the strip's `Person` carries it (`you` = `cx.state.emoji`, Watched uses each item's own room), the
viewers dropdown (`tile::viewer_list` gives `(ConnId, name)`), the idle tile and the stream menu look it up by the stream's
connection, the stream menu uses `cx.state.emoji` for my own item. *Why by connection:* "a person is a connection" under
"The stage"; it was by name until 2026-11-04 (#127).

**Menus and dropdowns** (task I3, `ui/menu.rs`): every dropdown is the app's own popover style (`ui::popover` frame:
`PANEL_RAISED`, radius 10, padding 6, shadow; `pop_item` rows with an icon and a faint second line; the current row has a lit
icon and a check at its end, the replica's `.mi.on`). The bar's menus and the stream menu are `Nav::popover` (`PopoverKind::Bar(BarMenu)`, `PopoverKind::Tile(ItemId)`), drawn
by `stage/menus.rs` *before* the bar so a click outside one that lands on another bar button closes it first. **Dropdown fields**
(`menu::select`) open the same menu **under the field, as wide as it**; the Stream settings modal's FPS and Audio dropdowns and
Settings > Presentation use it instead of `egui::ComboBox` (`widgets::select`, no longer called). *Why:* the user: every dropdown
looks like the app's menus, icons and sub-lines where they exist. Escape with a dropdown open closes the dropdown alone
(`App::update` takes the key first, 2026-11-04). *Why:* otherwise the modal under it closes on the same press, like a popover
would without its own rule.

**The whole stage can pop out** (task I3, `stage::show_windows`): the bar's pop-out button moves the stage (streams, strip and
bar) into its own OS window, an **immediate viewport** that runs the same `stage::show` with `Host::Window`, while the main window
shows "The stage is open in its own window" and **Bring it back**; the window's button, its close button or a vanished room bring
it back. Fullscreen there is the window's own (`ViewportCommand::Fullscreen`, F11 / Escape). *Why immediate, not deferred like the
per-stream pop-outs:* a deferred closure is `Send + Sync + 'static` and cannot hold `Cx` and the video views; an immediate one runs
inside our own frame, so there is no `Shared` round trip. **A reconnect** (2026-11-04, I5): while a room is suspended the window
is away and the stage stays popped (`AppState::reconnects` holds the same `RoomHandle`); it opens again with the room. *Why:* D1,
"a pop-out comes back after a reconnect or relay restart", the same as a stream's pop-out; only a room that is really gone (left,
cancelled) brings the stage home. **Modals and menus open in the window that opened them** (row 182, 2026-11-12):
`ui::modal_host` picks the viewport that draws the open modal: a new modal goes to the viewport that last got a press, release or
key (`Shell::input_vp`, set by `took_input` in `draw` and in the stage window's closure), if that is a live stage window, else the
main window; it keeps its host until closed. `draw` draws it in the main ctx only when the host is the root; `show_windows` takes
a `modal_ui` callback and draws it inside the window closure otherwise (scrim, focus and Escape / x / backdrop then belong to that
window; the main window stays usable meanwhile). If the window closes (or the stage comes back) with the modal open, it moves to
the main window next frame. Bar and tile menus (`Nav::popover`) were already drawn with the window's own ctx by `menus::show`; Escape
in the window now closes one (`App::update` only sees the main window's keys). Tooltips are egui's, per viewport. Toasts follow the same
rule (row 183): `Toast.host` is set when it is raised by `ui::toast_origin`: the stage window in `Shell::input_vp` only if it took input within `TOAST_INPUT_WINDOW_SECS` (2 s, `Shell::input_at`), else main (row 185: a toast nobody just triggered, e.g. "Connected", stays on main; same rule as the browser's `originHost`). `ui::toast_host` picks the painting viewport: that window while it is
still live (`live_windows`), else the main window, so a toast of a window that closed meanwhile moves to main instead of vanishing. Main paints its
toasts last in `App::update`, each window inside `show_windows`. *Why:* user request 2026-11-12: "toasts (e.g. Invite link copied) triggered by an
action in the popped-out stage window should appear in that pop-out window"; 2-s window: parity with the browser, user OK 2026-11-12. Modal *Why:* user request 2026-11-12: "The End call button opens a modal. This modal
isnt visible in the popped out view and instead shows on the main window itself. When something is pressed in the popped out
section, the menus/modals and so on should also show in the popped out window." Rule tests: `ui::tests::modal_host_follows_the_opening_window`, `toast_host_is_the_raising_window_while_it_lives`, `toast_origin_needs_recent_stage_input`.
Per-stream pop-outs (`popout.rs`) have no modals; they are unchanged (`popout_survives_resume`).

**Stage motion and draw order** (task I12, 2026-11-04; `stage/geom.rs` `ease`, `Glide`, `stage/mod.rs` `draw_arena`, `stage/pip.rs`
`PipGlide`; replica `.st-tile` / `.arena.nofx` / `.st-tile.dragging` / `.pip.snap`).
- **Tiles glide.** When a tile's layout rect changes (a swap, a layout switch, "Reset sizes", a bar double-click, a stream added or
  removed so the row count or the aspect fit changes) it slides to the new rect over **0.2 s with the CSS `ease` curve**
  (`cubic-bezier(.25,.1,.25,1)`, `geom::ease`; egui's own animations are linear). State per view and tile: `ViewGeom::glide`
  (`geom::Glide { from, to, start }`, **arena-relative**, so a moving arena carries a glide along); each frame `Glide::step` takes
  the layout rect, a changed target starts a new glide **from where the tile is drawn now** (a swap in the middle of a glide does
  not jump). Drawing, hit-testing, hover and the PIP use the drawn rect; the drop targets use the layout rect.
- **What does not glide** (the replica's `nofx`): a bar, a floating window or its resize handle being dragged (the tile must stay
  1:1 under the pointer; the other tiles of a bar drag follow it at once); a changed **arena size** (window resize, stage
  fullscreen in or out, the people strip hidden: nothing wanders, "streams do not jump when the bar fades" stays true because the
  arena does not change then); a tile that appears. The dragged **reorder** tile follows the pointer without easing; let go, it
  glides from where it was dropped to its cell (the swapped tile to its new one), as the replica lands it first and then glides.
- **The PIP glides** after a drag (`.pip.snap`, **0.16 s `ease`**, position only): on release the PIP goes from where it was let
  go to its spot (`pip::PipGlide { from (tile-relative), start }`; its end is the spot as the layout has it each frame, so a gliding
  tile carries it). The drag itself is 1:1: it is applied from the pointer's distance to the press **before** the frame is laid
  out (like bars). A resize does not glide. *Why:* the user, from their own test of the real client: "Moving streams around
  (swapping them) is not as smooth as on the artifact" and "Moving the webcam around when in webcam PIP mode isnt as smooth as the
  artifact". Not done: egui has no reduced-motion style, so the replica's `prefers-reduced-motion` has no twin.
- **Repaints** are requested only while a glide runs (`request_repaint` per frame for 0.2 / 0.16 s, plus one on the release that
  starts one); idle stays idle (rig: 0-1 CPU ticks per 3 s with nothing playing, before and after a swap).
- **Rig names:** `tile.N` is the **drawn** rect (mid-glide while one runs), `tile_goal.N` the layout rect it is gliding to and
  `cell.N` the grid cell; `pip.N` is the drawn PIP. Scripts that click a tile wait for the glide (0.2 s) or use `tile_goal.N`.
- **Draw order on a tile** (user: "The PIP webcam is drawn above the name in a screenshare (Should be below it, so its visible)"):
  picture, then the webcam in picture, then the name tag with its stats, the hover buttons and the placeholder chips. A video is a
  wgpu paint callback, so what covers what is the order shapes and callbacks are added to the layer: `tile::video_tile` and
  `tile::mine_tile` take a `Between` hook (`run_between`) that `draw_arena` fills with `pip::show` and that runs after the tile's
  picture and before its overlay. (Before I12 the PIP was drawn after the whole tile and covered the name when it sat on the top
  left; the hover buttons now also sit above the PIP, and win a press over it.)

**Fullscreen chrome** (task I3, `stage/chrome.rs`, the pure rule; the replica's behaviour and the user's request #136): the bar's
fullscreen button makes the main window fullscreen with **only the stage** drawn (`Nav::stage_fullscreen`; no sidebar, header or
side bars; Escape leaves, `App::update`). There the strip and bar **lie over the stage** (the arena keeps the whole window minus
8 px, so **streams do not jump when they fade**), on a `STAGE_CHROME` wash, and fade out over 0.25 s: they hide when the pointer
leaves the stage, and after 3 s without movement, click, scroll or key while it is on it; movement brings them back; the tile
overlays and the cursor go with them. **An open menu, context menu, modal, dropdown or a drag stops the timer and keeps the chrome
up wherever the pointer is; when it ends the chrome gets a full 3 s** (`chrome::step`: `held`, `grace`). Entering fullscreen starts
the same 3 s. Outside fullscreen the chrome always shows and the streams get what is left above it. Per-stream fullscreen
and the pop-out windows keep their older twin (`popout::overlay_shown`).

**Webcam and webcam in picture** (task I8, 2026-11-04; `ui/webcam_settings.rs`, `stage/pip.rs`, `stage/geom.rs`; replica
`superdoc/ui/replica/index.html` `camMenu`, `camModal`, `.pip`, `snapPip`, `layoutPip`; the design notes are in
`REPLICA.md` "Stream settings and webcam"; the capture side in `features/stream/webcam-capture.md`).

- **Your webcam** is an item of your own stage like your screen (`Kind::Mine`, `ItemId.label == "Webcam"`, `who == ""`):
  a tile `You · Webcam` with the live preview (while the window is focused, else the placeholder "Your webcam · live"),
  a viewer chip and a Webcam settings button (not Stream settings), its `⋯` menu says Webcam settings… / Stop webcam.
  Someone else's webcam is a stream whose label starts with `Webcam` (`webcam::is_webcam_label`): an idle tile with a
  Watch button, a watched tile with a `videocam` icon in its tag and no audio row ("No audio in this stream").
- **The webcam modal** (width `MODAL_W_WIDE`): *Source* card with **Camera** and **Mode** dropdowns (`menu::select`; the Mode
  list holds every mode the camera really offers, `1280×720 · 30 fps · MJPG`, usable ones first, and opens scrolled to
  the current one), *Compression* with **Codec** (when the machine encodes more than one) and **Bitrate** (200-12000,
  `Set by upload budget` in Absolute bandwidth mode), the fixed-encoder note, footer `[Start webcam | Stop webcam][fill][Done]`.
  Everything acts at once; camera and mode restart a running webcam, codec and bitrate change it live.
  Opened from the arrow's item, the tile's gear and `⋯`, and the sidebar's Outgoing popover (`Modal::WebcamSettings`).
- **Webcam in picture** (Layout menu checkbox, **per view** and **on until switched off** since 2026-11-04 (#131): `StageSettings::pip`,
  `UiAction::SetStagePip`) moves a person's webcam into their first screen share that is **playing on this stage**
  (`pip_pairs`: yours, or one you watch, not popped out; one parent holds one webcam). With none, with a share you do
  not watch (its tile holds the Watch button), with the share popped out, or while you do not watch the webcam itself,
  the webcam stays a tile. Your own webcam goes into your own screen the same way. Hide my own streams hides both.
  A person with a webcam and a screen share therefore gets the webcam in the picture by default (the rules above still apply).
  *Why a tile again in those cases:* a webcam you cannot see playing in a picture would have no Watch button anywhere.
- **Position and size** (`geom::PipPos`, per webcam and per view in `ViewGeom::pip`; replica `layoutPip`): the size is a
  fraction of the **picture's** width (default .24, minimum 72 px, never more than what shows) and the place is one of
  **eight spots** (corners and the middle of each side, 12 px inset, less in a small picture), default bottom right.
  Only a drag in progress is free; **on release it goes to the nearest spot** (`snap_pip`), so the spot stays across
  layouts, tile resizes, the stage's fullscreen, a popped-out stage and stream fullscreen of the parent. It stays inside the
  **visible** part of the picture, in Crop too (Crop shows the middle of a wider picture; the stored value is kept, so
  Split gets the old spot back). Drag past an edge stops at the edge. A click does nothing. The handle (20 px, drawn as the
  replica's L) is on the corner **facing the middle of the picture** and resizes with the aspect locked and the opposite
  corner fixed (`resize_pip`). Right click: the webcam's own stream menu; double click: its fullscreen. *Why the handle faces
  the middle:* a PIP pushed into a corner could otherwise only shrink, and in Floating its handle would land on the window's
  own; the PIP is registered above its tile and below the floating window's grip, so a press on it never moves or resizes the
  tile under it. Deviations from the replica: **square corners** (a video is a wgpu paint callback that cannot be clipped to a
  rounded rect; an outline and a shadow set it off) and, until I12, **no snap glide** (see "Stage motion" below: the snap glides now).
- **Fullscreen and pop-out:** the stage's own fullscreen and a popped-out stage draw the PIP with the stage; **stream
  fullscreen** (`Nav::fullscreen`, the double-click one) calls `stage::fullscreen_pip` for the parent's webcam (same
  spot, same size, same drag). A **stream pop-out window** (`popout.rs`) of the parent shows that stream alone, no PIP,
  and the webcam is a tile on the stage while the parent is popped out (shortest rule, the replica's: a popped-out parent is
  not playing on the stage).
- **Strip:** a webcam inside a picture says `In picture` on its owner's hover card.
- **Rig names** (`SSS_UI_DUMP`): `pip.N` and `pip.N.grip` (N = the parent's tile index; `pip.fs` in stream fullscreen),
  `pipinfo N spot=C,R w=.. handle=tl|tr|bl|br who=..`, `menu.pip`, and in the modal `webcam.camera`, `webcam.mode`,
  `webcam.bitrate`, `webcam.footer`, `webcam_codec.I`, `webcam_modes N`, `webcam_mode KEY`. `hs-pip-spots.py` drags a PIP
  onto all 8 spots with the real pointer. `CLIENT_ENV_1="SSS_FAKE_CAMERA=1"` gives a client a moving test camera.

## Video

`VideoViews` (`cx.videos.show(key, ui, size, Fit::Contain)`) draws a GPU `VideoView`
(`client/src/decode/render.rs`): NV12 decoded frames are uploaded as two textures and
converted to RGB in a WGSL shader through an `egui_wgpu` paint callback. **Repaint is
frame-driven**: each view runs a small waker thread that requests a repaint whenever the
decoder publishes a frame, so screens never call `request_repaint()` for video. Presentation
mode defaults to **Mailbox** (uncapped, no vsync ceiling); because of that the shell caps
idle polling at 50 ms focused / 500 ms unfocused (`IDLE_REPAINT_INTERVAL`,
`UNFOCUSED_REPAINT_INTERVAL`). Measured: **240 fps presented on a ~280 Hz display**.
Presentation mode can only be set at surface creation, so a change in Settings applies on
the next launch (`superdoc/features/presentation-modes.md`).

Pop-out: the view sits in an `Arc<Mutex<VideoView>>` so the deferred viewport closure
(`Send + Sync + 'static`) can paint the same view the tile would; while `popped` the stage leaves the
stream out. The pop-out has hover-gated chrome (statbar, audio pill; volume 0-500 % with a snap at 100 %, task #96, `features/audio/levelling-and-volume.md`), F11
fullscreen, and Escape only *leaves* fullscreen, never closes (the former Electron viewer's
rules, carried over; `superdoc/features/viewer-window.md`).

**A pop-out survives its watch being re-created** (2026-11-03, task #132; user report "the pop-out sometimes just
closes all by itself"). Cause: `popped` lives on the `VideoEntry`, which is keyed by `(RoomHandle, ConnId, StreamId)`;
a reconnect (connection drop, relay restart, viewer recovery L3), or the sharer stopping and re-sharing, ends the watch
and the wanted-stream logic (`session::apply_rewatch`) starts a new one under a *new* `ConnId`/`StreamId`, whose entry
started with `popped = false`: the window vanished and the stream reappeared inline in its tile. Reproduced in the
headless rig by popping out and restarting the relay (pre-fix binary: only the main window left, stream inline; fixed:
the pop-out window comes back). Not the cause (checked): viewer recovery L1 (Unsubscribe+Subscribe keeps the watch) and L2
(resets the pipeline in place, same `FrameSource`), `OVERLAY_IDLE`, Esc (never closes), modals/room switching
(`popout::show` runs on every path of `App::draw`, both fullscreen and normal). Fix: `VideoViews::sync_with` remembers a
vanished popped entry by owner name + stream label (`PendingPop`, only while its room is joined or suspended, for
`REWATCH_GRACE`) and hands the flag to the entry of a watch with `WatchEntry::resumed` (set by `rewatch_one`) that
matches. *Why only `resumed` watches:* a watch the user starts by hand (or after the 30 s resume window) must not pop
out by surprise; an explicit `Unwatch` clears `popped` first. The window itself is re-created (same position rules as
any new pop-out, windowed): it is closed while the room is suspended. Test:
`ui::tests::popout_survives_resume`.

**Double-click a watched tile = fullscreen** (2026-10-15, task #77). `stage::show` registers a
`Sense::click_and_drag` over the tile (before the hover overlay, so the overlay's buttons keep their own
clicks; a single click still does nothing) and a double-click pushes `UiAction::EnterStreamFullscreen(key)`.
`apply` records `Nav.fullscreen = Some(FullscreenStream { key, was_fullscreen })` (was the window already
fullscreen?) and sends `ViewportCommand::Fullscreen(true)`. While `Nav.fullscreen` is set, `App::draw` paints
*only* the stream (black `CentralPanel`, `VideoViews::paint(.., Fit::Contain)`, the same paint as a pop-out:
no sidebar, bands, modals or toasts; pop-out viewports are still declared). A double-click on it or Escape
(handled in `App::update`) pushes `ExitStreamFullscreen`, which sends `Fullscreen(was_fullscreen)`, so a window
that was already fullscreen stays fullscreen. If the stream disappears (unwatched, ended, popped out) the next
`draw` leaves the mode by itself. A popped-out stream is absent from the stage and not eligible. Inside a pop-out
viewport a double-click toggles that window's fullscreen like F11 (`popout.rs`, registered before the chrome).

**The fullscreen stream carries the pop-out's overlay** (2026-11-03, task #106): `App::draw` calls
`popout::fullscreen_overlay` after the double-click `interact`, which runs the *same* `popout::chrome`
(hover-gated vignette, statbar `[dot][sharer · room][WxH][dec/pres fps][bitrate]`, audio pill or "No audio
shared" chip) and the same always-visible centre chip (waiting / recovering / sharer stalled), and shows
it while the pointer is anywhere in the window. Differences: the top-right round button is
`fullscreen_exit` ("Exit fullscreen", pushes `ExitStreamFullscreen`) instead of the pop-out's `close`
("bring back to its tile"; pop-out-only), and volume edits go straight out as `UiAction::SetVolume`
instead of through `Shared::pending_volume` (the data is collected per frame with `collect_info`, no 4x/s
refresh). *Why:* user request 2026-11-03: "the same overlay, that a fullscreened pop-out has, should be
there when i fullscreen a screenshare inside of the main window". One `chrome` function serves both so
the two cannot drift apart.

**The fullscreen overlay auto-hides after 3 s idle** (2026-11-03, task #124): in fullscreen (the main
window's stream fullscreen *and* a fullscreened pop-out; a windowed pop-out keeps "visible while the pointer
is inside") `popout::overlay_visible` hides the chrome (vignette, statbar, round button, audio pill / chip)
and sets the cursor to `CursorIcon::None` once `OVERLAY_IDLE` (3 s) passes without a pointer move, press,
scroll or key; any of those brings both back. Resting the pointer on the statbar, the round button or the
audio pill/chip keeps it (`ChromeOut::over`, remembered one frame in egui temp memory, per viewport), so a
control never vanishes under the cursor. The chrome has no dropdown/menu, so there is no menu-open case yet.
The centre chip (waiting / recovering / stalled) stays regardless. The rule is the pure
`overlay_shown(now, last_active, over_control)` (test `overlay_hides_after_the_idle_time_unless_a_control_is_hovered`);
`request_repaint_after` is asked only while the overlay is up and not held by a control, so an idle hidden
overlay causes no repaint loop. No fade (the native chrome paints with raw painters; a hard cut, no flicker).
The browser twin is `popout.js` `autoHide` + `.pip-stage.idle` in `app.css` (fullscreen stage only, 3 s,
keeps while `.statbar`/`.round-btn` is hovered; the existing 120 ms opacity transition is the fade; e2e check in
`docs/dev/e2e/avatar-fullscreen.mjs`). *Why:* user request 2026-11-03: "The overlay in full screen should go
away when the mouse cursor isn't moved for like 2, 3, 4, 5 seconds" (3 s picked from that range). Verified in
the headless rig: overlay up after a move, gone after ~4 s idle (and the cursor, `grim -c`), back on a move,
kept while resting on the statbar, both for the in-window fullscreen and a fullscreened pop-out.
*Why:* user request 2026-10-15 ("double clicking a share should actually full-screen it"). *Why a mode of
the main window and not a new viewport:* the pop-out already exists for "own window"; this is the quick
"fill my screen" gesture and must restore the layout (scroll, selection) untouched. Test:
`stream_fullscreen_needs_a_watched_stream_and_exit_restores_the_window_state`; verified in the headless rig
(sway `fullscreen_mode` 0 -> 1 on the double-click, 0 again on Escape from a windowed start).

## Layout menu settings are saved (2026-11-04, #131)

Everything the Layout menu sets survives a restart, in both clients: the layout (Split / Crop / Floating), Webcam in
picture, Always show names, Hide my own streams, Show stats, Hide when unfocused (native only, stored inverted as `preview_unfocused`). Defaults for a fresh install, or a view with no saved value:
**Split, Webcam in picture on, Hide when unfocused on, the other three off.** *Why:* the user: "Layout settings should save, with these being the
defaults." (with a screenshot of the menu: Split, Webcam in picture checked, the rest unchecked).

- **Native** (`AppSettings::stage: StagePrefs` in `settings.toml`, `#[serde(default)]`, so an older file loads with the
  defaults). The three switches are global booleans `hide_own`, `always_names`, `show_stats`. Layout and Webcam in
  picture are **per view** ("each room and Watched keep their own"): `[stage.views."<key>"]` with `layout =
  "split"|"crop"|"floating"` and `pip = true|false`, the key being `"watched"` or the room's `room_identity` (server +
  room id + key, no cert hash: "Saved-room identity" in `APP-SHELL.md`), because a `RoomHandle` does not survive a restart. Only
  views that differ from the defaults have an entry (so `pip` defaulting to on costs nothing in the file). A room
  joined by a link that is not saved is saved here by its identity too (nothing extra to do, harmless).
- `StageSettings` (`ui/stage/mod.rs`) keeps the session's choices by `StageView` as before and learns each joined
  room's key (`sync_rooms`, every frame and before a change); a room's saved values are seeded when its key is first
  learnt, Watched at start (`StageSettings::from_saved`, `Shell` built in `App::with_state`). The actions `SetStageLayout`,
  `SetHideOwnStreams`, `SetAlwaysShowNames`, `SetShowStats` and `SetStagePip` write the file (`ui/apply.rs`).
- **Not saved** (per session, as before): tile order, grid sizes, floating window rects and z-order, the webcam's
  spot and size, the strip, the sidebar, pop-outs.
- **Browser** (`docs/js/ui/store.js`): `settings.stage = { hideOwn, alwaysNames, showStats, views: { "room:<id>" |
  "watched": { layout, pip } } }` in the `sss.settings.v1` localStorage entry, normalized on load, only non-default views
  stored; `S.stage` is built from it and `persistStage()` writes it after every Layout-menu change (`menus.js`,
  `setLayout`). The room id is the saved room's (`<roomIdHex>@<server>`), stable across reloads. `S.stage.pip` (the set of
  views with it on) became `S.stage.pipOff`.
