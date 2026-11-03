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
  omit it for a CA certificate. See `superdoc/meta/TERMINOLOGY.md` ("Join link", "Cert hash").
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
- **`reconnectNow()`** ("Retry now"): skips the backoff and cancels a stuck handshake; while
  connected it re-joins. **Link-expiry vs unreachable heuristics:** Chromium cannot say why a
  handshake failed, so only a hash that has *never* worked in this page yields the hedged "offline or
  invite link expired" message; once it has worked, a failing reconnect says "relay not answering"
  and never blames the link.

`docs/js/ui/`: `app.js` (render + modals), `dom.js` (`V`/`H`/`SL`/`FILL` = the same vbox/hbox/slot/fill
layout model as native, see `superdoc/meta/TERMINOLOGY.md`, plus shared row patterns), `store.js`, `rooms.js`
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
  it), no Low-Latency preset button (identical to Balanced here; a stored `low_latency` shows as
  Balanced and is kept as is), no greyed-out codecs (an unencodable codec has no button, the Codec row
  goes when at most one is left). Hidden only in the UI: the core's `ENCODER_PRESETS`, the persisted
  settings and the native app keep the setting. Details and the "why": `ui/APP-SHELL.md` (2026-11-01).
- **Viewer volume is 0-500 %** (task #96, 2026-11-02): a WebAudio `GainNode` (`core/audio.js` `MAX_VOLUME = 5`), tile slider
  with a tick and a snap at 100 % (`ui/dom.js` `snapVolumePct`, `.vol` in `app.css`). Loud audio hard-clips at the device, no
  limiter. The mouse wheel over the slider steps it (`volumeWheel`, `wheelStepVolumePct`, #113). Details and the why: `features/audio-sharing.md` (2026-11-02, 2026-11-03). In Firefox the tick only shows on the unfilled part of the track.
- **No audio source choice:** only what `getDisplayMedia({audio})` returns; the audio control is
  mute only. Native has the per-application picker (`pw-loopback`).
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
fine to hide it." The sharer's own card in "Sharing in this room" (`app.js` `myCard`) holds a persistent muted `<video
class="live-preview">` (`capture.js` `previewFor`) playing `new MediaStream([c.video])` (video only, never audio; audio-only
captures get none). It is attached only while `document.visibilityState === "visible" && document.hasFocus()`; `visibilitychange`
/ `focus` / `blur` pause it and drop `srcObject`, and the element is `hidden`, so the placeholder shows and nothing is composited
(the track itself keeps feeding the encoder). *Why a persistent element:* `render()` rebuilds the tree on every state change, so
`myCard()` re-inserts the same element and playback never restarts. A `PREVIEW` chip (CSS `:has()` on the thumb) replaces the "Your
screen" label while it plays. Native counterpart and shared rationale: `superdoc/features/self-preview.md`.
E2E: `docs/dev/e2e/live-preview.mjs <relay host:port> <certhash>` (headless Chromium: headless reports `document.hasFocus() ===
false` by default, so the run switches focus with CDP `Emulation.setFocusEmulationEnabled`, and hides the page with a second tab +
`Page.bringToFront`; checks attached/playing/video-only, survives `render()`, detached when unfocused or hidden, resumes).

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
