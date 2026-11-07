# Video: selective retransmission (NACK)

How a viewer asks for the media fragments (or whole frames) it is missing instead of discarding the frame; the relay side is in `features/relay-transport.md`, the wire format in `architecture/protocol-spec.md` section 6.6. Part of the video pipeline hub, `superdoc/features/video-pipeline.md`.

## Selective retransmission (NACK, #104, 2026-11-03)

**The problem it fixes.** Under #101 a frame missing any fragment is thrown away and the only cure is
an L1 keyframe request (~1.2-1.5 s, and the requested IDR is itself ~1000 fragments that die the same
way). Field case 2026-11-02: 1080p120 8.3 Mbit/s H.264 on AMD, IDRs of ~1 MB, at 0.1 % loss an IDR arrives
whole 37 % of the time, `a keyframe never completed (981/984 fragments arrived)` 295 times, recoveries of
30-100 s. #102 made IDRs smaller and rarer; the encoder's natural GOP IDRs are still big. Paths are short
(16-30 ms), so re-sending the three missing fragments beats a new keyframe by an order of magnitude.

**Design, with the why of every choice: `architecture/protocol-spec.md` section 6.6.** In one paragraph:
NACKs are < 19 byte datagrams (every older relay, client and browser drops them as undersized, so
nothing can break; a control-stream variant would have killed old connections), a Hello/HelloAck
datagram handshake with the relay switches the viewer's NACK mode on (so a new client on an old relay
behaves exactly as before), the relay answers from a 2 s / 8 MiB per-stream cache of the datagrams it
forwarded and forwards only what it lacks to the sender, whose 2 s ring re-sends it with the retransmit flag
(media `flags` bit2; the relay delivers those only to the viewers that asked). Retransmits are the original
datagrams, byte for byte: nothing is re-encrypted, no nonce is used twice. Audio is not repaired.

**Viewer side (`fragment::Reassembler`, NACK mode; driven by `net.rs::datagram_reader_loop`).**

- *Detect*: a missing fragment with >= 2 later fragments of its frame behind it, or any missing fragment
  once the frame has been quiet 20 ms (a lost tail). *Ask*: one Request per contiguous run; a fragment is
  asked again after `R = clamp(2 x rtt + 60 ms, 100, 300 ms)`, at most 3 times; a frame with > 128 suspect
  fragments is abandoned without asking (congestion, not loss).
- *Order*: while a frame is being repaired, later *complete* frames of that stream are held and released in
  order once it completes or is abandoned. Delivering N+1..N+5 and then N would corrupt the picture
  (and a late IDR after its deltas is worse than a short delay). The cost is up to ~4R (440 ms at 25 ms RTT)
  of added latency on the stream, only around a frame that cannot be repaired; a repaired frame costs
  about one RTT.
- *Give up* (rounds exhausted, `Miss`, 1 s hard limit): the frame is discarded and reported as `FrameLoss`
  exactly as before, so the L1 recovery above takes over unchanged. The two compose: NACK makes the
  `FrameLoss` evidence rare, L1 stays the safety net.
- *Duplicates* of delivered/abandoned frames are ignored (last 256 frame ids per stream), so a retransmit
  that races the original cannot open a ghost frame (an old reassembler would report that as loss).
- `SSS_DISABLE_NACK=1` (native) turns the whole thing off for that client: no Hello, no NACKs, no re-sends
  (A/B measurements, field rollback). The relay's own switch is `SSSHARE_NACK=0`.
- Logs: viewer and sharer log `NACK activity in the last 5 s requests=.. fragments_requested=..
  fragments_recovered=.. frames_recovered=.. frames_abandoned=.. fragments_resent_for_others=..`, the relay
  `NACK activity in the last 10 s requests=.. from_cache=.. forwarded_to_owner=.. miss_replies=..
  owner_resends_relayed=..`.

**Rig results** (headless rig, UDP impairment proxies: random loss on **both** legs at once, sharer -> relay
and relay -> viewer; H.264 1080p120 8300 kbps Quality, a detailed scrolling pattern (IDRs 100-500 kB, median
~300 kB = 100-500 fragments), 100 s per cell; NACK off = the same binaries with `SSS_DISABLE_NACK=1` on both
clients; evidence `evidence/` in the task scratchpad):

