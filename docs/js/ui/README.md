# Browser client UI (`docs/js/ui`)

The website's UI: the same room-centric layout as the native egui app (approved mockup, round 3), on top of the browser
core in `../core` (see its README). Static: `docs/index.html` + `docs/css/` + these ES modules, no build step, no CDN,
all paths relative so GitHub Pages can serve `docs/` under `/simple-screensharing/`. Fonts are self-hosted in
`docs/assets/fonts/` (licences in `LICENSES/`).

| module | |
| --- | --- |
| `app.js` | entry point: render (sidebar, room header, tiles, share cards, People, action bar), modals (create/join, stream settings, settings, leave/forget, rename), popups, boot |
| `dom.js` | `h()` plus the layout primitives (`V`/`H`/`SL`/`FILL` = vbox/hbox/slot/fill) and the row patterns shared with the egui port (`ctrlRow`, `roomRow`, `sharerRow`, `personRow`, `destRow`, `popItem`, `btnRow`, `gridRows`, `modalShell`) |
| `store.js` | the UI state object `S`, `localStorage` (settings, saved rooms), coalesced `invalidate()`, toasts |
| `rooms.js` | one `RoomEntry` per saved room: its `RoomClient`, state, roster, streams, previews, watches; core events -> state |
| `capture.js` | outgoing streams: a capture (one `getDisplayMedia` pick) and its destinations (one `startShare` per room), bandwidth split, live edits |
| `popout.js` | pop-out viewer: Document Picture-in-Picture, fallback Fullscreen |
| `i18n.js` | en/de dictionary, `t(key, vars)` with the native `%{var}` placeholders |

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
  watch. The UI remembers ended watches for 60 s and re-watches when the same sharer's stream (name + label) reappears;
  after a relay restart the viewer is back on the stream within about a second.
* **Expired links.** A failed first connect whose link carries a certificate hash says "This link expired, ask for a fresh
  invite link". During auto-reconnect the core cannot tell a stale hash from a relay that is down, so the reconnect banner
  switches to the expiry wording only after 4 failed attempts.
* **Room full** shows the same text for `JoinRejectedError{RoomFull}` and for `ConnectError{code: "join-no-reply"}`.
* **Audio source.** Browsers decide which audio comes with the picked screen/window/tab, so the audio control offers mute
  only; changing the source means picking again.
* **Pop-out.** `documentPictureInPicture.requestWindow()` (Chromium 116+) is a real always-on-top window; the tile's canvas
  moves into it (no second decode). One such window per page, so popping out another stream brings the first back.
  Without it, the canvas moves into a fullscreen stage instead.
* **Join links in the URL.** `index.html#join=<link>` opens the join dialog prefilled.
* `window.__sss` exposes the state for debugging and local verification scripts.
