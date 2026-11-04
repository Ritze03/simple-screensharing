//! Relay side of emoji avatars (spec §9.2): the Hello/Ack handshake, the whole list on every
//! change, the periodic refresh, validation of what a member sets, and the old-peer behaviour (a
//! member that never said Hello, or a relay with `SSSHARE_AVATARS=0`, gets and keeps nothing new).

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;

use crate::protocol::{
    AVATAR_TAG_ACK, AVATAR_TAG_HELLO, AVATAR_TAG_LIST, AVATAR_TAG_SET, ClientToRelay, ConnId, MAX_AVATAR_BYTES,
    RelayToClient, RoomId, pack_avatar_set, unpack_avatars,
};
use crate::relay::tests::{await_room_state, insecure_client_endpoint, recv_ctrl, send_ctrl};
use crate::relay::{build_endpoint, refresh_avatars, serve};
use crate::room::Registry;

const ANA: ConnId = 0xA000_0000_0000_0001;
const BEN: ConnId = 0xB000_0000_0000_0002;
const OLD: ConnId = 0xC000_0000_0000_0003;

struct Peer {
    conn: quinn::Connection,
    _send: quinn::SendStream,
    _recv: quinn::RecvStream,
}

struct Rig {
    registry: Arc<Registry>,
    room_id: RoomId,
    ana: Peer,
    ben: Peer,
    old: Peer,
}

async fn join(client: &quinn::Endpoint, addr: std::net::SocketAddr, id: ConnId, room_id: RoomId) -> Peer {
    let conn = client.connect(addr, "localhost").unwrap().await.unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    send_ctrl(&mut send, &ClientToRelay::Join { conn_id: id, room_id, display_name: format!("{id:x}") }).await;
    assert!(matches!(recv_ctrl(&mut recv).await, RelayToClient::PresenceSnapshot { .. }));
    Peer { conn, _send: send, _recv: recv }
}

async fn rig(registry: Registry) -> Rig {
    let registry = Arc::new(registry);
    let endpoint = build_endpoint((IpAddr::V4(Ipv4Addr::LOCALHOST), 0).into()).unwrap();
    let addr = endpoint.local_addr().unwrap();
    tokio::spawn(serve(registry.clone(), endpoint));
    let client = insecure_client_endpoint();
    let room_id: RoomId = [7u8; 16];
    let ana = join(&client, addr, ANA, room_id).await;
    let ben = join(&client, addr, BEN, room_id).await;
    let old = join(&client, addr, OLD, room_id).await;
    Rig { registry, room_id, ana, ben, old }
}

fn hello(p: &Peer) {
    p.conn.send_datagram(Bytes::from_static(&[AVATAR_TAG_HELLO])).unwrap();
}

fn set(p: &Peer, emoji: &str) {
    p.conn.send_datagram(Bytes::from(pack_avatar_set(emoji).unwrap())).unwrap();
}

async fn next_datagram(p: &Peer) -> Bytes {
    tokio::time::timeout(Duration::from_secs(3), p.conn.read_datagram()).await.expect("a datagram within 3 s").expect("read")
}

async fn no_datagram(p: &Peer) {
    assert!(tokio::time::timeout(Duration::from_millis(300), p.conn.read_datagram()).await.is_err(), "no datagram expected");
}

async fn next_list(p: &Peer) -> Vec<(ConnId, String)> {
    let d = next_datagram(p).await;
    assert_eq!(d[0], AVATAR_TAG_LIST, "expected an avatar list, got {d:?}");
    unpack_avatars(&d).expect("well-formed list")
}

async fn has_avatar(rig: &Rig, who: ConnId, emoji: &'static str) {
    await_room_state(&rig.registry, rig.room_id, "avatar stored", |r| {
        r.connections.get(&who).is_some_and(|c| c.avatar == emoji)
    })
    .await;
}

