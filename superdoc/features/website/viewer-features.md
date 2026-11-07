# Website: viewer features

Sounds, emoji avatars, decode-error clearing, double-click fullscreen, own-tile preview, who watches a share, rendering above 60 fps. Part of the browser client, see `superdoc/features/website.md`.

## Sounds (task I9, 2026-11-04)

Settings > **Sounds** has one volume (0-100, step 5, default 50, `settings.soundVol` in `localStorage`, applied live while the slider is dragged) and a Test button per
sound, like native. The four sounds (someone joins / leaves one of my streams, someone else starts / stops one), their edge rules and the *why* are on
`features/audio-sharing.md` "Notification sounds"; the browser code is `docs/js/ui/sounds.js` (`SOUNDS`, `soundPlan`, `RoomSounds`, `Gate`, `SoundOut`), a port of native
`client/src/sounds.rs` with the same tests in `dev/tests.js`. Rules specific to the browser:

- **An `AudioContext` is created only when a sound is really played**, never at boot or on a click, so a page that a script drives never touches the audio output (the dev
  harness replaces `SoundOut.play` and only looks at what would play). Volume 0 plays nothing and creates nothing.
- **An event sound before the page has had a click or key press is dropped** (`navigator.userActivation.hasBeenActive`): browsers block audio until then, and a sound
  that arrives late would be wrong. A Test button is a gesture, so it may resume a blocked context.
- A room that just resumed from a dropped connection is quiet for 15 s (`RESUME_QUIET_MS`: a restarted relay has everybody re-announce); the same sound is not
  started twice within a second (`MIN_GAP_MS`).

## Emoji avatars (task I6, 2026-11-04)

