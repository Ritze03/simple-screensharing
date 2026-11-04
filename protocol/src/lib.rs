//! Shared wire protocol types and constants for the relay server and the
//! client, per `superdoc/architecture/protocol-spec.md`.
//!
//! This is the single source of truth: `client/src/protocol.rs` and
//! `server/src/protocol.rs` both re-export it. **The wire format must not
//! change** — postcard is order-sensitive, so enum variant order and field
//! order here are frozen; see the `wire_bytes_unchanged` test.
//!
//! Two encodings coexist here, per spec §2:
//!   - Control-plane messages (`ClientToRelay` / `RelayToClient` and their
//!     nested types) are `postcard`-serialized and framed with a 4-byte
//!     big-endian length prefix (`encode_control_frame` / `decode_control_payload`).
//!   - The media datagram header (§6.2) and preview stream header (§7.1) are
//!     hand-packed fixed binary layouts, big-endian, deliberately NOT postcard
//!     (they must stay compact and are read by the relay without a key).

use serde::{Deserialize, Serialize};

mod cache;
pub use cache::{CACHE_MAX_BYTES, CACHE_TTL, FragmentCache};

/// Wire protocol version. Appears as AAD byte 0 for every AEAD operation and
/// as the `version` field in both fixed binary headers.
pub const PROTOCOL_VERSION: u8 = WIRE_VERSION;

/// Wire version byte used in media/preview headers and as AAD byte 0
/// (spec §5.3, §6.2, §7.1). Same value as [`PROTOCOL_VERSION`]; the relay
/// historically called it `WIRE_VERSION`.
pub const WIRE_VERSION: u8 = 0x01;

/// Maximum UTF-8 byte length for `display_name` / stream `label`, clamped
/// per spec §4.1 (truncated on a char boundary by the relay).
pub const MAX_NAME_LEN: usize = 64;

/// Hard cap on room membership (spec §8 / §10 — `RejectReason::RoomFull`).
pub const MAX_ROOM_MEMBERS: usize = 8;

/// Unified connection identifier — also the AES-GCM nonce's `client_id`
/// prefix (spec §3.1). Chosen fresh-random by the client per connection.
pub type ConnId = u64;

/// 128-bit room identifier, transported raw (spec §3.2).
pub type RoomId = [u8; 16];

/// Sender-assigned, per-connection stream identifier (spec §3.3).
pub type StreamId = u8;

/// Discriminates a stream's media type (spec §3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamKind {
    Video = 0,
    Audio = 1,
}

