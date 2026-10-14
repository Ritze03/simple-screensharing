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

`server|room-id|key[|certhash]`, exactly like the native client (`client/src/net.rs`):
`room-id` = 32 hex chars, `key` = 32 bytes base64url without padding, `certhash` = SHA-256 of the relay's leaf certificate
DER, base64url without padding (needed by browsers for a self-signed relay; omit for a CA certificate).

| function | |
| --- | --- |
| `parseJoinLink(str)` | `{ server, host, port, roomId, roomIdHex, roomKey, certHash }`; throws `JoinLinkError` with the native wording. Accepts 3 and 4 fields, trims, strict base64url (rejects padding/non-canonical like Rust). |
| `formatJoinLink(server, roomId, roomKey, certHash?)` | the inverse |
| `createRoom(server, { name?, certHash? })` | random id + key from `crypto.getRandomValues`; returns `{ link, name, server, roomId, roomKey }`. The name is a local label only, never on the wire. |

**Link expiry.** The relay rotates its self-signed certificate about every 11 days, which makes the hash in older links
stale. A failed handshake rejects `connect()` with a `ConnectError` whose `mayBeExpiredLink` is `true` (when the link had a
hash) and whose message tells the user to ask for a fresh invite link (`EXPIRED_LINK_HINT`). The browser cannot tell a stale
hash from a relay that is down; the message says so. During auto-reconnect the same condition is reported as an `error` event
with `code: "link-expired"` (non-fatal, retries continue).

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
| `connect(linkOrParsed, displayName?)` | Resolves when joined. Rejects `JoinLinkError`, `ConnectError` (`code`: `handshake-failed`, `no-webtransport`, `datagram-too-small`, `join-no-reply`), `JoinRejectedError` (`reason`: `RoomFull`, `Malformed`). After the first success the client reconnects by itself. |
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
A reconnect gives a fresh ConnId, so stream keys of the reconnecting peer change (see `watch-ended`).

### Events (`CustomEvent`, payload in `event.detail`)

| event | detail |
| --- | --- |
| `state` | `{ state, selfId, attempt, reason?, error?, reconnected? }`. `connecting -> connected`; on loss `reconnecting` (exponential backoff 0.5..10 s) then `connected` again. |
| `roster` | `{ members: [{ id, name, streamCount, self }] }`. Every member, including lurkers (`streamCount === 0`). |
| `streams` | `{ streams: [{ key, owner, ownerName, streamId, kind: "video"\|"audio", label, width, height, self }] }`. Full list on every change. |
| `preview` | `{ key, owner, streamId, blob }` a decrypted JPEG (about 320 px wide) of someone else's video stream. Delivered on join (the relay replays its cache) and about every 30 s. `client.previews` keeps the latest. |
| `stats` | every second: `{ watching: { key: {...} }, sharing: [...], transport, droppedFrames }`. Watch stats: `receivedFps, decodedFps, bitrateKbps, jitterMs, bufferMs, width, height, codec, codecString, lossEvents, decoderErrors, decryptFailures, skippedDeltas, needKeyframe, stalled, ...`; audio: `packets, decodedFrames, underruns, level, volume, muted`. `transport` is `WebTransport.getStats()` (RTT etc.) when the browser offers it. |
| `subscribers` | `{ streamId, count }` for **your own** shares (from the relay). |
| `watch-ended` | `{ key, reason }` the stream you were watching stopped (or its owner left or reconnected). The handle is already removed. |
| `media` | `{ key, type, ... }`: `codec` (a keyframe revealed `codec` / `codecString`), `unsupported` (this browser cannot decode that codec), `decoder-error`, `need-keyframe`. |
| `share` | `{ state: "started"\|"stopped"\|"ended-by-browser"\|"no-audio"\|"no-frames", share, message? }`. `ended-by-browser`: the user clicked the browser's "Stop sharing". `no-frames`: 4 s after starting, the source had not delivered a single frame. |
| `error` | `{ code, message, fatal, error }`. Codes: `connection-lost` (fatal, only without `autoReconnect`), `link-expired`, `reconnect-failed`, `join-rejected`, `nonce-exhausted`, `encoder-error`, `audio-encoder-error`, `audio-share-unavailable`, `send-failed`, `preview-failed`. |

### Rendering (`WatchHandle`)

The core hands you decoded `VideoFrame`s; two ways to consume them:

* `watch(key, { canvas })` / `handle.attachToCanvas(canvas)`: a `CanvasRenderer` draws **the newest decoded frame once per
  `requestAnimationFrame`** (`drawImage`, `alpha:false`, `desynchronized:true`) and closes every frame it supersedes, so a
  240 fps stream on a 60 Hz screen costs 60 draws, and on a 280 Hz screen every frame is shown. Decoding is never tied to the display.
* `watch(key, { onFrame })` / `handle.setFrameCallback(fn)`: you receive each `VideoFrame` and **must `close()` it** (hardware
  decoders have a small surface pool; holding frames stalls decoding).

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
| rate control CQP / QVBR | `bitrateMode: "constant"` / `"variable"` (CQP needs per-frame quantizers and Chromium 153 reports `bitrateMode: "quantizer"` unsupported for H.264; CBR is the closest predictable mode) |
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
| `jitter.js` | adaptive jitter buffer (RFC 3550 EWMA, growth cap, `catchUpSpeedPercent` shrink cap, decode-queue backstop) ported from the Electron `tile-video.js`, one timer drains all due chunks |
| `video-rx.js` | per-stream receive pipeline (`VideoReceiver`: loss gate -> jitter buffer -> `VideoDecoder`, reconfigures on codec change, recovers from decoder errors) and `CanvasRenderer` |
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
* **Dead relay detection takes as long as the QUIC idle timeout** (measured: about 21 s in Chromium 153, also when the relay
  is quit cleanly with `q` in its TUI) because no `CONNECTION_CLOSE` reaches the browser; then `reconnecting` begins and the
  restarted relay is rejoined automatically.
* **Loss recovery** (`lossRecovery: "resubscribe"`): a lost frame breaks the reference chain, so the receiver stops feeding
  deltas, and asks for a keyframe the only way a viewer can: `Unsubscribe` + `Subscribe` (the relay then sends the sender a
  `KeyframeRequest`), at most every 1.5 s and again while still waiting. `"wait"` just waits for the next scheduled keyframe.

## Known relay issues (found while building this; relay code is not part of this task)

1. **`JoinRejected` is lost to a connection drop.** After writing `JoinRejected` (e.g. `RoomFull`) `perform_join` returns and
   `handle_connection` drops the session immediately (`server/src/relay.rs`, the `None => return Ok(())` arm), without
   `finish()`ing the control stream or waiting for the peer to read it. Chromium never delivers the frame: the control stream
   errors with `WebTransportError: Connection lost.` and no close code/reason (`dev/scenarios.js`, "room is full"). The client
   therefore reports `ConnectError { code: "join-no-reply" }` ("the room may be full") instead of `JoinRejectedError`. A fix is to
   `ctrl_send.finish()` and wait briefly for `stopped()` (or send a WebTransport close with a reason) before dropping.
2. A relay that is stopped (even a clean quit via the TUI) is only noticed by browsers after the ~21 s idle timeout, because
   the endpoint is not closed with `CONNECTION_CLOSE` on shutdown. A graceful `endpoint.close()` on quit would make clients
   reconnect immediately.
