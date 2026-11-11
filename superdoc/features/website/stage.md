# Website: the stage

The browser stage (tiles, control bar, layouts, chrome), its fullscreen rule and what its tests cover. Part of the browser client, see `superdoc/features/website.md`.

## Tile fullscreen never errors (row 152, 2026-11-07)

The user, testing on an iPhone in Chrome (WebKit): "fullscreening a screenshare just results in an error." **Cause (inferred from the code, not reproduced on a real iPhone):** `popOut(..., "fullscreen")` called
`stage.requestFullscreen()` and, when it was missing (`document.fullscreenEnabled` false made it throw `ui.err.no_popout`) or rejected, `doFullscreen` showed the red "Pop-out failed: ..." toast.
**Fix (`docs/js/ui/popout.js` `popOut`, `fallback`):** the tile's fullscreen is always the body-level `.pip-stage` (already `position: fixed; inset: 0`); the browser's fullscreen is requested on top, and when
the API is missing or rejects the stage stays as a plain full-viewport layer (`.css-fs`: `100dvh`, exit button always visible because touch has no `:hover`, padded by `env(safe-area-inset-*)`, Escape and double-click
leave, survives `render()`). The tile button (`stage.js`) is no longer hidden without `document.fullscreenEnabled`. The bar's stage fullscreen (`toggleStageFullscreen`) keeps its stage-only layout when
`requestFullscreen` rejects (it used to snap back silently, so on an iPhone the button seemed dead).
Why: the user wanted a working fullscreen or a useful full-size view, never an error. Test: `e2e/phone.mjs` (API removed, API rejecting, at 390x844, 844x390, 360x640; simulated by overriding
`Element.prototype.requestFullscreen`) and the real desktop fullscreen in `e2e/stage.mjs`.

## The stage (task I6, 2026-11-04)

The browser client got the stage of the native client (`superdoc/ui/STAGE.md` "The stage", design approved in the replica
`superdoc/ui/replica/index.html`, `ui/REPLICA.md` "HTML replica"). It replaces the room's "On screen" tiles and "Sharing in this room" cards, the
People list on the right, the bottom action bar and the old popups, and the Watched view. Classic views were not built. A joined room and the whole
"Watched streams" view are one black stage with the streams, the people strip and the control bar; a room that is not joined (connecting,
reconnecting, not connected) keeps the scrolling body with its banner and a `⋯` in the header for the Room settings menu (native
`room::shows_stage`).

| module | |
| --- | --- |
| `stage.js` | the stage: tiles (watched video, idle "X is sharing Y", own share), arena layout, resize bars and drags, the people strip and hover card, the viewers dropdown, fullscreen + its auto-hiding chrome, the window listeners |
| `stage-state.js` | what a view shows (`collect`) and the per-view layout (`vg`: order, z, floating rects, grid; UI-private, per session, not persisted), keyed by `stableKey`. The Layout switches (layout Split / Crop / Float per view, Webcam in picture per view, Hide own, Always show names, Show stats) are different: they live in `S.stage` and are saved (`store.js` `persistStage` into `settings.stage`, keyed by saved room) |
| `stage-geom.js` | the pure maths, a line-for-line port of native `stage/geom.rs` (`bestRows`, `place`, `dragBar`, `fitFloat`, `resizeFloat`, `cardKeeps`), same tests |
| `chrome.js` | the pure fullscreen auto-hide rule, a port of native `stage/chrome.rs`, same tests |
| `bar.js` | the control bar (split buttons, groups, Leave, pop out, fullscreen) |
| `menus.js` | every dropdown and context menu: Share, Audio, Layout, Room, the stream menu, the avatar picker, "Also send to / Move to", and the dropdown *field* (`selectField`) |
| `actions.js` | the hooks app.js hands the stage modules (they cannot import it: app.js imports them) |

**Layouts** (per view; each room and Watched keep their own): **Split** every picture whole in a grid (the row count that shows the most picture area,
rows filled evenly from the top), **Crop** the same cells filled (the picture is `max(100cqw, 100cqh * ar)` and the tile clips it), **Floating**
one window per stream. A drag on a resize bar, a floating window or a reorder updates styles directly and holds `render()` back until the pointer is up
(`isDragging`; a slider being dragged does the same, `sliderHeld` in app.js): the full re-render would replace the element under the pointer.
Positions are applied after the new tree is inserted (`afterRender` -> `layoutArena`), the video `<canvas>` of a watch is re-adopted by the new tree as before.
A 400 ms poll re-lays-out an arena when a stream's picture changes shape, so a floating window keeps its stream's aspect ratio without black bars.

