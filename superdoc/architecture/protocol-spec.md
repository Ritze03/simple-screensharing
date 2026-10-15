# Wire Protocol Specification

Binding contract for the **Rust relay server** and the **Rust + egui desktop client**.
This resolves the four load-bearing protocol gaps and the presence-roster inconsistency
from the brainstorm's "Final Verify Results" section into exact, implementable wire
detail. Everything in `superdoc/brainstorm/simple-screensharing-platform.md` is already
decided and is not re-litigated here.

**Transports.** Native clients speak plain `quinn`-to-`quinn` QUIC; browsers speak
WebTransport (HTTP/3) to the *same* UDP port. Everything in §2–§10 is transport-agnostic and
identical for both; §11 specifies the accept layer (ALPN dispatch, URL path, pinned
self-signed certificate) and the one wire constraint it adds (the 1000-byte datagram cap,
§6.3).

**Single source of truth in code.** The wire types, header layouts, constants and cert-hash
encoding are defined once, in the shared crate `protocol/` (`simple-screensharing-protocol`);
`client/src/protocol.rs` and `server/src/protocol.rs` only re-export it. Its
`wire_bytes_unchanged` test pins the postcard bytes of every enum variant, so changing this
spec's wire format means changing that crate and that test deliberately. `Why:` the relay,
the native client and the coming browser client must change in lockstep (one repo), and two
independently derived copies (the old layout) could silently drift.

Conventions used throughout:

- All multi-byte integers in **fixed binary headers** (media datagrams, preview streams)
  are **big-endian (network byte order)**.
- Control-plane structs are serialized with **`postcard`** (see §2); postcard's own
  little-endian/varint encoding governs those bytes — the big-endian rule applies only
  to the hand-rolled fixed headers.
- `[u8; N]` denotes N raw bytes. `u64`/`u32`/`u16`/`u8` are unsigned integers.
- "MUST" / "MUST NOT" are hard requirements; violating them breaks interop or security.

---

## 1. Transport model and channel map

A client (native, or a browser over WebTransport — §11) makes one outbound QUIC connection
to the relay (single UDP port). All communication rides that one connection, split into three planes over QUIC's native
primitives:

| Plane | QUIC primitive | Reliability | Direction | Carries |
| --- | --- | --- | --- | --- |
| **Control** | one **bidirectional** stream (opened by client on connect) | reliable, ordered | both | JOIN/LEAVE/SUBSCRIBE/presence/subscriber-count/keyframe-request (§4) |
| **Preview** | short-lived **unidirectional** streams (one per preview blob) | reliable, ordered | sender→relay, relay→member | ~30 s encrypted JPEG snapshots (§7) |
| **Media** | **datagrams** (unreliable) | drop-and-continue, no retransmit | sender→relay, relay→subscriber | fragmented encrypted video (H.264 / H.265 / AV1) and Opus frames (§6) |

**Stream-type discriminator.** Every *unidirectional* stream begins with a 1-byte
`StreamType` so the receiver knows what it is:

| Value | Meaning |
| --- | --- |
| `0x01` | Preview stream (§7) |
| `0x02`–`0xFF` | Reserved for future reliable uni-stream uses (do not send) |

The single control **bidi** stream needs no type byte (there is exactly one, opened
first; its role is implicit). Reserved value `0x00` is conceptually "control" for
symmetry but is never written on a uni stream.

---

## 2. Wire encoding for control messages

**Choice: `postcard`** for serializing control-plane structs/enums, framed on the
control stream by a **4-byte big-endian length prefix**:

```
control frame = u32 length (BE, byte count of payload) ‖ postcard(<message enum>)
```

**Why postcard over bincode:** postcard is explicitly specified as a stable, compact
*wire* format (varint-length-prefixed, `no_std`, documented byte layout), whereas
bincode's encoding changed incompatibly across major versions and is framed as an
in-process serialization format. For a contract two independent binaries must agree on
byte-for-byte, a format with a written spec is the safer pick. Both are one-line serde
derives; postcard's varints also make the small roster/delta messages a few bytes
smaller. Protobuf/JSON would be over-engineering for a friends-only hobby tool.

**Why a length prefix even though postcard is self-describing:** it lets the reader pull
exactly one message off the byte stream without a streaming deserializer, and bounds a
malformed message (reject any frame whose length exceeds a sane cap, e.g. 64 KiB — the
largest legitimate control message is an 8-member snapshot, a few hundred bytes).

