# App Shell (native egui client)

Rewritten 2026-10-14. How the desktop client's one window is laid out, which module owns
which region, and the behavioural rules and design reasons behind it. Code:
`client/src/ui/` (screens), `client/src/widgets/` (layout + controls),
`client/src/theme.rs` (tokens). How controls are built and styled: `STYLING-GUIDE.md`.
What the UI sits on top of: `superdoc/architecture/overview.md`.

The client's UI is **one native egui app**. It replaced an Electron + HTML/CSS/JS frontend
(and, before that, a first egui UI with a Stream Control / View / Settings nav rail); both
are history, see the end of this page and `FRONTEND-SHELL.md`.

## Design

**Room-centric, Discord-like.** You pick a room in the left sidebar; the centre shows that
room; everything you can do about sharing is scoped to the selected room. The layout was
approved by the user as an HTML mockup (https://claude.ai/artifact/5yA1iHf1YnXqmABkZajCe4,
its "Proposed" tab).

```
┌ sidebar 272 ────────┐┌ room header band 56: title, status pill, meta                   ┐
│ brand band 56       │├────────────────────────────────────────────────────────────────┤
│ Watched streams     ││ the stage (black): streams as Split / Crop / Floating           │
│ ROOMS (rooms well,  ││   people strip: avatars, ring, LIVE badge, hover card           │
│  sharer sub-rows)   ││   control bar: [sidebar][invite] [share|audio|webcam][layout|   │
│ Create or join      ││      room|people][leave] [pop out][fullscreen]                  │
│ user band 56        ││  (other states: scrolling body, pad 16)                         │
└─────────────────────┘└────────────────────────────────────────────────────────────────┘
```

The People panel and the bottom action band are gone (task I3, 2026-11-04): the stage's strip and bar replace
them, and the room header lost its controls (Auto-connect, Copy invite link, Leave and `⋯` live in the bar and its
Room settings menu). *Why:* the user approved the replica's stage, where people and controls sit on the stage like a
call, not in side panels.

- Left sidebar **272 px** (hides with the bar's button, `Nav::sidebar_hidden`); room header band **56**. A room
  with no stage (not connected, connecting) keeps a `⋯` in the header for the Room settings menu, since there is
  no bar to hold it.
- **Connecting vs Reconnecting** (`ui::ConnState`: `Joined`, `Connecting`, `Reconnecting`,
  `Disconnected`). `Connecting` is a room's *first* connect in flight (sidebar/header pill
  "Connecting"); `Reconnecting` is only a suspended room (TERMINOLOGY) that the lib is
  retrying (`AppState::reconnect_info`). Both show the panels and a "stop connecting" Leave.
  *Why:* the first Connect of a saved room used to read "Reconnecting" (stage-6 QC).
  A failed Connect (`start_connect` error) raises an error toast and the room's Disconnected
  view shows an inline warning banner above the empty state, text
  `ui.room.connect_failed` ("Couldn't connect: <error>"), cleared on the next Connect.
- **Share labels** are translated at draw time by `ui::share_label(raw)`: the default capture
  label and the wire default "Screen" (in any locale) render in the current language, other
  names (audio sources, peer-chosen labels) as is. The lib stores the label translated at
  creation, so without this a language switch would leave old shares in the old language.
- The stage (strip and bar included) exists only while the shown room is joined; "Watched streams" is always a stage;
  a disconnected, connecting or reconnecting room and the welcome view show the scrolling body (`room::shows_stage`).
- **The Stream Control / View / Settings nav rail is gone.** Settings is a modal opened
  from the user band's gear; sharing lives in the stage's control bar; watching is the
  the room's stage.