**Crop pan (row 178, 2026-11-11).** In Crop, **Shift + left-drag** on a watched tile's picture pans it inside its cell (the picture follows the pointer 1:1; the cursor is `grab` while Shift is held over a
cropped tile, `grabbing` during the drag). The offset is per stream, `vg(view).pan[itemId] = { x, y }` (0..1 like CSS `object-position`, 0.5 centred; item ids are `stableKey`-based, so it survives a
reconnect; UI-private and not persisted, like grid sizes and floating rects). `stage-state.js` `panPos(start, delta, overflow)` is the pure maths (position falls by `delta / overflow`, clamped to 0..1 so the
picture always covers the cell; an axis with overflow < 1 px stays centred, so a 16:9 picture in a tall cell only pans sideways). `layoutArena` writes `--px`/`--py` on the tile and `.arena.crop .st-tile .vid`
places the picture with `left/top: p*100%` + `translate(-p*100%)` (CSS only inside Crop; Split and Floating ignore the stored offset and show it again when Crop returns); percentages mean a resize keeps the
framing and stays clamped. `tilePointerDown` starts a `drag.kind = "pan"` (pointer capture on the tile) only when Shift is down, the layout is Crop and the tile is a live picture; buttons and the webcam in
picture return earlier, so PIP keeps priority under the pointer, and Shift-drag starts no tile reorder. A plain drag, click and double-click (fullscreen) are unchanged. "Reset sizes and positions" clears
`pan` too. Fullscreen: the stage's fullscreen is the same tiles, so pan works there; a tile's own fullscreen / pop-out shows the whole picture (`object-fit: contain`), nothing to pan. The Crop menu's
sub-line says "Shift+drag to pan". *Why (user request 2026-11-11, parity with native row 177):* "For the cropping option, it would be really nice, if i could hold Shift+LeftMouseButton, to move around the
image in the cropped area (Essentially an offset)." Tests: `dev/tests.js` `panPos`, `dev/e2e/crop-pan.mjs` (real Shift+drag over CDP: 1:1, clamp, y axis fixed, plain drag, resize, Split, Reset).

**Behaviour that follows native, not the replica** (the native code carries the fixes found in testing; D1 in the plan):
- *Resize bars:* with the pointer between streams, including the black space around a picture, every bar shows dark grey (CSS
  `.arena:hover:not(:has(.st-tile:hover))`); over a stream they hide; the bar under the pointer is blue and stays blue for the whole drag while the others hide
  (`.gut.on`, `.arena.dragging`). A bar sits midway between the two *visible* pictures (`place`), and a dragged bar stays under the pointer (`dragBar` bisects
  the split, 24 steps, limit 12 % of the pair). Double-click a bar: even grid.
- *Floating:* `fitFloat` re-applies the stream's aspect every layout (width wins); the corner handle moves the corner to the nearest point of the diagonal
  (`resizeFloat`), so moving only left or only up shrinks too; press raises.
- *Tile overlays:* buttons and stats only while hovered; the name tag too unless "Always show names". The stats pill reserves `9999x9999`, `999 fps` and
  `99.9 Mbps` slots (`.tag .v`), so it is as wide as its content and never jitters; a narrow tile drops the resolution, then the rate (`fitTags`, native `stats_fields`).
  "Hide my own streams" removes your screen from your stage only.