// ---------------------------------------------------------------------
// §4 Control-plane message catalog
// ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ClientToRelay {
    Join {
        conn_id: u64,
        room_id: RoomId,
        display_name: String,
    },
    Leave,
    AnnounceStream {
        stream_id: u8,
        kind: StreamKind,
        label: String,
        width: u16,
        height: u16,
    },
    StopStream {
        stream_id: u8,
    },
    Subscribe {
        target: u64,
        stream_id: u8,
    },
    Unsubscribe {
        target: u64,
        stream_id: u8,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RelayToClient {
    PresenceSnapshot {
        members: Vec<Member>,
        streams: Vec<StreamInfo>,
    },
    PresenceDelta {
        event: PresenceEvent,
    },
    SubscriberCount {
        stream_id: u8,
        count: u32,
    },
    KeyframeRequest {
        stream_id: u8,
    },
    JoinRejected {
        reason: RejectReason,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Member {
    pub conn_id: u64,
    pub display_name: String,
    /// 0 => not sharing; >0 => sharing this many streams.
    pub stream_count: u8,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamInfo {
    pub owner: u64,
    pub stream_id: u8,
    pub kind: StreamKind,
    pub label: String,
    pub width: u16,
    pub height: u16,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PresenceEvent {
    MemberJoined(Member),
    MemberLeft { conn_id: u64 },
    StreamStarted(StreamInfo),
    StreamStopped { conn_id: u64, stream_id: u8 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RejectReason {
    RoomFull,
    ConnIdCollision,
    Malformed,
}

// ---------------------------------------------------------------------
// §2 Control-frame postcard + 4-byte BE length-prefix framing
// ---------------------------------------------------------------------

/// Largest legitimate control message is a small roster snapshot; reject
/// anything claiming to be bigger (spec §2).
pub const MAX_CONTROL_FRAME_LEN: u32 = 64 * 1024;

/// Serialize `msg` with postcard and prepend the 4-byte BE length prefix.
/// Ready to write directly to the control stream.
pub fn encode_control_frame<T: Serialize>(msg: &T) -> anyhow::Result<Vec<u8>> {
    let payload = postcard::to_stdvec(msg)?;
    let len = u32::try_from(payload.len())
        .map_err(|_| anyhow::anyhow!("control message too large to frame"))?;
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Deserialize a single postcard payload (the bytes *after* the length
/// prefix has already been read and validated by the caller).
pub fn decode_control_payload<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> anyhow::Result<T> {
    Ok(postcard::from_bytes(bytes)?)
}

// ---------------------------------------------------------------------
// §1 Stream-type discriminator for unidirectional streams
// ---------------------------------------------------------------------

/// Unidirectional stream type: a preview blob (spec §7).
pub const STREAM_TYPE_PREVIEW: u8 = 0x01;
/// Unidirectional stream type: one batch of client log lines for the relay to store (spec §6.7).
/// Sent only after the relay answered [`DIAG_TAG_ACK`].
pub const STREAM_TYPE_DIAG: u8 = 0x02;

// ---------------------------------------------------------------------
// §6.2 Media datagram header (19 bytes, fixed binary, cleartext)
// ---------------------------------------------------------------------

pub const MEDIA_HEADER_LEN: usize = 19;

/// bit0 of the `flags` byte: this frame is an H.264 keyframe (IDR).
pub const MEDIA_FLAG_KEYFRAME: u8 = 0b0000_0001;

/// bit1 of the `flags` byte: **advisory only** — the sender's capture source
/// is currently down (being switched/restarted) and this frame is a re-send of
/// the last cached IDR rather than freshly captured content. Set by
/// `capture.rs`'s stall protocol while a `DestinationState::Stalled`
/// destination keeps its stream alive at ~2fps; consumed by `decode.rs`
/// (`StreamDecoder::is_stalled`) purely so a later UI stage can draw a
/// "waiting for capture…" placeholder over the frozen frame.
///
/// `Why:` **this bit is unauthenticated and must stay cosmetic.** The AEAD's
/// AAD in `net.rs`'s `aead_encrypt`/`aead_decrypt` is exactly
/// `[PROTOCOL_VERSION, stream_id]` — the `flags` byte of [`MediaHeader`] is
/// *not* covered by it (and cannot be: the relay reads and forwards the header
/// without holding the room key, so the header is cleartext by design, spec
/// §6.2). Anyone on the path can therefore flip this bit on any datagram
/// without breaking decryption. That is fine for a UI hint — the worst outcome
/// is a spurious or missing spinner — but it means this bit must **never**
/// drive teardown, subscribe/unsubscribe, key rotation, resource release, or
/// any other decision with a security or protocol consequence. Note the
/// deliberate asymmetry with the stall protocol itself: a stalled destination
/// keeps sending real, authenticated frames and never sends `StopStream`, so
/// the viewer's actual media state is driven entirely by authenticated data
/// even if this bit is forged. [`MEDIA_FLAG_KEYFRAME`] sits in the same
/// unauthenticated byte and has the same standing (a forged keyframe bit only
/// costs a failed decode).
pub const MEDIA_FLAG_STALLED: u8 = 0b0000_0010;

/// bit2 of the `flags` byte: **retransmit** (#104). Set by a sender that re-sends a fragment because
/// a NACK (below) asked for it; the datagram is otherwise byte-identical to the original. The relay
/// routes such a datagram only to the viewers that recently NACKed that stream (never to every
/// subscriber, who would see a duplicate), and receivers otherwise ignore the bit. Unauthenticated
/// like the other flags and with the same standing: nothing may depend on it being truthful.
pub const MEDIA_FLAG_RETRANSMIT: u8 = 0b0000_0100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaHeader {
    pub version: u8,
    pub flags: u8,
    pub conn_id: ConnId,
    pub stream_id: StreamId,
    /// The frame's counter value — also nonce bytes 8..12 (spec §6.2).
    pub frame_id: u32,
    pub fragment_index: u16,
    pub fragment_count: u16,
}

impl MediaHeader {
    pub fn is_keyframe(&self) -> bool {
        self.flags & MEDIA_FLAG_KEYFRAME != 0
    }

    /// See [`MEDIA_FLAG_STALLED`] — advisory/cosmetic only, never
    /// security-relevant.
    pub fn is_stalled(&self) -> bool {
        self.flags & MEDIA_FLAG_STALLED != 0
    }

    pub fn pack(&self) -> [u8; MEDIA_HEADER_LEN] {
        let mut buf = [0u8; MEDIA_HEADER_LEN];
        buf[0] = self.version;
        buf[1] = self.flags;
        buf[2..10].copy_from_slice(&self.conn_id.to_be_bytes());
        buf[10] = self.stream_id;
        buf[11..15].copy_from_slice(&self.frame_id.to_be_bytes());
        buf[15..17].copy_from_slice(&self.fragment_index.to_be_bytes());
        buf[17..19].copy_from_slice(&self.fragment_count.to_be_bytes());
        buf
    }

    /// Parse a header off the front of `bytes`, returning the header and the
    /// remaining slice (this fragment's ciphertext payload).
    pub fn unpack(bytes: &[u8]) -> anyhow::Result<(Self, &[u8])> {
        if bytes.len() < MEDIA_HEADER_LEN {
            anyhow::bail!(
                "media datagram too short: {} bytes, need at least {MEDIA_HEADER_LEN}",
                bytes.len()
            );
        }
        let header = Self {
            version: bytes[0],
            flags: bytes[1],
            conn_id: u64::from_be_bytes(bytes[2..10].try_into().unwrap()),
            stream_id: bytes[10],
            frame_id: u32::from_be_bytes(bytes[11..15].try_into().unwrap()),
            fragment_index: u16::from_be_bytes(bytes[15..17].try_into().unwrap()),
            fragment_count: u16::from_be_bytes(bytes[17..19].try_into().unwrap()),
        };
        Ok((header, &bytes[MEDIA_HEADER_LEN..]))
    }
}

// ---------------------------------------------------------------------
// §6.6 NACK datagrams (selective retransmission, #104)
// ---------------------------------------------------------------------

/// Client -> relay, 1 byte: "I speak NACK" (I send NACKs for streams I watch and re-send fragments
/// of my own streams when asked). Repeated until [`NACK_TAG_HELLO_ACK`] arrives.
pub const NACK_TAG_HELLO: u8 = 0xE0;
/// Relay -> client, 1 byte: the relay speaks NACK too (it caches fragments and answers requests).
pub const NACK_TAG_HELLO_ACK: u8 = 0xE1;
/// Viewer -> relay, and relay -> stream owner: please (re-)send fragments `first .. first+count`
/// of one frame. 18 bytes, see [`NackRange`].
pub const NACK_TAG_REQUEST: u8 = 0xE2;
/// Relay -> viewer: the relay can not supply that range (not cached and the owner does not
/// re-send), stop waiting for it. Same 18-byte layout as a request.
pub const NACK_TAG_MISS: u8 = 0xE3;
/// Length of a request / miss datagram.
pub const NACK_DATAGRAM_LEN: usize = 18;

// ---------------------------------------------------------------------
// §6.7 Client diagnostics upload (#110): opt-in log lines to the relay
// ---------------------------------------------------------------------

/// Client -> relay, 1 byte: "I would like to upload my log to you" (the user ticked
/// Settings > Diagnostics > Send diagnostics). Repeated until [`DIAG_TAG_ACK`] arrives. Like the
/// NACK tags it is shorter than a media header and >= 0xE0, so every older relay drops it as
/// undersized ([`is_nack_datagram`] is true for it too).
pub const DIAG_TAG_HELLO: u8 = 0xE4;
/// Relay -> client, 1 byte: this relay stores uploads. Without it the client never opens a
/// diagnostics stream.
pub const DIAG_TAG_ACK: u8 = 0xE5;
/// First byte after the stream type of a [`STREAM_TYPE_DIAG`] stream: the batch format version.
pub const DIAG_UPLOAD_VERSION: u8 = 0x01;
/// Most bytes (type and version included) one diagnostics stream may carry; the relay reads no
/// more than this, the client never sends more.
pub const MAX_DIAG_BATCH_BYTES: usize = 256 * 1024;

// ---------------------------------------------------------------------
// §9.1 Viewer lists (#116): who watches my streams
// ---------------------------------------------------------------------

/// Client -> relay, 1 byte: "tell me who watches my streams" (a sharer's client sends it so its
/// UI can name the viewers). Repeated until [`VIEWERS_TAG_ACK`] arrives. Shorter than a media
/// header and >= 0xE0, so every older relay drops it as undersized ([`is_nack_datagram`]).
pub const VIEWERS_TAG_HELLO: u8 = 0xE6;
/// Relay -> client, 1 byte: this relay sends viewer lists. Followed at once by one
/// [`VIEWERS_TAG_LIST`] per stream the client owns.
pub const VIEWERS_TAG_ACK: u8 = 0xE7;
/// Relay -> stream owner (only one that said [`VIEWERS_TAG_HELLO`]): the complete current viewer
/// set of one of its streams. `tag u8 | stream_id u8 | count u8 | conn_id u64 BE x count`.
/// With 3 or more viewers it is longer than a media header, which is safe because only a
/// client that opted in ever receives one; a receiver tests the tag byte, not the length.
pub const VIEWERS_TAG_LIST: u8 = 0xE8;

/// Encode a viewer list datagram (at most 100 ids, far above the 8-member room cap).
pub fn pack_viewers(stream_id: StreamId, viewers: &[ConnId]) -> Vec<u8> {
    let viewers = &viewers[..viewers.len().min(100)];
    let mut b = Vec::with_capacity(3 + 8 * viewers.len());
    b.extend_from_slice(&[VIEWERS_TAG_LIST, stream_id, viewers.len() as u8]);
    for v in viewers {
        b.extend_from_slice(&v.to_be_bytes());
    }
    b
}

/// Decode a [`VIEWERS_TAG_LIST`] datagram; `None` if it is anything else or truncated.
pub fn unpack_viewers(bytes: &[u8]) -> Option<(StreamId, Vec<ConnId>)> {
    if bytes.len() < 3 || bytes[0] != VIEWERS_TAG_LIST {
        return None;
    }
    let n = bytes[2] as usize;
    if bytes.len() != 3 + 8 * n {
        return None;
    }
    let ids = bytes[3..].chunks_exact(8).map(|c| u64::from_be_bytes(c.try_into().unwrap())).collect();
    Some((bytes[1], ids))
}

// ---------------------------------------------------------------------
// §9.2 Emoji avatars (#106): one emoji per member, relayed as short tagged datagrams
// ---------------------------------------------------------------------

/// Client -> relay, 1 byte: "I show emoji avatars: tell me everyone's". Repeated until
/// [`AVATAR_TAG_ACK`] arrives. Undersized for every older relay ([`is_nack_datagram`]).
pub const AVATAR_TAG_HELLO: u8 = 0xE9;
/// Relay -> client, 1 byte: this relay carries avatars. Followed at once by a [`AVATAR_TAG_LIST`].
pub const AVATAR_TAG_ACK: u8 = 0xEA;
/// Client -> relay: `tag u8 | emoji UTF-8` (0..=[`MAX_AVATAR_BYTES`] bytes; none = "no emoji").
/// At most 17 bytes, so an older relay drops it as undersized.
pub const AVATAR_TAG_SET: u8 = 0xEB;
/// Relay -> client (only one that said [`AVATAR_TAG_HELLO`]): every member's emoji, the whole
/// current set: `tag u8 | count u8 | (conn_id u64 BE | len u8 | emoji UTF-8) x count`, members
/// without an emoji left out. Can exceed a media header, which is safe because only a client that
/// opted in ever receives one; a receiver tests the tag byte, not the length.
pub const AVATAR_TAG_LIST: u8 = 0xEC;
/// Longest emoji (UTF-8 bytes) a relay accepts and a client sends: a keycap or flag sequence, or
/// a four-codepoint ZWJ one. Keeps [`AVATAR_TAG_SET`] below the media header length.
pub const MAX_AVATAR_BYTES: usize = 16;
/// Most entries a list carries (a list entry is at most 25 bytes: 32 x 25 + 2 fits one datagram).
pub const MAX_AVATAR_ENTRIES: usize = 32;

/// Encode an [`AVATAR_TAG_SET`] datagram; `None` if `emoji` is longer than [`MAX_AVATAR_BYTES`].
/// An empty `emoji` clears the avatar.
pub fn pack_avatar_set(emoji: &str) -> Option<Vec<u8>> {
    (emoji.len() <= MAX_AVATAR_BYTES).then(|| [&[AVATAR_TAG_SET][..], emoji.as_bytes()].concat())
}

/// The emoji of an [`AVATAR_TAG_SET`] datagram ("" = clear); `None` if it is anything else, too
/// long, or not UTF-8.
pub fn unpack_avatar_set(bytes: &[u8]) -> Option<&str> {
    let body = bytes.strip_prefix(&[AVATAR_TAG_SET])?;
    (body.len() <= MAX_AVATAR_BYTES).then(|| std::str::from_utf8(body).ok()).flatten()
}

/// Encode an [`AVATAR_TAG_LIST`]: members with an empty or oversized emoji are skipped, and at most
/// [`MAX_AVATAR_ENTRIES`] are written.
pub fn pack_avatars<'a>(entries: impl IntoIterator<Item = (ConnId, &'a str)>) -> Vec<u8> {
    let mut b = vec![AVATAR_TAG_LIST, 0];
    for (id, emoji) in entries.into_iter().filter(|(_, e)| !e.is_empty() && e.len() <= MAX_AVATAR_BYTES).take(MAX_AVATAR_ENTRIES) {
        b.extend_from_slice(&id.to_be_bytes());
        b.push(emoji.len() as u8);
        b.extend_from_slice(emoji.as_bytes());
        b[1] += 1;
    }
    b
}

/// Decode an [`AVATAR_TAG_LIST`]; `None` if it is anything else, truncated, has trailing bytes, or
/// any entry is empty, too long or not UTF-8 (the whole list is distrusted then).
pub fn unpack_avatars(bytes: &[u8]) -> Option<Vec<(ConnId, String)>> {
    let [AVATAR_TAG_LIST, count, rest @ ..] = bytes else { return None };
    let mut rest = rest;
    let mut out = Vec::with_capacity(*count as usize);
    for _ in 0..*count {
        let (id, tail) = rest.split_first_chunk::<8>()?;
        let (&len, tail) = tail.split_first()?;
        let len = len as usize;
        if len == 0 || len > MAX_AVATAR_BYTES || tail.len() < len {
            return None;
        }
        out.push((u64::from_be_bytes(*id), std::str::from_utf8(&tail[..len]).ok()?.to_string()));
        rest = &tail[len..];
    }
    rest.is_empty().then_some(out)
}

/// Whether `bytes` is a NACK-family datagram rather than a media fragment: shorter than a media
/// header (so every older peer, relay or browser drops it as undersized instead of mistaking it
/// for media) and led by a tag no media `version` byte will use.
pub fn is_nack_datagram(bytes: &[u8]) -> bool {
    bytes.len() < MEDIA_HEADER_LEN && bytes.first().is_some_and(|t| *t >= NACK_TAG_HELLO)
}

/// The fragments of one frame a NACK asks for. `count` >= 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NackRange {
    /// The stream owner (the sender of the frame).
    pub conn_id: ConnId,
    pub stream_id: StreamId,
    pub frame_id: u32,
    pub first: u16,
    pub count: u16,
}

impl NackRange {
    /// `tag` is [`NACK_TAG_REQUEST`] or [`NACK_TAG_MISS`].
    pub fn pack(&self, tag: u8) -> [u8; NACK_DATAGRAM_LEN] {
        let mut b = [0u8; NACK_DATAGRAM_LEN];
        b[0] = tag;
        b[1..9].copy_from_slice(&self.conn_id.to_be_bytes());
        b[9] = self.stream_id;
        b[10..14].copy_from_slice(&self.frame_id.to_be_bytes());
        b[14..16].copy_from_slice(&self.first.to_be_bytes());
        b[16..18].copy_from_slice(&self.count.to_be_bytes());
        b
    }

    /// `(tag, range)` of a request / miss datagram; `None` for anything else (a hello, a
    /// malformed length, a zero count).
    pub fn unpack(bytes: &[u8]) -> Option<(u8, Self)> {
        if bytes.len() != NACK_DATAGRAM_LEN || !matches!(bytes[0], NACK_TAG_REQUEST | NACK_TAG_MISS) {
            return None;
        }
        let range = Self {
            conn_id: u64::from_be_bytes(bytes[1..9].try_into().ok()?),
            stream_id: bytes[9],
            frame_id: u32::from_be_bytes(bytes[10..14].try_into().ok()?),
            first: u16::from_be_bytes(bytes[14..16].try_into().ok()?),
            count: u16::from_be_bytes(bytes[16..18].try_into().ok()?),
        };
        (range.count > 0).then_some((bytes[0], range))
    }
}

// ---------------------------------------------------------------------
// §7.1 Preview stream header (15 bytes, fixed binary, cleartext)
// ---------------------------------------------------------------------

pub const PREVIEW_HEADER_LEN: usize = 15;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreviewHeader {
    pub stream_type: u8,
    pub version: u8,
    pub conn_id: ConnId,
    /// The VIDEO stream this preview belongs to.
    pub stream_id: StreamId,
    /// This preview's nonce suffix (spec §7.1).
    pub counter: u32,
}

impl PreviewHeader {
    pub fn new(conn_id: ConnId, stream_id: StreamId, counter: u32) -> Self {
        Self {
            stream_type: STREAM_TYPE_PREVIEW,
            version: PROTOCOL_VERSION,
            conn_id,
            stream_id,
            counter,
        }
    }

    pub fn pack(&self) -> [u8; PREVIEW_HEADER_LEN] {
        let mut buf = [0u8; PREVIEW_HEADER_LEN];
        buf[0] = self.stream_type;
        buf[1] = self.version;
        buf[2..10].copy_from_slice(&self.conn_id.to_be_bytes());
        buf[10] = self.stream_id;
        buf[11..15].copy_from_slice(&self.counter.to_be_bytes());
        buf
    }

    pub fn unpack(bytes: &[u8]) -> anyhow::Result<(Self, &[u8])> {
        if bytes.len() < PREVIEW_HEADER_LEN {
            anyhow::bail!(
                "preview stream header too short: {} bytes, need at least {PREVIEW_HEADER_LEN}",
                bytes.len()
            );
        }
        let header = Self {
            stream_type: bytes[0],
            version: bytes[1],
            conn_id: u64::from_be_bytes(bytes[2..10].try_into().unwrap()),
            stream_id: bytes[10],
            counter: u32::from_be_bytes(bytes[11..15].try_into().unwrap()),
        };
        Ok((header, &bytes[PREVIEW_HEADER_LEN..]))
    }
}

// ---------------------------------------------------------------------
// Transport constants shared by relay, native client and the website
// (spec §11 — WebTransport seam)
// ---------------------------------------------------------------------

/// ALPN of the native (raw QUIC) transport.
pub const NATIVE_ALPN: &[u8] = b"ssshare/1";

/// ALPN a browser (HTTP/3 + WebTransport) negotiates. The relay dispatches on
/// the negotiated ALPN: this one becomes a WebTransport session, [`NATIVE_ALPN`]
/// a raw-QUIC one.
pub const WEBTRANSPORT_ALPN: &[u8] = b"h3";

/// URL path a browser must request: `https://host:port/ssshare`. The relay
/// answers 404 to anything else.
pub const WEBTRANSPORT_PATH: &str = "/ssshare";

/// Hard cap on the size of **one media datagram as handed to the QUIC stack**
/// (19-byte [`MediaHeader`] + fragment payload), enforced by the *sender* when
/// it fragments a frame (`client/src/fragment.rs`; browsers must do the same).
///
/// `Why 1000:` the relay forwards fragments verbatim — it cannot re-fragment
/// without the room key — so the size has to fit the weakest peer, a browser
/// over WebTransport. **Measured: headless Chromium 153 reports
/// `WebTransport.datagrams.maxDatagramSize === 1024`** against this relay (the
/// WebTransport spec drops anything larger silently), well below what raw QUIC
/// allows (quinn reports 1162 at the 1200-byte minimum MTU). That figure is
/// application bytes, i.e. already net of the HTTP/3 quarter-stream-id prefix.
/// 1000 leaves 24 bytes of slack under it
/// and gives 981 payload bytes per fragment (vs ~1160 before): ~18% more
/// fragments for large frames, a fair price for a browser being able to send
/// at all. Receiving is the easy direction: 1000 + prefix (<= 8) fits a
/// 1200-byte QUIC packet with room to spare. Websites should still compare
/// `datagrams.maxDatagramSize` against this at runtime and refuse to join if
/// it is smaller, rather than silently losing every fragment.
pub const MAX_MEDIA_DATAGRAM_LEN: usize = 1000;

// ---------------------------------------------------------------------
// Certificate hash (join link 4th field)
// ---------------------------------------------------------------------

/// SHA-256 of a relay's leaf certificate (DER) — exactly what a browser needs
/// for `new WebTransport(url, { serverCertificateHashes: [{ algorithm:
/// "sha-256", value }] })`.
pub type CertHash = [u8; 32];

/// SHA-256 over the DER encoding of a certificate.
pub fn cert_hash_of_der(der: &[u8]) -> CertHash {
    use sha2::Digest;
    sha2::Sha256::digest(der).into()
}

/// Encode a [`CertHash`] for the join link: **base64url, no padding** (43
/// characters) — the same encoding as the room key field.
pub fn encode_cert_hash(hash: &CertHash) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hash)
}

/// Inverse of [`encode_cert_hash`].
pub fn decode_cert_hash(s: &str) -> anyhow::Result<CertHash> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s)
        .map_err(|e| anyhow::anyhow!("cert hash is not valid base64url (no padding): {e}"))?;
    <CertHash>::try_from(bytes.as_slice()).map_err(|_| {
        anyhow::anyhow!(
            "cert hash must decode to 32 bytes (SHA-256), got {} bytes",
            bytes.len()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cert_hash_round_trip() {
        let hash = cert_hash_of_der(b"not really a certificate");
        let enc = encode_cert_hash(&hash);
        assert_eq!(enc.len(), 43);
        assert!(!enc.contains(['=', '+', '/', '|']));
        assert_eq!(decode_cert_hash(&enc).unwrap(), hash);
        assert!(decode_cert_hash("AAAA").is_err());
        assert!(decode_cert_hash("!!!").is_err());
    }

    #[test]
    fn control_frame_round_trip() {
        let msg = ClientToRelay::Join {
            conn_id: 0x0102_0304_0506_0708,
            room_id: [7u8; 16],
            display_name: "alice".to_string(),
        };
        let framed = encode_control_frame(&msg).unwrap();
        let len = u32::from_be_bytes(framed[0..4].try_into().unwrap()) as usize;
        assert_eq!(len, framed.len() - 4);
        let decoded: ClientToRelay = decode_control_payload(&framed[4..]).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn media_header_round_trip() {
        let header = MediaHeader {
            version: PROTOCOL_VERSION,
            flags: MEDIA_FLAG_KEYFRAME,
            conn_id: 0xAABB_CCDD_EEFF_0011,
            stream_id: 3,
            frame_id: 42,
            fragment_index: 1,
            fragment_count: 5,
        };
        let packed = header.pack();
        assert_eq!(packed.len(), MEDIA_HEADER_LEN);
        let (parsed, rest) = MediaHeader::unpack(&packed).unwrap();
        assert_eq!(parsed, header);
        assert!(rest.is_empty());
        assert!(parsed.is_keyframe());
    }

    #[test]
    fn preview_header_round_trip() {
        let header = PreviewHeader::new(0x1122_3344_5566_7788, 9, 123);
        let packed = header.pack();
        assert_eq!(packed.len(), PREVIEW_HEADER_LEN);
        let (parsed, rest) = PreviewHeader::unpack(&packed).unwrap();
        assert_eq!(parsed, header);
        assert!(rest.is_empty());
        assert_eq!(parsed.stream_type, STREAM_TYPE_PREVIEW);
    }

    #[test]
    fn nack_datagrams_round_trip_and_never_look_like_media() {
        let r = NackRange { conn_id: 0x0102_0304_0506_0708, stream_id: 3, frame_id: 99, first: 7, count: 4 };
        for tag in [NACK_TAG_REQUEST, NACK_TAG_MISS] {
            let b = r.pack(tag);
            assert!(is_nack_datagram(&b));
            assert!(MediaHeader::unpack(&b).is_err(), "older peers must drop it as undersized");
            assert_eq!(NackRange::unpack(&b), Some((tag, r)));
        }
        assert_eq!(hex(&r.pack(NACK_TAG_REQUEST)), "e2010203040506070803000000630007 0004".replace(' ', ""));
        assert!(is_nack_datagram(&[NACK_TAG_HELLO]) && is_nack_datagram(&[NACK_TAG_HELLO_ACK]));
        // The diagnostics tags must look like "undersized, not media" to every older peer too.
        assert!(is_nack_datagram(&[DIAG_TAG_HELLO]) && is_nack_datagram(&[DIAG_TAG_ACK]));
        assert!(![NACK_TAG_HELLO, NACK_TAG_HELLO_ACK, NACK_TAG_REQUEST, NACK_TAG_MISS].contains(&DIAG_TAG_HELLO));
        assert!(![NACK_TAG_HELLO, NACK_TAG_HELLO_ACK, NACK_TAG_REQUEST, NACK_TAG_MISS].contains(&DIAG_TAG_ACK));
        assert_eq!(NackRange::unpack(&[NACK_TAG_HELLO]), None);
        let zero = NackRange { count: 0, ..r }.pack(NACK_TAG_REQUEST);
        assert_eq!(NackRange::unpack(&zero), None);
        // a media datagram, even a tiny one, is not a NACK
        let media = MediaHeader { version: PROTOCOL_VERSION, flags: 0, conn_id: 1, stream_id: 0, frame_id: 0, fragment_index: 0, fragment_count: 1 }.pack();
        assert!(!is_nack_datagram(&media));
    }

    #[test]
    fn viewer_list_round_trips_and_rejects_garbage() {
        let b = pack_viewers(3, &[1, 0x0102_0304_0506_0708]);
        assert_eq!(hex(&b), "e8030200000000000000010102030405060708");
        assert_eq!(unpack_viewers(&b), Some((3, vec![1, 0x0102_0304_0506_0708])));
        assert_eq!(unpack_viewers(&pack_viewers(9, &[])), Some((9, vec![])));
        assert_eq!(unpack_viewers(&b[..b.len() - 1]), None);
        assert_eq!(unpack_viewers(&[VIEWERS_TAG_ACK]), None);
        // Hello and Ack are undersized for every older relay; no tag collides with another family.
        assert!(is_nack_datagram(&[VIEWERS_TAG_HELLO]) && is_nack_datagram(&[VIEWERS_TAG_ACK]));
        let others = [NACK_TAG_HELLO, NACK_TAG_HELLO_ACK, NACK_TAG_REQUEST, NACK_TAG_MISS, DIAG_TAG_HELLO, DIAG_TAG_ACK];
        for t in [VIEWERS_TAG_HELLO, VIEWERS_TAG_ACK, VIEWERS_TAG_LIST] {
            assert!(!others.contains(&t) && t >= NACK_TAG_HELLO);
        }
    }

    #[test]
    fn avatar_datagrams_round_trip_and_reject_garbage() {
        // SET: tag + emoji, short enough that every older relay drops it as undersized
        let set = pack_avatar_set("🦊").unwrap();
        assert_eq!(hex(&set), "ebf09fa68a");
        assert_eq!(unpack_avatar_set(&set), Some("🦊"));
        assert_eq!(unpack_avatar_set(&pack_avatar_set("").unwrap()), Some(""));
        let longest = pack_avatar_set(&"x".repeat(MAX_AVATAR_BYTES)).unwrap();
        assert!(longest.len() < MEDIA_HEADER_LEN && is_nack_datagram(&longest));
        assert!(pack_avatar_set(&"x".repeat(MAX_AVATAR_BYTES + 1)).is_none());
        assert_eq!(unpack_avatar_set(&[&[AVATAR_TAG_SET][..], &[b'x'; 17]].concat()), None, "too long");
        assert_eq!(unpack_avatar_set(&[AVATAR_TAG_SET, 0xff, 0xfe]), None, "not UTF-8");
        assert_eq!(unpack_avatar_set(&[AVATAR_TAG_ACK]), None);
        // LIST
        let l = pack_avatars([(1, "🦊"), (2, ""), (0x0102_0304_0506_0708, "💀")]);
        assert_eq!(hex(&l), "ec02000000000000000104f09fa68a0102030405060708 04f09f9280".replace(' ', ""));
        assert_eq!(unpack_avatars(&l), Some(vec![(1, "🦊".into()), (0x0102_0304_0506_0708, "💀".into())]));
        assert_eq!(unpack_avatars(&pack_avatars([])), Some(vec![]));
        assert_eq!(unpack_avatars(&l[..l.len() - 1]), None, "truncated");
        assert_eq!(unpack_avatars(&[&l[..], &[0][..]].concat()), None, "trailing byte");
        assert_eq!(unpack_avatars(&[AVATAR_TAG_LIST, 1, 0, 0, 0, 0, 0, 0, 0, 1, 0]), None, "empty emoji entry");
        assert_eq!(unpack_avatars(&[AVATAR_TAG_LIST, 1, 0, 0, 0, 0, 0, 0, 0, 1, 1, 0xff]), None, "not UTF-8");
        let many: Vec<(ConnId, &str)> = (0..100).map(|i| (i, "🦊")).collect();
        let big = pack_avatars(many);
        assert_eq!(unpack_avatars(&big).unwrap().len(), MAX_AVATAR_ENTRIES);
        let widest = pack_avatars((0..100).map(|i| (i, "xxxxxxxxxxxxxxxx")));
        assert!(widest.len() <= 1200, "a full list fits one datagram: {}", widest.len());
        // Hello/Ack/Set are undersized for older relays; no tag collides with another family
        assert!(is_nack_datagram(&[AVATAR_TAG_HELLO]) && is_nack_datagram(&[AVATAR_TAG_ACK]));
        let others = [
            NACK_TAG_HELLO, NACK_TAG_HELLO_ACK, NACK_TAG_REQUEST, NACK_TAG_MISS, DIAG_TAG_HELLO, DIAG_TAG_ACK,
            VIEWERS_TAG_HELLO, VIEWERS_TAG_ACK, VIEWERS_TAG_LIST,
        ];
        for t in [AVATAR_TAG_HELLO, AVATAR_TAG_ACK, AVATAR_TAG_SET, AVATAR_TAG_LIST] {
            assert!(!others.contains(&t) && t >= NACK_TAG_HELLO);
        }
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// Wire-compat guard. The hex strings below were captured from the
    /// pre-extraction `client/src/protocol.rs` (postcard 1.x), one
    /// representative message per `ClientToRelay` / `RelayToClient` variant
    /// plus every nested `PresenceEvent` / `RejectReason` variant, and the two
    /// hand-packed headers. If this fails, the wire format changed and old
    /// and new peers can no longer talk to each other.
    #[test]
    fn wire_bytes_unchanged() {
        let m = Member {
            conn_id: 0x0102_0304_0506_0708,
            display_name: "bob".into(),
            stream_count: 2,
        };
        let si = StreamInfo {
            owner: 0x1112_1314_1516_1718,
            stream_id: 5,
            kind: StreamKind::Audio,
            label: "Screen 1".into(),
            width: 1920,
            height: 1080,
        };
        let c2r = [
            (
                ClientToRelay::Join {
                    conn_id: 0x0102_0304_0506_0708,
                    room_id: [7u8; 16],
                    display_name: "alice".into(),
                },
                "00888e98a8c0e08081010707070707070707070707070707070705616c696365",
            ),
            (ClientToRelay::Leave, "01"),
            (
                ClientToRelay::AnnounceStream {
                    stream_id: 3,
                    kind: StreamKind::Video,
                    label: "Screen 1".into(),
                    width: 1920,
                    height: 1080,
                },
                "0203000853637265656e2031800fb808",
            ),
            (ClientToRelay::StopStream { stream_id: 3 }, "0303"),
            (
                ClientToRelay::Subscribe { target: 0x1112_1314_1516_1718, stream_id: 4 },
                "0498aed8a8c1e284891104",
            ),
            (
                ClientToRelay::Unsubscribe { target: 0x1112_1314_1516_1718, stream_id: 4 },
                "0598aed8a8c1e284891104",
            ),
        ];
        for (msg, want) in &c2r {
            let bytes = postcard::to_stdvec(msg).unwrap();
            assert_eq!(hex(&bytes), *want, "{msg:?}");
            assert_eq!(&postcard::from_bytes::<ClientToRelay>(&bytes).unwrap(), msg);
        }

        let r2c = [
            (
                RelayToClient::PresenceSnapshot { members: vec![m.clone()], streams: vec![si.clone()] },
                "0001888e98a8c0e080810103626f62020198aed8a8c1e284891105010853637265656e2031800fb808",
            ),
            (
                RelayToClient::PresenceDelta { event: PresenceEvent::MemberJoined(m.clone()) },
                "0100888e98a8c0e080810103626f6202",
            ),
            (
                RelayToClient::PresenceDelta { event: PresenceEvent::MemberLeft { conn_id: 9 } },
                "010109",
            ),
            (
                RelayToClient::PresenceDelta { event: PresenceEvent::StreamStarted(si.clone()) },
                "010298aed8a8c1e284891105010853637265656e2031800fb808",
            ),
            (
                RelayToClient::PresenceDelta {
                    event: PresenceEvent::StreamStopped { conn_id: 9, stream_id: 2 },
                },
                "01030902",
            ),
            (RelayToClient::SubscriberCount { stream_id: 3, count: 300 }, "0203ac02"),
            (RelayToClient::KeyframeRequest { stream_id: 3 }, "0303"),
            (RelayToClient::JoinRejected { reason: RejectReason::RoomFull }, "0400"),
            (RelayToClient::JoinRejected { reason: RejectReason::ConnIdCollision }, "0401"),
            (RelayToClient::JoinRejected { reason: RejectReason::Malformed }, "0402"),
        ];
        for (msg, want) in &r2c {
            let bytes = postcard::to_stdvec(msg).unwrap();
            assert_eq!(hex(&bytes), *want, "{msg:?}");
            assert_eq!(&postcard::from_bytes::<RelayToClient>(&bytes).unwrap(), msg);
        }

        assert_eq!(hex(&postcard::to_stdvec(&StreamKind::Video).unwrap()), "00");
        assert_eq!(hex(&postcard::to_stdvec(&StreamKind::Audio).unwrap()), "01");

        let media = MediaHeader {
            version: 1,
            flags: MEDIA_FLAG_KEYFRAME | MEDIA_FLAG_STALLED,
            conn_id: 0x0102_0304_0506_0708,
            stream_id: 7,
            frame_id: 42,
            fragment_index: 3,
            fragment_count: 10,
        };
        assert_eq!(hex(&media.pack()), "01030102030405060708070000002a0003000a");
        assert_eq!(
            hex(&PreviewHeader::new(0x1122_3344_5566_7788, 9, 123).pack()),
            "01011122334455667788090000007b"
        );
    }
}
