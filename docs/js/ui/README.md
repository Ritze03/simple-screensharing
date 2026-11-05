# Browser client UI (`docs/js/ui`)

The website's UI: the same room-centric layout as the native egui app (approved mockup, round 3), on top of the browser
core in `../core` (see its README). Static: `docs/index.html` + `docs/css/` + these ES modules, no build step, no CDN,
all paths relative so GitHub Pages can serve `docs/` under `/simple-screensharing/`. Fonts are self-hosted in
`docs/assets/fonts/` (licences in `LICENSES/`).

| module | |
| --- | --- |
| `app.js` | entry point: render (sidebar, room header, the stage region or the scrolling body), modals (create/join, stream settings, settings (tabs Profile, Interface = Language + Sounds, Playback = jitter + catch-up (no Level loudness row: it is the bar button); `S.modal.tab`, kept in module `settingsTab` so it reopens on the last one; a fixed body height so the modal does not jump; no Display or Diagnostics tab, the browser has neither), leave/forget, rename, choose name), the stage pop-out window, boot |
| `stage.js` | the stage (task I6): tiles, the arena layout (Split / Crop / Floating), resize bars and drags, the people strip and its hover card, the viewers dropdown, fullscreen and its auto-hiding chrome, the document-level listeners (`bindWindow`) |
| `stage-state.js` | what a view shows (`collect`: watched video, idle, own share) and each view's layout state (`vg`: order, z, floating rects, grid), keyed by the core's `stableKey` |
| `stage-geom.js` | the pure stage maths, a port of native `stage/geom.rs` (grid, bars, `dragBar`, floating windows, `cardKeeps`, and since I10 the webcam-in-picture maths `layoutPip`, `pipAt`, `snapPip`, `movePip`, `resizePip`) with the same tests |
| `pip.js` | one webcam in picture (task I10): the element (persistent canvas / preview re-adopted), `placePip`, its pointer drag (move, resize, snap on release), the PIP inside a stream's fullscreen stage |
| `chrome.js` | the pure fullscreen auto-hide rule, a port of native `stage/chrome.rs` |
| `bar.js` | the control bar: split buttons (Share screen, Share audio, Webcam, Layout, Room settings), Level loudness (`#btn-level`), people, Leave, pop out, fullscreen |
| `menus.js` | every dropdown and context menu in one style: the bar's menus, the stream menu, the avatar picker, dropdown fields (`selectField`), "Also send to / Move to" |
| `actions.js` | the hooks `app.js` hands the stage modules (`A.render`, `A.openStreamModal`, ...): they cannot import it |
| `dom.js` | `h()` plus the layout primitives (`V`/`H`/`SL`/`FILL` = vbox/hbox/slot/fill) and the row patterns shared with the egui port (`ctrlRow`, `roomRow`, `sharerRow`, `destRow`, `btnRow`, `modalShell`), the avatar builder (`avatarEl`, `avatarBg`, `emojiInfo`, `pickEmoji`) |
| `store.js` | the UI state object `S` (incl. `S.stage` and `S.menu`), `localStorage` (settings incl. the emoji avatar, saved rooms), coalesced `invalidate()`, toasts |
| `rooms.js` | one `RoomEntry` per saved room: its `RoomClient`, state, roster, streams, previews, watches; core events -> state |
| `capture.js` | outgoing streams: a capture (`kind: "screen"` = one `getDisplayMedia` pick, `"webcam"` = `getUserMedia`, `"audio"` = an audio-only pick) and its destinations (one `startShare` per room), bandwidth split, live edits, the camera list / modes / settings (`S.settings.webcam`), the own-tile live preview element (`previewFor`, attached only while the page is visible and focused) |
| `popout.js` | pop-outs: a stream in Document Picture-in-Picture (fallback Fullscreen; the fullscreen stage is also the tile double-click; it survives a reconnect, `rebind`), and the whole stage in a PiP window (`popStage`) |
| `sounds.js` | notification sounds (task I9): the four generated sounds (WebAudio, the replica's definitions), the pure edge detector `RoomSounds` (a port of native `sounds::RoomSounds`: viewer-set compare, stream-key diff, quiet after a resume), the `Gate`, `SoundOut.play` (the one exit; replaced by the harness) |
| `i18n.js` | en/de dictionary, `t(key, vars)` with the native `%{var}` placeholders. German is the informal "du" form with the native terms (identical strings where key and English text match `client/locales/de.yml`) |

## Design notes (the why)

* **Full re-render, persistent media nodes.** `render()` rebuilds the DOM from `S` on every state change (a few hundred
  nodes). Each watch owns one `<canvas>` that the core's `CanvasRenderer` draws into; the new tree just re-adopts it, so
  video never restarts. Numbers that change every second (fps, kbps, viewers) and level meters are patched in place
  (`liveText` / `liveMeter`), so a re-render never interrupts a slider drag or a hover.
* **Fan-out is done in the UI.** The core shares one stream into one room per `startShare()`. A capture going to several
  rooms gets track clones and one `startShare` per room (own encoder, own key), like native ("each extra room is a separate
  encoded + encrypted stream").
* **Live edits.** Bitrate applies in place (`ShareHandle.setBitrate`). Framerate, codec, preset, encoding mode and keyframe interval cannot change on a
  running WebCodecs share, so the destination restarts (new share first, then the old one stops); the modal says so.
* **Resume after reconnects.** A reconnect gives the sharer a fresh ConnId and therefore a new stream key, which ends the
  watch. The core cannot tell that from a stop + restart, so the UI remembers ended watches for 60 s, keyed by the core's
  `stableKey` (room + sharer name + kind + label), and re-watches (same volume/mute) when the same one reappears; after a relay
  restart the viewer is back on the stream within about a second. The 60 s policy is UI-side on purpose (see the core README).
* **Retry now** calls the core's `reconnectNow()`: no backoff wait, nothing dropped (shares are re-announced, watches
  re-subscribed). It used to disconnect and rejoin, which dropped and restored shares by hand.
* **Why a room is unreachable.** First connect with a certificate hash that never worked in this page (neither pinned nor, as the retry, without the hash) says "could not reach the
  relay, it may be offline or this invite link may be outdated (a relay with a self-signed certificate changes it about every 11 days)" (Chromium cannot tell those apart, see the core README). Once the hash
  has worked, a failing reconnect says "the relay is not answering, it may be down or restarting" and never blames the link.
  A full room on reconnect gets its own banner and the page keeps trying.
* **Room full** shows `JoinRejectedError{RoomFull}`, which now arrives properly on the control stream (relay task #29);
  `ConnectError{code: "join-no-reply"}` gets its own softer text.
* **Audio source.** Browsers decide which audio comes with the picked screen/window/tab, so the audio control offers mute
  for that audio; changing the source means picking again. Since I10 the audio button is also an **audio-only share** of its own (D9: Desktop audio, groups and the
  exclusion list are native Linux only, a browser can't list apps): with no screen audio to mute, a click picks a tab or the system audio in the browser's picker
  (`capture.js` `startAudio`: `getDisplayMedia`, video track stopped, audio kept) and the next click stops it. An audio-only sender gets a **Listen tile**
  (`stage-state.js` `collect`, kind `audio`) only while its owner shares no screen (with a screen the audio rides along with the screen's watch).
* **A webcam is a capture like a screen** (`kind: "webcam"`, label exactly `Webcam`, `getUserMedia` with `audio: false`, Performance mode, keyframe 2000 ms, one at a time:
  a click while it runs in another room moves it). `stage-state.js` `collect` gives it an item (`cam: true`) next to the stream items; `pipPairs` (a port of native `pip_pairs`)
  pairs it with its owner's first playing screen share when the view has "Webcam in picture" on.
* **Pop-out.** `documentPictureInPicture.requestWindow()` (Chromium 116+) is a real always-on-top window; the tile's canvas
  moves into it (no second decode). One such window per page, so popping out another stream brings the first back.
  Without it, the canvas moves into a fullscreen stage instead. A tile double-click always uses that stage (`popOut(w, onBack, "fullscreen")`).
* **Join links in the URL.** `index.html#join=<link>` opens the join dialog prefilled (name field too, when the link has one).
* **Display name and room name in links (task #69, user requests 2026-10-15).** The default display name is `user-` + 4 random
  `[a-z0-9]` (`store.js` `autoName()`, own localStorage key `sss.autoname.v1`, so it is stable across reloads) while
  `settings.name` stays `""` until the user chooses one; `displayName()` is the chosen name or the auto one. `settings.nameSet`
  records that a name was chosen. While it is false, one "Choose your name" modal opens per page load as soon as no other
  modal is open (`app.js` `promptForName`): prefilled and selected, Save (Enter) / Later (Escape, x, backdrop), mirroring native
  `Modal::ChooseName`. Copy-invite (`RoomEntry.inviteLink()`) appends the room's saved name; a pasted link with a name prefills
  the join dialog's name field (`prefillName`, same rule as native: only when the field is empty or still holds the previous
  prefill); a saved room keeps its plain link (`stripLinkName`). The name is sent at connect, so rooms that auto-connected
  before the user saved keep the old name for their peers until the next reconnect (same as native; the settings note says so).
* `window.__sss` exposes the state for debugging and local verification scripts.

## Phones (row 145, 2026-11-05)

One breakpoint (`store.js` `PHONE_Q` = the last `@media` block of `css/app.css`): the rooms list is a drawer (`S.drawer`), touch gets 40 px targets and always-visible tile buttons,
Floating and the resize bars are hidden, long-press on a room opens its menu (`dom.js` `longPress`). Detail, the browser table and the *why*: `superdoc/features/website.md` "The website on a phone".

## The stage (task I6, 2026-11-04)

The room's centre and the Watched view are the native client's stage: Split / Crop / Floating, resize bars, a people strip with a hover card, a control bar with split
buttons, one menu style, stage pop-out, fullscreen auto-hide, emoji avatars. The detail, the D1 behaviours and the *why* of every call are in
`superdoc/features/website.md` "The stage" and "Emoji avatars"; the short rules for working on it:

* **Port, do not re-derive.** The geometry (`stage-geom.js`) and the fullscreen rule (`chrome.js`) are line-for-line ports of `client/src/ui/stage/{geom,chrome}.rs`; change
  both together and keep the ported tests (`dev/tests.js` "stage geometry", "fullscreen chrome") green. The CSS is the replica's (`superdoc/ui/replica/index.html`).
* **A drag must survive the re-render.** `render()` returns early while `isDragging()` (stage drags) or a slider is held; a drag updates element styles directly and calls
  `A.flush()` when it ends. Anything new that tracks the pointer across several events must do the same, or the full re-render replaces the element under the pointer.
* **Pointer-driven overlays decide by geometry, not by `mouseleave`.** Every render replaces both the trigger and the overlay, and a replaced element never gets its
  `mouseleave`: the hover card (`cardKeeps`) and the viewers dropdown test the pointer position on `pointermove` of the document.
* **Item ids are `room id + stableKey`** (your own share `<room>/me`), never the stream key: a reconnect gives the sharer a new ConnId and must keep the layout. Identical ones get `#1`, `#2` (`numberIdentical`).
* **A person is a connection:** whose an item is goes by `it.conn` (room + ConnId across rooms, `peopleOf`), never by `it.owner`, which is only the name to show. Two members may have one name.
* **Menus:** open one with `opener(mk, kind, extra)` (a control) or `openMenu`; add a body to `menus.js` `BODIES`. `S.menu.hostId` says which window draws it (main or the stage window).
* **No `<select>`.** A dropdown is `selectField`.
* **Webcam in picture state is per view, keyed by the webcam's item id** (`vg(key).pip[id] = { w, spot, x, y }`): the spot survives layouts, tile resizes and fullscreen.
  A PIP drag is its own pointer drag (`pip.js`, holds the render back like the stage's drags); a press on a PIP stops there so the tile under it is never dragged or resized.
* Dev flags: `?fakeaudio=1` makes every stream act as if its sender shared audio (ring, volume row, "Audio" in the card) without creating an audio stream; `window.__sss` also
  exposes `vg` and `stageWindow` for the harness.