The message-type tag is postcard's enum discriminant (the first varint of the payload) —
no separate type byte is needed.

Media datagrams do **not** use postcard: they use a hand-packed fixed binary header
(§6) to keep per-datagram overhead minimal.

---

## 3. Identifiers

### 3.1 `ConnId` (unified connection identifier = AES-GCM nonce prefix)

`ConnId` is a **`u64` (64-bit) value chosen fresh-random by the client at connection
time** (never persisted per device — see brainstorm Round 2). This single value serves
**three roles at once**:

1. The **nonce prefix** (`client_id`) of every AES-256-GCM encryption this connection
   performs (§5).
2. The **routing/presence identifier** used everywhere in the protocol: presence roster
   keys, SUBSCRIBE targets, and the source-id field in media/preview headers.
3. The relay's connection handle for its bookkeeping (§10).

**Why unify them:** the AES-GCM nonce is transmitted in cleartext anyway (nonces are
public by design), so reusing the 64-bit random `client_id` as the public connection id
leaks nothing and eliminates a second id space. It guarantees the media/preview headers
already carry exactly what a receiver needs to both *route* the frame to the right tile
and *build the nonce* to decrypt it.

**Collision handling:** the client presents its `ConnId` in `Join` (§4). If it already
exists among that room's members, the relay replies `JoinRejected{ ConnIdCollision }`
and the client regenerates and retries. With 64 random bits and ≤ 8 members, the
birthday collision probability is ~ C(8,2)/2⁶⁴ ≈ 1.5 × 10⁻¹⁸ per room — the retry path
exists only for completeness.

### 3.2 `RoomId`

`[u8; 16]` — a 128-bit high-entropy client-generated value (the `<ROOM-ID>` join-link
field, transported raw).

### 3.3 `StreamId`

`u8` — assigned by the **sender**, unique within that sender's connection, `0`-based. A
connection may run several concurrent streams (e.g. dual monitor + audio). Video and
audio are independently-announced streams distinguished by `StreamKind`. A `(ConnId,
StreamId)` pair globally identifies one media stream.

`u8` (256 streams/connection) is far beyond any realistic sharer.

### 3.4 `StreamKind`

`enum { Video = 0, Audio = 1 }` (u8 on the wire).

---

## 4. Control-plane message catalog

Two postcard enums, one per direction, both framed per §2 on the single control stream.

### 4.1 `ClientToRelay`

```
enum ClientToRelay {
    Join        { conn_id: u64, room_id: [u8;16], display_name: String },
    Leave,
    AnnounceStream { stream_id: u8, kind: StreamKind, label: String,
                     width: u16, height: u16 },
    StopStream  { stream_id: u8 },
    Subscribe   { target: u64 /*ConnId*/, stream_id: u8 },
    Unsubscribe { target: u64 /*ConnId*/, stream_id: u8 },
}
```

- **`Join`** MUST be the first message. `display_name` is UTF-8; the relay clamps it to
  64 bytes (truncating on a char boundary). `width`/`height` in `AnnounceStream` are the
  video source dimensions (`0`/`0` for audio) — they let the UI lay out a tile before the
  first frame decodes. `label` (e.g. `"Monitor 1"`) is UTF-8, clamped to 64 bytes.
- **`Leave`** is a graceful departure; an abrupt QUIC connection close MUST be treated by
  the relay as an implicit `Leave` (removes the member, stops their streams, emits the
  deltas).
- **`Subscribe` / `Unsubscribe`** target one `(ConnId, StreamId)`. Audio and video are
  subscribed independently; the *client convention* is to auto-subscribe a sender's audio
  stream alongside their video and to unsubscribe both together, but the protocol treats
  them as independent (no coupling is wired into the relay).

### 4.2 `RelayToClient`

```
enum RelayToClient {
    PresenceSnapshot { members: Vec<Member>, streams: Vec<StreamInfo> },
    PresenceDelta    { event: PresenceEvent },
    SubscriberCount  { stream_id: u8, count: u32 },   // to the stream OWNER only
    KeyframeRequest  { stream_id: u8 },               // to the stream OWNER only
    JoinRejected     { reason: RejectReason },
}

struct Member {
    conn_id: u64,
    display_name: String,
    stream_count: u8,     // 0 => not sharing; >0 => sharing this many streams
}

struct StreamInfo {
    owner: u64,           // ConnId
    stream_id: u8,
    kind: StreamKind,
    label: String,
    width: u16,
    height: u16,
}

enum PresenceEvent {
    MemberJoined  (Member),
    MemberLeft    { conn_id: u64 },
    StreamStarted (StreamInfo),
    StreamStopped { conn_id: u64, stream_id: u8 },
}

enum RejectReason { RoomFull, ConnIdCollision, Malformed }
```

