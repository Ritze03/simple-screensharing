//! Tests for the transport seam (test-only): ALPN dispatch between native and
//! WebTransport peers on one UDP port, hash-pinned WebTransport connects, and
//! certificate rotation under live connections.
//!
//! The WebTransport peer here is `web_transport_quinn`'s own client (a Rust
//! stand-in for a browser, with `serverCertificateHashes`-style pinning); the
//! real-Chromium version of the same flow is `browser_smoke`.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use web_transport_quinn::ClientBuilder;

use crate::cert::{CertManager, CertSource};
use crate::protocol::{
    ClientToRelay, ConnId, MAX_MEDIA_DATAGRAM_LEN, MediaHeader, PROTOCOL_VERSION, RelayToClient,
    RoomId, StreamKind, WEBTRANSPORT_PATH, cert_hash_of_der, decode_payload, encode_frame,
};
use crate::relay::tests::{await_room_state, insecure_client_endpoint, recv_ctrl, send_ctrl};
use crate::relay::{build_endpoint_with, serve};
use crate::room::{Registry, TransportKind};

struct Relay {
    registry: Arc<Registry>,
    certs: Arc<CertManager>,
    addr: SocketAddr,
    endpoint: quinn::Endpoint,
}

fn start_relay(source: CertSource) -> Relay {
    let registry = Arc::new(Registry::new((Ipv4Addr::LOCALHOST, 0).into()));
    let certs = CertManager::new(source).unwrap();
    let endpoint = build_endpoint_with((IpAddr::V4(Ipv4Addr::LOCALHOST), 0).into(), &certs).unwrap();
    let addr = endpoint.local_addr().unwrap();
    registry.set_cert_info(certs.info());
    tokio::spawn(serve(registry.clone(), endpoint.clone()));
    Relay { registry, certs, addr, endpoint }
}

fn wt_url(addr: SocketAddr, path: &str) -> url::Url {
    format!("https://127.0.0.1:{}{path}", addr.port()).parse().unwrap()
}

async fn connect_wt(
    addr: SocketAddr,
    path: &str,
    hash: &[u8; 32],
) -> Result<web_transport_quinn::Session, web_transport_quinn::ClientError> {
    ClientBuilder::new()
        .with_server_certificate_hashes(vec![hash.to_vec()])
        .unwrap()
        .connect(wt_url(addr, path))
        .await
}

/// Join over an already-open WebTransport session: returns the control stream.
async fn wt_join(
    session: &web_transport_quinn::Session,
    conn_id: ConnId,
    room_id: RoomId,
    name: &str,
) -> (web_transport_quinn::SendStream, web_transport_quinn::RecvStream, RelayToClient) {
    let (mut send, mut recv) = session.open_bi().await.expect("control stream");
    let join = ClientToRelay::Join { conn_id, room_id, display_name: name.into() };
    send.write_all(&encode_frame(&join).unwrap()).await.expect("write Join");
    let mut len = [0u8; 4];
    recv.read_exact(&mut len).await.expect("snapshot length");
    let mut payload = vec![0u8; u32::from_be_bytes(len) as usize];
    recv.read_exact(&mut payload).await.expect("snapshot body");
    let reply = decode_payload(&payload).expect("decode reply");
    (send, recv, reply)
}

fn datagram(conn_id: ConnId, stream_id: u8, frame_id: u32) -> Bytes {
    let header = MediaHeader {
        version: PROTOCOL_VERSION,
        flags: 0,
        conn_id,
        stream_id,
        frame_id,
        fragment_index: 0,
        fragment_count: 1,
    };
    let mut buf = header.pack().to_vec();
    buf.resize(MAX_MEDIA_DATAGRAM_LEN, 0x11);
    Bytes::from(buf)
}

fn leaf_hash(conn: &quinn::Connection) -> [u8; 32] {
    let chain = conn
        .peer_identity()
        .expect("peer identity")
        .downcast::<Vec<rustls::pki_types::CertificateDer<'static>>>()
        .expect("rustls identity");
    cert_hash_of_der(chain[0].as_ref())
}