- **Settings is a modal with four tabs**, Profile | Interface (Language, Display, Sounds) | Playback | Diagnostics (a
  `widgets::segmented` row, `ModalsState::settings_tab`, opens on Profile and keeps the choice while the app runs); the
  body has one height on every tab (`SETTINGS_BODY_H`). The browser has Profile | Interface (Language, Sounds) | Playback.
  Native Playback holds one control, Level loudness (the Jitter buffer and Catch-up speed sliders are gone, #125 below).
  *Why:* the user's request, 2026-11-04.
- **Stream settings open in a modal**, not a side panel. *Why:* the user said the side
  panel was too squished for the number of controls.
- **Leave sits in the control bar** (red, as tall as the button groups; before I3 in the room header). *Why:* the
  replica's call; it is a room-level action, so it is not in the Share group.
- **Narrow windows (800 px minimum, a ~528 px stage when the sidebar shows).** *Why 800 (PR #4, 2026-11-03; 960
  before):* so the window half-tiles on a 1920 px screen. The control bar degrades in a fixed order (`stage::bar::arrange`,
  pure, tested): the **Copy invite link label** goes first, then the bar takes **two rows** (the middle part on top, the
  side buttons under it, 104 px instead of 72); the middle part (two groups and Leave, ~500 px) still fits 528 px with 8 px
  side padding. Icons never move into a menu. The hover card and the menus clamp to the window. The header drops its meta
  line when it does not fit (never half-cut), a long room name truncates. Stage tiles: the name tag drops the stats
  resolution, then the bitrate (`tile::stats_fields`), the number slots never half-draw. Body and modal: the
  stream-settings modal stacks its two columns into one when a column would be narrower than its widest segmented
  control / Screen readout needs (measured from the current locale's labels, so it is one column at 800 and 960 and
  two at 1280+ in en and de); the reconnect banner moves its buttons under the text when the text column beside them
  would be under 320 px; empty states wrap, centred. *Why measured, not a fixed breakpoint:* German labels run ~20 %
  longer, so a fixed width either stacks English for nothing or still truncates German. Details in
  `stream_settings.rs` (`column_need`), `room::reconnect_banner` and `stage/bar.rs`.
- **The stage** (task I1, 2026-11-04; the replica's Stage, `ui/replica/index.html`, user-approved in the design
  sandbox, see `STYLING-GUIDE.md` "HTML replica"). The centre of a **joined room** and the whole **Watched streams**
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
  - **Keys:** a tile is identified by `stage::ItemId` = room handle + owner name + label (+ audio flag, n-th of
    identical ones), your own share is `room/me`; **not** by `StreamKey`, whose `ConnId`/`StreamId` change with every
    reconnect, so a reconnect keeps order, grid and window rects (`WantedStream`'s identity). Watch, pop-out, volume
    and stats belong to the stream (`StreamKey`) and are shared by every view; the layout state is per view
    (`StageState::views`, UI-private, per session, **not persisted**). It is never pruned, so a stream that comes
    back keeps its place. The layout choice and the two options sit in `Shell::stage` (`StageSettings`) because the
    Layout menu is another screen: `UiAction::SetStageLayout { view, layout }`, `SetHideOwnStreams`,
    `SetAlwaysShowNames`; `Cx::stage_view()` / `Cx::stage_layout()` read them.
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
    The Layout menu does not exist yet (control bar rebuild): until then nothing in the app sets Crop or
    Floating, the actions and state are there.
- **Tile overlay and stream menu** (task I3, D1): a watched tile shows nothing over its picture until the pointer is on it.
  Then: the **name tag** at the top left (`[icon][name][· label]`, the icon is the audio status: `volume_up` audible,
  `volume_off` muted or none) and three round buttons at the top right, **Pop out**, **Fullscreen** and `⋯`.
  **"Always show names"** (Layout menu) keeps the tag without hovering; the buttons and the stats never stay. **Show
  stats on the stream** (stream menu, per stream) adds `res · fps · rate` to the tag **while hovered**, in slots reserved
  for `9999×9999`, `999 fps` and `99.9 Mbps` (`tile::RESERVE`), so the pill is exactly as wide as its content and never
  jitters as a number gains a digit; its tooltip keeps what the old statbar's had (decoded vs shown fps, codec and decoder,
  dropped). A narrow tile drops the resolution, then the bitrate (`tile::stats_fields`). A green ring on the picture
  means the stream's shared audio is audible. Double-click a watched tile = fullscreen (below). *Why no audio pill and
  no Stop button on the tile any more:* the replica puts volume, level loudness, stats and Stop watching in the stream
  menu, so the picture stays clean (user, D1: "a stream's buttons and stats show only while it is hovered").
- **The stream menu** (`stage/menus.rs::tile_menu`, `PopoverKind::Tile(ItemId)`): the `⋯` button or a **right click on any
  tile** opens it (anchored to the button or the pointer). Header (avatar, who, `label · size`, or `label · room` in
  Watched); for a watched stream with audio a Volume row `[mute][slider 0-500 %][pct]` and the **Level loudness** switch
  (the global setting, same action as Settings), else "No audio in this stream"; **Show stats on the stream**; Pop out,
  Fullscreen, Stop watching. An unwatched tile's menu is Watch stream / Pop out; an audio-only sender's Listen and,
  while listening, the volume row; your own tile's Stream settings… / Stop sharing. **Volume acts during the drag**
  (`tile_volume_slider` pushes `SetVolume` on every change, #135; D1) and **scrolling on the slider** moves it 5 % a notch
  up to 200 %, 10 % above (`ui::volume_slider`, `wheel_step`, #113), here and in the pop-out. *Why the exception to "local
  edit state is pushed on `drag_stopped`" (below):* `SetVolume` only sets the decoder's volume element and the in-memory
  value, one per changed frame is cheap, and the user wants to hear the level while choosing it; the Boost slider in
  the audio settings menu does the same (`PreviewAudioGain` while dragging, `SetAudioGain` on release).
- **Own-tile live preview** (#78, user request 2026-10-15): your own tile shows a live <= 640x360 / <= 30 fps picture
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
  username"); "profile picture" is the app's existing avatar. Browser twin: `docs/js/ui/app.js`
  `viewersChip`, `features/website.md`.

**The control bar** (task I3, `stage/bar.rs`; the replica's `.ctlrow`): `[hide sidebar][Copy invite link]` left, two button
groups and Leave in the middle, `[pop out][fullscreen]` right (Watched has no Invite, Room settings or Leave and shows
Share and audio only while you have a share to act on, in the room you share in). **Share screen, Share audio, Webcam,
Layout and Room settings are split buttons that always show their dropdown arrow**, cut off from the button by a separator
line (`STAGE_BTN_SEP`); grouped buttons sit on a backdrop (`STAGE_BTN` in a `STAGE_GROUP` pill), the freestanding ones at the
sides have none; a live Share/Audio button is green, a staged one accent (`ready`); **Leave is 64x50, as tall as a group**.
The people button shows and hides the strip; there is no people list and no arrow above the avatars. *Share* click: idle
starts the picker (or opens its menu when your share is live in another room, so add / move here stay reachable), staged
or live opens the Stream settings modal; its arrow opens the **Share menu** (Off / Ready / Live header with pill, size,
codec and rate; FPS and Preset for the live destination or the next stream; Change source, All stream settings…, Go live
in / Stop sharing; a recovering or encoder-bound note). *Share audio* (I7) is a share of its own, **no screen needed** (it was "next to a screen only" until 2026-11-04): one click turns **Desktop audio** on (the icon goes `music_note`, green), the next turns it off; its arrow opens the **audio menu** (`share::audio_menu`: heading "Audio source", No audio, Desktop audio with the sub-line "Everything you hear" / "... minus N excluded", then every app under its **source group** heading (Games, Browsers, Media, Voice chat, Devices, Other) with a live level meter, a Mute shared audio switch while sharing, and "All audio settings…"). Picking a row starts, switches or stops the share in the room the bar acts on at once. "All audio settings…" opens the **audio settings menu** (`audio_settings.rs`, `Modal::AudioSettings`): Source card (the Audio dropdown, `menu::select`), Volume card (**Boost** 50-200 %, moved here from Settings > Shared audio), Desktop audio card (the **exclusion list**: Add opens a popover, `PopoverKind::AudioExclude`, with the sources detected now and an "Or enter a name" field; every entry has Remove), footer `[Stop sharing audio | Share desktop audio][fill][Done]`. Why and the audio side: `features/audio-sharing.md` "desktop audio ... (task I7)"; Linux only. *Webcam* (I8) is a share of its own, no screen needed: click starts it (the saved
camera in its saved mode, into the room the bar acts on; from another room it **moves**) and the next click stops it; its
arrow opens **only** the camera list (name and `/dev/video0` under it, the current one ticked; a new camera resets the
mode) and "All webcam settings…" (`share::webcam_menu`, `BarMenu::Webcam`). It is disabled with the reason as its tooltip
when there is no `v4l2src` plugin, no camera ("Looking for cameras…" while the first listing runs) or off Linux, and a
running webcam lights it green. Layout menu: Split / Crop / Floating (two-line items), **Webcam in picture** (per view),
Always show names, Hide my own streams (removes your screen and webcam, in-picture webcam included, from your stage only,
others still see them), Reset sizes and positions (also forgets the PIP spots and sizes). Room settings menu: header, Auto-connect, Rename, Copy invite link, Leave room, Forget room (the modals
the old header used). Bar layout and narrow windows: see "Narrow windows".

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
the client has no picture of (a peer's, outside the 40) draws the initial on the name's gradient. Call sites: the strip, the
hover card (head), the stream menu (head), the idle tile's big avatar, the viewers dropdown and the sidebar's user band.
*Where the emoji comes from:* `Cx::emoji_of_name(room, name)` (member by display name in that room's session, then
`RoomSession::emoji_of(conn_id)`; my own entry is my setting, so a change shows at once); the strip's `Person` carries it
(`you` = `cx.state.emoji`, Watched uses each item's own room), the viewers dropdown and idle tile look it up with the room
they already have, the stream menu uses `cx.state.emoji` for my own item. *Why by name:* stage items and the strip identify
people by display name (`ItemId.who`), as `ConnId` changes on reconnect.

**Settings > Profile > Avatar** (2026-11-04, task #114): a row under Display name with my avatar as a button; clicking it
opens the picker as a popover in the menu style (`modals.rs` `avatar_row`, `menu::frame()`, closes on a pick or an outside
click): header (my avatar, "Your avatar", "Friends see it instead of your initial"), the 8x5 grid of the 40 emoji
(`widgets::emoji_picker`), then "Use my initial" (`text_fields` icon). A choice is `UiAction::SetEmoji(String)` ("" = clear),
applied in `apply.rs` through `AppState::set_emoji` (saved, announced to every connected room). *Why no "Paste any emoji"
field (the replica has one):* only the 40 bundled emoji can be drawn (no colour-emoji font is guaranteed on every build).

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
cancelled) brings the stage home. *Left out:* a modal opened from it (Rename, Stream settings…) appears in the main window and
wants its input there (a click on it works; Escape in the popped window does not reach it). Per-stream pop-outs are unchanged
(`popout.rs`, `popout_survives_resume`).

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

### Layout implementation rule

Every region is built from explicit nested vertical/horizontal containers with fixed-size
slots (`widgets::HBox`, row helpers, `card`/`well`/`band`/`grid_rows`/`modal_shell`); never
`ui.horizontal` + `add_space` for rows. The shell itself is egui `SidePanel` /
`TopBottomPanel` / `CentralPanel` at the fixed sizes above. Full rule and rationale:
`STYLING-GUIDE.md`. *Why panels and not spacers for the shell:* "this region is fixed, that
one takes the rest" is exactly what panels express; hand-computed `add_space` fillers would
re-derive the split every frame and silently break when content height changes.

**Webcam and webcam in picture** (task I8, 2026-11-04; `ui/webcam_settings.rs`, `stage/pip.rs`, `stage/geom.rs`; replica
`superdoc/ui/replica/index.html` `camMenu`, `camModal`, `.pip`, `snapPip`, `layoutPip`; the design notes are in
`STYLING-GUIDE.md` "Stream settings and webcam"; the capture side in `features/stream-control.md` "Webcam capture").

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
- **Webcam in picture** (Layout menu checkbox, **per view** and off until switched on: `StageSettings::pip`,
  `UiAction::SetStagePip`) moves a person's webcam into their first screen share that is **playing on this stage**
  (`pip_pairs`: yours, or one you watch, not popped out; one parent holds one webcam). With none, with a share you do
  not watch (its tile holds the Watch button), with the share popped out, or while you do not watch the webcam itself,
  the webcam stays a tile. Your own webcam goes into your own screen the same way. Hide my own streams hides both.
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

## Module map (`client/src/ui/`)

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
| `stream_settings.rs` | the stream-settings modal (staged "Set up your stream", live "Stream settings"), width 1120 |
| `modals.rs` | create/join, app settings, leave / forget confirm, rename, choose-your-name |
| `popout.rs` | pop-out viewer windows: one deferred viewport per popped stream |
| `tests.rs` | session-model lookups, bandwidth wiring, shell geometry (`shell_regions_match_the_spec_sizes`), nav/action behaviour |

### The screen contract (data in, actions out)

A screen module owns one `<X>State` (its private UI state: text buffers, edit copies,
hover) and `pub fn show...(ui, cx: &mut Cx, st)` functions. It reads everything via `Cx`
(`&AppState` read-only, `nav`, `shell`, `videos`, plus selectors) and **never mutates
`AppState`, `Nav` or `Shell`**; every change is `cx.act(UiAction::..)`, applied by
`App::apply` after all screens have drawn (visible next frame). Local edit state is pushed
as one action when it should take effect (e.g. on `drag_stopped`, not every frame of a
drag), except where the user wants to hear or see the change while dragging (the viewer volume and the Boost
slider push on every change). *Why:* the borrow checker can't hand four screens `&mut AppState` while they also
read it, and a single apply site keeps every state transition in one reviewable place.

The shell hands each screen a `Ui` that is exactly its region; a screen must not paint
outside it.

### Popovers, modals, toasts

At most one popover (`Nav.popover`, anchored to the button's rect) and one modal
(`Nav.modal`). Opening a modal closes the popover; Escape closes the popover before a
modal; Escape, x and a backdrop click close every modal. Toasts are painted last by
`mod.rs` (2.6 s).

**Settings > Diagnostics > Logs: copy or save the app's recent log** (2026-11-03, task #107, user: "The GUI should
have an option in the settings to dump logs to the clipboard or a file, so investigation is easier."). Two
secondary buttons in an own "Diagnostics" card (`ui::modals::settings`): **Copy to clipboard**
(`UiAction::CopyLogs`: `ctx.copy_text(logbuf::dump())` + "Logs copied" toast) and **Save to file...**
(`UiAction::SaveLogs`: `logbuf::save_to_downloads()` writes `simple-screensharing-<UTC stamp>.log` into the user's
Downloads folder, home folder if there is none; a toast plus an info banner in the card with the full path,
`Shell::saved_logs`). The dump is a short header (version + commit hash from `client/build.rs`, OS, GPU from the
wgpu adapter, VA kernel driver once a share has probed it, time, line count) and then the ring's lines, oldest
first. The ring (`client/src/logbuf.rs`) is a second `fmt` layer next to the stderr one (`main.rs` `init_logging`):
last 5,000 events / 2 MB, **INFO+ from `simple_screensharing_client`/`..._protocol`, WARN+ from everything else**,
independent of `RUST_LOG` (which still only filters stderr / the Windows `client.log`). Same UTC timestamps as
stderr. A panic hook now logs `panic: ...` on every platform (it used to be Windows release only) and then runs the
default hook. *Why in memory, always on:* the old advice ("restart from a terminal with `RUST_LOG=info ... 2>
sss.log`") lost the first occurrence of a stall. *Why Downloads and no file dialog:* no dialog crate is a dependency
and a native one would pull a portal client / GTK in for one button. *No secrets:* no log line prints a room key or
join link (grepped 2026-11-03); `logbuf::redact` still turns any `server|room|key...` token into
`server|room|<redacted>` before a line enters the ring. Rig note: `wl-paste` only sees the copy when the headless
sway seat has a keyboard (hold one open with `wtype -P Shift_L -s 30000 -p Shift_L` while clicking; the existing
"Copy invite link" behaves the same). Tests: `logbuf::tests::*`,
`ui::tests::copy_logs_puts_the_dump_on_the_clipboard_and_toasts`.

**Boost slider** (2026-11-03, task #114; since #119 without the Calibrate row and capped at 200 %; **moved on 2026-11-04 (I7) from Settings > Shared audio to the audio settings menu's Volume card**, the Settings card is gone): a `ctrl_row_value` (Boost: `widgets::slider`
in dB, 50-200 %, 100 % mid-track, 0.5 dB knob steps, a `value_box` reading `150%`; applied live on every changed frame of a drag as
`UiAction::PreviewAudioGain`, saved on pointer release as `UiAction::SetAudioGain`, #135). The calibration was removed in #119 together with `UiAction::CalibrateAudio` / `CancelAudioCalibration`.
How it works, and why: `features/audio-sharing.md` "sender-side audio boost" and the #119 block. Tests: `ui::tests::audio_boost::*`. Rig dump: `audio.boost`, `boost N`.

**Settings > Playback > Level loudness** (2026-11-03, task #119; user: "the auto levelling should happen on the viewers
side"). A `ctrl_row` with a checkbox ("Even out the volume of what I watch", tooltip on the label) as the third row of
the Playback card, **on by default**; `UiAction::SetLevelLoudness` -> `AppState::set_level_loudness` (persists
`level_loudness`, flips the process-wide `leveller::ENABLED`, live for every watched stream). The browser has the same row
(`#set-level-loudness`). Design and numbers: `features/audio-sharing.md` (2026-11-03, #119). Tests:
`ui::tests::audio_boost::level_loudness_defaults_on_persists_and_drives_the_leveller_switch`.

**Settings > Sounds** (2026-11-04, task I9; `modals.rs`): a card with one **Volume** row (slider 0-100 %, default 50, applied live while dragged, saved on release; `UiAction::PreviewSoundVolume` / `SetSoundVolume`) and one **Test** button per notification sound (join / leave my stream, someone starts / stops a stream; `UiAction::TestSound`). Separate from every stream's volume. Design, edge rules and why: `features/audio-sharing.md` "Notification sounds". Rig dump: `sound.volume`, `sound.test.*`, `sound_volume N`.

**Level-loudness button next to every viewer volume slider** (2026-11-03, task #126; user: "make sure the dynamic volume is
something the viewer can actually select, whether he wants it or not ... next to the volume slider, have a little button
that indicates the dynamic volume setting"). The audio pill is `[25 mute][110 slider][34 %][25 level]` (pop-out / fullscreen pill in `popout.rs`; hidden with the slider while
muted). Since task I3 the stage's tiles have no pill: the same global switch is the **Level loudness** row of the stream menu. The button is
`ui::level_toggle` (25 px round, `graphic_eq` icon): **on** = accent ring + tinted disc + light-blue icon, **off** = faint
icon; tooltip "Level loudness: on/off" (`ui.room.level_on/off`). A click sends `UiAction::SetLevelLoudness(!on)`, the same
action and the same persisted global `level_loudness` setting as the Settings checkbox, so both stay in sync and it is live.
In a pop-out viewport the click goes through `Shared::pending_level` (like `pending_volume`) to the main window. *Why one
global switch, not per stream:* the user asked for the *setting* to be visible and reachable where the volume is, not for a
per-stream mode; per-stream state (storage per owner like `audio_volume_by_owner`, a flag on each decoder) is possible if
it is ever wanted. The browser had the same button on its tile pill (`levelToggle` in `app.js`, `.audio-pill button.level`); since I6 its tiles have no pill either and the switch is the **Level loudness** row of the stream menu (`docs/js/ui/menus.js`, `applyLevelLoudness`).
The viewer volume maximum stays 500 % (the same request first asked to cap it at 200 %, then the user said "leave it at 500%").
Tests: `ui::tests::level_toggle_reports_a_click_only_on_the_button`; gallery card "Viewer volume" shows both states.
*Geometry (2026-11-03, #130; user: "The dynamic volume button should be all the way in the right half-circle"):* the pill
pads 2 px on both ends while the slider shows (10 px on the right only after the "Muted" text), so the 25 px mute and level
buttons are concentric with the 29 px pill's half-circle ends, 2 px in all round. Browser: 1 px border + 1 px padding
(`.audio-pill`, `.audio-pill:not(.muted)`; the pill is gone from the browser since I6, the geometry now only applies to the pop-out's pill).

**Settings > Diagnostics > Relay: "Send diagnostics to the relay"** (2026-11-03, task #110; user: "he or all of my
friends could just tick the checkbox while we are still in heavy development, and then I can just see it.
Everyone knows this data is being collected."). A checkbox row (`ctrl_row` with the label "Relay", `widgets::checkbox`)
under the Logs row in the same card; `UiAction::SetSendDiagnostics(bool)` -> `AppState::set_send_diagnostics`
(persists `AppSettings::send_diagnostics`, default **false**, opt-in; flips `diag_upload::set_enabled`). The
explanation is the row's tooltip (the styling guide forbids helper text under options): off by default, sends the
same log text as "Logs" with your display name and no room keys, to the relay of each room you are in, and only a
relay that supports it receives anything. No UI for the state beyond the checkbox (no "uploading" indicator: the
upload is a background courtesy, not something to watch). What goes where, cadence and the wire format:
`architecture/protocol-spec.md` §6.7; the operator's side: `features/relay-transport.md` "Client diagnostics upload".
Tests: `ui::tests::the_send_diagnostics_checkbox_is_off_by_default_and_persists`, `rooms::tests::send_diagnostics_is_off_by_default`;
rig: `HS_SEND_DIAG_<n>=1` seeds the setting (`client/scripts/README.md`).

**"Live in <room>" toast fires when a room goes on air, not on Go live** (2026-10-15, task #73).
`App::poll` compares `Cx::on_air_rooms()` with `Shell.announced_live` (`newly_on_air`): a room
that is only Starting, or whose start failed (the destination is forgotten and the error toast
shown), never gets it. *Why:* it used to fire inside `UiAction::GoLive`, so a share that failed to
start showed "Live in Test" above the "Share error" toast (the user's WinBoat run, a missing
GStreamer element). Test: `live_in_toast_fires_once_per_on_air_spell`.

**Window icon** (2026-10-15): `ui::app_icon()` (`client/assets/icon.png`, the `docs/assets/icon.svg`
favicon rasterised once to 256 px with `rsvg-convert`) is set with `with_icon` on the main and the
pop-out viewports. *Why:* on Windows (and X11) eframe's default "e" showed in the title bar and
taskbar. Wayland takes the icon from `app_id` + the .desktop file, so it is harmless there. The
exe file itself (Explorer, shortcuts) has no embedded icon resource: that needs a build-time
resource compiler (`winresource`/`embed-resource` + an `.ico`), not added.

**Note (2026-11-04, plan D8):** the preset, tuning and "Advanced" paragraphs of #90-#95 below describe the state of 2026-11-01. Since then the presets are **Text / Quality / Gaming** (they set the FPS once, the encoder mode and the keyframe interval), the Advanced tuning rows and the Balanced / Low-Latency presets are gone, and the modal's right-hand cards are Compression and Video (`features/stream-control.md` "Presets", `STYLING-GUIDE.md` "Stream settings and webcam"). The reasons below (hide what does nothing) still apply.

**The stream-settings modal shows only controls that change the encode** (2026-11-01, task #90, user:
"Keep the [Tune] setting. But make sure to not show settings, if they wouldn't do anything!").
The backend that will encode is `capture::detect_encoder_backend()` (probed once per process: VA-API if
`vah264enc` reaches PAUSED, else software x264; a destination's `VideoParams::backend` is that same value, so
staged and live agree). Against it:
- **Tune** is drawn only on x264 (`EncoderBackend::tune_applies`, was `presets::tune_applies` until #95): `launch_fragment` maps it to `tune=zerolatency`
  there, and on every VA encoder (`vah264enc`/`vah265enc`/`vaav1enc`) it is dropped (`let _ = tune;`).
  On VA-API the Advanced rows are Quality level + Keyframe interval.
- **Codec** is drawn only when `supported_codecs` offers more than one (`presets::codec_choice_offered`).
  Software x264 is H.264 only, so the row used to be H.264 plus two greyed-out buttons; it is gone there
  (VA-API keeps all three, unsupported ones are not a thing on that backend).
- Everything else was audited and is effective on both backends: Framerate (`RateCap`), Bitrate, Quality
  level (`speed-preset` / `target-usage`), Keyframe interval (`key-int-max`), the bandwidth rows, the presets.
- The browser share modal (`docs/js/ui/app.js`) lost its "Quality level: desktop app only" row (WebCodecs has
  no such knob), and hides the Codec row when the probe finds a single encodable codec. (Its Tune and
  Keyframe interval readouts stayed at first; #93 hid them, see the next block.)
*Why Tune stays at all instead of being hard-wired to zerolatency:* the user judged x264's ~0.7-1 s lookahead
latency on "Best quality" acceptable ("It's screensharing, not remote gaming"), so the choice stays with the
user on the one backend where it matters. *Why hide, not grey out:* the user said "not show".
*Preset highlighting:* picking a preset still writes all three fields (tune included), so an x264 fallback
or a settings file used on another machine encodes as the preset says; but `EncoderPreset::detect_for`
ignores the stored tune where it does nothing, otherwise a stale hidden tune (e.g. Low-latency from an older
run, with Balanced's level and GOP) would leave every preset un-highlighted for a reason the user cannot see
or fix. The four presets differ in (quality level, GOP), so the comparison stays unambiguous. On VA-API
Low-Latency and Gaming differ only in the keyframe interval; the preset set was left as is (until #95, below,
which hides the twin presets). The known edge of #90, a live VA branch that fell back to x264 still hiding
Tune, is fixed by #95 (the backend is now per destination). Tests: `presets::tests::detect_for_*`,
`stream_settings::tests::tune_row_is_only_drawn_where_it_does_something`.

**Hide anything that does nothing, even when it is only a browser limitation** (2026-11-01, task #93).
User rule, verbatim: "If a setting doesn't do anything, hide it (Yes, even when its just a browser
limitation). Just hide it, so other platforms can still use it." So the rule is: *hide in the UI of the
platform where it is a no-op; never delete the setting from the shared data, persistence, protocol or
the other platforms' UI*, and a stored value this platform cannot honour must round-trip unchanged.
*Why hide, not delete:* the same settings file / join flow is used by the native app and the browser;
the setting still means something elsewhere. *Why hide, not grey out or show read-only:* a greyed or
read-only control still looks like a setting and invites the question what it is for (#90 said "not
show"; #93 extended it to the readouts and the disabled buttons).
Applied to the browser share modal (`docs/js/ui/app.js`):
- **Low-Latency preset button hidden.** In the browser `ENCODER_PRESETS.low_latency` equals `balanced`
  (`latencyMode: "realtime"`, 2000 ms keyframes), so the button did nothing different. Balanced is the
  default and Gaming (750 ms) is the low-latency choice. A stored `"low_latency"` is *displayed* as
  Balanced (`PRESET_TWIN`) and is not rewritten unless the user picks another preset; the core's
  `ENCODER_PRESETS` and `startShare({ preset: "low_latency" })` are unchanged.
- **Tune and Keyframe interval rows hidden, and the "Advanced tuning" disclosure with them** (nothing
  was left in it). They were read-only readouts of what the preset sets; the preset row already shows
  that choice. The "follows the preset" hint went too.
- **Unencodable codec buttons hidden** instead of greyed out ("This browser can't encode ..."); the Codec
  row is still dropped when one or none is left. If the stored default codec is not encodable the
  staged modal starts on the first encodable one (staged copy only; the stored default is untouched).
- Unused i18n keys removed in en and de: `ui.stream.advanced`, `tune`, `tune_low_latency`,
  `tune_best_quality`, `keyframe_interval`, `advanced_browser_note`, `codec_unavailable`,
  `ui.preset.low_latency`. (The native client's own `ui.preset.*` keys are in `client/locales`, untouched.)
- Kept on purpose (information, not settings, and they act in the browser): the Screen and Audio source
  rows with the level meter, the Bitrate "set by budget" value in absolute-bandwidth mode, the framerate
  select (the encoder is told the rate and excess frames are skipped), the bandwidth rows, jitter/catch-up (browser only, they act there).

**Native client: hide twin choices and per-destination backend** (2026-11-01, task #95, same user rule as #93:
"If a setting doesn't do anything, hide it (Yes, even when its just a browser limitation). Just hide it, so
other platforms can still use it." then "Make sure to fix all according to spec."). Hide, never grey out and never
delete: every stored value round-trips unchanged, so a settings file moved to another driver or platform still
works. What "does nothing" means is now computed from the *effective encode* of the backend that is running,
`capture::EncoderProfile { backend, mesa_amd }` (`EncoderProfile::of(backend)` probes the VA driver once; the
unit tests build either mapping by hand):
- **Quality level: Balanced hidden on Mesa AMD.** `vaapi_target_usage` gives Performance 2 / Balanced 3 / Quality 4
  there and Mesa reads target-usage as a bitfield where 2 and 3 are bit-identical ("balance" preset, #92), so the
  Balanced button was a second Performance. `EncoderProfile::quality_effect` folds the value the way the driver does
  (Mesa AMD: 1 alone, {2,3}, {4,5}, {6,7}; spec-order drivers and x264: every level distinct), derived from
  `vaapi_target_usage` itself, not from a hard-coded "AMD hides Balanced", so it follows a changed mapping.
  `visible_quality_levels()` keeps Performance and Quality ahead of Balanced when levels are twins. A stored
  Balanced is *displayed* as its twin (`display_quality_level`, Performance) and stays stored as Balanced until the
  user picks another level (clicking the shown, already-selected one is no change). Intel (7/4/1) and x264
  (ultrafast/medium/veryslow): nothing hidden.
- **Presets: Low-Latency hidden when it equals Balanced.** `EncoderPreset::visible_for(profile)` hides a preset
  whose *effective* tuning (`EncoderTuning::effect`: tune only where `tune_applies`, `quality_effect`, GOP) equals
  one already shown; Balanced (the default) wins, the rest win in display order. On Mesa AMD Tune is a no-op and
  Performance = Balanced, so Low-Latency (low-latency, Performance, 2000 ms) = Balanced (best-quality, Balanced,
  2000 ms): hidden, matching the browser (#93 `PRESET_TWIN`). Gaming (750 ms GOP) stays. `detect_for(tuning, profile)`
  compares effective settings, so a stored Low-Latency highlights Balanced on AMD, stays Low-Latency on Intel and x264,
  and the stored fields are untouched until a preset is picked (picking one still writes all three fields).
  Why derived and not a table: the answer differs per driver and per destination (below).
- **Tune and the twins follow the destination's running backend.** `Destination::encoder_backend()` is the backend
  its live branch was built on (`DestinationRuntime::actual_backend`, recorded by `spawn_destination`,
  `swap_to_new_branch` and `rebuild_after_source_restart` from `EncodeBranch::backend()`), the process-wide probe
  while no branch is up, and the probe for the staged modal. So a destination whose VA branch fell back to x264
  (`add_branch_with_fallback` or the no-IDR retry) shows Tune and every level and preset that differs on x264.
  *Why `params.backend` is untouched:* it is the *preferred* backend, tried first again on the next branch swap; a
  transient VA failure must not pin the destination to x264 for good, so the actual one is a separate field.
- **Codec buttons the machine cannot encode are hidden, not greyed out.** `capture::supported_codecs(Vaapi)` was a
  static "H.264, H.265, AV1"; it now offers H.265/AV1 only when GStreamer has `vah265enc`/`vaav1enc` (the VA plugin
  registers an encoder only for a profile the GPU's driver advertises), so a GPU without AV1 encode no longer shows
  an AV1 button that silently produced x264 H.264. A stored codec that is not encodable here is shown as H.264, the
  stream's real codec, and stays stored.
- **Settings > Presentation, fallback run.** When the surface lacks the stored present mode, `main` relaunches on Vsync
  for that run (`SSS_PRESENT_MODE_FALLBACK`). In that run the stored mode (`launch_presentation_mode`) does nothing,
  so it is hidden and Vsync is shown as selected; the stored value is kept and re-picking Vsync is no change
  (`ui::modals::presentation_choices`). Normal runs show all three.
- **Audit result (native, per backend/codec/driver):** Framerate (RateCap, every backend), Bitrate (every backend), Keyframe
  interval (`key-int-max`, every VA codec and x264), bandwidth mode/total/floor (floor and total only drawn in Absolute
  mode, where they act), the Codec row (hidden with one codec), Settings: display name, language, jitter buffer and
  catch-up speed were checked and all did something (this claim was wrong for those two: native never read them, removed in #125, below); the Advanced disclosure stays
  because Quality level and Keyframe interval are always in it. Not changed on purpose: the Source card's audio
  controls (they act on every platform). Known remaining: Gaming vs Low-Latency on VA differ only by GOP (a real
  difference, kept); the modal's two-column width still reserves room for the longest label set (`column_need_for`
  measures all options, so hiding some never makes the layout jump).
- Removed the now unused `.collapser` rule from `docs/css/app.css` (the browser stopped drawing it in #93; the egui
  `widgets::collapser` is unrelated and stays).
Tests: `presets::tests::{mesa_amd_hides_the_low_latency_twin, spec_order_va_shows_every_preset, software_shows_every_preset,
detect_for_follows_effective_settings, quality_levels_hide_the_balanced_twin_on_mesa_amd,
spec_order_mapping_has_no_twins_and_amd_mapping_has_one_pair}`, `capture::destination::tests::actual_backend_is_recorded_apart_from_the_preferred_one`,
`ui::tests::session_model::a_destination_reports_the_backend_its_branch_runs_on`,
`ui::modals::tests::presentation_choices_hide_the_mode_the_surface_lacked`, and the rig runs in
`superdoc/features/stream-control.md` (2026-11-01, #95).

**The stream-settings modal keeps one height whether "Advanced tuning" is open or closed**
(2026-10-15). While collapsed, `encoder_card` reserves the rows' height as empty space *below*
the Encoder card (`adv_rows_height`: the rows laid out once in an invisible, non-advancing child
just to read their height), so the modal is as tall as with Advanced open (still capped by the
window; the body scrolls when it is short). Test: `encoder_card_height_does_not_depend_on_advanced`.
*Why:* the user disliked the modal growing and shrinking, and, being centred, jumping, when
toggling Advanced. A measured pass beats remembering the expanded height (no one-frame resize
the first time the modal opens closed) and beats a fixed constant (follows font and language);
the space sits outside the card so it reads as empty body, not an oversized card. It only
changes the height when the right column is the taller one (it is, in the staged modal).
Not covered (can still resize it): the encoder-limit and bandwidth-overshoot banners (not
checked in the rig; the left column has ~90 px slack in the staged modal).

### Video

`VideoViews` (`cx.videos.show(key, ui, size, Fit::Contain)`) draws a GPU `VideoView`
(`client/src/decode/render.rs`): NV12 decoded frames are uploaded as two textures and
converted to RGB in a WGSL shader through an `egui_wgpu` paint callback. **Repaint is
frame-driven**: each view runs a small waker thread that requests a repaint whenever the
decoder publishes a frame, so screens never call `request_repaint()` for video. Presentation
mode defaults to **Mailbox** (uncapped, no vsync ceiling); because of that the shell caps
idle polling at 50 ms focused / 500 ms unfocused (`IDLE_REPAINT_INTERVAL`,
`UNFOCUSED_REPAINT_INTERVAL`). Measured: **240 fps presented on DP-1** (279.75 Hz).
Presentation mode can only be set at surface creation, so a change in Settings applies on
the next launch (`superdoc/features/presentation-modes.md`).

Pop-out: the view sits in an `Arc<Mutex<VideoView>>` so the deferred viewport closure
(`Send + Sync + 'static`) can paint the same view the tile would; while `popped` the stage leaves the
stream out. The pop-out has hover-gated chrome (statbar, audio pill; volume 0-500 % with a snap at 100 %, task #96, `features/audio-sharing.md`), F11
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

## Behavioural rules that carried over

These were decided in the earlier UIs and are still the rules.

- **There is no join gate.** Joining a room is one affordance in an always-present
  sidebar. *Why:* with N simultaneous rooms there is no moment at which the app is "not
  joined yet"; it is joined to some set of rooms, possibly empty, that changes while you
  use it.
- **Rooms list = the saved-room list.** Every saved room is a row; the ones with a live
  `RoomSession` get connected styling (left join, `RoomStore` -> sessions). *Why:* driving
  from the persistent side makes a disconnected saved room a row you can reconnect
  instead of something that vanishes when its connection drops. The two sides are joined
  on the **join string** (a `RoomHandle` is per-connection and a saved room has none), which
  is the UI's `RoomKey`. **It contains the room key: never render or log it**, show the
  room name.
- **Switching what is shown never starts or stops a share, a subscription, a decoder or a
  pop-out.** Starts and stops are explicit actions (`Watch`, `StageShare`, `GoLive`,
  `Unwatch`, ...); `VideoViews` is synced by the shell every frame regardless of `Nav.view`,
  so an off-screen room's streams keep decoding.
- **Leaving raises a confirm iff the room has an outgoing share or a live watch**
  (`Modal::LeaveConfirm`); otherwise it leaves immediately. *Why:* nothing to lose means a
  confirm is only a speed bump on the commonest case, but leaving a room you stream to is
  "killing an outgoing broadcast, not navigating", and a popped-out window vanishing
  unannounced is, if anything, more startling.
- **Display name is one global setting** (Settings modal, Profile card), not asked per
  room. *Why:* the wire protocol sends the name once at connect, so a per-room value was
  only ever the same string typed again.
- **"Choose your name" modal** (`Modal::ChooseName`, `MODAL_W`, `modals::choose_name`): while
  the user never saved a name (`AppState::display_name_set == false`, i.e. empty
  `AppSettings::display_name`; the shown name is then the `whoami_fallback()` OS username),
  `App::prompt_for_name` opens it once per run as soon as no other modal is open
  (`Shell::name_prompt_shown`). Field prefilled with the fallback and selected, Enter/Save
  = `SetDisplayName` + close (saving the prefilled name counts), Later/Escape/x = close until
  the next start; Save is disabled for an empty field. Saved once, it never shows again.
  Rig/launch clients are seeded with `display_name = "client-N"`, so scripted flows never see it.
  *Why:* user request 2026-10-15, encourage setting a name instead of silently using the
  default. Caveat: a room joined before the name is saved (auto-connect) keeps the old name
  for its peers until the next connect, the wire sends the name only once.
- **Server address validation (task #84):** the Create tab's server field and the server part of
  a pasted join link (checked once the first `|` is typed) must be `host:port` / `ip:port` /
  `[ipv6]:port`, port 1-65535 (`net::parse_server_addr`; one shared parser). While it is not,
  a red hint sits under the field ("Add the relay's port, e.g. example.org:4433", or the
  bad-port variant) and Create / Join is disabled (also Enter); an empty field shows no hint,
  only the disabled button. `AppState::create_room` rejects it too. A connect to a saved room
  with a portless server fails with "server address needs a port, e.g. host:4433"; a real DNS
  failure reads `could not resolve host "...": <resolver error>` (those two are English
  `anyhow` messages from `net.rs`, like the other connect errors; the dialog hints are i18n keys
  `ui.modal.server_no_port` / `server_bad_port`). *Why:* user report 2026-10-15, a relay
  address without its port (`vogelnest.any64.de`, relay on 41000) showed "could not resolve
  server address ...: invalid socket address", which read like DNS. Not silently defaulted to
  4433 because real relays use other ports and a default would become an opaque timeout.
  Browser twin: `docs/js/ui/app.js` (`serverHint`, `joinLinkHint`), `superdoc/features/website.md`.
- **Create/join modal:** server field pre-filled from the most recently saved room (the lib
  keeps no "last server" setting). A freshly minted link is shown masked (it carries the
  room key) and has no relay cert hash, so browsers can't use it; "Copy invite link" copies
  the *live connection's* link, which does, and which also carries the saved room name as the
  link's 5th field. Pasting a link with a name into the Join tab fills the Room name field
  (only while that field is empty or still holds the previous prefill; the user can edit it,
  and what is in the field on Join is what is saved; `apply` drops the 5th field from the
  stored join string). *Why:* user request 2026-10-15, so a friend does not have to retype
  the name. A room with no name is labelled from the first 4 bytes of
  its room id in hex (`session::room_label`), never from the join string.
- **Per-stream numbers are never invented**: sidebar fps = decoded fps while we watch a
  stream (the protocol's `StreamInfo` carries none), or the live destination's frame rate
  for our own share; empty otherwise.
- **Share-state drives the control bar's Share button and menu** (`ShareState`: Idle / Picking / Staged / Live /
  Paused). Starting a share opens the OS picker first (capture `Starting` = Picking); the
  stream-settings modal then opens by itself.
- **Stop frees the room at the click (2026-11-02, #98).** `AppState::stop_share` removes the
  capture's destinations from the model immediately (the capture stays in `Stopping` only to own
  the teardown), so `share_state` is `Idle`, the bar shows "Share screen" and the sidebar/tile
  stop claiming a live share while a slow driver teardown is still running. *Why:* the old
  behaviour (LIVE and "Stop sharing" for the whole teardown, up to ~15 s on a stuck VA branch)
  was reported by a user as "it just stays open". No separate "Stopping..." indicator: the
  teardown is invisible by design, and the info-level `capture stopped in N ms` log line is how
  a slow one is found. `features/stream-control.md` "Stop is instant".
- **Recovering (2026-11-02, #99).** While `AppState::recovery(capture)` is `Some`, the sharer's
  pill for that capture's destinations is *Recovering* (`PillKind::Progress`, in `BAR_PILLS` so
  the pill slot is wide enough in every language), the bar's title reads "Screen capture lost" /
  "Restarting the screen capture", and the room tile chip reads RECOVERING. The bar tooltip
  (`recovering_tip`) carries the attempt number ("Attempt 2 of 5"), the countdown to the next
  attempt and the last error. Only a capture that actually has a recovery shows it: a plain
  `Stalled` destination (a few seconds of silence that heals itself) keeps the *Stalled* pill.
  When the schedule is exhausted the destinations turn *Failed* with a share-error toast; there
  is no Retry button in the egui UI (`retry_destination` is lib-only), so the user stops sharing
  and shares again. The stream-settings modal's per-room rows still
  use `dest_pill` (Stalled / Failed), not the recovering override.

## Saved-room identity (2026-10-14)

One saved room per room. `RoomStore` (`client/src/rooms.rs`) identifies an entry by
`room_identity(join_string)`: server + room id + key, **without** the optional 4th cert-hash
field (parsed via `net::parse_join_string`, re-encoded canonically; an unparseable legacy
string falls back to its first three `|` fields). `upsert`, `get`, `name_for`, `rename`,
`remove` and `set_auto_connect` all go through it, so each touches exactly one entry.

`Why` not keyed by name: names are free local nicknames, so name-keying let one link joined
as "MovieNight" then "Films" become two entries (both shown connected, one leave dropping
both, forget removing only one). `Why` not the whole string: the relay's cert hash rotates;
a rotated cert must update the entry, not add a second one.

- `upsert(name, js)` on an existing room: non-blank `name` wins (blank keeps the old one),
  stored join string becomes `js` (newest form), `auto_connect` untouched.
- Migration: `RoomStore::load` calls `merge_duplicates` and writes the file back if
  anything merged. Per duplicate group: first entry's position, **last** non-blank name,
  last entry's join string, `auto_connect` OR-ed. `Why` last: the file has no timestamps and
  the old store appended on every new name / rename, so file order is the recency signal.
- UI `RoomKey` = the saved entry's stored (connectable) join string. It is stable across
  name-only upserts and re-saves of the identical string; it changes only when a different
  spelling of the same room (new cert hash) replaces it. `RoomStore::same_room(a, b)`
  compares two join strings by identity, for UI code matching a live session to a key.

## Native Playback: jitter buffer and catch-up sliders removed (2026-11-04, #125)

The native viewer has no playout pacing (frames are shown as decoded, `features/video-pipeline.md`), so the Settings >
Playback sliders "Jitter buffer" and "Catch-up speed" did nothing. **Why:** the user was told so and said "Remove them",
under their rule "If a setting doesn't do anything, hide it". Gone: both sliders, `UiAction::SetJitterBuffer` /
`SetCatchUpSpeed`, `AppState::set_max_jitter_buffer_ms` / `set_catch_up_speed_percent`, `AppSettings.max_jitter_buffer_ms`
/ `catch_up_speed_percent` with defaults and clamps, the locale keys. An old `settings.toml` with those keys still loads
(serde ignores unknown keys; there is no `deny_unknown_fields`, keep it so); the next save drops them. The browser keeps
its own working sliders (`docs/js/ui/`, `client.setJitterSettings`). Playback keeps Level loudness; the tab row is unchanged.

## Strings

All user-visible text goes through `ui::tr` / `tr_with` (English fallback inline; locales
in `client/locales/{en,de}.yml`, German in du-form; check with
`python3 -I client/locales/check_keys.py`). See `STYLING-GUIDE.md` > i18n.

## History (superseded designs; the reasons are kept above where they still apply)

- **First egui UI** (2026-07-25/26, `ui.rs` + a nav rail with Stream Control / View /
  Settings tabs, ROOMS and capped STREAMS sections in the sidebar). Superseded by the
  Electron frontend, then deleted. Its sidebar used nested panels instead of spacers (the
  idea survives in the shell above), a cross-room STREAMS list ("a destination is a thing
  I'm broadcasting right now; scoping it to the selected room would hide streams you still
  pay bandwidth for") and `active_room` as a single designated room driving the tabs. The
  new design replaces all three: sharing is room-scoped in the action bar, and "Sharing
  elsewhere" is reachable from the user band's Outgoing popover.
- **Electron + HTML frontend** (2026-07-26 to 2026-10, removed): see
  `FRONTEND-SHELL.md`. Its look (tokens, copy, behaviours) is the reference the egui UI
  copies; its structure is gone.
- The full text of the earlier versions of this page (nav rail, click semantics, STREAMS,
  Electron-era notes) is in git history: `git log -- superdoc/ui/APP-SHELL.md`; the last
  pre-rewrite content is at commit `3f2d5d3`.