| random loss, both legs | NACK | `never completed` | L1 requests | frozen total (longest) | fragments asked / recovered | repaired from relay cache / from sharer |
|---|---|---|---|---|---|---|
| 0.2 % | off | 38 | 42 | 1.4 s (1.4 s) | - | - |
| 0.2 % | **on** | **0** | **0** | **0 s** | 335 / 335 | 161 / 174 |
| 0.4 % | off | 31 | 37 | 4.0 s (1.7 s) | - | - |
| 0.4 % | **on** | **0** | **0** | **0 s** | 759 / 755 | 391 / 367 |
| 1 % | off | 57 | 55 | 19.7 s (5.3 s) | - | - |
| 1 % | **on** | **0** | **0** | **0 s** | 1818 / 1792 | 892 / 915 |

(Frozen = the viewer tile's pixels unchanged for 0.8 s. `frames_abandoned` was 0 in every "on" cell: every frame
that lost fragments was repaired. A first "on" run with a 60 ms floor on the retry interval had one
incident at 0.4 % (2 L1, 3.9 s frozen): a keyframe repair that had to cross the sharer's link at ~90 ms RTT
was abandoned; the floor is now 100 ms and the rerun was clean. After merging master a 60 s run at 1 % was clean
too: 0 / 0 / 0 s, 1185 asked, 1165 recovered.)

Extra bandwidth is the retransmitted fragments: at 1 % loss ~1800 fragments in 100 s = ~1.8 MB = ~0.15
Mbit/s (~2 % of the 7 Mbit/s stream, i.e. about the loss rate), plus 18-byte requests (~1800, negligible).
Of the repairs about half came from the relay cache (relay -> viewer loss) and half from the sender
(sharer -> relay loss), as expected from loss on both legs.

**Mixed versions** ("old" = the pre-#104 binaries built from `facc373`; 60 s, 1 % loss on both legs; frozen total,
`never completed`):

| viewer | relay | sharer | result |
|---|---|---|---|
| old | new | new | unchanged from old/old: 10.5 s frozen, 30 never completed; relay saw 0 requests, sent 0 duplicates |
| new | old | new | unchanged: no HelloAck, no NACK traffic at all, 24.7 s frozen, 40 never completed (the loss run varies) |
| new | new | old | relay cache repaired the viewer leg (347 fragments from cache), sharer-leg losses answered `Miss` at once (330) and fell back to L1: 1.8 s frozen, 25 never completed (vs ~10 s for old/old) |
| old | old | old | baseline: 10.7 s frozen, 34 never completed |
| **browser** new | old (`facc373`) | browser new | #111, 0.3 % both legs, 30 s, ~1 MB IDRs: no HelloAck (`nackStats().active` false on both), 0 requests, **0 fps / 246 loss events: identical to `?nack=0`** (a browser viewer never recovers at this loss without NACK); nothing got worse |
| **browser** new viewer | new | browser sharer with `?nack=0` (never says Hello) | #111, 0.5 % sharer leg: the relay answers `Miss` at once (219 fragments asked, 201 frames abandoned, 0 repaired), L1 as before, 0 fps / 190 loss events: same as NACK off |
| **browser** viewer with `?nack=0` | new | browser sharer new | #111, 0.5 % viewer leg: never says Hello, so never gets a retransmit or a duplicate: 0 fps / 199 loss events, same as NACK off |
| browser viewer new | new | native sharer (new) | #111 rig, 1 % on the browser's leg: 60 fps, 0 loss events (off: 2.7 fps, 306 loss events); relay cache repairs |
| native viewer (new) | new | browser sharer new | #111 rig, 0.5 % on the browser's leg: 0 `never completed`, ~300 fragments repaired by the browser's ring; with the browser sharer on `?nack=0`: 24 `never completed`, 23 L1 recoveries, "Recovering..." at 0 fps |

The old peers need no change and see no NACK traffic (undersized datagrams are dropped), which is the point
of the datagram design.