#[tokio::test]
async fn a_joiner_gets_everyones_emoji_and_a_change_reaches_every_capable_member() {
    let rig = rig(Registry::new((Ipv4Addr::LOCALHOST, 0).into())).await;
    hello(&rig.ana);
    assert_eq!(next_datagram(&rig.ana).await.as_ref(), &[AVATAR_TAG_ACK]);
    assert!(next_list(&rig.ana).await.is_empty(), "state right after the ack");

    set(&rig.ana, "🦊");
    assert_eq!(next_list(&rig.ana).await, vec![(ANA, "🦊".to_string())], "the sender's own entry is its ack");

    // Ben joins late: the ack, then the whole current set
    hello(&rig.ben);
    assert_eq!(next_datagram(&rig.ben).await.as_ref(), &[AVATAR_TAG_ACK]);
    assert_eq!(next_list(&rig.ben).await, vec![(ANA, "🦊".to_string())]);
    set(&rig.ben, "💀");
    let both = vec![(ANA, "🦊".to_string()), (BEN, "💀".to_string())];
    assert_eq!(next_list(&rig.ana).await, both);
    assert_eq!(next_list(&rig.ben).await, both);

    // clearing removes the entry; setting the same value again is not a change
    set(&rig.ana, "");
    assert_eq!(next_list(&rig.ana).await, vec![(BEN, "💀".to_string())]);
    assert_eq!(next_list(&rig.ben).await, vec![(BEN, "💀".to_string())]);
    set(&rig.ben, "💀");
    no_datagram(&rig.ben).await;

    // a member leaving takes its emoji with it
    rig.ben.conn.close(0u32.into(), b"bye");
    assert!(next_list(&rig.ana).await.is_empty());

    // the member that never said Hello was sent nothing in all of this
    no_datagram(&rig.old).await;
}

#[tokio::test]
async fn a_member_without_hello_can_set_but_receives_nothing_and_the_refresh_repeats_the_list() {
    let rig = rig(Registry::new((Ipv4Addr::LOCALHOST, 0).into())).await;
    set(&rig.old, "🐙"); // an old relay would drop this; an old client never sends it
    has_avatar(&rig, OLD, "🐙").await;
    no_datagram(&rig.old).await;
    hello(&rig.ana);
    assert_eq!(next_datagram(&rig.ana).await.as_ref(), &[AVATAR_TAG_ACK]);
    assert_eq!(next_list(&rig.ana).await, vec![(OLD, "🐙".to_string())], "a late hello gets the stored emoji");
    tokio::spawn(refresh_avatars(Arc::downgrade(&rig.registry)));
    assert_eq!(next_list(&rig.ana).await, vec![(OLD, "🐙".to_string())], "periodic refresh");
    no_datagram(&rig.ben).await;
}

#[tokio::test]
async fn malformed_sets_are_dropped() {
    let rig = rig(Registry::new((Ipv4Addr::LOCALHOST, 0).into())).await;
    set(&rig.ben, "🐙");
    has_avatar(&rig, BEN, "🐙").await;
    let too_long = [&[AVATAR_TAG_SET][..], &[b'x'; MAX_AVATAR_BYTES + 1]].concat();
    for bad in [too_long, vec![AVATAR_TAG_SET, 0xff, 0xfe, 0xfd]] {
        rig.ben.conn.send_datagram(Bytes::from(bad)).unwrap();
    }
    set(&rig.ana, "🦊"); // a valid one after them proves the bad ones were read and ignored
    has_avatar(&rig, ANA, "🦊").await;
    has_avatar(&rig, BEN, "🐙").await;
}

#[tokio::test]
async fn the_kill_switch_makes_the_relay_behave_like_an_old_one() {
    let mut registry = Registry::new((Ipv4Addr::LOCALHOST, 0).into());
    registry.avatars_enabled = false;
    let rig = rig(registry).await;
    hello(&rig.ana);
    set(&rig.ana, "🦊");
    no_datagram(&rig.ana).await; // no ack, no list
    await_room_state(&rig.registry, rig.room_id, "still no emoji", |r| {
        r.connections.get(&ANA).is_some_and(|c| c.avatar.is_empty() && !c.avatars)
    })
    .await;
}
