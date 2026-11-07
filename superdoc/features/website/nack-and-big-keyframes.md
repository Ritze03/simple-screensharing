# Website: NACK and big keyframes

Selective retransmission and big keyframes in the browser sharer. Part of the browser client, see `superdoc/features/website.md`.

## Big keyframes in the browser sharer (task #103, 2026-11-03)

The native sharer's #102 (`features/stream/capture-restart.md` "Big keyframes") applies here too: a frame is lost if any
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

The browser half of #104 (design and the why of every choice: `architecture/protocol-spec.md` 6.6, `features/video/selective-retransmission.md`
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
see `features/video/selective-retransmission.md` "Mixed versions" for the table with the native rows.

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
