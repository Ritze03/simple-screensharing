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
`superdoc/testing/relay-and-website-checks.md` "Website (browser viewer) checks".

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
- **Watched merges one name across rooms** (2026-11-04, user: "the list of participants should be filtered/condensed, so every person is only shown once"; native twin `ui/STAGE.md`). `peopleOf` for the Watched view groups by room + ConnId, then merges groups of one `owner` name from DIFFERENT rooms into one person (`conns`, `key` = the first): one avatar, the count of all their streams, one card with a row per stream (the row's third line already names its room in Watched), `audible` / LIVE if any stream is. Two same-named connections of ONE room stay two. *Why:* a name is the only identity across rooms, but two of one name in a room are provably two (#127); a person never takes a second connection of a room it has. Merged emoji: the first connection's. A single room's strip is unchanged. Tests: `dev/tests.js` (merge rule), `e2e/stage.mjs` "Watched: one person in two rooms".
- **A person is a connection** (2026-11-04, #127; native twin in `ui/STAGE.md` "The stage"). A stage item carries `conn` (its
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
see "The stage" in `superdoc/features/website/stage.md`), `dom.js` (`V`/`H`/`SL`/`FILL` = the same vbox/hbox/slot/fill
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
  limiter. The mouse wheel over the slider steps it (`volumeWheel`, `wheelStepVolumePct`, #113). Watched audio is levelled before the slider (`core/leveller.js`, the control bar's Level loudness button (`#btn-level`, left of the people button; was Settings > Playback and, from I6 to 2026-11-04, a stream-menu row), default on, #119; user 2026-11-04: "move level loudness (since its global) to the left of the \"Hide the people\" button"). Details and the why: `features/audio/levelling-and-volume.md` (2026-11-02, 2026-11-03). In Firefox the tick only shows on the unfilled part of the track.
- **No audio source choice:** only what `getDisplayMedia({audio})` returns (a tab's audio, or the system's where the browser offers it); the audio control is
  mute plus an audio-only share (I10, `superdoc/features/website/webcam-and-audio.md`). Native has the per-application picker, Desktop audio, source groups and the exclusion list (D9: *"native Linux only: a
  browser can't list apps"*).
- **Capture is damage-driven:** a static screen delivers no frames; the core re-encodes the newest
  frame when a viewer needs a keyframe.
- **Fan-out uses one encoder per room** (no shared encode): a capture sent to N rooms gets track
  clones and N `startShare()` calls with their own encoder and key, matching native ("each extra
  room is a separate encoded + encrypted stream"). Done in the UI, not the core.
- **Pop-out is Document Picture-in-Picture** (`documentPictureInPicture.requestWindow()`, Chromium
  116+): the tile's canvas moves into the PiP window (no second decode); one window per page; falls
  back to a fullscreen stage (a tile double-click uses that stage too, see "Double-click fullscreen and avatar colours" in `superdoc/features/website/viewer-features.md`). Native pop-outs are real viewports (`viewer-window.md`).
- **Room full is signalled with the `JoinRejected` frame, not a close code.** Chromium hides every
  WebTransport application close code (a `CONNECTION_CLOSE` surfaces as `WebTransportError:
  Connection lost.`, no `closeCode`), so the relay writes `JoinRejected{RoomFull|Malformed}` on the
  control stream first and `connect()` rejects with `JoinRejectedError`. The UI shows "This room is
  full (8 people at most)".

## Deployment status

The repo is public and GitHub Pages is enabled (checked 2026-11-07 with the GitHub API: built from `master`, folder `/docs`, HTTPS enforced); the site lives under `/simple-screensharing/`. Test locally as above first. Nothing
in `docs/` may rely on a CDN or a server-side route.

## Detail pages

The long sections moved to `superdoc/features/website/` on 2026-11-07 (row 164); the `##` headings are unchanged.

- `superdoc/features/website/stage.md`: The browser stage (tiles, control bar, layouts, chrome), its fullscreen rule and what its tests cover.
- `superdoc/features/website/phone.md`: Phone dropouts, decoder failures, which phones can watch, and the Screen Wake Lock.
- `superdoc/features/website/webcam-and-audio.md`: Webcam and audio-only sharing and the PIP pairing in the browser.
- `superdoc/features/website/viewer-features.md`: Sounds, emoji avatars, decode-error clearing, double-click fullscreen, own-tile preview, who watches a share, rendering above 60 fps.
- `superdoc/features/website/join-links.md`: Display name and room name in join links.
- `superdoc/features/website/nack-and-big-keyframes.md`: Selective retransmission and big keyframes in the browser sharer.
- `superdoc/features/website/harness-and-measurements.md`: Measured numbers and how to run the browser test harness.