- A successful `Join` is acknowledged **implicitly** by the relay immediately sending
  `PresenceSnapshot` (§8). A failed `Join` gets `JoinRejected` and the relay closes the
  connection (except `ConnIdCollision`, after which the client may retry `Join` with a
  fresh `conn_id` on the same connection).
- `RoomFull` is returned when the room already has 8 members (the accepted hard cap).
- `SubscriberCount` and `KeyframeRequest` are only ever sent to the connection that
  **owns** the named `stream_id`; a client applies them to its own streams (§9).
- `PresenceDelta` carries exactly one event; the relay emits one per state change.

`Member.stream_count` (not a bare boolean) directly satisfies gap 5: the client shows
**every** member (a lurker who never shares has `stream_count == 0` and still appears in
the roster), with a distinct "streaming" indicator driven by `stream_count > 0`, and can
show the count for multi-monitor sharers. `conn_id` disambiguates duplicate display names.

---

## 5. Encryption and nonce/counter discipline (security-critical)

One shared **AES-256-GCM** room key (the `<KEY>` join-link field), known only to clients.
**The relay never has the key and never decrypts anything.** GCM's nonce is **96 bits =
12 bytes**, laid out as:

```
 12-byte AES-GCM nonce
┌───────────────────────────┬───────────────────────┐
│ ConnId (client_id)        │ counter               │
│ u64, big-endian, 8 bytes  │ u32, big-endian, 4 B  │
└───────────────────────────┴───────────────────────┘
  bytes 0..8                   bytes 8..12
```

**8 + 4 = 12 bytes exactly**, matching GCM's nonce size with no truncation or padding.

Concretely: `nonce = conn_id.to_be_bytes() ‖ counter.to_be_bytes()`.

### 5.1 The single per-connection counter

There is **exactly one** monotonically increasing `u32` counter per connection, **shared
across everything that connection encrypts**: every video stream (including all
simultaneous streams of a multi-monitor sender), the audio stream, and every ~30 s
preview blob. Every AEAD encryption — of one whole frame, one audio packet, or one
preview — consumes **the next counter value**, and that value is never reused for any
other encryption on that connection.

- Start at `0`. Each encryption does an **atomic fetch-and-increment**: the returned
  prior value is that encryption's counter; the counter must be a single shared atomic
  (e.g. `AtomicU32`) because a dual-monitor sender runs multiple encoder threads plus an
  audio thread plus the preview task all drawing from it concurrently. A per-thread or
  per-stream counter would be a **critical bug** (two encryptions could share a
  `(ConnId, counter)` pair → GCM nonce reuse → catastrophic loss of confidentiality and
  integrity).

**Why a single shared counter rather than per-plane domain-separated counters** (the
verify note floated domain separation): a single never-reused counter space makes each
`(ConnId, counter)` pair globally unique across all planes *by construction* — there is
nothing left to collide, so domain-separation tags are unnecessary and strictly simpler
to get right. It also gives the media reassembler a free, globally-unique frame id (§6).

### 5.2 Overflow behavior

The counter is a `u32` (~4.29 billion encryptions). Worst realistic case — dual-monitor
240 Hz (≈ 480 video frames/s) + Opus audio (≈ 50 packets/s) + previews — is ≈ 530
encryptions/s, so the space lasts ≈ 2³²/530 ≈ **~94 days of continuous streaming**:
practically unreachable in a normal session.

The counter **MUST NOT wrap.** The value `u32::MAX` (`0xFFFF_FFFF`) is the last legal
nonce; upon consuming it the connection **MUST close**. **Reconnecting yields a fresh
random `ConnId`, which is the reset mechanism** — a brand-new 96-bit nonce space. No
epoch/rekey machinery is built; reconnect is the whole story.

### 5.3 AEAD inputs

For each encryption the sender computes:

```
ciphertext_with_tag = AES256-GCM-Encrypt(
    key   = room_key,
    nonce = ConnId ‖ counter,          // 12 bytes, §5
    aad   = [version_byte, stream_id], // 2 bytes, see below
    plaintext = <media payload, §6.3 / preview payload>
)
```

Output is `plaintext.len() + 16` bytes (16-byte GCM tag appended). The receiver rebuilds
the identical nonce and AAD from the cleartext header fields and decrypts; a mismatch
(wrong stream_id, corrupted counter, tampered payload) fails the tag check and the frame
is dropped.

