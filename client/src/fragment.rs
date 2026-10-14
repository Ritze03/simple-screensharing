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

use crate::protocol::{ConnId, MediaHeader, StreamId, MAX_MEDIA_DATAGRAM_LEN, MEDIA_HEADER_LEN};

/// Split one already-AEAD-encrypted frame (`ciphertext_with_tag`) into
/// wire-ready datagrams (header + this fragment's slice), per spec §6.3.
///
/// `max_datagram_size` should come from `quinn::Connection::max_datagram_size()`;
/// the usable payload per fragment is `max_datagram_size - MEDIA_HEADER_LEN`,
/// **but never more than [`MAX_MEDIA_DATAGRAM_LEN`] in total** — see below.
///
/// # Browser-compatible datagram size
///
/// `Why the cap:` the relay forwards fragments verbatim (it has no key, so it
/// cannot re-fragment), which means *this* sender decides whether a browser
/// viewer can receive them and a browser sender decides whether natives can.
/// A WebTransport peer's datagrams are limited to what
/// `WebTransport.datagrams.maxDatagramSize` allows — 1024 application bytes in
/// Chromium — so a native sender that filled quinn's ~1160-byte limit would
/// produce fragments no browser could ever forward or send. Every sender
/// therefore fragments to at most [`MAX_MEDIA_DATAGRAM_LEN`] (1000) bytes per
/// datagram, header included, i.e. 981 payload bytes. Native-to-native works as
/// before, with ~18% more fragments on big frames.
pub fn fragment_frame(
    conn_id: ConnId,
    stream_id: StreamId,
    frame_id: u32,
    keyframe: bool,
    ciphertext: &[u8],
    max_datagram_size: usize,
) -> anyhow::Result<Vec<Vec<u8>>> {
    let flags = if keyframe {
        crate::protocol::MEDIA_FLAG_KEYFRAME
    } else {
        0
    };
    fragment_frame_with_flags(conn_id, stream_id, frame_id, flags, ciphertext, max_datagram_size)
}

/// [`fragment_frame`] with the header's whole `flags` byte supplied directly,
/// for senders that need to set more than just [`crate::protocol::MEDIA_FLAG_KEYFRAME`]
/// — currently only `capture.rs`'s stall protocol, which also raises
/// [`crate::protocol::MEDIA_FLAG_STALLED`] (advisory only; see that constant's
/// doc comment for why it must never drive anything security-relevant).
///
/// `Why:` (a second entry point rather than widening [`fragment_frame`]'s
/// `keyframe: bool`) every other sender — `audio.rs`, and this module's own
/// tests — only ever has a keyframe bit to express, and reads far better
/// saying so than passing a bitmask. Keeping the bool-shaped call the default
/// also means a caller cannot accidentally pass a raw `1`/`0` where a flags
/// byte was meant.
pub fn fragment_frame_with_flags(
    conn_id: ConnId,
    stream_id: StreamId,
    frame_id: u32,
    flags: u8,
    ciphertext: &[u8],
    max_datagram_size: usize,
) -> anyhow::Result<Vec<Vec<u8>>> {
    let max_datagram_size = max_datagram_size.min(MAX_MEDIA_DATAGRAM_LEN);
    let fragment_payload_max = max_datagram_size.saturating_sub(MEDIA_HEADER_LEN);
    if fragment_payload_max == 0 {
        anyhow::bail!("max_datagram_size too small to fit even the media header");
    }

    // ceil(len / max) fragments, at least one (even for an empty frame).
    let fragment_count = ciphertext.len().div_ceil(fragment_payload_max).max(1);
    let fragment_count_u16 = u16::try_from(fragment_count)
        .map_err(|_| anyhow::anyhow!("frame too large: {fragment_count} fragments exceeds u16"))?;

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

    /// The stall protocol's advisory bit must survive fragmentation and land
    /// on **every** fragment's header, alongside the keyframe bit — a viewer
    /// that reassembles from any fragment must see the same flags. See
    /// `protocol::MEDIA_FLAG_STALLED` for why this bit is cosmetic-only.
    #[test]
    fn the_stalled_flag_rides_every_fragment_alongside_the_keyframe_flag() {
        use crate::protocol::{MEDIA_FLAG_KEYFRAME, MEDIA_FLAG_STALLED};

        let ciphertext: Vec<u8> = (0..4000u32).map(|i| (i % 251) as u8).collect();
        let datagrams =
            fragment_frame_with_flags(3, 1, 8, MEDIA_FLAG_KEYFRAME | MEDIA_FLAG_STALLED, &ciphertext, 1200).unwrap();
        assert!(datagrams.len() > 1, "expected multiple fragments");

        for datagram in &datagrams {
            let (header, _) = MediaHeader::unpack(datagram).unwrap();
            assert!(header.is_keyframe(), "a stalled re-send is always an IDR");
            assert!(header.is_stalled());
        }

        // And the ordinary path must not set it.
        let plain = fragment_frame(3, 1, 9, true, &ciphertext, 1200).unwrap();
        for datagram in &plain {
            let (header, _) = MediaHeader::unpack(datagram).unwrap();
            assert!(header.is_keyframe());
            assert!(!header.is_stalled(), "a normal frame must never claim to be a stalled re-send");
        }
    }

    #[test]
    fn no_datagram_exceeds_the_browser_safe_cap() {
        let ciphertext = vec![0xAAu8; 50_000];
        // Whatever the QUIC stack claims it can carry, stay browser-compatible.
        for claimed in [1200, 1452, 65_000] {
            let datagrams = fragment_frame(1, 0, 3, true, &ciphertext, claimed).unwrap();
            assert!(datagrams.iter().all(|d| d.len() <= MAX_MEDIA_DATAGRAM_LEN));
            assert_eq!(datagrams[0].len(), MAX_MEDIA_DATAGRAM_LEN);
            let mut reasm = Reassembler::new();
            let mut out = None;
            for d in &datagrams {
                let (h, p) = MediaHeader::unpack(d).unwrap();
                out = reasm.insert(&h, p).or(out);
            }
            assert_eq!(out.unwrap(), ciphertext);
        }
        // A smaller claimed size is still honoured.
        let small = fragment_frame(1, 0, 4, false, &ciphertext, 500).unwrap();
        assert!(small.iter().all(|d| d.len() <= 500));
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