The wire format is `protocol-spec.md` §9.2 (`AvatarHello 0xE9`, `AvatarAck 0xEA`, `AvatarSet 0xEB`, `AvatarList 0xEC`). Core: `protocol.js` `packAvatarSet` / `unpackAvatarSet` /
`packAvatars` / `unpackAvatars` (the Rust test's vectors are in `dev/tests.js`), `RoomClient.#startAvatars` (Hello once a second, at most 10 times, until the Ack),
`#onAvatarDatagram` (tag `0xEC` is tested **before** the media path, like `ViewersList`: a list can be longer than 19 bytes; a truncated list, trailing bytes or a bad entry
is discarded whole; each list replaces the room's map; the UI hears about it only when the map changed, the relay repeats it every 3 s), `#syncAvatar` (after the Ack, re-send
AvatarSet every 250 ms until the relay's list shows my own ConnId with my emoji: that entry is the Set's only ack; a reconnect has a new ConnId and runs it again),
`setAvatar(emoji)`, `emojiOf(id)` (mine from my setting, so it shows at once and on an older relay). An older relay never acks: nobody else's emoji shows, the initial does.

UI: Settings > Profile > Avatar opens the picker (`menus.js` `emojiMenu`: the user's own 56 emoji (2026-11-05, three groups; `Why:` the user chose the list; an explicit array in `dom.js`, since spreading a string splits 🐻‍❄️), a "Paste any emoji" field that takes the last grapheme, "Use my initial"; a pick
closes the picker); the choice is `settings.emoji` in `localStorage` (`store.js`), announced to every room at connect and on change (`app.js` `setEmoji`). **Rendering** is
`dom.js` `avatarEl`, the one builder (strip, hover card, stream menu header, idle tile, viewers dropdown, sidebar user band): the background comes from the emoji's strongest hue
(`emojiInfo`: the emoji drawn on a 48 px canvas, 10-degree bins weighted by saturation, `null` for a colourless one like 💀 which gets a neutral slate), a thin black outline follows
its shape (four 1 px `drop-shadow`s), and **the emoji is centred on its balance point** (the same canvas pass reads the alpha map, `dom.js` `balancePoint`, a port of native `balance_point`: the midpoint of the solid pixels' (alpha >= 128) mass centre and the centre of their smallest enclosing circle, measured once per emoji and cached; `translate` moves the glyph by the difference to its em box; D1's "crop and centre"). *Why that and not the bounding box:* a box centre does not balance asymmetric shapes (a pizza looked off-centre); mass alone overshoots the moon and the guitar, the circle alone lifts the gamepad by 2 px, the midpoint suits all. *Why the replica's system-font approach and not native's embedded font:* a browser has system emoji fonts; native draws any one-grapheme emoji (<= 16 bytes, one bitmap glyph) from its embedded Noto font and shows the initial otherwise. Without an emoji the avatar is the name's gradient as before.

Checked 2026-11-04 (`docs/dev/e2e/stage.mjs`, headless Chromium, own relay): Alice picks 🦊, Vera sees it on her avatar over the datagrams, the orange background, the four-shadow
outline, 💀 on slate, a pasted 🐻, text refused with a hint, Use my initial, the setting in `localStorage`, the hover card carries it, it is announced again after a relay
restart. With `SSSHARE_AVATARS=0` (acts like an older relay) the owner sees her own, the other sees the initial. **Browser and native in one room** (headless rig,
`HS_EMOJI_1=🦊`, a headless Chromium joining the rig's room by its join link): the native client's 🦊 shows on its avatar in the browser, and the browser's 🐻 (picked in Settings)
shows on the native viewer's strip; native draws any emoji its embedded font has as one glyph, the browser any emoji its system fonts have.

## "This browser can't decode H.265" cleared on a codec switch (task #87, 2026-10-15)

User: switching a native share to H.265 shows the tile error when the browser viewer cannot decode it
("that's good"), but switching back to H.264 left the message. The tile's `w.error` was set on the core's
`unsupported` media event and nothing cleared it. `docs/js/ui/media-event.js` `applyMediaEvent` now clears it on
every `codec` event (a keyframe with a different codec string); `VideoReceiver` emits `codec` before the
async `isConfigSupported` verdict, so an unsupported new codec sets the error again right after. Unit test
`media events: ...` in `docs/dev/tests.js`. Verified in the rig: headless Chromium viewer against the rig's
relay, native sharer H.265 (error shown, `h265`, `firstFrame:false`) -> H.264 (error gone, 59 fps).
The native viewer has no such tile message, so nothing changed there.

## Double-click fullscreen and avatar colours (task #77, 2026-10-15)

Both user requests, mirrored from the native client (`superdoc/ui/APP-SHELL.md`, `superdoc/ui/STYLING-GUIDE.md`).

- **Double-click a watched tile = fullscreen** (`app.js` `doFullscreen`), double-click again or Esc = back. It was "pop out"
  before (the button still pops out). It reuses the pop-out's fullscreen *stage* (`popout.js` `popOut(w, onBack, "fullscreen")`):
  a `.pip-stage` div appended to `document.body` that holds the canvas, with the statbar and a bring-back button on hover; the
  tile shows the "is open in fullscreen" placeholder meanwhile. *Why a stage and not `tile.requestFullscreen()` (what the task
  suggested):* `render()` rebuilds the whole tree (`root.replaceChildren`) on every state change, and a fullscreen element removed
  from the DOM drops fullscreen, so a tile would have been kicked out of fullscreen by any toast, member join or stats refresh. The
  stage lives outside `root`; the e2e checks a re-render while fullscreen. Double-click on the stage calls
  `closePopout` (Esc leaves fullscreen natively and `fullscreenchange` brings the canvas back). The Document-PiP pop-out is
  unchanged (a PiP window cannot go fullscreen, so no dblclick there). Needs the click's user activation: `dblclick` is one.
- **Avatar colour from the name** (`dom.js` `avatarHue` / `avatarColors` / `avatarBg`): same algorithm and vectors as native,
  the contract is in `STYLING-GUIDE.md` "Avatar colour"; `tests.js` asserts the vector table and >= 3:1 contrast on all 360 hues.
  Since #83 (2026-10-15, user: keep the fade) it is a `linear-gradient(140deg, A, B)` of two name-derived stops, set inline.
  The old `.avatar.c1..c4` classes (connection-id gradients) are gone; the "you" avatars use the same function.
- E2E: `docs/dev/e2e/avatar-fullscreen.mjs <relay host:port> <certhash>` (two pages named `client-1` / `client-2`, real double-clicks
  through CDP `Page.dblclick`, avatar gradient stops (every `.avatar` on the page is a gradient), fullscreen survives a re-render, second double-click and `exitFullscreen()` return the
  canvas). Headless Chromium's Fullscreen API works (`document.fullscreenElement` is set).

## Own-tile live preview (task #78, 2026-10-15)

User request 2026-10-15: "For the sharer, it should show a stream preview, while he is tabbed into the program. Otherwise, its
fine to hide it." The sharer's own tile on the stage (`stage.js` `tileEl`, kind `mine`; before I6 the card in "Sharing in this room") holds a persistent muted `<video
class="live-preview">` (`capture.js` `previewFor`) playing `new MediaStream([c.video])` (video only, never audio; audio-only
captures get none). It is attached only while `document.visibilityState === "visible" && document.hasFocus()`; `visibilitychange`
/ `focus` / `blur` pause it and drop `srcObject`, and the element is `hidden`, so the placeholder shows and nothing is composited
(the track itself keeps feeding the encoder). *Why a persistent element:* `render()` rebuilds the tree on every state change, so
`tileEl()` re-inserts the same element and playback never restarts. A `PREVIEW` chip (`.pchip`, shown by CSS `:has()` while it plays) is among the tile's hover buttons; the "Your screen" placeholder hides while it plays. Native counterpart and shared rationale: `superdoc/features/self-preview.md`.
E2E: `docs/dev/e2e/live-preview.mjs <relay host:port> <certhash>` (headless Chromium: headless reports `document.hasFocus() ===
false` by default, so the run switches focus with CDP `Emulation.setFocusEmulationEnabled`, and hides the page with a second tab +
`Page.bringToFront`; checks attached/playing/video-only, survives `render()`, detached when unfocused or hidden, resumes).

## Who watches my share (task #116, 2026-11-03)

User request: the sharer sees who watches, as a viewer number with a hover dropdown of avatar and name. Same wire
feature as native (`protocol-spec.md` §9.1: `ViewersHello` datagram `0xE6`, relay `ViewersAck` `0xE7`, `ViewersList` `0xE8`).
Core: `room-client.js` `#startViewersHello` (once a second, at most 10, after every (re)join), `#onViewersDatagram` (a datagram
whose first byte is >= `0xE0` never goes down the media path), `ShareHandle.viewers()` (display names from the roster, sorted;
`null` while the relay has not acked, so the UI draws nothing on an older relay), a 7 s expiry per share. UI: `stage.js` `viewersChip`
on the own tile's hover buttons (`[PREVIEW][viewers][tune]`) and `viewersPopEl` for the dropdown (`.vpop` in `app.css`, the same
`.avatar.sm` as the people strip). *Why a body-level `.layer` popup and not CSS `:hover`:* the tile clips its content (rounded
corners) and a list of 7 rows is taller than the tile. The dropdown is rebuilt by every `render()` like the other popups, so it
follows live changes; its position is recomputed from the chip after each render, and it closes when the pointer leaves chip
and popup. Verified end to end by `docs/dev/e2e/viewers.mjs <relay host:port> <certhash> [--old]` (headless Chromium: Alice shares, Bob and
Zoë watch; chip 0 -> 2, dropdown rows and avatars, pointer into the dropdown keeps it open, a viewer leaving updates it live, 0
viewers no dropdown; `--old` against a pre-#116 relay asserts that no chip is drawn). The dropdown closes through a document-level
`mousemove` check, not `mouseleave` on the elements, because every `render()` replaces both and a replaced element never gets its
`mouseleave`.

## Rendering above 60 fps (task #67, 2026-10-15)

User report: in the browser viewer (a Firefox-based browser, Gecko, on Linux) "anything above 60 fps hangs and skips, up to 60 is
flawless", H.264 and AV1 alike, while the native client takes 240 fps fine.

- **Root cause: the 2D canvas, not the network or the decoder.** `drawImage(VideoFrame)` into a 2D canvas is a software path in
  Firefox: it reads the hardware-decoded surface back to the CPU and converts it on the **main thread**. Measured in headed
  Firefox 157 inside the rig (VAAPI decode): **13 ms per 1440p frame** (7 ms headless). That alone is 80 % of the main thread at
  60 draws/s, and the `VideoDecoder` output/`decode()` callbacks run on that same thread, so decoding collapsed to the draw
  rate and the decode-queue backstop (`jitter.js`) then dropped whole GOPs to the next keyframe: stalls and skips. A monitor
  faster than 60 Hz makes it worse (more draws per second).
- **Fix 1, `js/core/renderer.js`:** `CanvasRenderer` draws through **WebGL2** (`texImage2D(videoFrame)` + one triangle: 0.2 ms
  per 1440p frame in the same browser, no CPU read-back) when a first-frame timing on scratch canvases says it is clearly
  cheaper than 2D (otherwise 2D, ties go to 2D). It also spaces draws so drawing never takes more than about half of the main
  thread (`minIntervalMs`), so a slow backend lowers the *picture* rate instead of starving decode.
  *Why measure instead of "always WebGL":* where WebGL is itself software (Chromium headless / SwiftShader, VMs) it is slower
  than 2D (18 ms vs 5 ms at 1440p) and made things worse in the first version. *Why the choice is made before touching the real
  canvas:* a canvas that got a `webgl2` context can never get a `2d` one.
- **Fix 2, `VideoReceiver` (Gecko only):** `optimizeForLatency: true` makes Firefox's software decoder single-threaded
  (3.5-5x slower: 1080p 336 vs 1188 fps, 1440p 206 vs 957, 4K 95 vs 440; Chromium no difference). The first time the decode-queue
  backstop has to drop frames, the receiver re-configures the decoder with `optimizeForLatency: false` (at the keyframe the
  backstop skipped to) and stays there; event `decoder-mode`, stat `throughputMode`. It is not the default because frame
  threading adds a few frames of latency.
- **Numbers** (headed Firefox 157 in the rig, VAAPI, `docs/dev/xbench.mjs --mode pipeline`: recorded H.264 chunks through the real
  `VideoReceiver` + `CanvasRenderer` at a fixed feed rate; window on a 60 Hz output): decoded fps before -> after, 1080p@120
  119 -> 119.5, 1080p@240 239 -> 239 (2D was still cheap enough), **1440p@120 67 -> 119.5, 1440p@240 69 -> 239, 4K@120 42 -> 119.4**;
  catch-up drops 519/1699/772 -> 0. Software decode, headless 4K@144 (no drawing): 92 -> 138.5 fps with fix 2.
- **Not verified:** a real monitor above 60 Hz (the rig's windows sit on 60 Hz outputs, so `drawn` stayed ~60; draws now cost
  ~0.2 ms so 280 draws/s is cheap, but it was not run), the Document PiP pop-out (a unit test moves the canvas into an iframe
  document and keeps drawing; PiP itself needs a real window), H.265 (Firefox has no HEVC decoder here), hardware decode on
  the user's GPU.
