//! Media datagram fragmentation (sender) and reassembly (receiver).
//!
//! Implements spec §6: a whole encrypted frame is split into ≤ datagram-sized
//! fragments carrying the 19-byte `MediaHeader` (protocol.rs), and the
//! receiver regroups fragments by `(conn_id, stream_id, frame_id)` back into
//! the original ciphertext. Kept as pure, synchronous, unit-testable
//! functions/structs — no I/O, no async — so `net.rs` just wires them to the
//! actual QUIC datagram send/recv calls.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::protocol::{ConnId, MediaHeader, StreamId, MEDIA_HEADER_LEN};

/// Split one already-AEAD-encrypted frame (`ciphertext_with_tag`) into
/// wire-ready datagrams (header + this fragment's slice), per spec §6.3.
///
/// `max_datagram_size` should come from `quinn::Connection::max_datagram_size()`;
/// the usable payload per fragment is `max_datagram_size - MEDIA_HEADER_LEN`.
pub fn fragment_frame(
    conn_id: ConnId,
    stream_id: StreamId,
    frame_id: u32,
    keyframe: bool,
    ciphertext: &[u8],
    max_datagram_size: usize,
) -> anyhow::Result<Vec<Vec<u8>>> {
    let fragment_payload_max = max_datagram_size.saturating_sub(MEDIA_HEADER_LEN);
    if fragment_payload_max == 0 {
        anyhow::bail!("max_datagram_size too small to fit even the media header");
    }

    // ceil(len / max) fragments, at least one (even for an empty frame).
    let fragment_count = ciphertext.len().div_ceil(fragment_payload_max).max(1);
    let fragment_count_u16 = u16::try_from(fragment_count)
        .map_err(|_| anyhow::anyhow!("frame too large: {fragment_count} fragments exceeds u16"))?;

    let flags = if keyframe {
        crate::protocol::MEDIA_FLAG_KEYFRAME
    } else {
        0
    };

    let mut datagrams = Vec::with_capacity(fragment_count);
    for (index, chunk) in ciphertext.chunks(fragment_payload_max.max(1)).enumerate() {
        let header = MediaHeader {
            version: crate::protocol::PROTOCOL_VERSION,
            flags,
            conn_id,
            stream_id,
            frame_id,
            fragment_index: index as u16,
            fragment_count: fragment_count_u16,
        };
        let mut datagram = Vec::with_capacity(MEDIA_HEADER_LEN + chunk.len());
        datagram.extend_from_slice(&header.pack());
        datagram.extend_from_slice(chunk);
        datagrams.push(datagram);
    }

    // Empty ciphertext edge case: chunks() on an empty slice yields nothing,
    // but we promised at least one datagram above.
    if datagrams.is_empty() {
        let header = MediaHeader {
            version: crate::protocol::PROTOCOL_VERSION,
            flags,
            conn_id,
            stream_id,
            frame_id,
            fragment_index: 0,
            fragment_count: 1,
        };
        datagrams.push(header.pack().to_vec());
    }

    Ok(datagrams)
}

/// Per-frame reassembly buffer.
struct FrameBuf {
    fragments: Vec<Option<Vec<u8>>>,
    received: usize,
    last_touched: Instant,
}

impl FrameBuf {
    fn new(fragment_count: usize) -> Self {
        Self {
            fragments: vec![None; fragment_count],
            received: 0,
            last_touched: Instant::now(),
        }
    }

    fn is_complete(&self) -> bool {
        self.received == self.fragments.len()
    }

    fn concat(self) -> Vec<u8> {
        let mut out = Vec::new();
        for frag in self.fragments.into_iter().flatten() {
            out.extend_from_slice(&frag);
        }
        out
    }
}

/// Reassembly state for a single `(conn_id, stream_id)` media stream.
struct StreamReassembly {
    /// Keyed by frame_id; BTreeMap keeps them ordered so "oldest" is cheap.
    /// frame_id is monotonically increasing per spec §5.1/§6.5.
    frames: BTreeMap<u32, FrameBuf>,
}

/// How many incomplete frames to keep in flight per stream before evicting
/// the oldest (spec §6.5: "keep at most the newest 1-2 in-progress
/// frame_ids per stream").
const MAX_INFLIGHT_FRAMES_PER_STREAM: usize = 2;

/// Backstop: drop any buffer untouched for this long (spec §6.5).
const STALE_TIMEOUT: Duration = Duration::from_millis(200);

/// Reassembles fragmented media datagrams for potentially many concurrent
/// `(conn_id, stream_id)` streams (e.g. multiple room members, multi-monitor
/// senders). Not `Sync` — intended to be owned and driven by a single
/// receiver task per connection.
#[derive(Default)]
pub struct Reassembler {
    streams: HashMap<(ConnId, StreamId), StreamReassembly>,
}

