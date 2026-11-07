# Website: webcam, audio-only shares and webcam in picture

Webcam and audio-only sharing and the PIP pairing in the browser. Part of the browser client, see `superdoc/features/website.md`.

## Webcam, audio-only shares and webcam in picture (task I10, 2026-11-04)

The browser caught up with native I7 (audio-only shares), I8 (webcam, webcam in picture) on the stage of I6. Decisions D1, D7, D9, D10 are the plan's; the native side is
`superdoc/ui/STAGE.md` "Webcam and webcam in picture" and `features/audio/desktop-audio.md` (I7).

**Listening to audio-only senders.** A native client can share audio with no screen. `stage-state.js` `collect` adds a `kind: "audio"` item for every audio stream whose owner
shares **no screen** (`RoomEntry.ownerHasVideo`: a webcam is not a screen; native `owner_has_video`) and a Listen tile for it: avatar, `X is sharing audio`, `Audio only`,
`Listen` / `Stop listening` (`ui.room.listen`, native keys and German). Listen is `watchStream` of the audio key: the same watch object with `audioOnly: true` and no canvas,
`client.watch(audioKey)` (the core always could), so volume and mute work through the stream menu (Level loudness is the bar button). With a screen share the audio rides along with the screen's
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
the Arch test VM only (`e2e/audio-only.mjs`, a headless Chromium 156 there, a native audio-only sender built from this tree, the emulated music-player tone source, a fake hardware sink): a **native audio-only sender is heard by a browser** (tile, Listen,
107 packets decoded in about 4 s, the meter sees the tone, Stop listening, the tile goes when the sender stops), a **browser audio-only share** (the real `#btn-audio` click, video track stopped) **is heard by a second browser and by a
native viewer** (`owner_has_video=false`, 350-400 packets), and a webcam-plus-audio sender gets a webcam tile and an audio tile with no audio subscribed by the webcam watch. *Not tested:* a real `getDisplayMedia` audio-only pick (the harness
stubs the picker, and Chromium's own refusal of `video: false` is documented, not exercised), Firefox (no `getDisplayMedia` encode path, no MediaStreamTrackProcessor), a real USB camera (the fake device delivers 1280x720 at 20 fps and reports
no capability list worth a mode menu), a real GPU, Windows.