**AAD = `version ‖ stream_id` (2 bytes), MANDATORY and identical on both sides.** `ConnId`
and `counter` are already bound because they *are* the nonce, so re-authenticating them
would be redundant; `stream_id` is not in the nonce, so binding it as AAD prevents a
misbehaving relay from relabeling one of a sender's streams as another (e.g. serving
monitor 1's frame under monitor 2's tile). It costs two bytes and one line on each side.
The preview path uses the same AAD construction with its own `version`/`stream_id`.

---

## 6. Media datagram fragmentation and reassembly

### 6.1 Encrypt-once-then-fragment

A whole media frame (one H.264 / H.265 access unit, one AV1 temporal unit, or one Opus packet) is **encrypted with a
single AEAD operation** (§5.3), consuming **one** counter value, producing
`ciphertext_with_tag`. **Only then** is that byte blob split into datagram-sized
fragments. Encryption is per *frame*, never per *fragment*.

### 6.2 Media datagram header (fixed binary, unencrypted)

The header is cleartext so the relay can route on it **without the key**. All integers
big-endian.

```
 offset  size  field           type   notes
 ------  ----  --------------  -----   -----------------------------------------
   0      1    version         u8     = 0x01 for this spec (also AAD byte 0)
   1      1    flags           u8     bit0 = keyframe (IDR/IRAP, or AV1 key frame); bit1 = stalled (advisory,
                                      see below); bits2-7 reserved=0
   2      8    conn_id         u64    source ConnId = nonce prefix (bytes 0..8 of nonce)
  10      1    stream_id       u8     which of the sender's streams (also AAD byte 1)
  11      4    frame_id        u32    = the frame's counter value (nonce bytes 8..12)
  15      2    fragment_index  u16    0-based index of this fragment
  17      2    fragment_count  u16    total fragments for this frame
  19    ...    payload                this fragment's slice of ciphertext_with_tag
 ------  ----
  19 bytes total header
```

**1+1+8+1+4+2+2 = 19 bytes.**

**Why `frame_id` is the counter, not a separate field:** each frame is encrypted with
exactly one counter value, and the counter is globally unique per connection (§5.1), so
`(conn_id, frame_id=counter)` already uniquely identifies the frame and all its fragments
share it. This satisfies the "frame_id to group fragments" requirement for free and means
the reassembler's grouping key doubles as the decryption nonce suffix — no extra bytes,
no risk of the two disagreeing.

**`flags` bit1 = stalled (advisory only, added 2026-07-26).** Set by a sender whose capture
source is temporarily down: rather than stopping the stream, it re-sends its last cached IDR
at ~2fps with this bit set, so the viewer can draw a "waiting for capture…" placeholder over
the frozen frame instead of seeing a disconnect. **Purely cosmetic — receivers must never
act on it.** The `flags` byte is outside the AAD (which is `[version, stream_id]`, §5.3), so
this bit is unauthenticated and an on-path attacker can flip it freely; nothing that has a
security or protocol consequence (teardown, subscribe/unsubscribe, key handling, resource
release) may depend on it. What keeps that safe: a stalled sender keeps sending real,
authenticated frames and never sends `StopStream`, so the receiver's actual media state
never depends on this bit being truthful. Additive and backward-compatible — an
implementation that ignores it behaves exactly as before (it just sees a low-framerate
stretch), so **no version bump**. Bit0 (`keyframe`) has always had the same standing.

**Why `u16` for fragment_index/count:** a 40 Mbit/s keyframe can be hundreds of KB; at
~1150 payload bytes/fragment that is a few hundred fragments — `u8` (max 255) could
overflow on large IDRs, `u16` (65 535 fragments ≈ 75 MB) has ample headroom.

