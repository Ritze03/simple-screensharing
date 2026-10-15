# Browser client core (`docs/js/core`)

A full peer of the native app, in the browser: join a room, watch streams, share a screen. Plain ES modules with
relative imports, no build step, no npm runtime dependencies, so GitHub Pages can serve `docs/` as is. **No UI here**:
the UI (I12) consumes the API below. Binding wire spec: `superdoc/architecture/protocol-spec.md` (§11 is the browser leg).

```js
import { RoomClient, createRoom, parseJoinLink, capabilities } from "./js/core/index.js";

const room = createRoom("relay.example.com:4433", { name: "Movie night", certHash });  // mint id + key, nothing sent yet
const client = new RoomClient({ displayName: "Mo", maxJitterBufferMs: 2000, catchUpSpeedPercent: 5 });
client.addEventListener("streams", (e) => render(e.detail.streams));
await client.connect(room.link);                       // or a pasted link
const watch = client.watch(streamKey, { canvas });     // video + the owner's audio
await client.startShare({ audio: true, codec: "h264", fps: 60, bitrateKbps: 8000 });  // needs a user gesture
```

One `RoomClient` = one room = one WebTransport connection. Several rooms at once = several `RoomClient`s (verified in
`dev/scenarios.js`). Saved rooms are UI-side (`localStorage` is fine; the link is the only thing to store).

## Browser requirements

WebTransport + WebCodecs (+ `MediaStreamTrackProcessor` and `getDisplayMedia` for sharing), in a secure context
(https or localhost). That means current Chromium-based browsers; Firefox/Safari lack parts of it. `capabilities()` says what
this browser can do (`canWatch`, `canShare`, and the individual features) so the UI can disable buttons with a reason.

## Join links

`server|room-id|key[|certhash[|name]]`, exactly like the native client (`client/src/net.rs`, spec `protocol-spec.md` §11.4):
`room-id` = 32 hex chars, `key` = 32 bytes base64url without padding, `certhash` = SHA-256 of the relay's leaf certificate
DER, base64url without padding (needed by browsers for a self-signed relay; omit for a CA certificate, or leave empty when
only a name follows: `server|id|key||name`), `name` = the room's display name, UTF-8 percent-encoded (task #69, user request
2026-10-15). The name is cosmetic and not part of the room's identity: a malformed `%`, invalid UTF-8 or a blank name means
"no name" and the link still parses; the decoded name is trimmed, stripped of control characters and cut at 64 characters.

| function | |
| --- | --- |
| `parseServerAddr(str)` | `{ host, port }` for `host:port`, `ip:port`, `[v6]:port` (port 1-65535); throws `JoinLinkError` with `code` `empty` / `no-port` / `bad-port`. The port is never defaulted (`protocol-spec.md` §11.4). `createRoom` and `RelaySession.open` use it. |
| `parseJoinLink(str)` | `{ server, host, port, roomId, roomIdHex, roomKey, certHash, name }` (`name` is `null` when absent or undecodable); throws `JoinLinkError` with the native wording. Accepts 3, 4 and 5 fields (the 5th keeps any further `\|`, like Rust `splitn(5)`), trims, strict base64url (rejects padding/non-canonical like Rust). |
| `formatJoinLink(server, roomId, roomKey, certHash?, name?)` | the inverse. The name is encoded like the native encoder: only `A-Za-z0-9-._~` stay raw (`encodeURIComponent` plus `!'()*` escaped), so a link formatted by either client is byte-identical. |
| `encodeLinkName(name)`, `decodeLinkName(field)`, `MAX_LINK_ROOM_NAME_CHARS` | the name codec on its own (native `encode_link_name` / `decode_link_name`) |
| `stripLinkName(link)` | the link without its 5th field: what a saved room stores |
| `createRoom(server, { name?, certHash? })` | random id + key from `crypto.getRandomValues`; returns `{ link, name, server, roomId, roomKey }`. The name is a local label, never on the wire to the relay; the returned `link` carries it as the 5th field. |