/// One port, two transports: a native (`ssshare/1`) and a WebTransport (`h3`)
/// member share a room and exchange media in both directions; wrong hash,
/// wrong path and an unknown ALPN are all refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn alpn_dispatch_puts_native_and_webtransport_members_in_one_room() {
    const NATIVE: ConnId = 0xA100_0000_0000_0001;
    const WEB: ConnId = 0xB200_0000_0000_0002;
    let room_id: RoomId = [0x33; 16];
    let relay = start_relay(CertSource::ephemeral());
    let hash = relay.certs.info().hash;

    // -- refusals --
    assert!(
        connect_wt(relay.addr, WEBTRANSPORT_PATH, &[0u8; 32]).await.is_err(),
        "a wrong certificate hash must be refused"
    );
    assert!(
        connect_wt(relay.addr, "/nope", &hash).await.is_err(),
        "only the /ssshare path is served"
    );
    let mut bogus_crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerify))
        .with_no_client_auth();
    bogus_crypto.alpn_protocols = vec![b"bogus/1".to_vec()];
    let mut ep = quinn::Endpoint::client((Ipv4Addr::LOCALHOST, 0).into()).unwrap();
    ep.set_default_client_config(quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(bogus_crypto).unwrap(),
    )));
    assert!(
        ep.connect(relay.addr, "localhost").unwrap().await.is_err(),
        "an ALPN the relay doesn't speak must fail the handshake"
    );

    // -- native member --
    let native_ep = insecure_client_endpoint();
    let native = native_ep.connect(relay.addr, "localhost").unwrap().await.unwrap();
    let (mut n_send, mut n_recv) = native.open_bi().await.unwrap();
    send_ctrl(
        &mut n_send,
        &ClientToRelay::Join { conn_id: NATIVE, room_id, display_name: "native".into() },
    )
    .await;
    assert!(matches!(recv_ctrl(&mut n_recv).await, RelayToClient::PresenceSnapshot { .. }));

    // -- WebTransport member --
    let web = connect_wt(relay.addr, WEBTRANSPORT_PATH, &hash).await.expect("pinned WebTransport connect");
    assert!(web.max_datagram_size() >= MAX_MEDIA_DATAGRAM_LEN);
    let (mut w_send, mut w_recv, reply) = wt_join(&web, WEB, room_id, "web").await;
    match reply {
        RelayToClient::PresenceSnapshot { members, .. } => {
            assert_eq!(members.len(), 2, "the web member sees the native one: {members:?}")
        }
        other => panic!("expected PresenceSnapshot, got {other:?}"),
    }

    // The relay knows which is which.
    use std::sync::atomic::Ordering::Relaxed;
    assert_eq!(relay.registry.native_connections.load(Relaxed), 1);
    assert_eq!(relay.registry.browser_connections.load(Relaxed), 1);
    await_room_state(&relay.registry, room_id, "both joined", |r| {
        r.connections.get(&NATIVE).is_some_and(|c| c.transport == TransportKind::Native)
            && r.connections.get(&WEB).is_some_and(|c| c.transport == TransportKind::Browser)
    })
    .await;

    // -- forwarding both ways in the shared room --
    let announce = |id| ClientToRelay::AnnounceStream {
        stream_id: id,
        kind: StreamKind::Video,
        label: "screen".into(),
        width: 640,
        height: 480,
    };
    send_ctrl(&mut n_send, &announce(0)).await;
    w_send.write_all(&encode_frame(&announce(1)).unwrap()).await.unwrap();
    await_room_state(&relay.registry, room_id, "both announced", |r| {
        r.streams.contains_key(&(NATIVE, 0)) && r.streams.contains_key(&(WEB, 1))
    })
    .await;
    // web watches native; native watches web
    w_send
        .write_all(&encode_frame(&ClientToRelay::Subscribe { target: NATIVE, stream_id: 0 }).unwrap())
        .await
        .unwrap();
    send_ctrl(&mut n_send, &ClientToRelay::Subscribe { target: WEB, stream_id: 1 }).await;
    await_room_state(&relay.registry, room_id, "cross-subscribed", |r| {
        r.streams.get(&(NATIVE, 0)).is_some_and(|s| s.subscribers.contains(&WEB))
            && r.streams.get(&(WEB, 1)).is_some_and(|s| s.subscribers.contains(&NATIVE))
    })
    .await;

    // Drain the control streams so the relay's writers never stall on flow control.
    tokio::spawn(async move { while crate::relay::tests::try_recv_ctrl(&mut n_recv).await.is_some() {} });
    tokio::spawn(async move {
        let mut len = [0u8; 4];
        while w_recv.read_exact(&mut len).await.is_ok() {
            let mut p = vec![0u8; u32::from_be_bytes(len) as usize];
            if w_recv.read_exact(&mut p).await.is_err() {
                break;
            }
        }
    });

    for i in 0..10 {
        native.send_datagram(datagram(NATIVE, 0, i)).unwrap();
        web.send_datagram(datagram(WEB, 1, i)).unwrap();
    }
    let at_web = tokio::time::timeout(Duration::from_secs(3), web.read_datagram())
        .await
        .expect("web member receives native media")
        .unwrap();
    let at_native = tokio::time::timeout(Duration::from_secs(3), native.read_datagram())
        .await
        .expect("native member receives web media")
        .unwrap();
    assert_eq!(at_web.len(), MAX_MEDIA_DATAGRAM_LEN);
    assert_eq!(at_native.len(), MAX_MEDIA_DATAGRAM_LEN);
    assert_eq!(MediaHeader::unpack(&at_web).unwrap().0.conn_id, NATIVE);
    assert_eq!(MediaHeader::unpack(&at_native).unwrap().0.conn_id, WEB);

    // Disconnect drops the gauges again.
    web.close(0, b"bye");
    native.close(0u32.into(), b"bye");
    for _ in 0..100 {
        if relay.registry.native_connections.load(Relaxed) == 0
            && relay.registry.browser_connections.load(Relaxed) == 0
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("connection gauges did not return to zero");
}