"Hide when unfocused" (on by default, row 174) controls whether your own screen's live preview stops on focus loss;
your webcam preview never does. *Why (user request 2026-11-11, parity with native row 173):* tabbing out should not
blank a preview you want to keep watching, and a webcam should never vanish. Details: `features/self-preview.md`.
- *People strip and hover card:* ring, avatar, LIVE badge and name share one centre line (the badge is positioned from the avatar's size, no transform); the ring is one
  2 px circle border 4 px outside the avatar (`::before { inset: -4px }`), even all round. Everyone who shares gets the same card, one row per stream with Audio / No
  audio, numbers and state. **The card is kept open by pointer geometry** (`cardKeeps`: the card, the avatar and the gap straight above it, 3 px grown), not by the
  replica's `::after` bridge under the card: a card is wider than a person's cell, so a bridge or the union of card and avatar covered the neighbours' avatars
  and cards stacked (the native bug of D1). A popped-out stream is off the stage, shows only on its owner's avatar (dashed outline + badge) and comes back from the
  avatar or the card's "Bring back".
- *Dropdowns:* every dropdown is the app's `.menu` (icons, sub-lines, a lit current row), no native `<select>` anywhere; a dropdown field opens under itself, as wide as it.
  Escape with a menu open closes the menu alone (stage.js handles the key in the capture phase and stops it, so the modal under a dropdown stays). A menu closes on any
  click outside it, and a second click on the control that opened it closes it too (`data-mk` pairs menu and opener). *Why click and not mousedown:* closing on mousedown
  re-rendered between press and release, so the release never made a click on the button under the pointer.
- *Fullscreen:* the bar's button makes `#app` fullscreen (not the stage: `render()` rebuilds the stage, and a fullscreen element removed from the DOM drops fullscreen,
  `#app` persists) and renders only the stage; the strip and bar lie over the streams (`.stagefs`, the arena keeps the whole window minus 8 px, so streams don't jump
  when they fade). They hide after 3 s without movement while the pointer is on the stage and at once when it leaves the window (`mouseleave` on `<html>`); movement, a
  press, the wheel or a key bring them back; an open menu, modal or drag holds them up and stops the timer, and when it ends they get a full 3 s (`chrome.js`, tested like
  native). The hover buttons and the cursor go with them (`.chrome-off`, `!important` because `.st-tile:hover .hover-ui` is more specific). In the browser "the middle part"
  of native is the whole window, since the stage is the window.
- *Streams don't jump* and *a tall bar swallows no clicks:* in fullscreen the chrome has `z-index: 30` over the bars' 20.

