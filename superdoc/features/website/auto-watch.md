# Website: automatically watch new streams (row 176, 2026-11-11)

Settings > **Playback** > checkbox "Automatically watch new streams" (`#set-autowatch`, `settings.autoWatch`, **on by default**; a missing stored value is on, `store.js:loadSettings`). German
text in `ui/i18n.js` (`ui.settings.auto_watch`, `_tip`). Native counterpart: the parallel native row 175. Part of the browser client, see `superdoc/features/website.md`.

## Before this option

The browser never watched anything by itself except the **resume** of a watch that ended (`rooms.js:resumeWatches`: same `stableKey` within 60 s, keeps volume and mute). A stream
was listed as an idle tile and watched only on click (Watch / Listen). That resume is independent of this option and still runs first.

## Rule (`stage-state.js:autoWatchKeys`, called from the `streams` event in `rooms.js`; rewritten 2026-11-14, row 197)

Watching only happens **in the call** (`RoomEntry.inCall`, `features/website/pick-up.md`): a room that is connected but not in the call watches nothing and marks nothing as seen.

- **At pick-up** (`rooms.js:pickUp`, `stage-state.js:pickUpWatchKeys`): option on = every video stream of others already running is watched, screens and webcams, never your own, never
  audio-only. Option off = they are listed with Watch buttons. This replaced the old join rule (row 181: webcams of the first join watched, screens only listed), now one clause of history.
- **In the call**, on every stream-list change, watch each stream that is **all** of: option on; room `joined` and in the call; video kind (webcams included); not my own (`self`); its key
  was **not in the room's previous list** (`RoomEntry.knownStreams`, re-based at pick-up); not watched already.
- The list a (re)connect delivers is only the baseline. A reconnect is restored by the `stableKey` resume (in the call only), never by this rule, so it works with the option off too.
- A watch I stopped by hand is not re-watched while that stream keeps running (its key is known). A sharer who stops and starts again is a new stream and is watched.
- Audio-only senders (Listen tiles) are not auto-listened: an `AudioContext` needs a gesture and a sudden sound is worse than a missing one.
- Off: streams are listed and watched on click, as before.

Why: user request 2026-11-11 ("automatically watch new streams ... on by default"), parity with native row 175. Why pick-up watches everything (user, 2026-11-14): "press on this new button
to actually watch all of the screen shares if it is automatic for him"; the old webcams-only join rule was a judgement call to avoid surprise decoding, pick-up is now the explicit consent.
History (row 181, user 2026-11-11): "When autoconnect is on and someone connects the room it should automatically connect to all of the cameras."

## Tests

`dev/tests.js` "autoWatchKeys" and "pickUpWatchKeys" (screen and webcam yes, own and audio-only no, option off nothing); `dev/e2e/auto-watch.mjs <relay host:port> <certhash>` drives the real UI (video only,
synthetic capture): default viewer auto-watches, a viewer with the option off lists only, a joiner not yet in the call watches nothing and watches the running stream at pick-up, a late joiner with a webcam AND a screen
running watches both at pick-up, with the option off nothing, the Settings checkbox flips and persists, a manually stopped watch stays stopped, a restarted share is auto-watched. Pick-up itself: `dev/e2e/pickup.mjs`.
