//! Media datagram fragmentation (sender) and reassembly (receiver).
//!
//! Implements spec §6: a whole encrypted frame is split into ≤ datagram-sized
//! fragments carrying the 19-byte `MediaHeader` (protocol.rs), and the
//! receiver regroups fragments by `(conn_id, stream_id, frame_id)` back into
//! the original ciphertext. Kept as pure, synchronous, unit-testable
//! functions/structs — no I/O, no async — so `net.rs` just wires them to the
//! actual QUIC datagram send/recv calls.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use crate::protocol::{
    ConnId, MediaHeader, NackRange, StreamId, MAX_MEDIA_DATAGRAM_LEN, MEDIA_FLAG_KEYFRAME, MEDIA_FLAG_STALLED,
    MEDIA_HEADER_LEN,
};

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
    first_seen: Instant,
    last_touched: Instant,
    /// The header's `flags` byte (same on every fragment), so a discarded frame can be reported as
    /// "an IDR was lost" and a released one carries its keyframe / stalled bits.
    flags: u8,
    /// Highest fragment index received so far (a hole below it with data behind it is a gap).
    max_index: usize,
    /// Requests sent for this frame, over all fragments (stats: "completed after asking").
    attempts: u32,
    /// Per fragment: how often it was asked for and when last (allocated on the first request);
    /// also tells which arrivals were retransmits.
    requested: Vec<(u8, Instant)>,
    /// The relay said it cannot supply what is missing: stop waiting.
    give_up: bool,
}

impl FrameBuf {
    fn new(fragment_count: usize, flags: u8, now: Instant) -> Self {
        Self {
            fragments: vec![None; fragment_count],
            received: 0,
            first_seen: now,
            last_touched: now,
            flags,
            max_index: 0,
            attempts: 0,
            requested: Vec::new(),
            give_up: false,
        }
    }

    fn keyframe(&self) -> bool {
        self.flags & MEDIA_FLAG_KEYFRAME != 0
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

    /// Missing fragments worth asking for now: a hole with at least [`REORDER_DEPTH`] later
    /// fragments behind it, or every hole once the frame has been quiet for [`IDLE_NACK`] (the
    /// lost tail has no later fragment to expose it).
    fn suspects(&self, now: Instant) -> Vec<usize> {
        let idle = now.duration_since(self.last_touched) >= IDLE_NACK;
        (0..self.fragments.len())
            .filter(|&i| self.fragments[i].is_none() && (idle || i + REORDER_DEPTH <= self.max_index))
            .collect()
    }

    /// `(times asked, when last)` for fragment `i`.
    fn asked(&self, i: usize) -> (u8, Option<Instant>) {
        self.requested.get(i).map_or((0, None), |&(n, at)| (n, (n > 0).then_some(at)))
    }
}

/// Contiguous runs `(first, count)` of ascending indices.
fn runs_of(indices: &[usize]) -> Vec<(u16, u16)> {
    let mut runs: Vec<(u16, u16)> = Vec::new();
    for &i in indices {
        match runs.last_mut() {
            Some((start, n)) if *start as usize + *n as usize == i => *n += 1,
            _ => runs.push((i as u16, 1)),
        }
    }
    runs
}

/// Reassembly state for a single `(conn_id, stream_id)` media stream.
struct StreamReassembly {
    /// Keyed by frame_id; BTreeMap keeps them ordered so "oldest" is cheap.
    /// frame_id is monotonically increasing per spec §5.1/§6.5.
    frames: BTreeMap<u32, FrameBuf>,
    /// Incomplete frames discarded since the last frame this stream delivered;
    /// handed out (and cleared) with the next delivered frame.
    pending_loss: FrameLoss,
    /// NACK mode: the last few hundred frame ids delivered or given up on, so a late duplicate
    /// (a retransmit racing the original) never opens a ghost frame.
    done: DoneIds,
}

/// A bounded set of recent frame ids.
#[derive(Default)]
struct DoneIds {
    set: HashSet<u32>,
    order: VecDeque<u32>,
}

impl DoneIds {
    const CAP: usize = 256;

