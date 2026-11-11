# Website: automatically watch new streams (row 176, 2026-11-11)

Settings > **Playback** > checkbox "Automatically watch new streams" (`#set-autowatch`, `settings.autoWatch`, **on by default**; a missing stored value is on, `store.js:loadSettings`). German
text in `ui/i18n.js` (`ui.settings.auto_watch`, `_tip`). Native counterpart: the parallel native row 175. Part of the browser client, see `superdoc/features/website.md`.

## Before this option

The browser never watched anything by itself except the **resume** of a watch that ended (`rooms.js:resumeWatches`: same `stableKey` within 60 s, keeps volume and mute). A stream
was listed as an idle tile and watched only on click (Watch / Listen). That resume is independent of this option and still runs first.

## Rule (`stage-state.js:autoWatchKeys`, called from the `streams` event in `rooms.js`)

On every stream-list change of a room, watch each stream that is **all** of: option on; room `joined`; video kind (webcams included: they are video streams); not my own (`self`); its key
was **not in the room's previous list** (`RoomEntry.knownStreams`); not watched already. Consequences:

- The list a (re)connect delivers arrives while the room is not `joined` and is only the baseline: streams already running when I join are listed, not watched (same baseline as the
  notification sounds). A reconnect is restored by the `stableKey` resume, never by this rule, so it works with the option off too.
- A watch I stopped by hand is not re-watched while that stream keeps running (its key is known). A sharer who stops and starts again is a new stream and is watched.
- Audio-only senders (Listen tiles) are not auto-listened: an `AudioContext` needs a gesture and a sudden sound is worse than a missing one.
- Off: streams are listed and watched on click, as before.

Why: user request 2026-11-11 ("automatically watch new streams ... on by default"), parity with native row 175. The join snapshot stays unwatched so entering a room never starts
decoding several streams at once (judgement call, not from the user).

## Tests

`dev/tests.js` "autoWatchKeys" (gating rule + default); `dev/e2e/auto-watch.mjs <relay host:port> <certhash>` drives the real UI (video only, synthetic capture): default viewer auto-watches,
a viewer with the option off lists only, a late joiner lists only, the Settings checkbox flips and persists, a manually stopped watch stays stopped, a restarted share is auto-watched.