impl Reassembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one received datagram's cleartext header + fragment payload.
    /// Returns `Some(ciphertext_with_tag)` once the frame is fully
    /// reassembled (caller then AEAD-decrypts it), otherwise `None`.
    pub fn insert(&mut self, header: &MediaHeader, fragment_payload: &[u8]) -> Option<Vec<u8>> {
        let key = (header.conn_id, header.stream_id);
        let stream = self.streams.entry(key).or_insert_with(|| StreamReassembly {
            frames: BTreeMap::new(),
        });

        // Stale-timeout backstop: drop any buffer untouched for too long.
        let now = Instant::now();
        stream
            .frames
            .retain(|_, buf| now.duration_since(buf.last_touched) < STALE_TIMEOUT);

        if header.fragment_count == 0 {
            return None; // malformed, nothing to reassemble
        }

        let buf = stream
            .frames
            .entry(header.frame_id)
            .or_insert_with(|| FrameBuf::new(header.fragment_count as usize));

        let idx = header.fragment_index as usize;
        if idx >= buf.fragments.len() {
            return None; // malformed: index outside the declared count
        }
        if buf.fragments[idx].is_none() {
            buf.fragments[idx] = Some(fragment_payload.to_vec());
            buf.received += 1;
        }
        buf.last_touched = now;

        let complete = buf.is_complete();

        // Evict older incomplete frames once we exceed the in-flight cap —
        // a fragment was lost for them and they'll never complete (§6.5).
        while stream.frames.len() > MAX_INFLIGHT_FRAMES_PER_STREAM {
            let oldest_id = match stream.frames.keys().next() {
                Some(&id) => id,
                None => break,
            };
            if oldest_id == header.frame_id {
                break; // never evict the frame we just touched
            }
            stream.frames.remove(&oldest_id);
        }

        if complete {
            stream.frames.remove(&header.frame_id).map(FrameBuf::concat)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::MediaHeader;

    #[test]
    fn split_and_reassemble_round_trip_large_payload() {
        // A payload well over one datagram's worth (~1181 usable bytes at a
        // 1200-byte MTU per spec §6.3), to force multiple fragments.
        let ciphertext: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        let conn_id: ConnId = 0x1122_3344_5566_7788;
        let stream_id: StreamId = 2;
        let frame_id = 7;
        let max_datagram_size = 1200;

        let datagrams = fragment_frame(
            conn_id,
            stream_id,
            frame_id,
            true,
            &ciphertext,
            max_datagram_size,
        )
        .unwrap();
        assert!(datagrams.len() > 1, "expected multiple fragments");

        let mut reassembler = Reassembler::new();
        let mut result = None;
        for datagram in &datagrams {
            let (header, payload) = MediaHeader::unpack(datagram).unwrap();
            assert_eq!(header.conn_id, conn_id);
            assert_eq!(header.stream_id, stream_id);
            assert_eq!(header.frame_id, frame_id);
            assert!(header.is_keyframe());
            let out = reassembler.insert(&header, payload);
            if out.is_some() {
                result = out;
            }
        }

        assert_eq!(result.expect("frame should be complete"), ciphertext);
    }

    #[test]
    fn single_fragment_frame_round_trips() {
        let ciphertext = b"short frame fits in one datagram".to_vec();
        let datagrams = fragment_frame(1, 0, 3, false, &ciphertext, 1200).unwrap();
        assert_eq!(datagrams.len(), 1);

        let mut reassembler = Reassembler::new();
        let (header, payload) = MediaHeader::unpack(&datagrams[0]).unwrap();
        let out = reassembler.insert(&header, payload).unwrap();
        assert_eq!(out, ciphertext);
    }

    #[test]
    fn out_of_order_fragments_still_reassemble() {
        let ciphertext: Vec<u8> = (0..3000u32).map(|i| (i % 200) as u8).collect();
        let datagrams = fragment_frame(9, 1, 11, false, &ciphertext, 1200).unwrap();
        assert!(datagrams.len() >= 2);

        let mut reassembler = Reassembler::new();
        let mut result = None;
        for datagram in datagrams.iter().rev() {
            let (header, payload) = MediaHeader::unpack(datagram).unwrap();
            let out = reassembler.insert(&header, payload);
            if out.is_some() {
                result = out;
            }
        }
        assert_eq!(result.unwrap(), ciphertext);
    }

    #[test]
    fn newer_frame_evicts_stale_incomplete_older_frame() {
        // Exercise the in-flight cap (MAX_INFLIGHT_FRAMES_PER_STREAM = 2):
        // three frames stay incomplete concurrently (each missing its 2nd
        // fragment), so the 3rd one arriving must evict the oldest
        // incomplete frame (frame_id 1), per spec §6.5.
        let mut reassembler = Reassembler::new();
        let conn_id = 1;
        let stream_id = 0;

        let first_fragment = |frame_id: u32| MediaHeader {
            version: 1,
            flags: 0,
            conn_id,
            stream_id,
            frame_id,
            fragment_index: 0,
            fragment_count: 2,
        };

        assert!(reassembler.insert(&first_fragment(1), b"a").is_none());
        assert!(reassembler.insert(&first_fragment(2), b"a").is_none());
        // Frame 3 pushes the in-flight set over the cap; frame 1 (oldest)
        // must be evicted.
        assert!(reassembler.insert(&first_fragment(3), b"a").is_none());

        // Frame 1's late second fragment must NOT complete anything: it was
        // evicted, so this just starts a fresh (still-incomplete) buffer.
        let h1_second = MediaHeader {
            fragment_index: 1,
            ..first_fragment(1)
        };
        assert!(reassembler.insert(&h1_second, b"b").is_none());

        // Frame 2 was NOT evicted (it's within the cap), so its second
        // fragment should complete it normally.
        let h2_second = MediaHeader {
            fragment_index: 1,
            ..first_fragment(2)
        };
        assert_eq!(reassembler.insert(&h2_second, b"b").unwrap(), b"ab");
    }
}
