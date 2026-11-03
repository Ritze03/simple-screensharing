//! Native side of selective retransmission (spec §6.6, task #104): the capability probe, the
//! sender's ring of recent datagrams, and the kill switch. The viewer half (detecting holes,
//! asking, holding frames in order) is `fragment::Reassembler`; the datagram loop in `net.rs`
//! connects the two to the wire.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use bytes::Bytes;

use crate::protocol::{FragmentCache, MEDIA_FLAG_RETRANSMIT, NACK_TAG_HELLO, NackRange, StreamId};

/// `SSS_DISABLE_NACK=1` turns the whole feature off on this client: it never says Hello, so the
/// relay never marks it NACK-capable (no requests are forwarded to it, and as a viewer it never
/// sees a HelloAck and so never asks). The A/B switch for measurements and the field rollback.
pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| !std::env::var("SSS_DISABLE_NACK").is_ok_and(|v| !v.is_empty() && v != "0"))
}

/// Most fragments one request is answered with (mirrors the relay's cap).
const MAX_RESEND_FRAGMENTS: u16 = 256;

/// The video datagrams this connection sent recently, per stream (`FragmentCache`: 2 s, 8 MiB
/// each), so the relay can ask for the ones that never reached it. Cheap: the cache holds
/// `Bytes` clones of what went to quinn.
#[derive(Default)]
pub struct SendRing {
    streams: Mutex<HashMap<StreamId, FragmentCache<Bytes>>>,
    /// Fragments re-sent on request (log / measurements).
    pub resent: AtomicU64,
}

impl SendRing {
    /// Remember a prepared frame's datagrams.
    pub fn record(&self, stream_id: StreamId, frame_id: u32, datagrams: &[Bytes]) {
        if !enabled() {
            return;
        }
        let now = Instant::now();
        let count = datagrams.len().min(u16::MAX as usize) as u16;
        let mut streams = self.streams.lock().unwrap_or_else(|p| p.into_inner());
        let cache = streams.entry(stream_id).or_default();
        for (i, d) in datagrams.iter().enumerate().take(count as usize) {
            cache.insert(now, frame_id, i as u16, count, d.clone());
        }
    }

    /// The stream ended: drop what it cached.
    pub fn forget(&self, stream_id: StreamId) {
        self.streams.lock().unwrap_or_else(|p| p.into_inner()).remove(&stream_id);
    }

    /// The datagrams answering a relay-forwarded request: the requested fragments that are still
    /// cached, byte for byte except for the retransmit flag (spec §6.6).
    pub fn resends(&self, range: &NackRange) -> Vec<Bytes> {
        let streams = self.streams.lock().unwrap_or_else(|p| p.into_inner());
        let Some(cache) = streams.get(&range.stream_id) else {
            return Vec::new();
        };
        cache
            .lookup(range.frame_id, range.first, range.count.min(MAX_RESEND_FRAGMENTS))
            .0
            .into_iter()
            .map(|d| {
                let mut copy = d.to_vec();
                copy[1] |= MEDIA_FLAG_RETRANSMIT;
                Bytes::from(copy)
            })
            .collect()
    }

    /// Answer a relay-forwarded request on `connection`.
    pub fn resend(&self, connection: &quinn::Connection, range: &NackRange) {
        let sent = self.resends(range).into_iter().filter(|d| connection.send_datagram(d.clone()).is_ok()).count();
        self.resent.fetch_add(sent as u64, Ordering::Relaxed);
    }
}

/// Say Hello to the relay once per second (at most 10 times) until it acknowledges (spec §6.6). An
/// old relay drops the 1-byte datagram as undersized and never answers, which is the signal to
/// carry on without NACK.
pub async fn hello_loop(connection: quinn::Connection, acked: Arc<AtomicBool>) {
    if !enabled() {
        return;
    }
    for _ in 0..10 {
        if acked.load(Ordering::Relaxed) || connection.send_datagram(Bytes::from_static(&[NACK_TAG_HELLO])).is_err() {
            return;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fragment::fragment_frame;
    use crate::protocol::MediaHeader;

    #[test]
    fn a_resend_is_the_original_datagram_plus_the_retransmit_flag() {
        let ring = SendRing::default();
        let ciphertext: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        let sent: Vec<Bytes> = fragment_frame(7, 2, 40, true, &ciphertext, 1000).unwrap().into_iter().map(Bytes::from).collect();
        assert_eq!(sent.len(), 6);
        ring.record(2, 40, &sent);
        let got = ring.resends(&NackRange { conn_id: 7, stream_id: 2, frame_id: 40, first: 1, count: 3 });
        assert_eq!(got.len(), 3);
        for (i, d) in got.iter().enumerate() {
            let (h, payload) = MediaHeader::unpack(d).unwrap();
            let (oh, opayload) = MediaHeader::unpack(&sent[i + 1]).unwrap();
            assert_eq!(h.flags, oh.flags | MEDIA_FLAG_RETRANSMIT);
            assert!(h.is_keyframe());
            assert_eq!((h.frame_id, h.fragment_index, h.fragment_count), (oh.frame_id, oh.fragment_index, oh.fragment_count));
            assert_eq!(payload, opayload, "same ciphertext slice: nothing is re-encrypted");
        }
        // unknown frame / stream / forgotten stream: nothing
        assert!(ring.resends(&NackRange { conn_id: 7, stream_id: 2, frame_id: 41, first: 0, count: 2 }).is_empty());
        assert!(ring.resends(&NackRange { conn_id: 7, stream_id: 3, frame_id: 40, first: 0, count: 2 }).is_empty());
        ring.forget(2);
        assert!(ring.resends(&NackRange { conn_id: 7, stream_id: 2, frame_id: 40, first: 0, count: 2 }).is_empty());
    }
}