**How the relay routes without decrypting:** it reads `conn_id` (source) and `stream_id`
from the cleartext header, looks up `streams[(conn_id, stream_id)].subscribers` (§10),
and forwards the datagram **verbatim** (no header rewrite, no payload touch) to each
subscriber's connection. If there are no subscribers it drops the datagram. The relay
never needs `frame_id`, `fragment_index`, or the payload — those are purely for the
receiving client. (The relay SHOULD validate that `conn_id` matches the id registered for
the connection the datagram arrived on, to reject a client spoofing another's source id.)

### 6.3 Fragment sizing

The sender queries quinn's current `max_datagram_size()` for the connection and clamps it to
**`MAX_MEDIA_DATAGRAM_LEN = 1000` bytes** (header included, `simple-screensharing-protocol`
crate), then uses `fragment_payload_max = min(max_datagram_size, 1000) − 19 = 981`. It splits
`ciphertext_with_tag` into `ceil(len / fragment_payload_max)` fragments. The 16-byte GCM tag
is simply the tail of `ciphertext_with_tag` and is fragmented like any other bytes.

`Why 1000 and not the ~1160 quinn allows:` the relay forwards fragments verbatim — it has no
key, so it cannot re-fragment — which means the **sender** decides whether every receiver can
take its datagrams. A browser's `WebTransport.datagrams.maxDatagramSize` is smaller than raw
QUIC's: headless Chromium 153 reports **1024** application bytes against this relay (measured
by `server/src/browser_smoke.rs`), and the WebTransport spec drops anything larger silently.
Every sender — native (`client/src/fragment.rs`) and browser alike — therefore fragments to at
most 1000 bytes per datagram. Cost: ~18% more fragments on large frames; benefit: browsers can
send and receive at all. A browser should still compare `datagrams.maxDatagramSize` against
1000 at runtime and refuse to join if it is smaller.

### 6.4 Plaintext media payload layout (inside the AEAD)

The plaintext that gets encrypted is:

```
 offset  size  field   notes
   0      8    pts     u64, big-endian, capture timestamp in microseconds
   8    ...    codec   one video access unit (H.264 / H.265 Annex-B, or AV1 OBUs), or one Opus packet
```

**Video codecs: H.264, H.265 and AV1, with no codec tag on the wire.** The receiver works the
codec out from the bitstream: it waits for a keyframe access unit carrying parameter sets and
sniffs it (Annex-B start codes with H.264 vs H.265 parameter-set NAL headers, which are
disjoint, versus an AV1 OBU sequence header; `client/src/decode/codec.rs::sniff`). A later
keyframe that sniffs differently rebuilds the decoder. `Why no tag (decision D5):` a tag in the
header would change the protocol and the relay for no gain, and a tag can go stale when a
sender edits a destination to another codec mid-stream while the bitstream cannot. The relay
never looks at the payload, so it needed no change for H.265/AV1.

**Why an 8-byte PTS inside the ciphertext:** it gives receivers an A/V-sync timestamp
(the verify note flagged its absence) while keeping it authenticated by the GCM tag and
hidden from the content-blind relay. Being inside the encrypted region, it costs no
cleartext-header bytes. Receivers MAY ignore it (the accepted "glitches are fine"
tolerance) but SHOULD use it to align audio to video when both are present.

### 6.5 Receiver reassembly

Per `(conn_id, stream_id)` the receiver keeps a small reassembly map keyed by
`frame_id`. For each frame_id it allocates a `Vec` of `fragment_count` slots and places
each fragment by `fragment_index`. When all `fragment_count` slots are filled it
concatenates them, AEAD-decrypts with `nonce = conn_id ‖ frame_id` and `aad = [version,
stream_id]`, strips the PTS, and hands the codec bytes to the decoder.

**Stale-frame discard (matches the accepted drop-and-continue media policy — no
retransmission):** because counters increase monotonically, `frame_id` values observed on
a given `(conn_id, stream_id)` are strictly increasing. When a fragment arrives whose
`frame_id` is **newer** than an incomplete buffered frame for the same stream, the older
incomplete frame is **discarded** (a fragment was lost and will never arrive). Concretely,
keep at most the newest 1–2 in-progress frame_ids per stream and evict older incomplete
ones; also drop any buffer untouched for a short timeout (e.g. 200 ms) as a backstop
against a stream that stops entirely. Never block waiting for a missing fragment.

---

## 7. Preview plane (reliable, broadcast)

Previews are the ~30 s JPEG discovery thumbnails, produced by the **sender** (the relay
cannot, being content-blind), encrypted with the same room key and the same shared
counter (§5.1), and delivered reliably.

### 7.1 Preview stream layout

Each preview is one short-lived **unidirectional** stream, read to FIN (self-delimiting —
no length prefix needed):

```
 offset  size  field        notes
   0      1    StreamType   = 0x01 (preview)
   1      1    version      = 0x01
   2      8    conn_id      u64 BE, source ConnId = nonce prefix
  10      1    stream_id    u8, the VIDEO stream this previews (also AAD byte 1)
  11      4    counter      u32 BE, this preview's nonce suffix
  15    ...    ciphertext   AES-256-GCM(JPEG bytes) incl. 16-byte tag, until FIN
```