    fn insert(&mut self, id: u32) {
        if self.set.insert(id) {
            self.order.push_back(id);
            if self.order.len() > Self::CAP
                && let Some(old) = self.order.pop_front()
            {
                self.set.remove(&old);
            }
        }
    }

    fn contains(&self, id: u32) -> bool {
        self.set.contains(&id)
    }
}

/// Frames the receiver gave up on: an incomplete frame was evicted by a newer
/// one or aged out (spec §6.5), i.e. some of its fragments never arrived.
///
/// `Why this exists:` the wire is lossy and has no retransmission, but the
/// reassembler used to discard such frames silently, so the viewer could not
/// tell "a reference frame is missing, the picture is corrupt, ask for a
/// keyframe" from "all fine". Deliberately *only* this: a frame whose every
/// fragment was lost leaves no trace here (the connection-wide `frame_id`
/// counter is shared by every stream and the previews, so gaps in it are not
/// evidence). The browser has the same blind spot (`fragment.js`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrameLoss {
    /// Incomplete frames discarded.
    pub frames: u32,
    /// At least one of them was a keyframe: everything after it (up to the
    /// next keyframe) references a picture the viewer never got.
    pub keyframe: bool,
    /// For the last lost keyframe: fragments it was split into / how many arrived (diagnostics:
    /// how large the keyframes that do not make it are).
    pub keyframe_fragments: u16,
    pub keyframe_received: u16,
}

impl FrameLoss {
    pub fn is_none(&self) -> bool {
        self.frames == 0
    }

    fn note(&mut self, buf: &FrameBuf) {
        self.frames = self.frames.saturating_add(1);
        if buf.keyframe() {
            self.keyframe = true;
            self.keyframe_fragments = buf.fragments.len().min(u16::MAX as usize) as u16;
            self.keyframe_received = buf.received.min(u16::MAX as usize) as u16;
        }
    }
}

/// One fully received frame, still encrypted.
#[derive(Debug)]
pub struct Completed {
    pub conn_id: ConnId,
    pub stream_id: StreamId,
    pub frame_id: u32,
    pub flags: u8,
    pub ciphertext: Vec<u8>,
    /// Incomplete frames of this stream given up on since the previous frame it delivered.
    pub loss: FrameLoss,
}

impl Completed {
    pub fn is_keyframe(&self) -> bool {
        self.flags & MEDIA_FLAG_KEYFRAME != 0
    }

    pub fn is_stalled(&self) -> bool {
        self.flags & MEDIA_FLAG_STALLED != 0
    }
}

/// What the NACK machinery did (spec §6.6), for the periodic log and the measurements.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NackStats {
    /// Request datagrams sent (one per contiguous run per round).
    pub requests: u64,
    /// Fragments asked for, counted per round (a fragment asked twice counts twice).
    pub fragments_requested: u64,
    /// Fragments that arrived after having been asked for.
    pub fragments_recovered: u64,
    /// Frames that completed after at least one request.
    pub frames_recovered: u64,
    /// Frames given up on after asking (rounds exhausted, `Miss`, too many holes, or the time limit).
    pub frames_abandoned: u64,
}

/// How many incomplete frames to keep in flight per stream before evicting
/// the oldest (spec §6.5: "keep at most the newest 1-2 in-progress
/// frame_ids per stream"). Legacy mode only.
const MAX_INFLIGHT_FRAMES_PER_STREAM: usize = 2;

/// Backstop: drop any buffer untouched for this long (spec §6.5). Legacy mode only.
const STALE_TIMEOUT: Duration = Duration::from_millis(200);

/// A hole counts as a gap once this many fragment positions later have arrived.
const REORDER_DEPTH: usize = 2;
/// A frame quiet this long has lost its tail. (A paced keyframe sends a chunk every 2 ms.)
const IDLE_NACK: Duration = Duration::from_millis(20);
/// Times one fragment is asked for; the frame is abandoned one retry interval after the last ask
/// if it is still missing.
const MAX_NACK_ROUNDS: u8 = 3;
/// A frame with more suspect fragments than this is congestion, not loss: more traffic would
/// only hurt, so it is abandoned without asking.
const MAX_REPAIR_FRAGMENTS: usize = 128;
/// Nothing waits longer than this for a frame, whatever else happens.
const NACK_HARD_LIMIT: Duration = Duration::from_secs(1);
/// Memory guard: frames held per stream in NACK mode.
const MAX_FRAMES_NACK: usize = 256;