**The browser** (#111, 2026-11-03) now speaks it too, both as viewer and as sharer: `features/website/nack-and-big-keyframes.md` "Selective
retransmission in the browser" (design differences are mechanical: no `connection.rtt` in Chromium 153, so a fixed 150 ms
RTT; re-sends bypass the #103 pacing loop) with its own loss tables and the mixed rows above. Without NACK a browser
viewer decoded **0 fps** at 0.1 % random loss with ~1 MB IDRs (no keyframe ever completes); with it 49.4 fps and 0 loss events up
to 1 %.

**Not verified**: a real internet path
(burst loss, reordering beyond 2 fragments, RTT jitter), more than one viewer of the same stream (unit tests
only), audio.

### A frame lost whole (browser viewer, row 150, 2026-11-05)

NACK repairs a frame it knows about. A frame with no fragment left is known to nobody, and `frame_id` cannot show it (nonce counter, shared by every stream). Native still has this blind spot (`fragment.rs` `FrameLoss`). In a browser it can be fatal: Chromium's AV1 decoder fails on such a frame (measured), and WebKit closes the decoder on one undecodable frame. So the browser follows the picture numbering in the bitstream (`codec.js` `RefChain`) and treats a jump like any loss. Detail, evidence and what is still inference: `features/website/phone.md` "Phone decoder failures".

**Repaired by its id (row 151, 2026-11-07).** `Why:` the user: "There are still hiccups when watching from mobile." Row 150 turned decoder failures into a wait for the next keyframe, about 70 times in 6 minutes on an iPhone, and keyframes can be 1-2 s apart. A gap now costs one round trip instead:

- **Ask.** `RefChain` shows the jump at the next frame, whose `frame_id` is above the last one by `idStep`. The ids in between are the only trace of what is missing. `VideoReceiver` asks for each (at most 32) with a Request `first 0, count 256`, the most a relay or sender answers, because the size of a frame nobody saw is unknown. A bigger frame shows its real count with its first fragment and `Reassembler` finishes it as a normal repair.
- **Skipped ids that are not this stream's.** `frame_id` is shared with audio and previews, so some of the ids asked for have no video frame behind them. Nothing depends on them: the repair ends as soon as `RefChain` accepts the held frame, whichever ids stay unanswered. `Why not ask for every id step:` with audio on, nearly every step skips an id; only a jump in the picture numbering says a frame is missing.
- **Hold.** Later frames wait in `held`, sorted by `frame_id`, and each try goes through `RefChain.step`, which does not note a frame that shows a jump. A repaired frame goes back to its place. `Why here and not in the reassembler:` only the decrypted bitstream shows the jump.
- **Give up.** Two rounds, `min(150 ms, nackRetryInterval)` apart (so at most 300 ms), a `Miss` with `first 0` for every id, more than 32 ids, or no NACK: the keyframe wait of row 150, same event. A keyframe during a repair ends it. `Why 150 ms:` the fragment repair's interval is 300 ms where the browser reports no RTT, and a failed repair delays the keyframe request.
- **Tail of the range.** The relay answers what it has and treats the rest of the 256 as missing: it forwards that to the owner (who has nothing, and says nothing) or, for an owner without Hello, answers `Miss` with `first` past the frame's end. `nackMiss` ignores such a Miss. No relay or native change: `server/` and `protocol/` are unchanged since the deployed `e3fa711`.
- **Order at the decoder.** A frame with a `frame_id` at or below the last one taken never goes on (`stale`). Before, a delta arriving after a newer keyframe was decoded: `RefChain` only reports a jump for `idStep > 1`, a negative step passed, and the timestamp went back at `decode()`.

**Are the gaps real?** Yes, by the 2026-11-07 log and the encoder source (`gstvaav1enc.c`): `order_hint = frame_num`, the index of the input frame since the keyframe, and hidden frames share a temporal unit with the next shown frame, so a stream with nothing lost steps by 1. A jump with `idStep` 1 switches the check off for good, and it stayed on through about 70 gaps, each with an unseen id, and in the excerpt `id +N` is never below the hint step (`gap 4->8, id +4` is exact: 3 frames, 3 ids). Lines like `gap 0->2` right after a keyframe are plain losses (the keyframe's hint is 0). `gap 14->0, id +3` is the one exception and is not explained: this encoder flags every hint 0 as a keyframe, and only an unflagged frame gets here. It costs what any failed repair costs.

**Why so many?** Not proven, no phone here. Deltas of a quiet screen are 1.5-2 kB, 2 datagrams, so a frame vanishes whole when two neighbouring datagrams do; NACK repairs every partial loss, so whole frames are what was left to see. `ptsback` was 4 at 17 gaps: wire PTS is strictly monotonic (`advance_pts`), so each of those is a frame that arrived behind a newer one, and about one gap in four may be such a late frame rather than a lost one (inference), which the repair also fixes, since the original lands in its place. Nothing in the viewer accumulates over time. To tell the rest apart: `dropIn` (the browser's own drops), `bytes` of `repaired` (small frames or bursts), and the relay's `viewer send queue full` line (quinn evicting).

**Tests.** `tests.js` (strict decoder: repair for H.264 and AV1, several ids, unanswered, Miss, keyframe during a repair, stale delta, tail Miss); `scenarios.js` "frame lost whole (h264), relay -> viewer / sharer -> relay": real relay, every frame repaired, nothing skipped, no keyframe wanted. A phone has not run this.
