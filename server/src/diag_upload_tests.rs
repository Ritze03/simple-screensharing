//! Relay side of the opt-in diagnostics upload (spec §6.7, task #110) over real QUIC
//! connections: DiagHello/DiagAck, one uni stream per batch stored under the client log
//! directory, refusals (no Hello, kill switch, oversize, not joined), and that previews on the
//! same dispatch still work. The old-relay half of the mixed-version story is the client's
//! (it only uploads after an ack); the old-client half is that nothing here is sent unasked.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;

use crate::client_logs::{ClientLogs, Limits};
use crate::protocol::{
    ClientToRelay, ConnId, DIAG_TAG_ACK, DIAG_TAG_HELLO, DIAG_UPLOAD_VERSION, MAX_DIAG_BATCH_BYTES, PreviewHeader,
    RelayToClient, RoomId, STREAM_TYPE_DIAG, StreamKind,
};
use crate::relay::tests::{await_room_state, insecure_client_endpoint, recv_ctrl, send_ctrl};
use crate::relay::{build_endpoint, serve};
use crate::room::Registry;

const ROOM: RoomId = [7u8; 16];

struct Peer {
    conn: quinn::Connection,
    send: quinn::SendStream,
    _recv: quinn::RecvStream,
}

async fn join(client: &quinn::Endpoint, addr: SocketAddr, id: ConnId, name: &str) -> Peer {
    let conn = client.connect(addr, "localhost").unwrap().await.unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    send_ctrl(&mut send, &ClientToRelay::Join { conn_id: id, room_id: ROOM, display_name: name.to_owned() }).await;
    assert!(matches!(recv_ctrl(&mut recv).await, RelayToClient::PresenceSnapshot { .. }));
    Peer { conn, send, _recv: recv }
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sss-diag-upload-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// A relay on a free port that stores uploads under `root`.
async fn relay(root: &Path, enabled: bool) -> (Arc<Registry>, SocketAddr, quinn::Endpoint) {
    let mut registry = Registry::new((Ipv4Addr::LOCALHOST, 0).into());
    registry.client_logs = ClientLogs::new(root, enabled, Limits::default());
    let registry = Arc::new(registry);
    let endpoint = build_endpoint((IpAddr::V4(Ipv4Addr::LOCALHOST), 0).into()).unwrap();
    let addr = endpoint.local_addr().unwrap();
    tokio::spawn(serve(registry.clone(), endpoint));
    (registry, addr, insecure_client_endpoint())
}

fn batch(text: &str) -> Vec<u8> {
    let mut b = vec![STREAM_TYPE_DIAG, DIAG_UPLOAD_VERSION];
    b.extend_from_slice(text.as_bytes());
    b
}

/// Send one batch the way the client does and wait until the relay has read it.
async fn upload(peer: &Peer, bytes: &[u8]) -> bool {
    let mut s = peer.conn.open_uni().await.unwrap();
    s.write_all(bytes).await.unwrap();
    s.finish().unwrap();
    // `stopped` ends with Ok(None) when the relay read everything, Ok(Some(_)) when it refused
    // (but a refusal can lose the race against the stream already being fully delivered: only
    // an accepted batch is reliably `true`, so assert on stored files for refusals).
    matches!(tokio::time::timeout(Duration::from_secs(3), s.stopped()).await, Ok(Ok(None)))
}

async fn say_hello(peer: &Peer) -> bool {
    peer.conn.send_datagram(Bytes::from_static(&[DIAG_TAG_HELLO])).unwrap();
    match tokio::time::timeout(Duration::from_millis(700), peer.conn.read_datagram()).await {
        Ok(Ok(d)) => {
            assert_eq!(d.as_ref(), &[DIAG_TAG_ACK]);
            true
        }
        _ => false,
    }
}

/// The only `.log` under `root` (one per connection in these tests), waiting for it to appear.
async fn only_log(root: &Path) -> Option<PathBuf> {
    for _ in 0..40 {
        let mut found = Vec::new();
        if let Ok(days) = std::fs::read_dir(root) {
            for day in days.flatten() {
                if let Ok(files) = std::fs::read_dir(day.path()) {
                    found.extend(files.flatten().map(|f| f.path()));
                }
            }
        }
        if found.len() == 1 {
            return found.pop();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    None
}

async fn wait_for(path: &Path, needle: &str) -> String {
    for _ in 0..60 {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        if text.contains(needle) {
            return text;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{needle:?} never appeared in {}: {:?}", path.display(), std::fs::read_to_string(path));
}

#[tokio::test]
async fn hello_is_acked_and_batches_are_stored_per_person_and_session() {
    let root = scratch("store");
    let (_registry, addr, client) = relay(&root, true).await;
    let mut peer = join(&client, addr, 0xA1, "Mo Müller").await;
    assert!(say_hello(&peer).await, "a new relay answers DiagHello");

    assert!(upload(&peer, &batch("Simple Screensharing client log\nversion: test\n----\n2026-11-03T12:00:00Z  INFO x: first\n")).await);
    let path = only_log(&root).await.expect("one file");
    // The relay takes one batch at a time and refuses an overlapping one (the client retries
    // with the next batch), and `upload` returns once the relay has *read* the stream, not
    // once it has stored it: wait for the first batch before sending the second (task #124).
    wait_for(&path, "first").await;
    assert!(upload(&peer, &batch("2026-11-03T12:00:30Z  INFO x: second\n")).await);
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    assert_eq!(name, "Mo_Müller-00000000000000a1.log");
    let text = wait_for(&path, "second").await;
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[0].starts_with("# relay: client diagnostics from \"Mo Müller\" (connection 00000000000000a1) started "), "{text}");
    assert_eq!(&lines[1..], [
        "Simple Screensharing client log",
        "version: test",
        "----",
        "2026-11-03T12:00:00Z  INFO x: first",
        "2026-11-03T12:00:30Z  INFO x: second",
    ]);

    // Leaving writes the closing note.
    peer.send.finish().ok();
    peer.conn.close(0u32.into(), b"bye");
    wait_for(&path, "# relay: connection closed").await;
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_stream_without_hello_is_refused_and_nothing_is_stored() {
    let root = scratch("nohello");
    let (_registry, addr, client) = relay(&root, true).await;
    let peer = join(&client, addr, 0xB1, "mo").await;
    // Not asserted: the refusal is a STOP_SENDING, and for a tiny stream whose data and FIN
    // were already delivered `stopped()` may resolve Ok(None) first (1 run in ~30, task #124).
    // What matters is that nothing is stored, checked below.
    let _ = upload(&peer, &batch("sneaky\n")).await;
    // The connection is fine afterwards: a Hello still works and then the upload is stored.
    assert!(say_hello(&peer).await);
    assert!(upload(&peer, &batch("now allowed\n")).await);
    let path = only_log(&root).await.unwrap();
    let text = wait_for(&path, "now allowed").await;
    assert!(!text.contains("sneaky"));
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn the_kill_switch_means_no_ack_and_no_storage_but_a_normal_connection() {
    let root = scratch("off");
    let (_registry, addr, client) = relay(&root, false).await;
    let peer = join(&client, addr, 0xC1, "mo").await;
    assert!(!say_hello(&peer).await, "no DiagAck while SSSHARE_DIAG_UPLOAD=0");
    let _ = upload(&peer, &batch("x\n")).await; // refusal not observable reliably, see above
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!root.exists(), "nothing written");
    assert!(peer.conn.close_reason().is_none(), "the connection stays up");
}

#[tokio::test]
async fn an_oversized_batch_is_not_stored_and_the_connection_survives() {
    let root = scratch("big");
    let (_registry, addr, client) = relay(&root, true).await;
    let peer = join(&client, addr, 0xD1, "mo").await;
    assert!(say_hello(&peer).await);
    let mut too_big = batch("");
    too_big.extend(std::iter::repeat_n(b'x', MAX_DIAG_BATCH_BYTES));
    too_big.push(b'\n');
    // The relay stops reading at the cap, so the client's write may fail: either way, no file.
    let mut s = peer.conn.open_uni().await.unwrap();
    let _ = s.write_all(&too_big).await;
    let _ = s.finish();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!root.exists(), "an oversized batch is dropped whole");
    assert!(peer.conn.close_reason().is_none());
    // A normal one after it is stored.
    assert!(upload(&peer, &batch("fine\n")).await);
    wait_for(&only_log(&root).await.unwrap(), "fine").await;
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_wrong_batch_version_is_dropped() {
    let root = scratch("version");
    let (_registry, addr, client) = relay(&root, true).await;
    let peer = join(&client, addr, 0xE1, "mo").await;
    assert!(say_hello(&peer).await);
    upload(&peer, &[STREAM_TYPE_DIAG, 9, b'x', b'\n']).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!root.exists());
}

#[tokio::test]
async fn control_characters_in_an_upload_cannot_reach_the_owners_terminal() {
    let root = scratch("ctrl");
    let (_registry, addr, client) = relay(&root, true).await;
    let peer = join(&client, addr, 0xF1, "mo").await;
    assert!(say_hello(&peer).await);
    assert!(upload(&peer, &batch("\x1b]0;pwned\x07 hello\n")).await);
    let text = wait_for(&only_log(&root).await.unwrap(), "hello").await;
    assert!(!text.contains('\x1b') && !text.contains('\x07'), "{text:?}");
    let _ = std::fs::remove_dir_all(&root);
}

/// The uni-stream dispatch was split into preview and diagnostics: previews still fan out.
#[tokio::test]
async fn previews_still_reach_the_other_members_through_the_new_dispatch() {
    let root = scratch("preview");
    let (registry, addr, client) = relay(&root, true).await;
    let mut sharer = join(&client, addr, 0x51, "sharer").await;
    send_ctrl(
        &mut sharer.send,
        &ClientToRelay::AnnounceStream { stream_id: 0, kind: StreamKind::Video, label: "m".into(), width: 1, height: 1 },
    )
    .await;
    await_room_state(&registry, ROOM, "announced", |r| r.streams.contains_key(&(0x51, 0))).await;
    let viewer = join(&client, addr, 0x52, "viewer").await;

    let mut blob = PreviewHeader::new(0x51, 0, 1).pack().to_vec();
    blob.extend_from_slice(b"jpeg-ciphertext");
    let mut s = sharer.conn.open_uni().await.unwrap();
    s.write_all(&blob).await.unwrap();
    s.finish().unwrap();

    let mut got = tokio::time::timeout(Duration::from_secs(3), viewer.conn.accept_uni()).await.expect("a preview").unwrap();
    assert_eq!(got.read_to_end(1 << 20).await.unwrap(), blob);
    assert!(!root.exists());
}