**The stream menu** (`⋯` or a right click on a tile): header, Volume (mute, slider 0-500 % acting **during the drag**, scrolling on it 5 % a notch up to 200 %, 10 % above,
`volumeWheel`), Pop out, Fullscreen, Stop watching (no stats row); an unwatched tile: Watch stream / Pop out; your own
tile: Stream settings / Stop sharing. **Share button** (2026-11-05): idle, its main part opens the browser's picker (`getDisplayMedia`) and, once a source is picked, goes live at once with the remembered `settings.stream` (`app.js` `startStage(r, true)` -> `goLive`; no modal; cancelling the picker leaves nothing staged); the Share menu's start entry ("Share your screen…") still opens the modal after the pick (`startStage(r)`). Every edit of a live screen share (`capture.js` `editDest`) is stored in `settings.stream` too, not only what Go live sent (webcam edits are not). *Why:* user, 2026-11-05, "it should just open the picker and once something is selected, it should automatically start the screenshare, without opening the full settings. When it is started from the context menu, it should open up the full settings of it. It should also save the most recent settings". **Share menu** (the Share button's arrow): state header, FPS and Preset (for the live destination, the staged stream or the next one),
Change source, All stream settings, Go live / Stop sharing; when your share is live in another room, Add here / Move here stay reachable. **Stream settings modal**: Source (with
**Change** while setting up and while live: `capture.js` `changeSource` picks again and restarts every destination on the new tracks, the new share first, like a live FPS
change), Compression (Codec, Bitrate), Video (Preset Text / Quality / Gaming, FPS dropdown, **Encoding** Performance | Quality, **Keyframe** 250 / 500 / 1000 / 2000 ms, and the caption `fps · mode · keyframe`, never "(custom)"). A preset sets the FPS,
the encoding mode and the keyframe interval at once (`presetPatch`); changing the FPS by hand keeps the encoder mode and keyframe but unlights the preset (2026-11-05: a preset is lit only when FPS, encoding and keyframe all equal its row; the Share menu lists Preset and its caption above FPS). *Why (user, 2026-11-05):* "In the settings, of the stream, it should only show a preset, if the settings match it perfectly. Also, FPS should be below the preset line." (a screenshot of 120 fps + Quality + 250 ms with Text lit). Encoding and Keyframe are stored next to the
preset as `latencyMode` ("realtime" = Performance, "quality") and `gopMs` (`ui/store.js` `encoderOf`, `presetOf`), travel with every destination (`capture.js` `encCfg`) and go to `startShare`
as explicit `latencyMode` / `gopMs` (which override the preset's); editing one by hand sets the other two from `presetOf(fps, latencyMode, gopMs)`; the lit button is always derived (`menus.js` `litPreset`), never the stored `preset`, so a stale one does not relight. A combination that matches no preset leaves **no preset lit**
(`preset: ""`). A live edit restarts the destination like a preset does, and the modal's note says so ("Changing framerate, codec, preset, encoding or keyframe restarts the stream"). An older stored
config with only a `preset` loads with that preset's values. *Why:* the user: "The settings for the keyframe and the Performance/Quality setting should still be shown (right now only accessible
through the presets Text/Quality/Gaming itself)." **Show stats** is one Layout-menu checkbox for every watched stream (`S.stage.showStats`, saved, off by default, hover-only like before); the user:
""Show stats on the stream" should be inside of the "LAYOUT" dropdown and not per individual stream."
`store.js` loads a stored `balanced` / `low_latency` as `quality`.

**Pop-outs.** A stream's pop-out is unchanged (Document PiP, fallback fullscreen) but **survives a reconnect or a relay restart**: `endWatch` keeps the window open while the 60 s
resume entry lives, `resumeWatches` moves the new watch's canvas into it (`popout.js` `rebind`), and after 60 s without the stream the window closes. *Why keep it open and
not re-open it:* a Document PiP window needs a user gesture and a reconnect has none. **The whole stage** can pop out too (`popStage`, the bar's button; Document PiP only, so
the button is hidden without it): the same `renderStage` tree is rendered into the PiP window's document (`renderPop`), the main window shows "The stage is open in its own
window" + Bring it back, and the window stays open while its room is suspended (a note meanwhile) and goes home when the room is left. Its bar has no sidebar and no fullscreen
button (a PiP window cannot go fullscreen) and its pop-out button brings the stage back. A modal opened from it (Rename, Stream settings) appears in the main window.

**Right click on a sidebar room row** (2026-11-05): `app.js` `oncontextmenu` on the `roomRow` (`dom.js`) calls `openMenu("room", row, { roomId, pos })` after `preventDefault()`: the Room settings menu (`menus.js` `roomMenu`, reads `S.menu.roomId`) opens at the pointer for that room, whichever is open, also while disconnected; `stage.mjs` covers it. *Why:* user, "on desktop, make sure that right clicking a room in the left view opens the same context menu, as the bottom button bar has".

**Control bar.** Share screen, Share audio, Webcam, Layout and Room settings are split buttons (the Level loudness button, `#btn-level`, sits left of the people button in the second group) with an always-visible arrow cut off by a separator line (inset 1 px
`box-shadow`); grouped buttons sit on a backdrop (`--stage-btn`), the freestanding ones (hide sidebar, Copy invite link, pop out, fullscreen) have none; Leave is 64x50,
as tall as a group (4 px padding + 40 px button + 1 px border each side); the people button shows and hides the strip. Narrow stages degrade by container queries on `.stg`
(label first, then two rows). **Share audio**: while a screen share has audio riding along with the picked screen, the button is the mute switch (enabled only when the
source has audio) and the arrow opens the same one-source menu with its meter; otherwise a click starts an audio-only share (see "Webcam, audio-only shares and webcam in picture", `docs/js/ui/bar.js`). *Where I9 and I10 plug in:* `bar.js` builds the three split buttons of the first group
(`shareBtn`, `audioBtn`, `camBtn`); since I10 `audioBtn` and `camBtn` are live and `menus.js` `BODIES` has `audio` and `cam` bodies, see "Webcam, audio-only shares and webcam in
picture (task I10)" below.

## What the stage tests cover (D1, task I6)

`docs/dev/e2e/stage.mjs` drives the real UI with CDP pointer input (details and results: `docs/dev/README.md` "Stage end to end"): four pages (three sharers, one viewer who shares
too), 138 checks with the relay restarts: control bar (arrows, separators, backdrops, Leave height, no people list), bars (hidden over a stream, grey between, blue under the
pointer and during the whole drag, midway, 60 px = 60 px, the 12 % limit, even grid, horizontal, reorder), Crop, Floating (ratio, move, corner resize left/up/down-right, edge,
raise), overlays (hover, stats slots without jitter, narrow tile, Always show names, Hide my own streams), strip and card (centre line, ring, same card for all, keep zone, no
stacking, popped-out badge, Bring back), volume (during the drag, the wheel steps), menus (no `<select>`, dropdown width and place, Escape), presets and captions, Change source,
Watched, fullscreen auto-hide (3 s, leave, key, menu hold + full 3 s, a bar button over a resize bar), emoji (above), pop-out survives a relay restart, the stage window.

**What differs from native, and why:** the stage pop-out needs Document Picture-in-Picture; the
fullscreen is the page's, so "leaves the middle part" is leaving the window; modals are the page's (Rename, Stream settings appear over the stage, not in a viewport); the
emoji picker takes any emoji; `Escape` in fullscreen is the browser's: Chromium leaves fullscreen itself (the page hears `fullscreenchange`, not the key), so with a menu open in fullscreen the first Escape may leave fullscreen instead of closing the menu (not tested; the e2e closes the menu with a synthetic Escape key).