/// Time between two NACK rounds for one frame: long enough for the answer to come back even when
/// it has to go on to the sharer (viewer RTT + sharer RTT + processing). The viewer only knows its
/// own RTT; the 60 ms and the 100 ms floor are the allowance for the sharer's leg (a first rig run
/// with floor 60 ms lost a keyframe whose repair had to cross a sharer link at ~90 ms RTT).
pub fn nack_retry_interval(rtt: Duration) -> Duration {
    (rtt * 2 + Duration::from_millis(60)).clamp(Duration::from_millis(100), Duration::from_millis(300))
}

/// Reassembles fragmented media datagrams for potentially many concurrent
/// `(conn_id, stream_id)` streams (e.g. multiple room members, multi-monitor
/// senders). Not `Sync` — intended to be owned and driven by a single
/// receiver task per connection.
///
/// Two modes. **Legacy** (default): the spec §6.5 drop-and-continue policy, a frame is delivered the
/// moment its last fragment lands, and an incomplete one is dropped when two newer ones are in
/// flight or after 200 ms. **NACK** ([`Self::set_nack`], spec §6.6): an incomplete frame is repaired
/// by [`Self::poll`] asking for its missing fragments, and later complete frames of the stream wait
/// for it so the decoder still sees the frames in order.
#[derive(Default)]
pub struct Reassembler {
    streams: HashMap<(ConnId, StreamId), StreamReassembly>,
    nack: bool,
    stats: NackStats,
}

