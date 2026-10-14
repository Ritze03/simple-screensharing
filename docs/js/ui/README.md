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
* **Why a room is unreachable.** First connect with a certificate hash that never worked in this page says "could not reach the
  relay, it may be offline or this invite link expired" (Chromium cannot tell those apart, see the core README). Once the hash
  has worked, a failing reconnect says "the relay is not answering, it may be down or restarting" and never blames the link.
  A full room on reconnect gets its own banner and the page keeps trying.
* **Room full** shows `JoinRejectedError{RoomFull}`, which now arrives properly on the control stream (relay task #29);
  `ConnectError{code: "join-no-reply"}` gets its own softer text.
* **Audio source.** Browsers decide which audio comes with the picked screen/window/tab, so the audio control offers mute
  only; changing the source means picking again.
* **Pop-out.** `documentPictureInPicture.requestWindow()` (Chromium 116+) is a real always-on-top window; the tile's canvas
  moves into it (no second decode). One such window per page, so popping out another stream brings the first back.
  Without it, the canvas moves into a fullscreen stage instead.
* **Join links in the URL.** `index.html#join=<link>` opens the join dialog prefilled.
* `window.__sss` exposes the state for debugging and local verification scripts.