#[derive(Debug)]
struct NoVerify;
impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: &rustls::pki_types::ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Rotation with a forced-short validity: the served cert changes by itself,
/// connections that were already up keep working, and new connections (native
/// and WebTransport) see — and can pin — the new hash while the old hash stops
/// being accepted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn certificate_rotates_without_dropping_existing_connections() {
    const OLD: ConnId = 0xC300_0000_0000_0001;
    const NEW: ConnId = 0xC300_0000_0000_0002;
    let room_id: RoomId = [0x44; 16];
    // Valid ~6s, rotates when < 4s are left, i.e. about 1.4s after start.
    let relay = start_relay(CertSource::SelfSigned {
        state_dir: None,
        extra_sans: vec![],
        validity: Duration::from_secs(6),
        rotate_before: Duration::from_secs(4),
    });
    tokio::spawn(relay.certs.clone().maintain(relay.endpoint.clone(), relay.registry.clone()));
    let first_hash = relay.certs.info().hash;

    let ep = insecure_client_endpoint();
    let before = ep.connect(relay.addr, "localhost").unwrap().await.unwrap();
    assert_eq!(leaf_hash(&before), first_hash, "hash = SHA-256 of the presented leaf cert (DER)");
    let (mut b_send, mut b_recv) = before.open_bi().await.unwrap();
    send_ctrl(
        &mut b_send,
        &ClientToRelay::Join { conn_id: OLD, room_id, display_name: "before".into() },
    )
    .await;
    assert!(matches!(recv_ctrl(&mut b_recv).await, RelayToClient::PresenceSnapshot { .. }));

    // Wait for the rotation.
    let mut rotated = None;
    for _ in 0..200 {
        let info = relay.registry.cert_info().unwrap();
        if info.rotations >= 1 {
            rotated = Some(info);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let rotated = rotated.expect("certificate rotated within 20s");
    assert_ne!(rotated.hash, first_hash);

    // The connection that predates the rotation is untouched.
    send_ctrl(
        &mut b_send,
        &ClientToRelay::AnnounceStream {
            stream_id: 0,
            kind: StreamKind::Video,
            label: "still here".into(),
            width: 1,
            height: 1,
        },
    )
    .await;
    await_room_state(&relay.registry, room_id, "old connection still works", |r| {
        r.streams.contains_key(&(OLD, 0))
    })
    .await;
    assert!(before.close_reason().is_none());

    // New connections get the new cert; browsers must pin the new hash.
    let after = ep.connect(relay.addr, "localhost").unwrap().await.unwrap();
    assert_eq!(leaf_hash(&after), rotated.hash);
    assert!(
        connect_wt(relay.addr, WEBTRANSPORT_PATH, &first_hash).await.is_err(),
        "the old hash no longer matches what the relay presents"
    );
    let web = connect_wt(relay.addr, WEBTRANSPORT_PATH, &rotated.hash)
        .await
        .expect("new hash pins fine");
    let (_s, _r, reply) = wt_join(&web, NEW, room_id, "after").await;
    match reply {
        RelayToClient::PresenceSnapshot { members, streams } => {
            assert_eq!(members.len(), 2);
            assert_eq!(streams.len(), 1, "sees the stream announced on the pre-rotation connection");
        }
        other => panic!("expected PresenceSnapshot, got {other:?}"),
    }
}