impl Reassembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Switch the NACK mode on (the relay acknowledged our Hello). Frames already in flight carry on
    /// under the new rules.
    pub fn set_nack(&mut self, on: bool) {
        self.nack = on;
    }

    pub fn stats(&self) -> NackStats {
        self.stats
    }

    /// True while a NACK-mode frame is incomplete, i.e. [`Self::poll`] has work to do.
    pub fn has_pending(&self) -> bool {
        self.nack && self.streams.values().any(|s| s.frames.values().any(|f| !f.is_complete()))
    }

    /// Feed one received datagram's cleartext header + fragment payload. Returns the frames that
    /// became deliverable: at most one in legacy mode, possibly several in NACK mode (a repaired
    /// frame releases the ones held behind it).
    pub fn ingest(&mut self, now: Instant, header: &MediaHeader, fragment_payload: &[u8]) -> Vec<Completed> {
        let key = (header.conn_id, header.stream_id);
        let nack = self.nack;
        let stream = self.streams.entry(key).or_insert_with(|| StreamReassembly {
            frames: BTreeMap::new(),
            pending_loss: FrameLoss::default(),
            done: DoneIds::default(),
        });
        let mut out = Vec::new();
        if nack {
            if stream.done.contains(header.frame_id) {
                return out;
            }
        } else {
            // Stale-timeout backstop: drop any buffer untouched for too long.
            let pending = &mut stream.pending_loss;
            stream.frames.retain(|_, buf| {
                let fresh = now.duration_since(buf.last_touched) < STALE_TIMEOUT;
                if !fresh {
                    pending.note(buf);
                }
                fresh
            });
        }

        if header.fragment_count == 0 {
            return out; // malformed, nothing to reassemble
        }

        let buf = stream
            .frames
            .entry(header.frame_id)
            .or_insert_with(|| FrameBuf::new(header.fragment_count as usize, header.flags, now));

        let idx = header.fragment_index as usize;
        if idx >= buf.fragments.len() {
            return out; // malformed: index outside the declared count
        }
        if buf.fragments[idx].is_none() {
            buf.fragments[idx] = Some(fragment_payload.to_vec());
            buf.received += 1;
            buf.max_index = buf.max_index.max(idx);
            if buf.asked(idx).0 > 0 {
                self.stats.fragments_recovered += 1;
            }
        }
        buf.last_touched = now;

        if nack {
            Self::release(stream, header.conn_id, header.stream_id, &mut out, &mut self.stats);
            Self::attach_loss(stream, &mut out);
            return out;
        }

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
            if let Some(dropped) = stream.frames.remove(&oldest_id) {
                stream.pending_loss.note(&dropped);
            }
        }

        if complete && let Some(done) = stream.frames.remove(&header.frame_id) {
            out.push(Completed {
                conn_id: header.conn_id,
                stream_id: header.stream_id,
                frame_id: header.frame_id,
                flags: done.flags,
                ciphertext: done.concat(),
                loss: FrameLoss::default(),
            });
            Self::attach_loss(stream, &mut out);
        }
        out
    }

    /// Hand the stream's pending loss to the first frame delivered by this call.
    fn attach_loss(stream: &mut StreamReassembly, out: &mut [Completed]) {
        if let Some(first) = out.first_mut() {
            first.loss = std::mem::take(&mut stream.pending_loss);
        }
    }

    /// NACK mode: deliver every complete frame older than the oldest incomplete one, in order.
    fn release(
        stream: &mut StreamReassembly,
        conn_id: ConnId,
        stream_id: StreamId,
        out: &mut Vec<Completed>,
        stats: &mut NackStats,
    ) {
        let blocker = stream.frames.iter().find(|(_, f)| !f.is_complete()).map(|(id, _)| *id);
        let ready: Vec<u32> = stream
            .frames
            .iter()
            .filter(|(id, f)| f.is_complete() && blocker.is_none_or(|b| **id < b))
            .map(|(id, _)| *id)
            .collect();
        for id in ready {
            let Some(frame) = stream.frames.remove(&id) else { continue };
            if frame.attempts > 0 {
                stats.frames_recovered += 1;
            }
            stream.done.insert(id);
            out.push(Completed { conn_id, stream_id, frame_id: id, flags: frame.flags, ciphertext: frame.concat(), loss: FrameLoss::default() });
        }
    }

    /// NACK mode: the relay cannot supply (part of) `range`'s frame. Give it up at the next poll.
    pub fn nack_miss(&mut self, range: &NackRange) {
        if let Some(f) = self
            .streams
            .get_mut(&(range.conn_id, range.stream_id))
            .and_then(|s| s.frames.get_mut(&range.frame_id))
        {
            f.give_up = true;
        }
    }

    /// NACK mode, call every few milliseconds while [`Self::has_pending`]: decides which frames need
    /// a (further) round of Requests, gives up on the ones that are past saving, and returns the
    /// frames that became deliverable by that plus the Requests to send. `rtt` is the viewer's
    /// connection RTT (sets the retry interval, [`nack_retry_interval`]).
    pub fn poll(&mut self, now: Instant, rtt: Duration) -> (Vec<Completed>, Vec<NackRange>) {
        let mut released = Vec::new();
        let mut requests = Vec::new();
        if !self.nack {
            return (released, requests);
        }
        let interval = nack_retry_interval(rtt);
        for (&(conn_id, stream_id), stream) in &mut self.streams {
            let mut abandon = Vec::new();
            for (&frame_id, f) in stream.frames.iter_mut().filter(|(_, f)| !f.is_complete()) {
                if f.give_up || now.duration_since(f.first_seen) >= NACK_HARD_LIMIT {
                    abandon.push(frame_id);
                    continue;
                }
                let suspects = f.suspects(now);
                if suspects.is_empty() {
                    continue;
                }
                // Too many holes (congestion), or a fragment asked for as often as allowed and
                // still missing a whole interval after the last ask: past saving.
                let exhausted = suspects.iter().any(|&i| {
                    let (n, at) = f.asked(i);
                    n >= MAX_NACK_ROUNDS && at.is_some_and(|at| now >= at + interval)
                });
                if suspects.len() > MAX_REPAIR_FRAGMENTS || exhausted {
                    abandon.push(frame_id);
                    continue;
                }
                // Ask for the fragments never asked for, and again for those whose last ask is
                // older than the interval (a hole that shows up late in a paced keyframe starts
                // its own rounds).
                let due: Vec<usize> = suspects
                    .into_iter()
                    .filter(|&i| {
                        let (n, at) = f.asked(i);
                        n < MAX_NACK_ROUNDS && at.is_none_or(|at| now >= at + interval)
                    })
                    .collect();
                if due.is_empty() {
                    continue;
                }
                if f.requested.is_empty() {
                    f.requested = vec![(0, now); f.fragments.len()];
                }
                for &i in &due {
                    f.requested[i] = (f.requested[i].0 + 1, now);
                }
                f.attempts += 1;
                for (first, count) in runs_of(&due) {
                    requests.push(NackRange { conn_id, stream_id, frame_id, first, count });
                    self.stats.requests += 1;
                    self.stats.fragments_requested += u64::from(count);
                }
            }
            for id in abandon {
                if let Some(dropped) = stream.frames.remove(&id) {
                    if dropped.attempts > 0 {
                        self.stats.frames_abandoned += 1;
                    }
                    stream.pending_loss.note(&dropped);
                    stream.done.insert(id);
                }
            }
            // Memory guard: a stream that never settles must not grow without bound.
            while stream.frames.len() > MAX_FRAMES_NACK {
                let Some((&id, _)) = stream.frames.first_key_value() else { break };
                if let Some(dropped) = stream.frames.remove(&id) {
                    stream.pending_loss.note(&dropped);
                    stream.done.insert(id);
                }
            }
            let before = released.len();
            Self::release(stream, conn_id, stream_id, &mut released, &mut self.stats);
            Self::attach_loss(stream, &mut released[before..]);
        }
        (released, requests)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::MediaHeader;

    /// The pre-NACK entry points, kept as test helpers (the legacy-mode tests use them).
    impl Reassembler {
        fn insert(&mut self, header: &MediaHeader, payload: &[u8]) -> Option<Vec<u8>> {
            self.insert_with_loss(header, payload).0
        }

        fn insert_with_loss(&mut self, header: &MediaHeader, payload: &[u8]) -> (Option<Vec<u8>>, FrameLoss) {
            let mut out = self.ingest(Instant::now(), header, payload);
            assert!(out.len() <= 1);
            match out.pop() {
                Some(c) => (Some(c.ciphertext), c.loss),
                None => (None, FrameLoss::default()),
            }
        }
    }

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

    fn hdr(frame_id: u32, flags: u8, index: u16, count: u16) -> MediaHeader {
        MediaHeader { version: 1, flags, conn_id: 1, stream_id: 0, frame_id, fragment_index: index, fragment_count: count }
    }

    #[test]
    fn an_evicted_incomplete_frame_is_reported_with_the_next_delivered_frame() {
        let mut r = Reassembler::new();
        let (done, loss) = r.insert_with_loss(&hdr(1, 0, 0, 2), b"a");
        assert!(done.is_none() && loss.is_none());
        r.insert_with_loss(&hdr(2, 0, 0, 2), b"a");
        // Frame 3 pushes the in-flight set over the cap: frame 1 is evicted. Nothing is
        // delivered yet, so the loss waits for the next delivery.
        let (done, loss) = r.insert_with_loss(&hdr(3, 0, 0, 2), b"a");
        assert!(done.is_none());
        assert!(loss.is_none(), "reported only alongside a delivered frame");
        let (done, loss) = r.insert_with_loss(&hdr(3, 0, 1, 2), b"b");
        assert_eq!(done.unwrap(), b"ab");
        assert_eq!((loss.frames, loss.keyframe), (1, false));
        // ... and is cleared once reported.
        let (done, loss) = r.insert_with_loss(&hdr(4, 0, 0, 1), b"x");
        assert!(done.is_some());
        assert!(loss.is_none());
    }

    #[test]
    fn a_lost_keyframe_is_flagged_as_such() {
        use crate::protocol::MEDIA_FLAG_KEYFRAME;
        let mut r = Reassembler::new();
        r.insert_with_loss(&hdr(1, MEDIA_FLAG_KEYFRAME, 0, 3), b"a"); // IDR, 2 of 3 fragments never arrive
        r.insert_with_loss(&hdr(2, 0, 0, 2), b"a");
        r.insert_with_loss(&hdr(3, 0, 0, 2), b"a"); // evicts frame 1
        let (done, loss) = r.insert_with_loss(&hdr(3, 0, 1, 2), b"b");
        assert!(done.is_some());
        assert_eq!((loss.frames, loss.keyframe, loss.keyframe_fragments, loss.keyframe_received), (1, true, 3, 1));
    }

    #[test]
    fn an_aged_out_incomplete_frame_is_reported() {
        let mut r = Reassembler::new();
        r.insert_with_loss(&hdr(1, 0, 0, 2), b"a");
        std::thread::sleep(STALE_TIMEOUT + Duration::from_millis(30));
        let (done, loss) = r.insert_with_loss(&hdr(2, 0, 0, 1), b"x");
        assert!(done.is_some());
        assert_eq!(loss.frames, 1);
    }

    #[test]
    fn clean_delivery_reports_no_loss() {
        let mut r = Reassembler::new();
        let ciphertext: Vec<u8> = (0..5000u32).map(|i| i as u8).collect();
        for id in 0..20u32 {
            for d in fragment_frame(1, 0, id, id == 0, &ciphertext, 1200).unwrap() {
                let (h, p) = MediaHeader::unpack(&d).unwrap();
                let (_, loss) = r.insert_with_loss(&h, p);
                assert!(loss.is_none());
            }
        }
    }

    // ---- NACK mode (spec §6.6) ----

    const RTT: Duration = Duration::from_millis(25);

    fn nack_reassembler() -> Reassembler {
        let mut r = Reassembler::new();
        r.set_nack(true);
        r
    }

    /// Frame `id` as `count` one-byte-labelled fragments.
    fn frame_datagrams(id: u32, flags: u8, count: u16) -> Vec<(MediaHeader, Vec<u8>)> {
        (0..count).map(|i| (hdr(id, flags, i, count), vec![i as u8; 3])).collect()
    }

    fn feed(r: &mut Reassembler, now: Instant, frags: &[(MediaHeader, Vec<u8>)], skip: &[u16]) -> Vec<Completed> {
        let mut out = Vec::new();
        for (h, p) in frags {
            if !skip.contains(&h.fragment_index) {
                out.extend(r.ingest(now, h, p));
            }
        }
        out
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn a_gap_is_requested_after_two_later_fragments_and_repair_completes_the_frame() {
        let mut r = nack_reassembler();
        let t0 = Instant::now();
        let f = frame_datagrams(5, 0, 10);
        assert!(feed(&mut r, t0, &f, &[3, 4]).is_empty());
        let (done, reqs) = r.poll(t0 + ms(1), RTT);
        assert!(done.is_empty());
        assert_eq!(reqs, vec![NackRange { conn_id: 1, stream_id: 0, frame_id: 5, first: 3, count: 2 }]);
        // the retransmits arrive
        let out = feed(&mut r, t0 + ms(30), &f[3..5], &[]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].frame_id, 5);
        assert_eq!(out[0].ciphertext, f.iter().flat_map(|(_, p)| p.clone()).collect::<Vec<_>>());
        assert!(out[0].loss.is_none());
        let s = r.stats();
        assert_eq!((s.requests, s.fragments_requested, s.fragments_recovered, s.frames_recovered, s.frames_abandoned), (1, 2, 2, 1, 0));
        assert!(!r.has_pending());
    }

    #[test]
    fn a_hole_right_behind_the_newest_fragment_is_not_asked_for_yet() {
        let mut r = nack_reassembler();
        let t0 = Instant::now();
        // fragment 8 missing, 9 arrived: only one fragment behind it, could be reordering
        feed(&mut r, t0, &frame_datagrams(1, 0, 10), &[8]);
        assert!(r.poll(t0 + ms(1), RTT).1.is_empty());
        // ... but a quiet frame has lost its tail
        let (_, reqs) = r.poll(t0 + IDLE_NACK + ms(1), RTT);
        assert_eq!(reqs.len(), 1);
        assert_eq!((reqs[0].first, reqs[0].count), (8, 1));
    }

    #[test]
    fn a_lost_tail_is_requested_when_the_frame_goes_quiet() {
        let mut r = nack_reassembler();
        let t0 = Instant::now();
        feed(&mut r, t0, &frame_datagrams(1, 0, 10), &[7, 8, 9]);
        assert!(r.poll(t0 + ms(5), RTT).1.is_empty(), "still arriving as far as we know");
        let (_, reqs) = r.poll(t0 + ms(25), RTT);
        assert_eq!((reqs[0].first, reqs[0].count), (7, 3));
    }

    #[test]
    fn later_complete_frames_wait_for_the_repair_and_come_out_in_order() {
        let mut r = nack_reassembler();
        let t0 = Instant::now();
        let f1 = frame_datagrams(1, MEDIA_FLAG_KEYFRAME, 6);
        let f2 = frame_datagrams(2, 0, 3);
        let f3 = frame_datagrams(3, 0, 1);
        assert!(feed(&mut r, t0, &f1, &[2]).is_empty());
        assert!(feed(&mut r, t0 + ms(2), &f2, &[]).is_empty(), "held behind frame 1");
        assert!(feed(&mut r, t0 + ms(3), &f3, &[]).is_empty());
        let (done, reqs) = r.poll(t0 + ms(4), RTT);
        assert!(done.is_empty() && reqs.len() == 1);
        let out = feed(&mut r, t0 + ms(40), &f1[2..3], &[]);
        assert_eq!(out.iter().map(|c| c.frame_id).collect::<Vec<_>>(), vec![1, 2, 3]);
        assert!(out[0].is_keyframe() && !out[1].is_keyframe());
        assert!(out.iter().all(|c| c.conn_id == 1 && c.stream_id == 0));
    }

    #[test]
    fn retries_are_spaced_by_the_interval_then_the_frame_is_abandoned_with_loss_reported() {
        let mut r = nack_reassembler();
        let t0 = Instant::now();
        let f1 = frame_datagrams(1, MEDIA_FLAG_KEYFRAME, 8);
        feed(&mut r, t0, &f1, &[2]);
        let iv = nack_retry_interval(RTT);
        assert_eq!(iv, ms(110));
        assert_eq!(r.poll(t0 + ms(1), RTT).1.len(), 1); // round 1
        assert!(r.poll(t0 + ms(1) + iv - ms(1), RTT).1.is_empty(), "not before the interval");
        assert_eq!(r.poll(t0 + ms(1) + iv, RTT).1.len(), 1); // round 2
        assert_eq!(r.poll(t0 + ms(1) + iv * 2, RTT).1.len(), 1); // round 3
        // a later frame arrives complete while we wait
        let f2 = frame_datagrams(2, 0, 1);
        assert!(feed(&mut r, t0 + ms(1) + iv * 2 + ms(5), &f2, &[]).is_empty());
        let (done, reqs) = r.poll(t0 + ms(1) + iv * 3, RTT);
        assert!(reqs.is_empty());
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].frame_id, 2);
        assert_eq!((done[0].loss.frames, done[0].loss.keyframe), (1, true));
        assert_eq!(r.stats().frames_abandoned, 1);
        assert!(!r.has_pending());
    }

    #[test]
    fn a_miss_from_the_relay_gives_the_frame_up_at_once() {
        let mut r = nack_reassembler();
        let t0 = Instant::now();
        let f1 = frame_datagrams(1, 0, 6);
        feed(&mut r, t0, &f1, &[1]);
        let (_, reqs) = r.poll(t0 + ms(1), RTT);
        feed(&mut r, t0 + ms(2), &frame_datagrams(2, 0, 1), &[]);
        r.nack_miss(&reqs[0]);
        let (done, _) = r.poll(t0 + ms(3), RTT);
        assert_eq!(done.len(), 1);
        assert_eq!((done[0].frame_id, done[0].loss.frames), (2, 1));
    }

    #[test]
    fn a_frame_with_too_many_holes_is_abandoned_without_asking() {
        let mut r = nack_reassembler();
        let t0 = Instant::now();
        let f1 = frame_datagrams(1, 0, 400);
        let skip: Vec<u16> = (100..300).collect();
        feed(&mut r, t0, &f1, &skip);
        let (_, reqs) = r.poll(t0 + ms(1), RTT);
        assert!(reqs.is_empty());
        assert!(!r.has_pending());
    }

    #[test]
    fn duplicates_of_finished_or_abandoned_frames_never_open_a_ghost_frame() {
        let mut r = nack_reassembler();
        let t0 = Instant::now();
        let f1 = frame_datagrams(1, 0, 4);
        assert_eq!(feed(&mut r, t0, &f1, &[]).len(), 1);
        // the whole frame again (a retransmit that raced the original)
        assert!(feed(&mut r, t0 + ms(5), &f1, &[]).is_empty());
        assert!(!r.has_pending(), "no ghost incomplete frame");
        // an abandoned frame's late fragments are ignored too
        let f2 = frame_datagrams(2, 0, 4);
        feed(&mut r, t0, &f2, &[0]);
        let iv = nack_retry_interval(RTT);
        for k in 0..4u32 {
            r.poll(t0 + ms(1) + iv * k, RTT);
        }
        assert!(!r.has_pending());
        assert!(feed(&mut r, t0 + ms(900), &f2[..1], &[]).is_empty());
        assert!(!r.has_pending());
        let out = feed(&mut r, t0 + ms(901), &frame_datagrams(3, 0, 1), &[]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].loss.frames, 1, "the abandoned frame is reported once");
    }

    #[test]
    fn nothing_is_held_without_nack_mode() {
        // legacy: a later complete frame is delivered at once, an older incomplete one is not waited for
        let mut r = Reassembler::new();
        let t0 = Instant::now();
        feed(&mut r, t0, &frame_datagrams(1, 0, 4), &[1]);
        assert_eq!(feed(&mut r, t0, &frame_datagrams(2, 0, 1), &[]).len(), 1);
        assert!(r.poll(t0 + ms(100), RTT).1.is_empty());
        assert!(!r.has_pending());
    }

    #[test]
    fn retry_interval_follows_the_rtt_within_bounds() {
        assert_eq!(nack_retry_interval(ms(5)), ms(100));
        assert_eq!(nack_retry_interval(ms(25)), ms(110));
        assert_eq!(nack_retry_interval(ms(100)), ms(260));
        assert_eq!(nack_retry_interval(ms(400)), ms(300));
    }

    #[test]
    fn a_hole_counts_only_while_the_hard_limit_has_not_passed() {
        let mut r = nack_reassembler();
        let t0 = Instant::now();
        feed(&mut r, t0, &frame_datagrams(1, 0, 6), &[1]);
        // a huge RTT would space rounds beyond the hard limit; the limit still ends the wait
        let (done, _) = r.poll(t0 + NACK_HARD_LIMIT + ms(1), Duration::from_secs(5));
        assert!(done.is_empty());
        assert!(!r.has_pending());
    }

    #[test]
    fn a_hole_that_shows_up_late_in_a_long_frame_gets_its_own_rounds() {
        let mut r = nack_reassembler();
        let t0 = Instant::now();
        let f = frame_datagrams(1, MEDIA_FLAG_KEYFRAME, 100);
        feed(&mut r, t0, &f[..30], &[3]);
        let (_, reqs) = r.poll(t0 + ms(1), RTT);
        assert_eq!(reqs.iter().map(|q| (q.first, q.count)).collect::<Vec<_>>(), vec![(3, 1)]);
        // the paced keyframe is still arriving; another hole appears behind the newest fragments
        feed(&mut r, t0 + ms(50), &f[30..60], &[40]);
        let (_, reqs) = r.poll(t0 + ms(55), RTT);
        assert_eq!(reqs.iter().map(|q| (q.first, q.count)).collect::<Vec<_>>(), vec![(40, 1)], "3 is not due yet");
        // both are asked again when their own interval is over
        let (_, reqs) = r.poll(t0 + ms(1) + nack_retry_interval(RTT), RTT);
        assert!(reqs.iter().any(|q| q.first == 3), "{reqs:?}"); // (the quiet frame's tail is asked for too)
        let (_, reqs) = r.poll(t0 + ms(55) + nack_retry_interval(RTT), RTT);
        assert!(reqs.iter().any(|q| q.first == 40), "{reqs:?}");
        assert!(r.has_pending());
    }
}
