//! Relay side of selective retransmission (spec §6.6): the Hello handshake, answering a Request
//! from the cache, forwarding a miss to the stream owner, the retransmit flag's routing, and the
//! old-peer behaviour (no Hello: no answers, no duplicates).

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::Bytes;

use crate::protocol::{
    ClientToRelay, ConnId, MEDIA_FLAG_RETRANSMIT, MediaHeader, NACK_TAG_HELLO, NACK_TAG_HELLO_ACK, NACK_TAG_MISS,
    NACK_TAG_REQUEST, NackRange, PROTOCOL_VERSION, RelayToClient, RoomId, StreamKind,
};
use crate::relay::tests::{await_room_state, insecure_client_endpoint, recv_ctrl, send_ctrl};
use crate::relay::{build_endpoint, serve};
use crate::room::Registry;

const SHARER: ConnId = 0xA000_0000_0000_0001;
const VIEWER: ConnId = 0xB000_0000_0000_0002;
const OTHER: ConnId = 0xC000_0000_0000_0003;
const STREAM: u8 = 0;

/// One joined test client; the control stream halves are kept so the connection stays open.
struct Peer {
    conn: quinn::Connection,
    _send: quinn::SendStream,
    _recv: quinn::RecvStream,
}

async fn join(client: &quinn::Endpoint, addr: std::net::SocketAddr, id: ConnId, room_id: RoomId) -> Peer {
    let conn = client.connect(addr, "localhost").unwrap().await.unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    send_ctrl(&mut send, &ClientToRelay::Join { conn_id: id, room_id, display_name: format!("{id:x}") }).await;
    assert!(matches!(recv_ctrl(&mut recv).await, RelayToClient::PresenceSnapshot { .. }));
    Peer { conn, _send: send, _recv: recv }
}

fn fragment(frame_id: u32, index: u16, count: u16, flags: u8) -> Bytes {
    let header = MediaHeader {
        version: PROTOCOL_VERSION,
        flags,
        conn_id: SHARER,
        stream_id: STREAM,
        frame_id,
        fragment_index: index,
        fragment_count: count,
    };
    let mut buf = header.pack().to_vec();
    buf.extend_from_slice(&[index as u8; 40]);
    Bytes::from(buf)
}

async fn next_datagram(conn: &quinn::Connection) -> Bytes {
    tokio::time::timeout(Duration::from_secs(2), conn.read_datagram())
        .await
        .expect("a datagram within 2 s")
        .expect("datagram read")
}

async fn no_datagram(conn: &quinn::Connection) {
    assert!(
        tokio::time::timeout(Duration::from_millis(300), conn.read_datagram()).await.is_err(),
        "no datagram expected"
    );
}

struct Rig {
    registry: Arc<Registry>,
    sharer: Peer,
    viewer: Peer,
    other: Peer,
}

/// A relay with a video stream (SHARER, 0) watched by VIEWER and OTHER.
async fn rig(registry: Registry) -> Rig {
    let registry = Arc::new(registry);
    let endpoint = build_endpoint((IpAddr::V4(Ipv4Addr::LOCALHOST), 0).into()).unwrap();
    let addr = endpoint.local_addr().unwrap();
    tokio::spawn(serve(registry.clone(), endpoint));
    let client = insecure_client_endpoint();
    let room_id: RoomId = [5u8; 16];
    let mut sharer = join(&client, addr, SHARER, room_id).await;
    send_ctrl(
        &mut sharer._send,
        &ClientToRelay::AnnounceStream { stream_id: STREAM, kind: StreamKind::Video, label: "m".into(), width: 1, height: 1 },
    )
    .await;
    await_room_state(&registry, room_id, "announced", |r| r.streams.contains_key(&(SHARER, STREAM))).await;
    let mut viewer = join(&client, addr, VIEWER, room_id).await;
    let mut other = join(&client, addr, OTHER, room_id).await;
    for p in [&mut viewer, &mut other] {
        send_ctrl(&mut p._send, &ClientToRelay::Subscribe { target: SHARER, stream_id: STREAM }).await;
    }
    await_room_state(&registry, room_id, "subscribed", |r| {
        r.streams.get(&(SHARER, STREAM)).is_some_and(|s| s.subscribers.len() == 2)
    })
    .await;
    Rig { registry, sharer, viewer, other }
}

fn hello(p: &Peer) {
    p.conn.send_datagram(Bytes::from_static(&[NACK_TAG_HELLO])).unwrap();
}

fn request(frame_id: u32, first: u16, count: u16) -> Bytes {
    Bytes::copy_from_slice(&NackRange { conn_id: SHARER, stream_id: STREAM, frame_id, first, count }.pack(NACK_TAG_REQUEST))
}

/// Forward `frames` x `count` fragments from the sharer and drain them at both viewers.
async fn send_frames(rig: &Rig, frames: std::ops::Range<u32>, count: u16, lost_before_relay: &[u16]) {
    let frame_count = frames.len();
    for f in frames {
        for i in 0..count {
            if !lost_before_relay.contains(&i) {
                rig.sharer.conn.send_datagram(fragment(f, i, count, 0)).unwrap();
            }
        }
    }
    let n = frame_count * (0..count).filter(|i| !lost_before_relay.contains(i)).count();
    for p in [&rig.viewer, &rig.other] {
        for _ in 0..n {
            next_datagram(&p.conn).await;
        }
    }
}

