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
  every `gopMs`; the preset table is in `docs/js/core/README.md`.
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

Verified through the real UI with `docs/dev/e2e/name-modal.mjs` (headless Chromium, throwaway relay) and by the unit vectors
in `docs/dev/tests.js`.

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
