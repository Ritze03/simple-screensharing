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
  back to a fullscreen stage. Native pop-outs are real viewports (`viewer-window.md`).
- **Room full is signalled with the `JoinRejected` frame, not a close code.** Chromium hides every
  WebTransport application close code (a `CONNECTION_CLOSE` surfaces as `WebTransportError:
  Connection lost.`, no `closeCode`), so the relay writes `JoinRejected{RoomFull|Malformed}` on the
  control stream first and `connect()` rejects with `JoinRejectedError`. The UI shows "This room is
  full (8 people at most)".

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
