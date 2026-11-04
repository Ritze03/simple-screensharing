# Website (browser client)

The browser peer of the native app: a static site in `docs/` that joins a room, watches streams and
shares a screen. This page is the map and the "why"; the three READMEs below are the detail and are
kept next to the code, so read them instead of re-deriving anything:

- `docs/js/core/README.md`: the API (`RoomClient`, `WatchHandle`, `ShareHandle`), join links, relay
  close codes, encoder presets, transport notes. **Read before touching `docs/js/core/`.**
- `docs/js/ui/README.md`: UI modules and the design notes (the why). **Read before touching
  `docs/js/ui/`.**
- `docs/dev/README.md`: the test/measurement harness and the recorded results.

Wire spec (binding for both clients): `superdoc/architecture/protocol-spec.md`, section 11 is the
browser leg. How to test safely on this machine (headless vs headed inside the headless sway rig, relay):
`superdoc/claude-instructions/testing-and-verification.md` "Website (browser viewer) checks".

## Shape

Static GitHub-Pages site, **no build step, no bundler or package-manager dependencies, no CDN** (fonts are self-hosted
in `docs/assets/fonts/`). Plain ES modules with relative imports; all paths relative so `docs/` can be
served under `/simple-screensharing/`.

```text
docs/index.html, docs/css/, docs/assets/   the page
docs/js/core/    transport + crypto + codecs, no UI    (README.md)
docs/js/ui/      the room-centric UI on top of core    (README.md)
docs/dev/        harness, tests, scenarios, e2e        (README.md)  not linked from the site
```

`docs/js/core/` (one `RoomClient` = one room = one WebTransport connection):

- **Transport:** WebTransport to the relay, ALPN `h3`, path `/ssshare`
  (`WEBTRANSPORT_PATH` in `protocol.js`). A self-signed relay is trusted by **cert-hash pinning**:
  the join link's fourth field (`certhash`) goes into `serverCertificateHashes` (`session.js`);
  omit it for a CA certificate (a failed pinned attempt is retried once without the hash, so a link
  carrying a hash also works against a CA relay). See `superdoc/meta/TERMINOLOGY.md` ("Join link", "Cert hash").
- **Protocol:** `postcard.js` + `protocol.js` are a hand port of `protocol/src/lib.rs`; correctness
  is pinned by cross-language vectors (`docs/dev/vectors.json`, Rust to JS and JS to Rust).
- **Crypto:** WebCrypto AES-256-GCM. `NonceSequence` is **one u32 counter per connection shared by
  video, audio and previews** (same scheme as native), never wraps; at exhaustion the room client
  reconnects with a fresh ConnId.
- **Fragmentation:** encrypt once, then split to **at most 1000-byte datagrams** including the
  header (`MAX_MEDIA_DATAGRAM_LEN`). `Why 1000:` Chromium's `datagrams.maxDatagramSize` is 1024; the
  session refuses to join below 1000.
- **Media:** WebCodecs encode/decode (H.264, AV1; H.265 decode only), Opus via WebCodecs +
  WebAudio, an adaptive jitter buffer ported from the former Electron frontend's `tile-video.js` (removed in the egui rework), the 30 s JPEG room
  previews, and the same bitstream-based codec sniffing as native (`codec.js`).
- **Identity across reconnects:** every stream info carries a **`stableKey`** (room + sharer name +
  kind + label); a reconnect gives the sharer a fresh ConnId and so a new stream key, and the core
  cannot tell that from stop + restart. Resume policy is therefore UI-side (60 s list in
  `docs/js/ui/rooms.js`, same window as the native `REWATCH_GRACE`).
- **A person is a connection** (2026-11-04, #127; native twin in `ui/APP-SHELL.md` "The stage"). A stage item carries `conn` (its
  owner's ConnId) and `peopleOf`, `sameOwner`, `audibleFor`, the hover card (`S.card.person`, `data-person` = room + ConnId; `data-user`
  stays the name, for tests only), the emoji (`r.emojiOf(id)`, `emojiByName` is gone) and the viewers dropdown (`viewersOf`, from
  `share.viewerIds`) go by it, never by name. `stableKey` stays what it is, so two same-named sharers of one label have one
  `stableKey`: `numberIdentical` gives their item ids `#1`, `#2` in stream key order. After a reconnect the two may swap numbers, places
  and resumed watches; nothing is lost. *Why:* the user, running two clients under one name: "Both are
  technically me, but that should be properly handled!".
- **`reconnectNow()`** ("Retry now"): skips the backoff and cancels a stuck handshake; while
  connected it re-joins. **Link-expiry vs unreachable heuristics:** Chromium cannot say why a
  handshake failed, so only a hash that has *never* worked in this page (pinned or, after the retry
  without the hash, plain) yields the hedged "offline or invite link outdated" message; once it has worked, a failing reconnect says "relay not answering"
  and never blames the link.

`docs/js/ui/`: `app.js` (render, sidebar, modals), the stage (`stage.js`, `stage-state.js`, `stage-geom.js`, `bar.js`, `menus.js`, `chrome.js`,
see "The stage" below), `dom.js` (`V`/`H`/`SL`/`FILL` = the same vbox/hbox/slot/fill
layout model as native, see `superdoc/meta/TERMINOLOGY.md`, plus shared row patterns and the avatar builder), `store.js`, `rooms.js`
(one `RoomEntry` per saved room), `capture.js` (captures and destinations), `popout.js`, `i18n.js`
(en/de, native `%{var}` placeholders; German strings match `client/locales/de.yml`). The UI is a full
re-render from state with persistent media nodes (the `<canvas>` per watch survives re-renders).

