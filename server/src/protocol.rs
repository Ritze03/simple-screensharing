//! Wire protocol types, per `superdoc/architecture/protocol-spec.md`.
//!
//! Control-plane messages are `postcard`-encoded and framed on the control
//! bidi stream with a 4-byte big-endian length prefix (§2 of the spec).
//! Media datagrams (§6.2) and preview uni-streams (§7.1) use hand-packed
//! fixed binary headers instead of postcard, to keep per-datagram overhead
//! minimal — the relay parses/builds these manually.

use serde::{Deserialize, Serialize};

/// Connection identifier: a client-chosen random `u64`, doubling as the
/// AES-GCM nonce prefix for that connection (spec §3.1). The relay treats
/// it as an opaque routing handle.
pub type ConnId = u64;

/// Room identifier: 128-bit client-generated value from the join link
/// (spec §3.2).
pub type RoomId = [u8; 16];

/// Per-sender-connection stream index, spec §3.3.
pub type StreamId = u8;

/// Maximum length (in bytes) of a control-frame payload the relay will
/// accept, per spec §2 ("reject any frame whose length exceeds a sane
/// cap, e.g. 64 KiB").
pub const MAX_CONTROL_FRAME_LEN: u32 = 64 * 1024;

/// Maximum UTF-8 byte length for `display_name` / stream `label`, clamped
/// per spec §4.1 (truncated on a char boundary by the relay).
pub const MAX_NAME_LEN: usize = 64;

/// Hard cap on room membership (spec §8 / §10 — `RejectReason::RoomFull`).
pub const MAX_ROOM_MEMBERS: usize = 8;

/// `StreamType` discriminator byte that begins every unidirectional QUIC
/// stream (spec §1).
pub const STREAM_TYPE_PREVIEW: u8 = 0x01;

/// Wire version byte used in media/preview headers and as AAD byte 0
/// (spec §5.3, §6.2, §7.1).
pub const WIRE_VERSION: u8 = 0x01;

// ---------------------------------------------------------------------
// §3.4 StreamKind
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamKind {
    Video = 0,
    Audio = 1,
}

// ---------------------------------------------------------------------
// §4.1 ClientToRelay
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
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

