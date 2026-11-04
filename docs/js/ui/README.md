# Browser client UI (`docs/js/ui`)

The website's UI: the same room-centric layout as the native egui app (approved mockup, round 3), on top of the browser
core in `../core` (see its README). Static: `docs/index.html` + `docs/css/` + these ES modules, no build step, no CDN,
all paths relative so GitHub Pages can serve `docs/` under `/simple-screensharing/`. Fonts are self-hosted in
`docs/assets/fonts/` (licences in `LICENSES/`).

| module | |
| --- | --- |
| `app.js` | entry point: render (sidebar, room header, the stage region or the scrolling body), modals (create/join, stream settings, settings, leave/forget, rename, choose name), the stage pop-out window, boot |
| `stage.js` | the stage (task I6): tiles, the arena layout (Split / Crop / Floating), resize bars and drags, the people strip and its hover card, the viewers dropdown, fullscreen and its auto-hiding chrome, the document-level listeners (`bindWindow`) |
| `stage-state.js` | what a view shows (`collect`: watched video, idle, own share) and each view's layout state (`vg`: order, z, floating rects, grid), keyed by the core's `stableKey` |
| `stage-geom.js` | the pure stage maths, a port of native `stage/geom.rs` (grid, bars, `dragBar`, floating windows, `cardKeeps`) with the same tests |
| `chrome.js` | the pure fullscreen auto-hide rule, a port of native `stage/chrome.rs` |
| `bar.js` | the control bar: split buttons (Share screen, Share audio, Webcam, Layout, Room settings), people, Leave, pop out, fullscreen |
| `menus.js` | every dropdown and context menu in one style: the bar's menus, the stream menu, the avatar picker, dropdown fields (`selectField`), "Also send to / Move to" |
| `actions.js` | the hooks `app.js` hands the stage modules (`A.render`, `A.openStreamModal`, ...): they cannot import it |
| `dom.js` | `h()` plus the layout primitives (`V`/`H`/`SL`/`FILL` = vbox/hbox/slot/fill) and the row patterns shared with the egui port (`ctrlRow`, `roomRow`, `sharerRow`, `destRow`, `btnRow`, `modalShell`), the avatar builder (`avatarEl`, `avatarBg`, `emojiInfo`, `pickEmoji`) |
| `store.js` | the UI state object `S` (incl. `S.stage` and `S.menu`), `localStorage` (settings incl. the emoji avatar, saved rooms), coalesced `invalidate()`, toasts |
| `rooms.js` | one `RoomEntry` per saved room: its `RoomClient`, state, roster, streams, previews, watches; core events -> state |
| `capture.js` | outgoing streams: a capture (one `getDisplayMedia` pick) and its destinations (one `startShare` per room), bandwidth split, live edits, the own-tile live preview element (`previewFor`, attached only while the page is visible and focused) |
| `popout.js` | pop-outs: a stream in Document Picture-in-Picture (fallback Fullscreen; the fullscreen stage is also the tile double-click; it survives a reconnect, `rebind`), and the whole stage in a PiP window (`popStage`) |
| `i18n.js` | en/de dictionary, `t(key, vars)` with the native `%{var}` placeholders. German is the informal "du" form with the native terms (identical strings where key and English text match `client/locales/de.yml`) |

## Design notes (the why)

* **Full re-render, persistent media nodes.** `render()` rebuilds the DOM from `S` on every state change (a few hundred
  nodes). Each watch owns one `<canvas>` that the core's `CanvasRenderer` draws into; the new tree just re-adopts it, so
  video never restarts. Numbers that change every second (fps, kbps, viewers) and level meters are patched in place
  (`liveText` / `liveMeter`), so a re-render never interrupts a slider drag or a hover.
* **Fan-out is done in the UI.** The core shares one stream into one room per `startShare()`. A capture going to several
  rooms gets track clones and one `startShare` per room (own encoder, own key), like native ("each extra room is a separate
  encoded + encrypted stream").
* **Live edits.** Bitrate applies in place (`ShareHandle.setBitrate`). Framerate, codec and preset cannot change on a
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
  only; changing the source means picking again.
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
* **Item ids are `room id + stableKey`** (your own share `<room>/me`), never the stream key: a reconnect gives the sharer a new ConnId and must keep the layout.
* **Menus:** open one with `opener(mk, kind, extra)` (a control) or `openMenu`; add a body to `menus.js` `BODIES`. `S.menu.hostId` says which window draws it (main or the stage window).
* **No `<select>`.** A dropdown is `selectField`.
* Dev flags: `?fakeaudio=1` makes every stream act as if its sender shared audio (ring, volume row, "Audio" in the card) without creating an audio stream; `window.__sss` also
  exposes `vg` and `stageWindow` for the harness.