#[tokio::test]
async fn hello_is_acknowledged_and_a_request_is_served_from_the_cache_to_the_asker_only() {
    let rig = rig(Registry::new((Ipv4Addr::LOCALHOST, 0).into())).await;
    hello(&rig.viewer);
    assert_eq!(next_datagram(&rig.viewer.conn).await.as_ref(), &[NACK_TAG_HELLO_ACK]);

    send_frames(&rig, 10..12, 6, &[]).await;
    // the viewer lost fragments 2 and 3 of frame 11 on its own leg
    rig.viewer.conn.send_datagram(request(11, 2, 2)).unwrap();
    let a = next_datagram(&rig.viewer.conn).await;
    let b = next_datagram(&rig.viewer.conn).await;
    assert_eq!(a, fragment(11, 2, 6, 0), "byte-identical to the original");
    assert_eq!(b, fragment(11, 3, 6, 0));
    no_datagram(&rig.other.conn).await; // nobody else gets a duplicate
    assert_eq!(rig.registry.nack.from_cache.load(Ordering::Relaxed), 2);
    assert_eq!(rig.registry.nack.forwarded.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn a_miss_goes_to_an_owner_that_said_hello_and_its_flagged_resend_reaches_only_the_asker() {
    let rig = rig(Registry::new((Ipv4Addr::LOCALHOST, 0).into())).await;
    hello(&rig.sharer);
    assert_eq!(next_datagram(&rig.sharer.conn).await.as_ref(), &[NACK_TAG_HELLO_ACK]);
    hello(&rig.viewer);
    next_datagram(&rig.viewer.conn).await;

    // fragments 1 and 4 of frame 20 never reached the relay
    send_frames(&rig, 20..21, 6, &[1, 4]).await;
    rig.viewer.conn.send_datagram(request(20, 0, 6)).unwrap();
    // 0, 2, 3, 5 come from the cache (the viewer already had them, but it asked for the range)
    for i in [0u16, 2, 3, 5] {
        assert_eq!(next_datagram(&rig.viewer.conn).await, fragment(20, i, 6, 0));
    }
    // 1 and 4 are forwarded to the sharer as two runs
    let mut asked = Vec::new();
    for _ in 0..2 {
        let d = next_datagram(&rig.sharer.conn).await;
        let (tag, r) = NackRange::unpack(&d).expect("a request");
        assert_eq!((tag, r.conn_id, r.stream_id, r.frame_id, r.count), (NACK_TAG_REQUEST, SHARER, STREAM, 20, 1));
        asked.push(r.first);
    }
    asked.sort();
    assert_eq!(asked, vec![1, 4]);
    // the sharer re-sends with the retransmit flag: only the asker gets it, the cache learns it
    for i in [1u16, 4] {
        rig.sharer.conn.send_datagram(fragment(20, i, 6, MEDIA_FLAG_RETRANSMIT)).unwrap();
    }
    for i in [1u16, 4] {
        assert_eq!(next_datagram(&rig.viewer.conn).await, fragment(20, i, 6, MEDIA_FLAG_RETRANSMIT));
    }
    no_datagram(&rig.other.conn).await;
    assert_eq!(rig.registry.nack.owner_resends.load(Ordering::Relaxed), 2);
    // a later asker now hits the cache for them
    hello(&rig.other);
    next_datagram(&rig.other.conn).await;
    rig.other.conn.send_datagram(request(20, 1, 1)).unwrap();
    assert_eq!(next_datagram(&rig.other.conn).await, fragment(20, 1, 6, MEDIA_FLAG_RETRANSMIT));
}

#[tokio::test]
async fn an_owner_without_hello_makes_a_miss_answer_miss_at_once() {
    let rig = rig(Registry::new((Ipv4Addr::LOCALHOST, 0).into())).await;
    hello(&rig.viewer);
    next_datagram(&rig.viewer.conn).await;
    send_frames(&rig, 30..31, 6, &[2]).await;
    rig.viewer.conn.send_datagram(request(30, 2, 1)).unwrap();
    let d = next_datagram(&rig.viewer.conn).await;
    let (tag, r) = NackRange::unpack(&d).unwrap();
    assert_eq!((tag, r.frame_id, r.first, r.count), (NACK_TAG_MISS, 30, 2, 1));
    no_datagram(&rig.sharer.conn).await; // an old sender is never bothered
    assert_eq!(rig.registry.nack.misses.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn requests_for_unknown_streams_are_ignored_and_media_still_flows_unchanged() {
    let rig = rig(Registry::new((Ipv4Addr::LOCALHOST, 0).into())).await;
    send_frames(&rig, 1..3, 4, &[]).await;
    hello(&rig.viewer);
    next_datagram(&rig.viewer.conn).await;
    let mut r = NackRange { conn_id: SHARER, stream_id: 9, frame_id: 2, first: 0, count: 1 };
    rig.viewer.conn.send_datagram(Bytes::copy_from_slice(&r.pack(NACK_TAG_REQUEST))).unwrap();
    r.conn_id = OTHER;
    rig.viewer.conn.send_datagram(Bytes::copy_from_slice(&r.pack(NACK_TAG_REQUEST))).unwrap();
    no_datagram(&rig.viewer.conn).await;
    // and an ordinary media datagram still reaches both viewers
    rig.sharer.conn.send_datagram(fragment(7, 0, 1, 0)).unwrap();
    for p in [&rig.viewer, &rig.other] {
        assert_eq!(next_datagram(&p.conn).await, fragment(7, 0, 1, 0));
    }
}

#[tokio::test]
async fn with_nack_disabled_the_relay_behaves_like_the_old_one() {
    let mut registry = Registry::new((Ipv4Addr::LOCALHOST, 0).into());
    registry.nack.enabled = false;
    let rig = rig(registry).await;
    hello(&rig.viewer);
    send_frames(&rig, 1..2, 4, &[]).await;
    rig.viewer.conn.send_datagram(request(1, 0, 4)).unwrap();
    no_datagram(&rig.viewer.conn).await; // no HelloAck, no retransmit
}
