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

/// The only currently-defined unidirectional stream type: a preview blob.
pub const STREAM_TYPE_PREVIEW: u8 = 0x01;

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