Header 15 bytes, then ciphertext until stream FIN. Nonce and AAD are built exactly as in
§5.3 (`version` here is the byte at offset 1). The plaintext is the raw JPEG (no PTS
prefix — previews are stills).

### 7.2 Flow and caching

1. **Sender → relay:** every ~30 s the sender opens a uni stream, writes the layout above,
   FINs it.
2. **Relay caches** the received bytes **verbatim as opaque ciphertext** in
   `streams[(conn_id, stream_id)].last_preview`, replacing any prior cached blob. **Why
   cache ciphertext, not plaintext:** the relay has no key and must stay content-blind; it
   stores and later replays the exact bytes it received, decrypting nothing. The cache
   exists purely so a *just-joined* member can see a thumbnail immediately instead of
   waiting up to 30 s for the next cycle (§8).
3. **Relay → every room member** (broadcast, no SUBSCRIBE required — previews are the
   discovery mechanism): the relay opens a uni stream to each member and replays the
   cached bytes. Previews go to everyone regardless of subscription state.

Because previews ride reliable streams, a lost packet yields a retransmit (a slightly
delayed thumbnail), not a discard — appropriate, since a dropped preview would otherwise
mean a 30 s-stale thumbnail.

---

## 8. Join-time state sync

Immediately after accepting a `Join` (and before any `PresenceDelta` for the new member),
the relay sends the newcomer a full snapshot so it starts with a complete picture:

1. **`PresenceSnapshot`** on the control stream, containing:
   - **`members`**: the complete room roster — **every** connected member (not just
     sharers), each as `Member { conn_id, display_name, stream_count }`. This is what makes
     silent lurkers visible and delivers the social-verification UI (gap 5 / the verify
     inconsistency).
   - **`streams`**: every currently active stream as `StreamInfo { owner, stream_id, kind,
     label, width, height }` — i.e. which connection is running which stream.
2. **Cached previews:** for each active video stream that has a cached preview, the relay
   then opens a preview uni stream (§7.1) to the newcomer replaying
   `last_preview` — opaque bytes, never decrypted by the relay.

After this snapshot, normal operation resumes: periodic ~30 s preview broadcasts, and
`PresenceDelta` events to existing members on future joins/leaves/stream start/stop. The
newcomer's own arrival is announced to everyone *else* via `PresenceEvent::MemberJoined`
**after** its snapshot is sent.

---

## 9. Relay → sender subscriber-count and keyframe signaling

When the relay processes a `Subscribe` / `Unsubscribe` (or removes a departing member's
subscriptions), it updates `streams[(owner, stream_id)].subscribers` and then signals the
**owner** of that stream:

- **`SubscriberCount { stream_id, count }`** on **every** change to the count. This drives
  the accepted idle optimization: the sender pauses/reduces main-stream encoding when
  `count == 0` and resumes when `count >= 1`. (Preview generation continues regardless —
  it is exempt from the idle pause.)
- **`KeyframeRequest { stream_id }`** on **every subscribe that adds a new subscriber** —
  both the `0 → 1` transition *and* a new viewer joining an already-watched stream.

**Why keyframe on every new subscriber, not only `0 → 1`:** video decoding (any of the three codecs) must begin at
a keyframe. A viewer who subscribes mid-GOP to an already-watched stream has no IDR reference
and would see nothing/garbage until the next scheduled 2 s keyframe — a bad experience
right after a preview enticed them to click. Forcing an immediate IDR on each new
subscriber fixes this; the cost is one larger frame, shared to all current subscribers,
which is negligible at friend scale (≤ 8 members, rarely many simultaneous joins).

To avoid an IDR burst when several viewers join at once, the **sender** coalesces
requests: it ignores a `KeyframeRequest` if it already emitted an IDR within the last
~100 ms (a sender-side detail, not a wire requirement). The `0 → 1` case additionally
triggers resume-from-idle before the forced IDR.

The relay sends both messages only to the stream's owner; other members never receive
another stream's subscriber count.

---

## 10. Relay bookkeeping data model

Sketch (not full Rust) sufficient for a server implementer:

```
Relay {
    rooms: HashMap<RoomId, RoomState>,          // RoomId = [u8;16]
}

RoomState {
    room_id: RoomId,
    connections: HashMap<ConnId, ConnState>,    // ConnId = u64; ≤ 8 entries
    streams:     HashMap<(ConnId, StreamId), StreamState>,
}

ConnState {
    conn_id: ConnId,
    display_name: String,                       // clamped to 64 bytes
    control_tx: <handle to write RelayToClient on this conn's control stream>,
    connection: <quinn::Connection handle, for opening uni streams / datagrams>,
    subscriptions: HashSet<(ConnId, StreamId)>, // streams THIS conn watches
}

StreamState {
    owner: ConnId,
    stream_id: StreamId,
    kind: StreamKind,
    label: String,
    width: u16,
    height: u16,
    subscribers: HashSet<ConnId>,               // count = subscribers.len()
    last_preview: Option<Bytes>,                // opaque cached ciphertext (§7.2)
}
```

Key operations:

- **Join:** reject if room has 8 members (`RoomFull`) or `conn_id` already present
  (`ConnIdCollision`); else insert `ConnState`, send `PresenceSnapshot` + cached previews
  (§8), then broadcast `MemberJoined` to others.
- **Leave / connection close:** remove the `ConnState`; for each stream it owned, remove
  from `streams` and broadcast `StreamStopped`; remove its `conn_id` from every other
  stream's `subscribers` (emitting `SubscriberCount` to those owners); broadcast
  `MemberLeft`.
- **AnnounceStream / StopStream:** insert/remove `StreamState`; update the owner's
  `Member.stream_count`; broadcast `StreamStarted` / `StreamStopped`.
- **Subscribe / Unsubscribe:** update `subscribers` and the subscriber's `subscriptions`;
  send `SubscriberCount` to the owner (and `KeyframeRequest` on a new subscriber, §9).
- **Media datagram in:** parse the 19-byte header, validate source `conn_id`, forward
  verbatim to `streams[(conn_id, stream_id)].subscribers` (§6.2). Drop if no subscribers
  or unknown stream.
- **Preview uni stream in:** parse the 15-byte header, cache bytes in `last_preview`,
  broadcast to all room members (§7.2).

---

## 11. WebTransport leg (browsers) and the accept layer

Everything above (message catalog, headers, encryption, reassembly, bookkeeping) is
transport-agnostic. The three planes map 1:1 onto WebTransport primitives — control = a WT
**bidirectional stream**, previews = WT **unidirectional streams**, media = WT **datagrams** —
so the same framing (§2), the same fixed binary headers (§6/§7), the same nonce/AAD rules, and
the same relay logic apply unchanged. Only the connection-accept layer differs.

### 11.1 One port, two ALPNs

| Peer | ALPN | Session |
| --- | --- | --- |
| Native desktop client | `ssshare/1` | raw QUIC |
| Browser | `h3` | HTTP/3 + WebTransport |

The relay offers both in the TLS handshake and dispatches on the negotiated one
(`server/src/relay.rs::accept_session`). A browser must request
**`https://<host>:<port>/ssshare`**; any other path gets HTTP 404. Both kinds of peer end up as
the same `web_transport_quinn::Session` inside the relay, so native and browser members are
equal in rooms and forwarding.

### 11.2 What a browser must do

1. `new WebTransport("https://host:port/ssshare", { serverCertificateHashes: [{ algorithm:
   "sha-256", value: <32 bytes> }] })` — the hash comes from the join link (§11.4); omit
   `serverCertificateHashes` when the relay uses a CA certificate.
2. **Ordering assumption:** open the control stream with `createBidirectionalStream()` and
   write `Join` to it immediately. The relay treats the **first bidi stream** as control and
   its first message must be `Join`. (A bidi stream only becomes visible to the relay once the
   peer writes to it.) The HTTP/3 CONNECT stream is consumed by the WebTransport layer and is
   not seen here.
3. Previews: `createUnidirectionalStream()` per blob, §7.1 layout, close to FIN. Incoming
   previews arrive on `incomingUnidirectionalStreams`, one per blob.
4. Media: `datagrams.writable` / `datagrams.readable`, one fragment per datagram, **at most
   1000 bytes** (§6.3). Datagrams are unordered and lossy as ever; the receiver drops a frame
   with a missing fragment.
5. Limits worth knowing: control frames ≤ 64 KiB; previews ≤ 2 MiB (relay-side cap); idle
   timeout 60 s with relay keep-alives every 15 s.

### 11.3 Certificate: pinned, self-signed, auto-rotating