// ---------------------------------------------------------------------
// §4.2 RelayToClient
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RelayToClient {
    PresenceSnapshot {
        members: Vec<Member>,
        streams: Vec<StreamInfo>,
    },
    PresenceDelta {
        event: PresenceEvent,
    },
    /// Sent only to the stream OWNER.
    SubscriberCount {
        stream_id: u8,
        count: u32,
    },
    /// Sent only to the stream OWNER.
    KeyframeRequest {
        stream_id: u8,
    },
    JoinRejected {
        reason: RejectReason,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Member {
    pub conn_id: u64,
    pub display_name: String,
    /// `0` => not sharing; `>0` => sharing this many streams.
    pub stream_count: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamInfo {
    pub owner: u64,
    pub stream_id: u8,
    pub kind: StreamKind,
    pub label: String,
    pub width: u16,
    pub height: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
// §2 control-frame length-prefix framing helpers
// ---------------------------------------------------------------------

/// Errors that can occur while framing/deframing a control message.
#[derive(Debug)]
pub enum FrameError {
    TooLarge(u32),
    Encode(postcard::Error),
    Closed,
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::TooLarge(len) => write!(
                f,
                "control frame length {len} exceeds cap {MAX_CONTROL_FRAME_LEN}"
            ),
            FrameError::Encode(e) => write!(f, "postcard encode error: {e}"),
            FrameError::Closed => write!(f, "connection closed while reading frame"),
        }
    }
}

impl std::error::Error for FrameError {}

impl From<postcard::Error> for FrameError {
    fn from(e: postcard::Error) -> Self {
        FrameError::Encode(e)
    }
}

/// Encode `msg` as `u32 BE length ‖ postcard(msg)` (spec §2).
pub fn encode_frame<T: Serialize>(msg: &T) -> Result<Vec<u8>, FrameError> {
    let payload = postcard::to_allocvec(msg)?;
    let len: u32 = payload
        .len()
        .try_into()
        .map_err(|_| FrameError::TooLarge(u32::MAX))?;
    if len > MAX_CONTROL_FRAME_LEN {
        return Err(FrameError::TooLarge(len));
    }
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Decode a postcard payload (the bytes *after* the 4-byte length prefix
/// have already been read off the stream by the caller).
pub fn decode_payload<T: for<'de> Deserialize<'de>>(payload: &[u8]) -> Result<T, FrameError> {
    Ok(postcard::from_bytes(payload)?)
}

// ---------------------------------------------------------------------
// §6.2 Media datagram header (19 bytes, big-endian, hand-packed)
// ---------------------------------------------------------------------

/// Cleartext header prepended to every media datagram. All multi-byte
/// integers are big-endian. See spec §6.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaDatagramHeader {
    pub version: u8,
    pub flags: u8,
    pub conn_id: ConnId,
    pub stream_id: StreamId,
    pub frame_id: u32,
    pub fragment_index: u16,
    pub fragment_count: u16,
}

impl MediaDatagramHeader {
    pub const LEN: usize = 19;
    /// bit0 of `flags`: this fragment belongs to a keyframe (H.264 IDR).
    pub const FLAG_KEYFRAME: u8 = 0b0000_0001;

    pub fn pack(&self) -> [u8; Self::LEN] {
        let mut buf = [0u8; Self::LEN];
        buf[0] = self.version;
        buf[1] = self.flags;
        buf[2..10].copy_from_slice(&self.conn_id.to_be_bytes());
        buf[10] = self.stream_id;
        buf[11..15].copy_from_slice(&self.frame_id.to_be_bytes());
        buf[15..17].copy_from_slice(&self.fragment_index.to_be_bytes());
        buf[17..19].copy_from_slice(&self.fragment_count.to_be_bytes());
        buf
    }

    /// Parse the header from the front of `data`. Returns `None` if
    /// `data` is shorter than [`Self::LEN`].
    pub fn unpack(data: &[u8]) -> Option<Self> {
        if data.len() < Self::LEN {
            return None;
        }
        Some(Self {
            version: data[0],
            flags: data[1],
            conn_id: u64::from_be_bytes(data[2..10].try_into().unwrap()),
            stream_id: data[10],
            frame_id: u32::from_be_bytes(data[11..15].try_into().unwrap()),
            fragment_index: u16::from_be_bytes(data[15..17].try_into().unwrap()),
            fragment_count: u16::from_be_bytes(data[17..19].try_into().unwrap()),
        })
    }

    pub fn is_keyframe(&self) -> bool {
        self.flags & Self::FLAG_KEYFRAME != 0
    }
}

// ---------------------------------------------------------------------
// §7.1 Preview uni-stream header (15 bytes, big-endian, hand-packed)
// ---------------------------------------------------------------------

/// Cleartext header at the start of every preview uni-stream, *after*
/// the 1-byte `StreamType` discriminator (spec §1, §7.1). All multi-byte
/// integers are big-endian.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreviewHeader {
    pub version: u8,
    pub conn_id: ConnId,
    pub stream_id: StreamId,
    pub counter: u32,
}

impl PreviewHeader {
    /// Header length *not including* the leading `StreamType` byte
    /// (offsets 1..15 in the spec's table, i.e. 14 bytes) plus the
    /// version byte at offset 1 which is part of this struct... per the
    /// spec's table the full header (including `StreamType`) is 15
    /// bytes: `StreamType(1) + version(1) + conn_id(8) + stream_id(1) +
    /// counter(4) = 15`. This constant is that full 15-byte length,
    /// including the `StreamType` byte, for callers that read/write the
    /// whole uni-stream header in one shot.
    pub const LEN: usize = 15;

    /// Pack the full 15-byte header, including the leading
    /// `StreamType = 0x01` byte.
    pub fn pack(&self) -> [u8; Self::LEN] {
        let mut buf = [0u8; Self::LEN];
        buf[0] = STREAM_TYPE_PREVIEW;
        buf[1] = self.version;
        buf[2..10].copy_from_slice(&self.conn_id.to_be_bytes());
        buf[10] = self.stream_id;
        buf[11..15].copy_from_slice(&self.counter.to_be_bytes());
        buf
    }

    /// Parse the full 15-byte header (including the `StreamType` byte)
    /// from the front of `data`. Returns `None` if too short or the
    /// `StreamType` byte isn't `0x01` (preview).
    pub fn unpack(data: &[u8]) -> Option<Self> {
        if data.len() < Self::LEN || data[0] != STREAM_TYPE_PREVIEW {
            return None;
        }
        Some(Self {
            version: data[1],
            conn_id: u64::from_be_bytes(data[2..10].try_into().unwrap()),
            stream_id: data[10],
            counter: u32::from_be_bytes(data[11..15].try_into().unwrap()),
        })
    }
}

/// Clamp a UTF-8 string to at most `max_bytes` bytes, truncating on a
/// char boundary (spec §4.1: `display_name` / `label` clamped to 64
/// bytes).
pub fn clamp_utf8(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_header_roundtrip() {
        let h = MediaDatagramHeader {
            version: WIRE_VERSION,
            flags: MediaDatagramHeader::FLAG_KEYFRAME,
            conn_id: 0x0102_0304_0506_0708,
            stream_id: 7,
            frame_id: 42,
            fragment_index: 3,
            fragment_count: 10,
        };
        let packed = h.pack();
        assert_eq!(packed.len(), MediaDatagramHeader::LEN);
        let parsed = MediaDatagramHeader::unpack(&packed).unwrap();
        assert_eq!(h, parsed);
        assert!(parsed.is_keyframe());
    }

    #[test]
    fn preview_header_roundtrip() {
        let h = PreviewHeader {
            version: WIRE_VERSION,
            conn_id: 0xAABB_CCDD_EEFF_0011,
            stream_id: 2,
            counter: 99,
        };
        let packed = h.pack();
        assert_eq!(packed.len(), PreviewHeader::LEN);
        assert_eq!(packed[0], STREAM_TYPE_PREVIEW);
        let parsed = PreviewHeader::unpack(&packed).unwrap();
        assert_eq!(h, parsed);
    }

    #[test]
    fn control_frame_roundtrip() {
        let msg = ClientToRelay::Join {
            conn_id: 1,
            room_id: [9u8; 16],
            display_name: "alice".into(),
        };
        let framed = encode_frame(&msg).unwrap();
        let len = u32::from_be_bytes(framed[0..4].try_into().unwrap());
        assert_eq!(len as usize, framed.len() - 4);
        let decoded: ClientToRelay = decode_payload(&framed[4..]).unwrap();
        match decoded {
            ClientToRelay::Join {
                conn_id,
                room_id,
                display_name,
            } => {
                assert_eq!(conn_id, 1);
                assert_eq!(room_id, [9u8; 16]);
                assert_eq!(display_name, "alice");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn clamp_utf8_char_boundary() {
        let s = "a".repeat(63) + "€"; // 3-byte char pushing over 64 bytes
        let clamped = clamp_utf8(&s, 64);
        assert!(clamped.len() <= 64);
        assert!(String::from_utf8(clamped.into_bytes()).is_ok());
    }
}