**Link expiry vs relay down (what the browser can and cannot tell).** The relay rotates its self-signed certificate about
every 11 days, which makes the hash in older links stale. **Chromium 153 reports every handshake failure identically**: a
wrong hash, a closed port, an unresolvable host and a self-signed certificate without a hash all reject `ready` and `closed`
with `WebTransportError: Opening handshake failed.` (`source: "session"`, no `streamErrorCode`, and the durations overlap, so
timing does not separate them either; measured in `dev/` runs). So the core infers from history, per page:
a server+hash pair that has completed a handshake once is *verified*. After that a failed handshake is `ConnectError{code:
"handshake-failed", reason: "unreachable"}` (`mayBeExpiredLink` false; reconnect `error` code `relay-unreachable`): the relay
is not answering. Only a hash that has **never** worked here gives `reason: "unreachable-or-expired-link"` with
`mayBeExpiredLink: true` and the hedged message `EXPIRED_LINK_HINT` ("may be offline, or this invite link may have expired").
Consequence: the `link-expired` reconnect error can only fire for a client that never connected with that hash, i.e. in practice
never during auto-reconnect; the narrow case it no longer catches is a certificate rotated while the page stayed open and the
connection then dropped (the reconnect then reports `relay-unreachable` until the page is reloaded and the link is replaced).
`handshake-failed` attempts that hang are cut off after 8 s (`HANDSHAKE_TIMEOUT_MS`) and can be cancelled (see `reconnectNow()`).