Browsers only accept a hash-pinned certificate if it is **ECDSA P-256** and valid for **at
most 14 days**. By default the relay generates exactly that: a self-signed P-256 cert valid
**13 days** (`notBefore` back-dated 1 h for clock skew, inside the 13), persisted in
`./state/{cert.pem,key.pem,cert.meta}` (so a restart keeps the hash), and **replaced when fewer
than 2 days remain** by swapping the endpoint's server config. Existing connections keep the
certificate they were set up with and are never dropped by a rotation; only *new* handshakes see
the new certificate. With `--cert <pem> --key <pem>` the relay serves a CA-issued chain
instead (e.g. Let's Encrypt): browsers verify it normally, no hash is needed, nothing is rotated
(the files are re-read hourly if their mtime changed). Implementation: `server/src/cert.rs`.

### 11.4 Join link: 4th field (certhash) and 5th field (name)

```
server|room-id|key                     (CA-certificate relay, or native-only use)
server|room-id|key|certhash            (self-signed relay; required for browsers)
server|room-id|key|certhash|name       (any of the above plus the room's display name)
server|room-id|key||name               (CA relay: empty certhash field, name present)
```

All forms are valid everywhere; a parser that knows only 3/4 fields rejects the 5-field forms
(both shipped clients, native and browser, parse all forms; browser: task #69, 2026-10-15).

`certhash` = SHA-256 of the relay's **leaf certificate DER**, **base64url, no padding** (43
characters), same encoding as `key`. An **empty** `certhash` field means "no hash" (that is how a
name is attached to a link for a CA relay). The native client does
not enforce it (it logs a mismatch and carries on — the room key already protects the media,
and a saved link would otherwise break at every rotation) but records the hash of the cert the
relay actually presented so links it generates (`ScreenshareClient::current_cert_hash()` /
`invite_link()`) work in a browser.

`name` (5th field, optional) = the room's display name as the sender knows it, **UTF-8,
percent-encoded**: every byte except the RFC 3986 unreserved set `A-Z a-z 0-9 - . _ ~` is written
`%XX` (uppercase hex), so `|` and `%` can never appear raw and the field is plain ASCII. This is
what JS `encodeURIComponent` produces, apart from `! ' ( ) *`, which `encodeURIComponent` leaves
raw and the native encoder escapes; a decoder must accept both (JS `decodeURIComponent` does).
Native: `net::format_join_string_full` / `parse_join_string` (`ParsedJoinString::name`).
Decode rules, all lenient because the name is cosmetic: a missing or empty field, a malformed
`%` sequence, invalid UTF-8, or a name that is empty after trimming all mean **no name** (the link
is still accepted); the decoded name is trimmed, control characters are dropped and it is cut at
**64 characters** (`net::MAX_LINK_ROOM_NAME_CHARS`). The name is not part of the room's identity
(identity is server + room-id + key, `rooms::room_identity`) and is not stored in the saved
join string: it only pre-fills the add-room dialog's name field, where the user can still edit it.
Nothing about the name is secret-relevant: the link already carries the room key and the relay
never sees the link.

`Why:` user request 2026-10-15: copy-join-link should carry the room name, and pasting it into
the add-room dialog should fill the name field (still editable before adding). `Why percent-encoding
and not base64url:` the link stays human-readable, and the browser mirrors it with stdlib
(`encodeURIComponent` / `decodeURIComponent`).

`Browser:` `docs/js/core/joinlink.js` (`parseJoinLink`, `formatJoinLink`, `encodeLinkName`,
`decodeLinkName`) implements the same rules, with `!'()*` escaped on encode so both clients produce
byte-identical links; `docs/dev/tests.js` checks the native vectors (`a|b`, `100%`, `%41`, umlauts,
emoji, `a|b|c|d`, bad `%4`/`%zz`/`%FF%FE`/`%0A`). Before task #69 the browser split at the first three
`|` and rejected `hash|name` as an invalid cert hash.

`Why links go stale:` a pinned hash is only valid while the relay presents that exact
certificate, i.e. until its next rotation (a link made right after a rotation is good for ~11
days; one made just before is good for days less). A browser whose pinned hash no longer
matches fails the handshake and should ask for a fresh link. A CA certificate avoids this
entirely.

### 11.5 Constants, in one place

`simple-screensharing-protocol`: `NATIVE_ALPN = b"ssshare/1"`, `WEBTRANSPORT_ALPN = b"h3"`,
`WEBTRANSPORT_PATH = "/ssshare"`, `MAX_MEDIA_DATAGRAM_LEN = 1000`, plus
`cert_hash_of_der` / `encode_cert_hash` / `decode_cert_hash`.
