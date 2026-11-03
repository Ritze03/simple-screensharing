//! Bounded cache of recently sent / forwarded media datagrams, the source of NACK retransmits
//! (spec §6.6). Used by the relay (what it forwarded, per video stream) and by the native sender
//! (what it sent, per video stream); both store `Bytes`, so a cached datagram is a refcount, not a copy.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// How long a datagram stays available for a retransmit. A viewer asks within ~1 RTT of noticing
/// the hole and retries for at most ~3 x 250 ms, so 2 s covers every request that can still help.
pub const CACHE_TTL: Duration = Duration::from_secs(2);
/// Hard byte cap per stream (the 1 MB IDRs of a 8 Mbit/s stream are a small part of it; a 40 Mbit/s
/// stream holds ~1.6 s). Oldest frames go first.
pub const CACHE_MAX_BYTES: usize = 8 * 1024 * 1024;

struct CachedFrame<T> {
    at: Instant,
    frags: Vec<Option<T>>,
    bytes: usize,
}

/// Datagrams of one stream by `(frame_id, fragment_index)`.
pub struct FragmentCache<T> {
    frames: BTreeMap<u32, CachedFrame<T>>,
    bytes: usize,
    ttl: Duration,
    max_bytes: usize,
}

impl<T: Clone + AsRef<[u8]>> Default for FragmentCache<T> {
    fn default() -> Self {
        Self::new(CACHE_TTL, CACHE_MAX_BYTES)
    }
}

impl<T: Clone + AsRef<[u8]>> FragmentCache<T> {
    pub fn new(ttl: Duration, max_bytes: usize) -> Self {
        Self { frames: BTreeMap::new(), bytes: 0, ttl, max_bytes }
    }

    /// Bytes held (payload plus slot overhead).
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn frames(&self) -> usize {
        self.frames.len()
    }

    /// Remember fragment `index` of `count` of `frame_id`, then evict what is too old or over the cap
    /// (never the frame just touched).
    pub fn insert(&mut self, now: Instant, frame_id: u32, index: u16, count: u16, data: T) {
        let (index, count) = (index as usize, count as usize);
        if count == 0 || index >= count {
            return;
        }
        let frame = self.frames.entry(frame_id).or_insert_with(|| CachedFrame { at: now, frags: Vec::new(), bytes: 0 });
        if frame.frags.len() != count {
            self.bytes -= frame.bytes;
            frame.frags = vec![None; count];
            frame.bytes = count * std::mem::size_of::<Option<T>>();
            self.bytes += frame.bytes;
        }
        if frame.frags[index].is_none() {
            let len = data.as_ref().len();
            frame.frags[index] = Some(data);
            frame.bytes += len;
            self.bytes += len;
        }
        self.evict(now, frame_id);
    }

    fn evict(&mut self, now: Instant, keep: u32) {
        while let Some((&oldest, frame)) = self.frames.first_key_value() {
            let expired = now.duration_since(frame.at) > self.ttl;
            if oldest == keep || !(expired || self.bytes > self.max_bytes) {
                break;
            }
            if let Some(f) = self.frames.remove(&oldest) {
                self.bytes -= f.bytes;
            }
        }
    }

    /// The cached datagrams of `first .. first + count` of `frame_id`, in index order, and the
    /// contiguous runs `(first, count)` that are not cached.
    pub fn lookup(&self, frame_id: u32, first: u16, count: u16) -> (Vec<T>, Vec<(u16, u16)>) {
        let mut hits = Vec::new();
        let mut missing: Vec<(u16, u16)> = Vec::new();
        let frame = self.frames.get(&frame_id);
        for i in first..first.saturating_add(count) {
            match frame.and_then(|f| f.frags.get(i as usize)).and_then(|d| d.as_ref()) {
                Some(d) => hits.push(d.clone()),
                None => match missing.last_mut() {
                    Some((start, n)) if *start + *n == i => *n += 1,
                    _ => missing.push((i, 1)),
                },
            }
        }
        (hits, missing)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frag(n: usize) -> Vec<u8> {
        vec![7u8; n]
    }

    #[test]
    fn lookup_returns_hits_and_missing_runs() {
        let mut c = FragmentCache::<Vec<u8>>::default();
        let now = Instant::now();
        for i in [0u16, 1, 3, 4, 7] {
            c.insert(now, 10, i, 8, frag(5));
        }
        let (hits, missing) = c.lookup(10, 0, 8);
        assert_eq!(hits.len(), 5);
        assert_eq!(missing, vec![(2, 1), (5, 2)]);
        // an unknown frame is all missing; a range past the frame's count too
        assert_eq!(c.lookup(11, 3, 4), (vec![], vec![(3, 4)]));
        assert_eq!(c.lookup(10, 6, 4).1, vec![(6, 1), (8, 2)]);
    }

    #[test]
    fn ttl_evicts_old_frames_on_the_next_insert() {
        let mut c = FragmentCache::<Vec<u8>>::new(Duration::from_millis(100), 1 << 20);
        let t0 = Instant::now();
        c.insert(t0, 1, 0, 1, frag(10));
        c.insert(t0 + Duration::from_millis(80), 2, 0, 1, frag(10));
        c.insert(t0 + Duration::from_millis(160), 3, 0, 1, frag(10));
        assert_eq!(c.lookup(1, 0, 1).0.len(), 0, "older than the ttl");
        assert_eq!(c.lookup(2, 0, 1).0.len(), 1);
        assert_eq!(c.frames(), 2);
    }

    #[test]
    fn byte_cap_evicts_oldest_first_but_never_the_frame_being_filled() {
        let slot = std::mem::size_of::<Option<Vec<u8>>>();
        let cap = 3 * (1000 + slot);
        let mut c = FragmentCache::<Vec<u8>>::new(Duration::from_secs(60), cap);
        let now = Instant::now();
        for id in 1..=5u32 {
            c.insert(now, id, 0, 1, frag(1000));
            assert!(c.bytes() <= cap, "frame {id}: {} > {cap}", c.bytes());
        }
        assert_eq!(c.frames(), 3);
        assert!(c.lookup(1, 0, 1).0.is_empty() && c.lookup(2, 0, 1).0.is_empty());
        assert_eq!(c.lookup(5, 0, 1).0.len(), 1);
        // a single frame bigger than the cap is kept while it is the one being filled
        let mut c = FragmentCache::<Vec<u8>>::new(Duration::from_secs(60), 100);
        c.insert(now, 9, 0, 2, frag(500));
        c.insert(now, 9, 1, 2, frag(500));
        assert_eq!(c.lookup(9, 0, 2).0.len(), 2);
        c.insert(now, 10, 0, 1, frag(10));
        assert_eq!(c.frames(), 1, "the oversized frame goes as soon as another one needs room");
    }

    #[test]
    fn accounting_returns_to_zero_and_duplicates_do_not_double_count() {
        let mut c = FragmentCache::<Vec<u8>>::new(Duration::from_millis(10), 1 << 20);
        let t0 = Instant::now();
        c.insert(t0, 1, 0, 2, frag(100));
        let after_one = c.bytes();
        c.insert(t0, 1, 0, 2, frag(100));
        assert_eq!(c.bytes(), after_one);
        c.insert(t0 + Duration::from_secs(1), 2, 0, 1, frag(1));
        c.insert(t0 + Duration::from_secs(2), 3, 0, 1, frag(1));
        assert_eq!(c.frames(), 1);
        assert_eq!(c.bytes(), 1 + std::mem::size_of::<Option<Vec<u8>>>());
    }
}