**Relay close codes (task #29).** The relay now writes `JoinRejected{RoomFull|Malformed}`, `finish()`es the control stream and
waits (up to 2 s) for the peer before closing with `0x102` / `0x101`, and closes the endpoint with `0x200` "relay shutting down".
Chromium **does not expose any of these codes**: a QUIC `CONNECTION_CLOSE` with an application code surfaces as
`WebTransportError: Connection lost.` (no `closeCode`, `closed` *rejects* instead of resolving with a `WebTransportCloseInfo`),
measured for all three. The control-stream frame is what carries the information: `connect()` now rejects with
`JoinRejectedError{reason: "RoomFull"|"Malformed"}` (the old `join-no-reply` guess remains only for an undecodable first
frame, which the relay drops without a reply, and for relays older than #29). `classifyClose(info)` maps the codes if a browser
ever reports them. A relay shutdown is noticed within a few milliseconds (CONNECTION_CLOSE arrives; no 21 s idle wait) and the
first reconnect attempt is immediate (50 ms), then exponential backoff, so a restarted relay is rejoined in about 0.6 s.

## `RoomClient` (extends `EventTarget`)

```js
new RoomClient({
  displayName, maxJitterBufferMs = 2000 /* 0..5000 */, catchUpSpeedPercent = 5 /* 5..10 */,
  autoReconnect = true, lossRecovery = "resubscribe" /* | "wait" */,
  hardwareAcceleration = "no-preference" /* | "prefer-hardware" | "prefer-software" */,
  audioLatencyMs = 80, audioContext /* optional */,
})
```

`maxJitterBufferMs` / `catchUpSpeedPercent` are the native settings of the same name (defaults and clamps copied from
`client/src/rooms.rs`); `setJitterSettings({...})` changes them live.

### Methods

| | |
| --- | --- |
| `connect(linkOrParsed, displayName?)` | Resolves when joined. Rejects `JoinLinkError`, `ConnectError` (`code`: `handshake-failed` + `reason`, `no-webtransport`, `datagram-too-small`, `join-no-reply`, `relay-shutdown`), `JoinRejectedError` (`reason`: `RoomFull`, `Malformed`). After the first success the client reconnects by itself. |
| `reconnectNow()` | "Retry now". While `reconnecting`: skips the backoff wait and cancels a handshake that is stuck. While `connected`: drops the connection and joins again (fresh ConnId, so **other** viewers of your shares see them restart). Shares stay registered and are re-announced, watches stay (a watched stream keeps its key unless its owner reconnected). Resolves with the state after the attempt (`connected`, or `reconnecting` if it failed and retries go on). Nothing is dropped, unlike `disconnect()` + `connect()`. |
| `findStreamByStableKey(stableKey, { exceptKeys })` | The stream now in the room with that identity, or `null` (see "Stable stream identity"). |
| `disconnect()` | Stops shares and watches, sends `Leave`, lets queued datagrams flush, closes. |
| `watch(key, { canvas?, onFrame?, audio = true })` | Subscribe. Returns a `WatchHandle`. Watching a video stream also subscribes the owner's audio stream ("audio follows video", re-evaluated when that audio stream appears or disappears). |
| `unwatch(key)` | Unsubscribes the stream and its followed audio. |
| `setVolume(key, 0..2)`, `setMuted(key, bool)` | Per-stream playback gain (0-200 %) and mute. `key` may be the video key (applies to its audio) or an audio key. Persists across re-watch. |
| `resumeAudio()` | Call from a user gesture if the autoplay policy left the `AudioContext` suspended. |
| `startShare(opts)` | Share a screen/window/tab (+ audio). Returns a `ShareHandle`. See below. |
| `stopShare(handle?)` | Stop one share, or all. |
| `setJitterSettings({ maxJitterBufferMs?, catchUpSpeedPercent? })` | Live. |
| `watchStats(key)` | Latest numbers for one watched stream (also delivered in `stats`). |

### State

`state` (`idle | connecting | connected | reconnecting | closed`), `selfId`, `members` (Map id -> `{ id, name, streamCount, self }`),
`streams` (Map key -> stream info), `previews` (Map key -> `{ blob, receivedAt }`), `shares`, `joinLink`.

**Ids** are lowercase hex strings (`selfId`, `owner`, member `id`: the 64-bit ConnId, 16 chars). A stream key is
`"<ownerHex>:<streamId>"`; `streamKey(owner, id)` builds one.
A reconnect gives a fresh ConnId, so stream keys of the reconnecting peer change (see `watch-ended` and "Stable stream identity").

**Stable stream identity.** Every stream info (in `streams`, `client.streams`, `findStreamByStableKey`) carries `stableKey` =
`<room id hex>/<owner display name>/<kind>/<label>`, which survives the owner's reconnect while `key` does not. `watch-ended` carries
the same `stableKey` (plus `ownerName`, `label`, `kind`), also when the stream vanished because *this* client reconnected into a
restarted relay. Decision (task I20): the core does **not** transparently re-bind a `WatchHandle` to the new key, because on the
wire a sharer that reconnected is indistinguishable from one that stopped and started again, so how long to wait and what to show
in between is a UI policy. The UI keeps a 60 s "resume" list keyed by `stableKey` and re-watches (same volume/mute) when
`findStreamByStableKey` finds the stream again. Limits: two members with the same name sharing the same label collide (the UI
picks the first unwatched match); a renamed member or a changed label is a different stream.

### Events (`CustomEvent`, payload in `event.detail`)

| event | detail |
| --- | --- |
| `state` | `{ state, selfId, attempt, reason?, error?, reconnected? }`. `connecting -> connected`; on loss `reconnecting` (exponential backoff 0.5..10 s) then `connected` again. |
| `roster` | `{ members: [{ id, name, streamCount, self }] }`. Every member, including lurkers (`streamCount === 0`). |
| `streams` | `{ streams: [{ key, owner, ownerName, streamId, kind: "video"\|"audio", label, width, height, self }] }`. Full list on every change. |
| `preview` | `{ key, owner, streamId, blob }` a decrypted JPEG (about 320 px wide) of someone else's video stream. Delivered on join (the relay replays its cache) and about every 30 s. `client.previews` keeps the latest. |
| `stats` | every second: `{ watching: { key: {...} }, sharing: [...], transport, droppedFrames }`. Watch stats: `receivedFps, decodedFps, bitrateKbps, jitterMs, bufferMs, width, height, codec, codecString, lossEvents, decoderErrors, decryptFailures, skippedDeltas, needKeyframe, stalled, ...`; audio: `packets, decodedFrames, underruns, level, volume, muted` (`level` = peak meter 0..1 with a ~0.5 s hold, not one frame's instant peak). `transport` is `WebTransport.getStats()` (RTT etc.) when the browser offers it. |
| `subscribers` | `{ streamId, count }` for **your own** shares (from the relay). |
| `watch-ended` | `{ key, reason, stableKey, ownerName, label, kind }` the stream you were watching stopped (or its owner left or reconnected, or you reconnected into a relay that no longer has it). The handle is already removed. |
| `media` | `{ key, type, ... }`: `codec` (a keyframe revealed `codec` / `codecString`), `unsupported` (this browser cannot decode that codec; the UI drops that error on the next `codec` event, i.e. when the sharer switches to a codec string it can decode), `decoder-error`, `need-keyframe`. |
| `share` | `{ state: "started"\|"stopped"\|"ended-by-browser"\|"no-audio"\|"no-frames", share, message? }`. `ended-by-browser`: the user clicked the browser's "Stop sharing". `no-frames`: 4 s after starting, the source had not delivered a single frame. |
| `error` | `{ code, message, fatal, error }`. Codes: `connection-lost` (fatal, only without `autoReconnect`), `relay-unreachable` (reconnect attempt failed, the hash is known good), `room-full` (reconnect hit a full room; retries go on), `link-expired` (only for a hash that never worked in this page, see above), `reconnect-failed`, `join-rejected`, `nonce-exhausted`, `encoder-error`, `audio-encoder-error`, `audio-share-unavailable`, `send-failed`, `preview-failed`. |

### Rendering (`WatchHandle`)

The core hands you decoded `VideoFrame`s; two ways to consume them:

* `watch(key, { canvas })` / `handle.attachToCanvas(canvas)`: a `CanvasRenderer` draws **the newest decoded frame once per
  `requestAnimationFrame`** and closes every frame it supersedes, so a 240 fps stream on a 60 Hz screen costs 60 draws, and
  on a 280 Hz screen every frame is shown. Decoding is never tied to the display. The draw is a WebGL2 texture upload
  (`renderer.js`) when that is measurably cheaper than 2D `drawImage` on the first frame, else 2D (`alpha:false`,
  `desynchronized:true`); either way draws are spaced so they use at most half of the main thread.
  *Why (user report 2026-10-15, "hangs and skips above 60 fps" in Firefox/Floorp):* Firefox's 2D `drawImage(VideoFrame)` is a
  CPU read-back + convert on the main thread (13 ms at 1440p), which starved the decoder's callbacks. See
  `superdoc/features/website.md` "Rendering above 60 fps". Pass `new CanvasRenderer(canvas, { webgl: false })` to force 2D,
  `{ preserveDrawingBuffer: true }` if you need `canvas.toDataURL()`. Moving the canvas into another document (pop-out) is fine.
* **Decoder latency mode (Gecko only):** `VideoReceiver` starts with `optimizeForLatency: true` and, the first time the
  decode-queue backstop drops frames (the decoder is slower than the stream), re-configures without it (Firefox's low-latency
  software decode is single-threaded, 3.5-5x slower). Emits media event `decoder-mode { lowLatency: false }`; watch stat
  `throughputMode`. Constructor options `lowLatency`, `adaptiveLatency` (default: true only on Gecko).
* `watch(key, { onFrame })` / `handle.setFrameCallback(fn)`: you receive each `VideoFrame` and **must `close()` it** (hardware
  decoders have a small surface pool; holding frames stalls decoding).

* **Padding is cropped before you see a frame.** Some encoders code more lines than the picture has (the AMD VCN4 AV1 encoder
  codes 1920x1080 as 1920x1082, with no crop in the bitstream; Mesa issue 9185). `VideoReceiver` crops every decoded frame
  (zero-copy `new VideoFrame(frame, { visibleRect })`) to the decoder's own `visibleRect` if it reports one, else to the
  size the sender announced in the stream list (`StreamInfo.width/height`) when the frame is larger by less than 64 px in each
  direction. Canvas, track and `onFrame` consumers all get the cropped frame, and watch stats `width`/`height` are the visible
  size. Same rule as the native viewer (`client/src/decode/frame.rs` `visible_rect`); the pure function is `visibleRectFor(frame, hint)`.

* `handle.videoTrack()`: a `MediaStreamTrack` (Chromium `MediaStreamTrackGenerator`) of the decoded frames, for a `<video>`,
  Picture-in-Picture or recording. It adds the browser's own video pipeline latency and pacing, so the canvas is the default.

`handle.setVolume(v)`, `handle.setMuted(m)`, `handle.stats()`, `handle.stop()`, `handle.audioKey`.

### Sharing (`startShare`)

```js
const share = await client.startShare({
  audio: false,                 // also capture + share audio (only if the browser offers it for the chosen source)
  codec: "auto",                // "h264" | "h265" | "av1"; auto = h264
  fps: 60, bitrateKbps: 8000,
  preset: "balanced",           // low_latency | balanced | quality | gaming  (see below)
  label: "Screen", previews: true,
  stream,                       // a MediaStream to share instead of calling getDisplayMedia (tests, custom pickers)
  displayMediaOptions,          // merged into the getDisplayMedia() argument
  gopMs, latencyMode, bitrateMode, hardwareAcceleration,   // overrides
});
share.stats(); share.muteAudio(true); share.setBitrate(kbps); share.requestKeyframe(); share.stop();
share.stream  // the MediaStream, e.g. for a local <video> preview
```

Behaviour that matches the native sender:

* **Idle pause.** Nothing is encoded or sent until the relay reports `SubscriberCount >= 1`; encoding stops again at 0.
  Previews continue regardless.
* **KeyframeRequest** (a new viewer subscribed) forces `keyFrame: true` on the next `encode()`, coalesced to one per ~100 ms.
  If the source is static (screen capture delivers no frames when nothing changes) the newest frame is re-encoded as the
  keyframe, so a new viewer never waits for something to move.
* **Wire format** is byte-compatible with what `client/src/decode/codec.rs` sniffs: Annex-B with SPS/PPS(/VPS) on every
  keyframe for H.264/H.265, a raw OBU temporal unit starting with a temporal delimiter (`12 00`) and a sequence header on
  keyframes for AV1; PTS in microseconds, monotonic from 0; audio is raw 20 ms Opus packets, 48 kHz stereo, 64 kbit/s, every
  packet flagged keyframe. Verified in both directions against the real native decoders (see `dev/README.md`).
* **Previews**: a JPEG (<= 320 px wide, quality 0.75) of the outgoing video about 1.5 s after the share starts, then every 30 s
  (native waits a full interval for the first one).
* **Backpressure.** If the transport backs up, whole frames are dropped and a keyframe is forced (an encoded frame was lost);
  if the encoder backs up, input frames are dropped (nothing was encoded, so no keyframe is needed).
* `getDisplayMedia` must be called from a user gesture, so call `startShare()` from a click handler.
* Audio capture is whatever the browser offers: it differs per browser/OS/source (tab audio is the common denominator; system
  audio only on some platforms). When `audio: true` but the source has none, a `share` event with `state: "no-audio"` fires and
  the share continues video-only.

#### Encoder presets: what maps and what does not

| native (`client/src/presets.rs`) | browser |
| --- | --- |
| tune `LowLatency` / `BestQuality` | `latencyMode: "realtime"` / `"quality"` |
| rate control Static (CQP) / Dynamic (QVBR) | `bitrateMode: "constant"` / `"variable"`; every preset uses `"variable"` since #87 (CQP needs per-frame quantizers and Chromium 153 reports `bitrateMode: "quantizer"` unsupported for H.264; CBR is the closest predictable mode) |
| GOP / keyframe interval (2 s, Gaming 0.75 s) | forced keyframes every `gopMs` (WebCodecs has no GOP knob) |
| quality level (x264 speed-preset, VA target-usage) | **native-only**: no WebCodecs equivalent |
| H.264 profile/level | chosen from size and fps (High, falling back to Main, then Baseline), `probeEncoders()` shows what works |

`probeEncoders({ width, height, fps, bitrateKbps })` resolves `[{ codec, string }]` of the codecs this browser can encode, for a codec dropdown.

## Lower-level modules

| module | |
| --- | --- |
| `index.js` | public re-exports |
| `room-client.js` | `RoomClient`, `WatchHandle`, `ShareHandle`, `capabilities`, `probeEncoders`, `streamKey` |
| `session.js` | `RelaySession`: one WebTransport connection (control stream + framing, datagram writer with backpressure counter, uni streams, join handshake incl. ConnId collision retry, `ConnectError`, `JoinRejectedError`) |
| `protocol.js` | port of `protocol/src/lib.rs`: every `ClientToRelay`/`RelayToClient` encode+decode, control framing (`ControlFrameReader`, 64 KiB cap), 19-byte media header, 15-byte preview header, constants |
| `postcard.js` | the postcard subset used (varints, strings, fixed arrays) |
| `crypto.js` | AES-256-GCM via WebCrypto; `NonceSequence` (ONE u32 counter per connection shared by video, audio and previews, never wraps: the last legal value is `0xFFFFFFFF`, then `NonceSpaceExhausted` and the room client reconnects with a fresh ConnId); AAD `[version, stream_id]` |
| `fragment.js` | `fragmentFrame` (encrypt once, then split to <= 1000 bytes including the header) and `Reassembler` (drops a frame on any missing fragment, in-flight cap 2, 200 ms staleness, straggler floor, reports drops) |
| `codec.js` | codec sniffing (H.264/H.265/AV1) and WebCodecs codec strings from the SPS / sequence header (`avc1.PPCCLL`, `hev1…`, `av01…`) |
| `jitter.js` | adaptive jitter buffer (RFC 3550 EWMA, growth cap, `catchUpSpeedPercent` shrink cap, decode-queue backstop) ported from the desktop client's former WebCodecs `tile-video.js` (replaced by the native decoder), one timer drains all due chunks |
| `video-rx.js` | per-stream receive pipeline (`VideoReceiver`: loss gate -> jitter buffer -> `VideoDecoder`, reconfigures on codec change, recovers from decoder errors, Gecko decoder-latency fallback); re-exports `CanvasRenderer` |
| `renderer.js` | `CanvasRenderer`: newest frame per `requestAnimationFrame`, WebGL2 or 2D backend picked by a first-frame timing, draw spacing |
| `video-tx.js` | `VideoSender` (track -> `VideoEncoder`, keyframe policy, static-source keepalive, AV1 shaping), `probeEncoders`, `ENCODER_PRESETS`, `MediaClock` |
| `audio.js` | `AudioReceiver` (Opus `AudioDecoder` -> WebAudio, PTS-placed playback, gain 0..2) and `AudioSender` (`AudioEncoder`) |
| `preview.js` | preview sampler (downscale + JPEG) and cadence constants |
| `joinlink.js` | link parse/format/mint, base64url |

## Transport notes

* `new WebTransport("https://<server>/ssshare", { serverCertificateHashes })`; ALPN `h3`, same UDP port as native (default 4433).
* The first bidirectional stream is control; `Join` is written immediately (the relay only sees the stream once data arrives).
* Media is datagrams of at most 1000 bytes (`MAX_MEDIA_DATAGRAM_LEN`); the session refuses to join if
  `datagrams.maxDatagramSize < 1000` (Chromium reports 1024). `datagrams.incomingHighWaterMark` is raised because a keyframe arrives as hundreds of fragments in one burst.
* `disconnect()` waits ~150 ms after `Leave` before closing: datagrams written just before a close can be lost.
* **Relay gone.** A relay that shuts down gracefully (TUI `q`, task #29) is noticed within milliseconds (measured 0 ms to the
  `reconnecting` state) and the client is back in about 0.6 s after the relay restarts. A relay that is killed or unplugged
  sends nothing, so that is only noticed after the QUIC idle timeout (about 21 s in Chromium 153).
* **Loss recovery** (`lossRecovery: "resubscribe"`): a lost frame breaks the reference chain, so the receiver stops feeding
  deltas, and asks for a keyframe the only way a viewer can: `Unsubscribe` + `Subscribe` (the relay then sends the sender a
  `KeyframeRequest`), at most every 1.5 s and again while still waiting. `"wait"` just waits for the next scheduled keyframe.

## Known relay issues

Both issues found while building the core (JoinRejected lost to a connection drop; no CONNECTION_CLOSE on shutdown) were fixed
by relay task #29 and verified in `dev/scenarios.js` ("room is full", "relay restart", "close codes"). What remains: the relay's
application close codes are invisible to Chromium (see "Relay close codes"), and an undecodable first control frame is dropped
without any reply (`read_control_frame(...)?` in `perform_join`), so a browser with a protocol bug sees `join-no-reply`.
