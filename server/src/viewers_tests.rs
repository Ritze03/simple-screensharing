//! Relay side of "who watches my stream" (spec §9.1): the Hello/Ack handshake, a list on every
//! change of a stream's subscriber set, the periodic refresh, and the old-peer behaviour (an owner
//! that never said Hello, or a relay with `SSSHARE_VIEWERS=0`, sends nothing new).

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;

use crate::protocol::{
    ClientToRelay, ConnId, RelayToClient, RoomId, StreamKind, VIEWERS_TAG_ACK, VIEWERS_TAG_HELLO, VIEWERS_TAG_LIST,
    unpack_viewers,
};
use crate::relay::tests::{await_room_state, insecure_client_endpoint, recv_ctrl, send_ctrl};
use crate::relay::{build_endpoint, refresh_viewers, serve};
use crate::room::Registry;

const SHARER: ConnId = 0xA000_0000_0000_0001;
const VIEWER: ConnId = 0xB000_0000_0000_0002;
const OTHER: ConnId = 0xC000_0000_0000_0003;
const STREAM: u8 = 4;

struct Peer {
    conn: quinn::Connection,
    send: quinn::SendStream,
    _recv: quinn::RecvStream,
}

async fn join(client: &quinn::Endpoint, addr: std::net::SocketAddr, id: ConnId, room_id: RoomId) -> Peer {
    let conn = client.connect(addr, "localhost").unwrap().await.unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    send_ctrl(&mut send, &ClientToRelay::Join { conn_id: id, room_id, display_name: format!("{id:x}") }).await;
    assert!(matches!(recv_ctrl(&mut recv).await, RelayToClient::PresenceSnapshot { .. }));
    Peer { conn, send, _recv: recv }
}

async fn next_datagram(conn: &quinn::Connection) -> Bytes {
    tokio::time::timeout(Duration::from_secs(3), conn.read_datagram()).await.expect("a datagram within 3 s").expect("read")
}

async fn no_datagram(conn: &quinn::Connection) {
    assert!(tokio::time::timeout(Duration::from_millis(300), conn.read_datagram()).await.is_err(), "no datagram expected");
}

/// The next list for STREAM, skipping nothing: asserts the tag.
async fn next_list(conn: &quinn::Connection) -> Vec<ConnId> {
    let d = next_datagram(conn).await;
    assert_eq!(d[0], VIEWERS_TAG_LIST, "expected a viewer list, got {d:?}");
    let (sid, ids) = unpack_viewers(&d).expect("well-formed list");
    assert_eq!(sid, STREAM);
    ids
}

struct Rig {
    registry: Arc<Registry>,
    room_id: RoomId,
    sharer: Peer,
    viewer: Peer,
    other: Peer,
}

/// A relay with a sharer (stream 4 announced) and two members that have not subscribed yet.
async fn rig(registry: Registry) -> Rig {
    let registry = Arc::new(registry);
    let endpoint = build_endpoint((IpAddr::V4(Ipv4Addr::LOCALHOST), 0).into()).unwrap();
    let addr = endpoint.local_addr().unwrap();
    tokio::spawn(serve(registry.clone(), endpoint));
    let client = insecure_client_endpoint();
    let room_id: RoomId = [6u8; 16];
    let mut sharer = join(&client, addr, SHARER, room_id).await;
    send_ctrl(
        &mut sharer.send,
        &ClientToRelay::AnnounceStream { stream_id: STREAM, kind: StreamKind::Video, label: "m".into(), width: 1, height: 1 },
    )
    .await;
    await_room_state(&registry, room_id, "announced", |r| r.streams.contains_key(&(SHARER, STREAM))).await;
    let viewer = join(&client, addr, VIEWER, room_id).await;
    let other = join(&client, addr, OTHER, room_id).await;
    Rig { registry, room_id, sharer, viewer, other }
}

async fn subscribe(p: &mut Peer) {
    send_ctrl(&mut p.send, &ClientToRelay::Subscribe { target: SHARER, stream_id: STREAM }).await;
}

async fn unsubscribe(p: &mut Peer) {
    send_ctrl(&mut p.send, &ClientToRelay::Unsubscribe { target: SHARER, stream_id: STREAM }).await;
}

fn hello(p: &Peer) {
    p.conn.send_datagram(Bytes::from_static(&[VIEWERS_TAG_HELLO])).unwrap();
}

#[tokio::test]
async fn hello_is_acked_and_every_change_sends_the_whole_list_to_the_owner_only() {
    let mut rig = rig(Registry::new((Ipv4Addr::LOCALHOST, 0).into())).await;
    hello(&rig.sharer);
    assert_eq!(next_datagram(&rig.sharer.conn).await.as_ref(), &[VIEWERS_TAG_ACK]);
    assert_eq!(next_list(&rig.sharer.conn).await, Vec::<ConnId>::new(), "state right after the ack");

    subscribe(&mut rig.viewer).await;
    assert_eq!(next_list(&rig.sharer.conn).await, vec![VIEWER]);
    subscribe(&mut rig.other).await;
    assert_eq!(next_list(&rig.sharer.conn).await, vec![VIEWER, OTHER]);
    unsubscribe(&mut rig.viewer).await;
    assert_eq!(next_list(&rig.sharer.conn).await, vec![OTHER]);
    // a viewer leaving the room is a change too
    rig.other.send.finish().ok();
    rig.other.conn.close(0u32.into(), b"bye");
    assert_eq!(next_list(&rig.sharer.conn).await, Vec::<ConnId>::new());

    // privacy: the viewers (who never said Hello) were sent nothing
    no_datagram(&rig.viewer.conn).await;
}

#[tokio::test]
async fn a_late_hello_gets_the_current_state_and_the_refresh_repeats_it() {
    let mut rig = rig(Registry::new((Ipv4Addr::LOCALHOST, 0).into())).await;
    subscribe(&mut rig.viewer).await;
    await_room_state(&rig.registry, rig.room_id, "subscribed", |r| {
        r.streams.get(&(SHARER, STREAM)).is_some_and(|s| s.subscribers.len() == 1)
    })
    .await;
    no_datagram(&rig.sharer.conn).await; // no Hello yet: nothing, like an old relay
    hello(&rig.sharer);
    assert_eq!(next_datagram(&rig.sharer.conn).await.as_ref(), &[VIEWERS_TAG_ACK]);
    assert_eq!(next_list(&rig.sharer.conn).await, vec![VIEWER]);
    tokio::spawn(refresh_viewers(Arc::downgrade(&rig.registry)));
    assert_eq!(next_list(&rig.sharer.conn).await, vec![VIEWER], "periodic refresh");
}

#[tokio::test]
async fn an_owner_that_never_said_hello_is_left_alone() {
    let mut rig = rig(Registry::new((Ipv4Addr::LOCALHOST, 0).into())).await;
    subscribe(&mut rig.viewer).await;
    unsubscribe(&mut rig.viewer).await;
    no_datagram(&rig.sharer.conn).await;
}

#[tokio::test]
async fn with_viewers_disabled_the_relay_behaves_like_the_old_one() {
    let mut registry = Registry::new((Ipv4Addr::LOCALHOST, 0).into());
    registry.viewers_enabled = false;
    let mut rig = rig(registry).await;
    hello(&rig.sharer);
    subscribe(&mut rig.viewer).await;
    no_datagram(&rig.sharer.conn).await; // no ack, no list
}
