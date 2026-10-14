//! Relay-side view of the wire protocol, per `superdoc/architecture/protocol-spec.md`.
//!
//! The wire types, constants and the hand-packed header layouts live in the
//! shared `simple-screensharing-protocol` crate (`protocol/`), which the client
//! also uses; everything from it is re-exported here so `crate::protocol::X`
//! paths keep working. What stays in this file are the relay-only helpers: the
//! `Option`-returning header parsers the relay's datagram path uses, the
//! cap-enforcing control-frame codec, and `clamp_utf8`.
//!
//! `Why:` the header helpers are thin wrappers that delegate to the shared
//! crate's `pack`/`unpack` rather than re-implementing the byte layout, so the
//! layout has exactly one source of truth.

pub use simple_screensharing_protocol::*;

use serde::{Deserialize, Serialize};
use simple_screensharing_protocol as shared;

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
/// integers are big-endian. See spec §6.2. Same fields as the shared crate's
/// [`shared::MediaHeader`]; this wrapper adds the relay's `Option`-returning
/// parser.
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
    pub const LEN: usize = shared::MEDIA_HEADER_LEN;
    /// bit0 of `flags`: this fragment belongs to a keyframe (H.264 IDR).
    pub const FLAG_KEYFRAME: u8 = shared::MEDIA_FLAG_KEYFRAME;

    fn to_shared(self) -> shared::MediaHeader {
        shared::MediaHeader {
            version: self.version,
            flags: self.flags,
            conn_id: self.conn_id,
            stream_id: self.stream_id,
            frame_id: self.frame_id,
            fragment_index: self.fragment_index,
            fragment_count: self.fragment_count,
        }
    }

    pub fn pack(&self) -> [u8; Self::LEN] {
        self.to_shared().pack()
    }

    /// Parse the header from the front of `data`. Returns `None` if
    /// `data` is shorter than [`Self::LEN`].
    pub fn unpack(data: &[u8]) -> Option<Self> {
        let (h, _) = shared::MediaHeader::unpack(data).ok()?;
        Some(Self {
            version: h.version,
            flags: h.flags,
            conn_id: h.conn_id,
            stream_id: h.stream_id,
            frame_id: h.frame_id,
            fragment_index: h.fragment_index,
            fragment_count: h.fragment_count,
        })
    }

    pub fn is_keyframe(&self) -> bool {
        self.flags & Self::FLAG_KEYFRAME != 0
    }
}

// ---------------------------------------------------------------------
// §7.1 Preview uni-stream header (15 bytes, big-endian, hand-packed)
// ---------------------------------------------------------------------

/// The relay's view of the preview header (spec §1, §7.1): it always carries
/// `StreamType = 0x01` implicitly, so there is no `stream_type` field, and
/// [`Self::unpack`] returns `None` for anything that isn't a preview header.
/// Shadows the shared crate's `PreviewHeader` (which is what the client uses)
/// within `crate::protocol`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreviewHeader {
    pub version: u8,
    pub conn_id: ConnId,
    pub stream_id: StreamId,
    pub counter: u32,
}

impl PreviewHeader {
    /// Full header length including the leading `StreamType` byte.
    pub const LEN: usize = shared::PREVIEW_HEADER_LEN;

    /// Pack the full 15-byte header, including the leading
    /// `StreamType = 0x01` byte.
    pub fn pack(&self) -> [u8; Self::LEN] {
        shared::PreviewHeader {
            stream_type: STREAM_TYPE_PREVIEW,
            version: self.version,
            conn_id: self.conn_id,
            stream_id: self.stream_id,
            counter: self.counter,
        }
        .pack()
    }

    /// Parse the full 15-byte header (including the `StreamType` byte)
    /// from the front of `data`. Returns `None` if too short or the
    /// `StreamType` byte isn't `0x01` (preview).
    pub fn unpack(data: &[u8]) -> Option<Self> {
        let (h, _) = shared::PreviewHeader::unpack(data).ok()?;
        if h.stream_type != STREAM_TYPE_PREVIEW {
            return None;
        }
        Some(Self {
            version: h.version,
            conn_id: h.conn_id,
            stream_id: h.stream_id,
            counter: h.counter,
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
            conn_id: 0xAABB_CCDD_EEFF_0011,
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

    /// The relay's wrappers must produce/accept the same bytes as the shared
    /// crate (the client's view), and the relay's framing must equal the
    /// client's `encode_control_frame` for in-cap messages.
    #[test]
    fn relay_helpers_match_shared_crate() {
        let m = MediaDatagramHeader {
            version: WIRE_VERSION,
            flags: 3,
            conn_id: 0x0102_0304_0506_0708,
            stream_id: 7,
            frame_id: 42,
            fragment_index: 3,
            fragment_count: 10,
        };
        assert_eq!(m.pack(), m.to_shared().pack());
        let p = PreviewHeader { version: WIRE_VERSION, conn_id: 5, stream_id: 9, counter: 123 };
        assert_eq!(p.pack(), shared::PreviewHeader::new(5, 9, 123).pack());
        assert!(PreviewHeader::unpack(&p.pack()[..14]).is_none());
        let mut wrong = p.pack();
        wrong[0] = 0x02;
        assert!(PreviewHeader::unpack(&wrong).is_none());

        let msg = RelayToClient::KeyframeRequest { stream_id: 3 };
        assert_eq!(encode_frame(&msg).unwrap(), encode_control_frame(&msg).unwrap());
    }
}