## What the browser can and cannot do

Watch and share both work. Differences from native, all platform limits:

- **No H.265 encode** (Chromium here has no encoder for it); H.265 *decode* only with a hardware
  decoder, i.e. a **headed** run with VAAPI. Headless Chromium has no software HEVC decoder.
- **No GOP / quality-level / CQP knobs** (WebCodecs has none): GOP is emulated by forced keyframes
  every `gopMs`; the preset table is in `docs/js/core/README.md`. **The share modal hides what does
  nothing in the browser** (user rule 2026-11-01, #93: "If a setting doesn't do anything, hide it (Yes,
  even when its just a browser limitation). Just hide it, so other platforms can still use it."): no
  Quality level, Tune or Keyframe interval rows (and no "Advanced tuning" disclosure, nothing is left in
  it; since I2/I6 the presets are Text, Quality and Gaming like native, a stored `balanced` or `low_latency` loads as Quality), no greyed-out codecs (an unencodable codec has no button, the Codec row
  goes when at most one is left). Hidden only in the UI: the core's `ENCODER_PRESETS`, the persisted
  settings and the native app keep the setting. Details and the "why": `ui/APP-SHELL.md` (2026-11-01).
- **Viewer volume is 0-500 %** (task #96, 2026-11-02): a WebAudio `GainNode` (`core/audio.js` `MAX_VOLUME = 5`), the slider in the stream menu (since I6; before
  that on the tile's audio pill) with a tick and a snap at 100 % (`ui/dom.js` `snapVolumePct`, `.vol` in `app.css`). Loud audio hard-clips at the device, no
  limiter. The mouse wheel over the slider steps it (`volumeWheel`, `wheelStepVolumePct`, #113). Watched audio is levelled before the slider (`core/leveller.js`, Settings > Playback > Level loudness, default on, #119); since I6 the stream menu's "Level loudness" switch flips the same global `levelLoudness` setting as the Settings checkbox (the tile's audio pill with its `graphic_eq` button, #126, is gone: the pill moved into the menu, like native). Details and the why: `features/audio-sharing.md` (2026-11-02, 2026-11-03). In Firefox the tick only shows on the unfilled part of the track.
- **No audio source choice:** only what `getDisplayMedia({audio})` returns (a tab's audio, or the system's where the browser offers it); the audio control is
  mute plus an audio-only share (I10, below). Native has the per-application picker, Desktop audio, source groups and the exclusion list (D9: *"native Linux only: a
  browser can't list apps"*).
- **Capture is damage-driven:** a static screen delivers no frames; the core re-encodes the newest
  frame when a viewer needs a keyframe.
- **Fan-out uses one encoder per room** (no shared encode): a capture sent to N rooms gets track
  clones and N `startShare()` calls with their own encoder and key, matching native ("each extra
  room is a separate encoded + encrypted stream"). Done in the UI, not the core.
- **Pop-out is Document Picture-in-Picture** (`documentPictureInPicture.requestWindow()`, Chromium
  116+): the tile's canvas moves into the PiP window (no second decode); one window per page; falls
  back to a fullscreen stage (a tile double-click uses that stage too, see "Double-click fullscreen and avatar colours"). Native pop-outs are real viewports (`viewer-window.md`).
- **Room full is signalled with the `JoinRejected` frame, not a close code.** Chromium hides every
  WebTransport application close code (a `CONNECTION_CLOSE` surfaces as `WebTransportError:
  Connection lost.`, no `closeCode`), so the relay writes `JoinRejected{RoomFull|Malformed}` on the
  control stream first and `connect()` rejects with `JoinRejectedError`. The UI shows "This room is
  full (8 people at most)".

## The stage (task I6, 2026-11-04)

The browser client got the stage of the native client (`superdoc/ui/APP-SHELL.md` "The stage", design approved in the replica
`superdoc/ui/replica/index.html`, `ui/STYLING-GUIDE.md` "HTML replica"). It replaces the room's "On screen" tiles and "Sharing in this room" cards, the
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
`volumeWheel`), Level loudness (the global setting), Pop out, Fullscreen, Stop watching (no stats row); an unwatched tile: Watch stream / Pop out; your own
tile: Stream settings / Stop sharing. **Share menu** (the Share button's arrow): state header, FPS and Preset (for the live destination, the staged stream or the next one),
Change source, All stream settings, Go live / Stop sharing; when your share is live in another room, Add here / Move here stay reachable. **Stream settings modal**: Source (with
**Change** while setting up and while live: `capture.js` `changeSource` picks again and restarts every destination on the new tracks, the new share first, like a live FPS
change), Compression (Codec, Bitrate), Video (FPS dropdown, Preset Text / Quality / Gaming, **Encoding** Performance | Quality, **Keyframe** 250 / 500 / 1000 / 2000 ms, and the caption `fps · mode · keyframe`, never "(custom)"). A preset sets the FPS,
the encoding mode and the keyframe interval at once (`presetPatch`); changing the FPS by hand keeps the encoder mode and keyframe. Encoding and Keyframe are stored next to the
preset as `latencyMode` ("realtime" = Performance, "quality") and `gopMs` (`ui/store.js` `encoderOf`, `presetOf`), travel with every destination (`capture.js` `encCfg`) and go to `startShare`
as explicit `latencyMode` / `gopMs` (which override the preset's); editing one by hand sets the other two from `presetOf(latencyMode, gopMs)`, so a combination that matches no preset leaves **no preset lit**
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

**Control bar.** Share screen, Share audio, Webcam, Layout and Room settings are split buttons with an always-visible arrow cut off by a separator line (inset 1 px
`box-shadow`); grouped buttons sit on a backdrop (`--stage-btn`), the freestanding ones (hide sidebar, Copy invite link, pop out, fullscreen) have none; Leave is 64x50,
as tall as a group (4 px padding + 40 px button + 1 px border each side); the people button shows and hides the strip. Narrow stages degrade by container queries on `.stg`
(label first, then two rows). **Share audio keeps today's behaviour**: audio rides along with the picked screen, so the button is the mute switch (enabled only when the
source has audio) and the arrow opens the same one-source menu with its meter. *Where I9 and I10 plug in:* `bar.js` builds the three split buttons of the first group
(`shareBtn`, `audioBtn`, `camBtn`); since I10 `audioBtn` and `camBtn` are live and `menus.js` `BODIES` has `audio` and `cam` bodies, see "Webcam, audio-only shares and webcam in
picture (task I10)" below.

## Webcam, audio-only shares and webcam in picture (task I10, 2026-11-04)

The browser caught up with native I7 (audio-only shares), I8 (webcam, webcam in picture) on the stage of I6. Decisions D1, D7, D9, D10 are the plan's; the native side is
`superdoc/ui/APP-SHELL.md` "Webcam and webcam in picture" and `features/audio-sharing.md` (I7).

**Listening to audio-only senders.** A native client can share audio with no screen. `stage-state.js` `collect` adds a `kind: "audio"` item for every audio stream whose owner
shares **no screen** (`RoomEntry.ownerHasVideo`: a webcam is not a screen; native `owner_has_video`) and a Listen tile for it: avatar, `X is sharing audio`, `Audio only`,
`Listen` / `Stop listening` (`ui.room.listen`, native keys and German). Listen is `watchStream` of the audio key: the same watch object with `audioOnly: true` and no canvas,
`client.watch(audioKey)` (the core always could), so volume, mute and Level loudness work through the stream menu. With a screen share the audio rides along with the screen's
watch and has no tile, as before; a webcam watch never subscribes audio (`WatchHandle.followAudio` is false for a `Webcam*` label), so a sender with only a webcam and audio has
that audio as an audio-only tile. "Watched streams" lists pictures only (audio watches are left out of `allWatches`). The label `Desktop audio` arrives in English on the wire and
is shown through `share.desktop_audio_label` (en "Desktop audio", de "Desktop-Audio"; `stage-state.js` `shareLabel`, native `share_label`, which also translates `Webcam`).

**Sharing audio without a screen.** D9: *"Desktop audio, source groups and the exclusion list are native Linux only: a browser can't list apps"*, so a browser's audio-only share is a
`getDisplayMedia({ video, audio })` pick (tab audio, or system audio where the browser offers it) of which **the audio track is kept and the video track stopped**
(`capture.js` `startAudio`, `kind: "audio"`; Chromium refuses `video: false`). The core's `startShare` takes an explicit `audioOnly: true` (it no longer throws "no video track"
for that case; it still does for a normal share): one audio stream is announced, `videoStreamId` and `sender` are null. The Share audio split button: with a screen share whose
audio came along it stays the mute switch; **otherwise a click starts an audio-only share and the next click stops it** (native: Desktop audio on / off); its arrow menu has "Share audio only…", the
mute switch(es), the meter, "Stop sharing audio" and a note that the source is the browser's choice. One audio share per room, no tile of its own on your stage (the button is the indicator,
green; the Outgoing popover lists it, `ui.sidebar.outgoing.audio_sub`), like native. *Why a click on a screen that came with no audio starts an audio-only share too:* the mute switch would
have nothing to mute, and the old dead button said "share again and tick the browser's option". *Not done:* a viewer list for audio-only shares (the relay's lists are per video stream).

**The webcam** (D7: *"A webcam is an ordinary `StreamKind::Video` stream whose label starts with `Webcam`"*, D10: *"the browser uses `getUserMedia`"*). `capture.js` has a third capture kind
`webcam`: the label is exactly `Webcam` (`WEBCAM_LABEL`), `getUserMedia({ video, audio: false })` (a sender with a webcam and audio shares that audio as an audio-only share), encoder fixed
to Performance with a keyframe every 2000 ms (`startShare({ gopMs: 2000, latencyMode: "realtime" })`, D8), one at a time like native (a click while it runs in another room **moves** it). The
bar's Webcam button starts it with the saved camera in its saved mode, the next click stops it; **its arrow only offers the camera and "All webcam settings…"** (D1), cameras from
`enumerateDevices` (no names until the camera permission exists: `Camera 1`…; the list is refreshed once a camera was opened and on `devicechange`). **The modal** (`app.js` `renderWebcamModal`, the
native fields): Camera, Mode, Codec (only when more than one encodes), Bitrate 200-12000 kbps (default 2500; `Set by upload budget` in Absolute bandwidth mode), the note `Encoder: Performance ·
Keyframe 2000 ms`, footer `[Start | Stop webcam][fill][Done]`; everything acts at once (camera and mode restart a running webcam, new share first; codec restarts the destination; bitrate is applied in place).
The settings are `settings.webcam` `{ cameraId, mode: "WxH@fps", codec, kbps }` in `localStorage`. *Modes:* a browser has no list of what a camera delivers, so the list is the camera's maximum
(`getCapabilities()` of a track that was open: a running webcam, or a brief probe when the modal opens for a camera whose permission exists, never a permission prompt out of nowhere) and the common
sizes (1920x1080, 1280x720, 960x540, 640x480, 320x240) that fit, at 30 fps (and 60 at 1280 and up where the camera reports it); the sizes are `ideal` constraints, the announced size is what the
track really delivers. Your webcam is an item of your stage (`<room>/me/cam`, tile `You · Webcam`, settings button, `⋯` menu Webcam settings… / Stop webcam). Someone else's is an idle tile and, when
watched, a tile whose tag has the `videocam` icon and whose menu says it has no audio.

**Webcam in picture** (Layout menu checkbox, per view, saved, on by default: `S.stage.pipOff` holds the views where it is switched off; native I8). Port, line for line like `stage-geom.js`: `layoutPip`, `pipAt`, `nearestSpot`, `snapPip`, `movePip`, `resizePip` of
`client/src/ui/stage/geom.rs` with the same tests (`dev/tests.js` "pip geometry"), the pairing `pipPairs` of `stage/mod.rs` (`dev/tests.js` "pip pairing"), `pip.js` for `stage/pip.rs`. Inset 12, minimum width 72,
default `{ w: .24, spot: [2, 2] }`; the size is a fraction of the picture's width and a spot is column and row 0..2 without (1,1) (corners and side middles). A drag moves it free, clamped to the visible
picture (also in Crop), and on release it **snaps to the nearest spot** and glides there (.16 s, not under `prefers-reduced-motion`); the handle (20 px) is on the corner facing the middle of the picture and resizes with
the aspect locked and the opposite corner fixed (a resize keeps the spot). A press on the PIP stops there: the tile under it is never dragged, resized or raised by it (a floating parent is raised, as native).
Right click: the webcam's own stream menu; double click: its fullscreen. **Pairing:** the parent is the webcam owner's first screen share, in the view's order, that is **playing on this stage** (yours, or one you watch, not
popped out; same room; yours pairs with yours), the webcam must also be playing; otherwise it stays a tile (a webcam you cannot see playing in a picture would have no Watch button). One parent holds one webcam (the
first; native's map keeps the last, its doc comment says the second keeps its tile, the tests agree on the count only). A parent in a pop-out window shows alone there and the webcam is a tile; a parent in the
**fullscreen** of its stream counts as playing and takes its webcam into the fullscreen (`pip.js` `fullscreenPip`, `popout.js` `popOut(..., decorate)`, native `fullscreen_pip`). The position is per view and per webcam
(`vg(key).pip[itemId]`, item id = room + `stableKey`), so it stays on its spot across layouts, tile resizes, the stage's fullscreen and the stream's fullscreen; "Reset sizes and positions" forgets it. The webcam's
`<canvas>` (or, for yours, the capture's preview `<video>`) is persistent and **re-adopted** by whichever element draws it, so it never restarts when it moves between its tile and the PIP. **Hide my own streams**
removes your screen and, with it, your in-picture webcam from your stage (a webcam you share while hiding is hidden too); others still see both. The strip's hover card says `In picture` (`ui.strip.in_picture`) on a
webcam inside a picture. Differences from native: rounded corners and the glide (the browser can; native cannot clip a video callback); `Escape` and the browser's own fullscreen as before.

**Checked 2026-11-04.** Host (headless Chromium, `--use-fake-device-for-media-stream`, own relay, never any audio): `dev/tests.html` 119 checks (the PIP geometry and pairing tests are ports of the Rust ones; no
shared vector file, the numbers are the same); `e2e/stage.mjs` 190 checks (the webcam, all 8 spots with real drags, Crop, Floating, both fullscreens, Hide my own streams, pop-out, ...); `e2e/native-webcam.mjs` in the headless rig
(`SSS_FAKE_CAMERA=1`): a native webcam shows in the browser and sits in the native screen's picture, the browser's webcam shows on the native viewer and sits in the browser's screen there (native `pipinfo` spot=2,2). Audio, in the
arch-ssd VM only (`e2e/audio-only.mjs`, a headless Chromium 156 there, a native audio-only sender built from this tree, the emulated Floorp tone source, a fake hardware sink): a **native audio-only sender is heard by a browser** (tile, Listen,
107 packets decoded in about 4 s, the meter sees the tone, Stop listening, the tile goes when the sender stops), a **browser audio-only share** (the real `#btn-audio` click, video track stopped) **is heard by a second browser and by a
native viewer** (`owner_has_video=false`, 350-400 packets), and a webcam-plus-audio sender gets a webcam tile and an audio tile with no audio subscribed by the webcam watch. *Not tested:* a real `getDisplayMedia` audio-only pick (the harness
stubs the picker, and Chromium's own refusal of `video: false` is documented, not exercised), Firefox (no `getDisplayMedia` encode path, no MediaStreamTrackProcessor), a real USB camera (the fake device delivers 1280x720 at 20 fps and reports
no capability list worth a mode menu), a real GPU, Windows.

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

UI: Settings > Profile > Avatar opens the picker (`menus.js` `emojiMenu`: the replica's 40 emoji, a "Paste any emoji" field that takes the last grapheme, "Use my initial"; a pick
closes the picker); the choice is `settings.emoji` in `localStorage` (`store.js`), announced to every room at connect and on change (`app.js` `setEmoji`). **Rendering** is
`dom.js` `avatarEl`, the one builder (strip, hover card, stream menu header, idle tile, viewers dropdown, sidebar user band): the background comes from the emoji's strongest hue
(`emojiInfo`: the emoji drawn on a 48 px canvas, 10-degree bins weighted by saturation, `null` for a colourless one like 💀 which gets a neutral slate), a thin black outline follows
its shape (four 1 px `drop-shadow`s), and **the emoji is centred on its balance point** (the same canvas pass reads the alpha map, `dom.js` `balancePoint`, a port of native `balance_point`: the midpoint of the solid pixels' (alpha >= 128) mass centre and the centre of their smallest enclosing circle, measured once per emoji and cached; `translate` moves the glyph by the difference to its em box; D1's "crop and centre"). *Why that and not the bounding box:* a box centre does not balance asymmetric shapes (a pizza looked off-centre); mass alone overshoots the moon and the guitar, the circle alone lifts the gamepad by 2 px, the midpoint suits all. *Why the replica's system-font approach and not native's embedded font:* a browser has system emoji fonts; native draws any one-grapheme emoji (<= 16 bytes, one bitmap glyph) from its embedded Noto font and shows the initial otherwise. Without an emoji the avatar is the name's gradient as before.

Checked 2026-11-04 (`docs/dev/e2e/stage.mjs`, headless Chromium, own relay): Alice picks 🦊, Vera sees it on her avatar over the datagrams, the orange background, the four-shadow
outline, 💀 on slate, a pasted 🐻, text refused with a hint, Use my initial, the setting in `localStorage`, the hover card carries it, it is announced again after a relay
restart. With `SSSHARE_AVATARS=0` (acts like an older relay) the owner sees her own, the other sees the initial. **Browser and native in one room** (headless rig,
`HS_EMOJI_1=🦊`, a headless Chromium joining the rig's room by its join link): the native client's 🦊 shows on its avatar in the browser, and the browser's 🐻 (picked in Settings)
shows on the native viewer's strip; native draws any emoji its embedded font has as one glyph, the browser any emoji its system fonts have.

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

## Display name and room name in join links (task #69, 2026-10-15)

Three user requests, the browser mirrors native tasks #68 and #70 so both clients behave the same:

- **Default display name `user-xxxx`.** It used to be "friend" (`ui.default_name`, gone). Now `user-` + 4 random `[a-z0-9]`
  (`crypto.getRandomValues`, rejection-sampled), generated once and persisted under its own localStorage key
  (`sss.autoname.v1`, `store.js` `autoName()`), so it is stable across reloads. `settings.name` stays `""` while the name is
  automatic, which is how the UI tells "auto default" from "chosen" (native keeps `display_name == ""` as unset the same way);
  `displayName()` = chosen name or the auto one, used for the connect and for "you". *Why separate from the settings name:*
  the "Choose your name" modal has to know whether the user ever chose, and an auto name stored as the name would hide that.
- **"Choose your name" modal** (`app.js` `promptForName`, strings `ui.modal.choose_name_title` / `_body`, en + de from
  `client/locales`): on page load, while `settings.nameSet` is false, one small modal (the default 520 px width) opens as soon as no
  other modal is open (a `#join=` dialog at load goes first, like native "first time nothing is open"). The field is prefilled with the
  current default, focused and selected. **Save** (primary, Enter) trims and stores it (saving the untouched default counts, it
  becomes the chosen name), disabled while blank; **Later** (also Escape, x, backdrop) only closes it until the next load. `nameSet`
  turns true on Save and on any non-blank name typed in Settings, and is never cleared again (emptying the settings field later does
  not bring the modal back); profiles that already have a name never see it. *Why:* user request 2026-10-15, make people pick a
  name instead of silently being "friend"/an OS username. *Caveat (same as native, not fixed):* the name is sent at connect, so
  rooms that auto-connect before the user saves keep the old name in their peers' rosters until the next reconnect.
- **Room name in the join link** (5th field, `protocol-spec.md` §11.4, `docs/js/core/joinlink.js`): every copy-invite path
  (`RoomEntry.inviteLink()`: room header button, room menu, the Create dialog's minted link) appends the room's saved name (not the
  "Room abcd1234" placeholder); the join dialog prefills its name field from a pasted link with the native rule (`prefillName`: only when
  the field is empty or still holds the previous prefill, so a typed name is never overwritten and clearing the field re-enables it);
  saved rooms keep the plain 3/4-field link in localStorage (`stripLinkName`; the name is a local, editable label, not identity).
  *Why:* user request 2026-10-15 (copy link carries the name, pasting fills it). The encoder escapes `!'()*` on top of
  `encodeURIComponent` so a link is byte-identical whichever client made it.

- **Server address needs a port** (task #84, user report 2026-10-15; `protocol-spec.md` §11.4): `parseServerAddr`
  (`docs/js/core/joinlink.js`, twin of native `parse_server_addr`) rejects a bare host (a browser would quietly use 443 for
  `https://host/`). The create/join dialog shows an inline hint (`#add-hint`, updated on input without a re-render so focus stays)
  and disables Create/Join; `createRoom` throws, `RelaySession.open` throws `ConnectError` code `server-no-port` / `server-bad-port`
  (so a saved portless room shows `ui.err.server_no_port`). A DNS failure cannot be told apart in the browser (WebTransport reports
  a generic handshake failure). Checked with `docs/dev/e2e/server-port.mjs` (no relay needed).

Verified through the real UI with `docs/dev/e2e/name-modal.mjs` (headless Chromium, throwaway relay) and by the unit vectors
in `docs/dev/tests.js`.

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

User report: in the browser viewer (Floorp 157 = Firefox/Gecko on Linux) "anything above 60 fps hangs and skips, up to 60 is
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

## Big keyframes in the browser sharer (task #103, 2026-11-03)

The native sharer's #102 (`features/stream-control.md` "Big keyframes") applies here too: a frame is lost if any
fragment is, and a 1 MB IDR is ~1000 datagrams. `docs/js/core/fragment.js` `pacePlan` is a line-for-line port of
native `pace_plan` (same constants and tests: keyframe window `max(min(60 ms, 4 frame periods), bytes / (2.5 x
configured bitrate))`, deltas paced only above 2 frame periods, cap 600 ms, chunks every 2 ms); `RoomClient#sendMedia`
sends a frame's datagrams chunk by chunk and `st.pacing` is set meanwhile.

- **Why the browser had a worse failure than the native one:** a 1000-datagram IDR written at once pushed
  `pendingDatagrams` over `MAX_PENDING_DATAGRAMS` (400); `#sendMedia` then dropped the next frames, and every drop
  called `forceKeyframe()`: *another* ~1 MB IDR, again in one burst. A self-sustaining IDR storm (measured below), not just
  random loss. The pending limit is only checked *before* a frame, so an accepted frame was never truncated by it; the loss
  was the storm.
- **Input frames are dropped while a paced frame goes out** (`VideoSender` option `isBackedUp`, set from `st.pacing`):
  like native's capture dropping frames while the pump is held. Dropping *before* encode breaks no reference chain; letting
  the encoded frames pile into the 8-frame tx queue would drop encoded ones and force yet another IDR.
- **Chunks are due by the clock, not by tick count** (`due = floor(elapsed / 2 ms) + 1`): a hidden tab's timers are
  throttled to ~1 s, and a plain "sleep 2 ms per chunk" would stretch a frame over minutes; this sends what is due in one go
  instead (the old behaviour, never worse). Inside a paced frame the loop also waits (never skips a fragment) while more than
  `MAX_PENDING_DATAGRAMS` are queued, so the browser's own datagram queue cannot age out the tail of an IDR.
- **Stop stays prompt:** the loop checks `sess === this.session && this.tx.get(streamId) === st && !sess.closedFlag`
  every tick (<= 2 ms); `stopShare` deletes the tx entry, so a paced frame ends at once.
- `share.stats().bigKeyframes` (last 20 keyframes of >= 24 datagrams: `{datagrams, bytes, spreadMs}`) is the diagnostic,
  like native's `sending a keyframe of N datagrams ... paced over ~Y ms` log line.

Measured (headless Chromium 153 sharer -> real relay -> browser viewer, 1080p, `live.html?detail=2` = a static colour-noise
texture, so a ~1 MB IDR and small deltas like the field case; Chromium software H.264, 8000 kbps, 60 fps, 8 s + 6 s):

| | keyframes encoded | IDR datagrams / spread | tx drops | sent | viewer |
|---|---|---|---|---|---|
| before | 499 | 1078 / 1 ms (burst) | 501 | 345 Mbit/s | 12 fps |
| after | 8 (the 2 s GOP) | 1075 / 360 ms median, 484 max | 0 | 12.6 Mbit/s | 49 fps (p50 latency 720 ms vs 270 ms: the 0.4 s IDR delay, then the jitter buffer grows) |

Not verified: loss on a real path (the loopback relay never drops; the win shown is the end of the storm and the burst
size, not survival odds), a hidden-tab sender, other browsers (Chromium only can share).

## Selective retransmission in the browser (NACK, task #111, 2026-11-03)

The browser half of #104 (design and the why of every choice: `architecture/protocol-spec.md` 6.6, `features/video-pipeline.md`
"Selective retransmission"; API and constants: `docs/js/core/README.md`). Why it matters more here than on native: before it,
a browser viewer **never recovered** from random loss once the sharer's IDRs were ~1 MB (1000 datagrams): every keyframe it
asked for died the same way. Measured below, a browser viewer at 0.1 % loss decoded 0 fps for 30 s.

- **Pieces.** `docs/js/core/fragment.js` `Reassembler` got a NACK mode (`setNack`, `ingest`, `poll`, `nackMiss`; the legacy mode
  and its `insert()` are unchanged), `docs/js/core/nack.js` has the sender's `SendRing`, and `RoomClient` wires them
  (`#startHello`, `#onNackDatagram`, `#nackPoll`). A NACK-family datagram (< 19 bytes, tag >= 0xE0) is intercepted *before*
  `unpackMediaHeader`, which would throw on it.
- **Capability.** Hello (`0xE0`) once per second, at most 10 times, until the relay's 1-byte HelloAck; an old relay never
  answers and the client carries on as before (verified, table below). Hello is sent whether we view or share: it means both
  "I NACK" and "ask me".
- **Viewer.** The same rules as native with the same constants (2 later fragments or 20 ms quiet, retry `clamp(2 rtt + 60,
  100, 300)` ms, 3 asks, > 128 holes abandoned, 1 s hard limit, later frames held in order, 256 finished frame ids remembered).
  `rtt` is `WebTransport.getStats().smoothedRtt` (read from the 1 s stats timer) where the browser has it, else 150 ms,
  i.e. a 300 ms retry interval. **Chromium 153 here has no `WebTransport.getStats()` at all** (`getStats is not a function`;
  `client.nackStats().rttReported` says which case applies), so every measurement below ran with the 150 ms fallback. It only
  stretches the *retries* (a lost request or answer waits 300 ms instead of ~100); the first request goes out at once.
  A repair that is given up calls `onDrop` -> the existing `loss` marker -> `need-keyframe` -> resubscribe, so L1-style
  recovery is unchanged underneath. The 5 ms poll timer runs only while a frame is incomplete.
- **Sender.** `SendRing` keeps the last 2 s / 8 MiB of video datagrams per stream (references to the arrays that were sent,
  recorded in `#sendMedia` right after `fragmentFrame`, before pacing; audio is never recorded). A Request whose owner
  `conn_id` is ours is answered with copies carrying `flags |= 0x04`, sent **directly** (not through the #103 pacing loop: a
  repair is a handful of datagrams, and holding it behind a 400 ms paced IDR would make it useless) and skipped when
  `MAX_PENDING_DATAGRAMS` are already queued. Pacing of the original frames is untouched.
  Checked: the browser does not detach or reuse a datagram array after `writer.write`, so the ring can hold references.
- **Duplicates.** The legacy `Reassembler` already ignored fragments at or below a per-stream `floor` (frames can only finish
  in order there). In NACK mode frames finish out of order (a frame is held while an older one is repaired), so the floor
  cannot be used; the last 256 finished/abandoned ids are remembered instead (`DoneIds`). That is the
  "ghost buffer for a duplicate" fix: a retransmit that races the original is ignored, and a retransmit of an abandoned frame
  never opens a new incomplete frame that would later be reported as loss.
- **Kill switch** (like native `SSS_DISABLE_NACK=1`): `?nack=0` in the page URL, `localStorage["sss-nack"]="0"`, or
  `new RoomClient({ nack: false })`. The dev harness pages (`live.html`, ...) read the URL, so `&nack=0` on one peer's URL gives
  an A/B or a "browser that does not speak NACK".
- **Stats.** `client.nackStats()` (also `nack` in a video `watchStats`): `requests, fragmentsRequested, fragmentsRecovered,
  framesRecovered, framesAbandoned, active (HelloAck seen), served {requests, resent, missed}`; `live.html` puts them (and a
  frozen-time measure: gaps > 150 ms between shown frames) in its result JSON.

**Measured** (headless Chromium 153 sharer -> real relay built from this branch -> headless Chromium viewer, H.264 1080p60
8000 kbit/s, `live.html?detail=2` (a static colour-noise texture: ~1 MB IDRs of ~1075 datagrams every 2 s, small deltas, the
field case), 30 s each, **uniform random datagram loss on both directions** of the leg(s) named, by a UDP proxy per browser
(`lossproxy.py` of #99/#104; each browser reaches the relay through its own proxy, so the legs can be impaired separately).
Evidence JSON/logs: the task scratchpad `w111/`. `lossEvents` = receiver `markLoss` count, `frozen` = sum of gaps > 150 ms
between shown frames (the 5.8 s floor in every row that works is the sender's own 360-400 ms paced-IDR gap x 15, #103, not loss):

| loss | NACK | decoded fps | loss events | frozen | fragments asked / recovered | frames repaired / abandoned | sharer re-sent |
|---|---|---|---|---|---|---|---|
| none | off | 49.3 | 0 | 5.8 s | - | - | - |
| none | on | 49.3 | 0 | 5.9 s | 0 / 0 | 0 / 0 | 0 |
| 0.1 % both legs | off | **0** | 85 | **30 s** (never shown a frame) | - | - | - |
| 0.1 % both legs | on | **49.4** | **0** | 5.8 s | 129 / 128 | 96 / 0 | 53 |
| 0.3 % both legs | off | **0.2** | 213 | 29.9 s | - | - | - |
| 0.3 % both legs | on | **49.3** | **0** | 5.8 s | 374 / 369 | 231 / 0 | 194 |
| 0.5 % viewer leg only | off | **0** | 180 | 30 s | - | - | - |
| 0.5 % viewer leg only | on | **49.3** | **0** | 5.8 s | 278 / 276 | 203 / 0 | 0 (relay cache) |
| 0.5 % sharer leg only | off | **0** | 188 | 30 s | - | - | - |
| 0.5 % sharer leg only | on | **49.4** | **0** | 5.8 s | 291 / 289 | 198 / 0 | 291 (relay forwarded, ring) |
| 1 % both legs | off | **0** | 539 | 30 s | - | - | - |
| 1 % both legs | on | **49.1** | **0** | 5.8 s | 1167 / 1128 | 610 / 1 | 584 |

(NACK "off" = `&nack=0` on both pages. At 1 % the median capture-to-display latency was 1122 ms vs 705 ms without loss: a frame
waiting for its repair holds the stream, and the jitter buffer then grows. The one abandoned frame at 1 % was followed by the
usual keyframe request. Extra upstream cost: the re-sent fragments, ~the loss rate.)

Mixed versions (browser peers; "old relay" = the pre-#104 server built from `facc373`; 0.3 % / 0.5 % loss, 30 s):
see `features/video-pipeline.md` "Mixed versions" for the table with the native rows.

Native interop in the headless rig (relay + native clients built from this branch; the browser peer joins the rig's room
through a loss proxy; evidence in `w111/`):

| direction | loss | NACK | result |
|---|---|---|---|
| native sharer (vah264enc 1080p60 8 Mbit/s, ~1070-datagram IDRs) -> browser viewer | 1 % both legs of the browser | off | 2.7 fps decoded, 306 loss events, frozen 28.9 s of 30 s |
| same | same | on (two runs) | **60.0 / 59.8 fps, 0 loss events**, 331 / 322 fragments asked, 327 / 314 recovered, frozen 1.1 s / 1.9 s |
| browser sharer -> native viewer (egui, `vah264dec`) | 0.5 % both legs of the browser | on | tile shows 50 fps; viewer log: **0** `never completed`, 0 L1 recoveries, ~300 fragments asked and recovered, 0 frames abandoned; the browser re-sent 314 fragments for 313 requests (0 missed) |
| same | same | sharer `&nack=0` | **24 `a keyframe never completed (120/1080 fragments arrived)`**, 23 L1 recoveries, tile stuck on "Recovering..." at 0 fps; the relay answered `Miss` at once (the browser never said Hello) |

Not verified: a real internet path (burst loss, reordering beyond 2 fragments, RTT jitter; the proxy loses datagrams
independently and adds no delay), a hidden-tab viewer (timers throttled to ~1 s: repairs then arrive late and the 1 s hard limit
gives up, i.e. the old behaviour), Firefox as viewer under loss (the 68 unit tests pass in Firefox too, but no loss run was made there), a
browser sharer with more than one viewer behind different loss, the browser against the **native** sharer's *ring* repair
(the rig cannot put a loss proxy in front of the native sharer; only the relay-cache path was exercised with a native
sharer), a browser that does report `smoothedRtt` (no such browser was available; the code path is the same `nackRetryInterval`), audio.

## Measured (this machine, Chromium 153, local relay; full tables in `docs/dev/README.md`)

- Native sender to browser viewer: **H.264 and AV1 at 60 and 120 fps** received at full rate
  (e.g. `vah264enc` 1080p120 gave 120.1 fps, 0 loss). **H.265 only with headed VAAPI decode**
  (60 fps); 0 frames headless.
- Browser sender is limited by the browser's *software* encoder (H.264 ~150-190 fps, AV1 ~90-110 fps
  at 1080p), not by transport or decode; 4K60 is not sustainable.
- Browser to native: H.264 and AV1 at 60 fps decode on the native hardware path.
- Measure with `--flag --disable-frame-rate-limit --flag --disable-gpu-vsync`, otherwise headless
  Chromium's 60 Hz compositor caps `canvas.captureStream` and every 120/240 number is meaningless.

## Working on it

- **Run locally under the real sub-path:** `node docs/dev/headless.mjs serve --port 8123`, open
  `http://127.0.0.1:8123/simple-screensharing/`. `headless.mjs` serves `docs/` under
  `/simple-screensharing/` on purpose, to prove nothing depends on being at the root.
- **Unit tests:** `node docs/dev/headless.mjs run dev/tests.html`. **Scenarios** against a throwaway
  relay (`docs/dev/relay.sh`, tmux, prints the cert hash): `docs/dev/scenarios.mjs` (17 behaviour
  scenarios: reconnect, room full, loss, nonce exhaustion...). **Matrix/native interop:**
  `docs/dev/matrix.mjs`, `docs/dev/native.mjs`. **Real UI end to end:** `docs/dev/cdp.mjs` (a
  zero-dependency DevTools driver) + `e2e-lib.mjs` + `e2e/`.
- **Firefox:** `BROWSER=firefox` runs the same harness in the system Firefox (the user's Floorp is Gecko); `docs/dev/xbench.mjs` benches decode/render per browser. See `docs/dev/README.md` "Firefox (Gecko)".
- **Do not** pass `--use-fake-ui-for-media-stream` for `getDisplayMedia` tests (it picks a
  `screen:0:0` source that yields no frames); use `canvas.captureStream()` or the harness runner.
- Headed runs only inside the headless sway rig (`hs-rig.sh run -- env SSS_GUI_LOCKED=1 HEADED=1 ...`, see `docs/dev/README.md`), never on the user's desktop (user request 2026-10-14); `SSS_GUI_LOCKED=1` is the
  harness's historical opt-in guard.
- `window.__sss` exposes the UI state for debugging. `index.html#join=<link>` opens the join dialog
  prefilled.

## Deployment status

The repo is **private**; the project owner enables GitHub Pages later, so there is no public URL yet
(the site will live under `/simple-screensharing/`). Until then test only locally as above. Nothing
in `docs/` may rely on a CDN or a server-side route.
