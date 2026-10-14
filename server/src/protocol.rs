//! Relay-side view of the wire protocol, per `superdoc/architecture/protocol-spec.md`.
//!
//! The wire types, constants and the hand-packed header layouts live in the
//! shared `simple-screensharing-protocol` crate (`protocol/`), which the client
//! also uses; everything from it is re-exported here so `crate::protocol::X`
//! paths keep working. What stays in this file are the relay-only helpers: the
//! cap-enforcing control-frame codec and `clamp_utf8`. The media/preview
//! header types are the shared crate's `MediaHeader` / `PreviewHeader`
//! directly, so the byte layout has exactly one source of truth.

pub use simple_screensharing_protocol::*;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------
// §2 control-frame length-prefix framing helpers
// ---------------------------------------------------------------------

/// Errors that can occur while framing/deframing a control message.
#[derive(Debug)]
pub enum FrameError {
    TooLarge(u32),
    Encode(postcard::Error),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::TooLarge(len) => write!(
                f,
                "control frame length {len} exceeds cap {MAX_CONTROL_FRAME_LEN}"
            ),
            FrameError::Encode(e) => write!(f, "postcard encode error: {e}"),
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

    /// The relay's framing must equal the client's `encode_control_frame`
    /// for in-cap messages.
    #[test]
    fn relay_framing_matches_shared_crate() {
        let msg = RelayToClient::KeyframeRequest { stream_id: 3 };
        assert_eq!(encode_frame(&msg).unwrap(), encode_control_frame(&msg).unwrap());
    }
}
