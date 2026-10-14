//! Real-browser smoke test for the WebTransport leg (test-only).
//!
//! Starts the real relay in-process on an ephemeral UDP port with its
//! self-signed cert, serves `server/tests/browser_smoke.html` over a throwaway
//! loopback HTTP server, and drives **headless** Chromium (no window, no
//! display) against it. The page connects with
//! `new WebTransport(url, { serverCertificateHashes: [...] })` using the
//! cert's SHA-256 exactly as it would arrive in a join link's 4th field, then:
//!
//! 1. proves a wrong hash is refused;
//! 2. opens the control bidi stream, sends `Join`, parses the `PresenceSnapshot`;
//! 3. announces a stream and subscribes to a native sharer's stream;
//! 4. receives a max-size media datagram and a preview uni-stream from the
//!    native sharer, and sends the same two kinds of traffic to a native viewer.
//!
//! The native peers are plain quinn clients, so this also covers
//! browser<->native forwarding in a shared room. The test skips (loudly) when
//! no `chromium`/`google-chrome` binary is on `PATH`.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use crate::cert::{CertManager, CertSource};
use crate::protocol::{
    ClientToRelay, ConnId, MAX_MEDIA_DATAGRAM_LEN, MediaHeader, PROTOCOL_VERSION, PreviewHeader,
    RelayToClient, RoomId, StreamKind, encode_cert_hash,
};
use crate::relay::tests::{await_room_state, insecure_client_endpoint, recv_ctrl, send_ctrl};
use crate::relay::{build_endpoint_with, serve};
use crate::room::Registry;

const PAGE: &str = include_str!("../tests/browser_smoke.html");

fn find_chromium() -> Option<String> {
    for name in ["chromium", "google-chrome", "google-chrome-stable", "chromium-browser"] {
        let found = std::env::var_os("PATH").is_some_and(|paths| {
            std::env::split_paths(&paths).any(|dir| dir.join(name).is_file())
        });
        if found {
            return Some(name.to_string());
        }
    }
    None
}

/// Serve the page on `GET /`; hand the body of `POST /result` to `tx`.
async fn serve_page(listener: TcpListener, tx: oneshot::Sender<String>, done: Arc<AtomicBool>) {
    let mut tx = Some(tx);
    loop {
        let Ok((mut sock, _)) = listener.accept().await else { return };
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        // Read until the headers are complete and Content-Length bytes of body arrived.
        let (head_end, content_len) = loop {
            let n = sock.read(&mut tmp).await.unwrap_or(0);
            if n == 0 {
                break (0, 0);
            }
            buf.extend_from_slice(&tmp[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..pos]).to_ascii_lowercase();
                let len = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if buf.len() >= pos + 4 + len {
                    break (pos + 4, len);
                }
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end.saturating_sub(4)]).to_string();
        let body = if head.starts_with("POST /result") {
            let body = String::from_utf8_lossy(&buf[head_end..head_end + content_len]).to_string();
            if let Some(tx) = tx.take() {
                let _ = tx.send(body);
            }
            String::new()
        } else if head.starts_with("POST /log") {
            eprintln!("[page] {}", String::from_utf8_lossy(&buf[head_end..head_end + content_len]));
            String::new()
        } else if head.starts_with("GET /done") {
            if done.load(Ordering::Relaxed) { "1" } else { "0" }.to_string()
        } else if head.starts_with("GET / ") || head.starts_with("GET /?") {
            PAGE.to_string()
        } else {
            String::new()
        };
        let resp = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = sock.write_all(resp.as_bytes()).await;
    }
}

async fn join_native(
    endpoint: &quinn::Endpoint,
    addr: SocketAddr,
    conn_id: ConnId,
    room_id: RoomId,
    name: &str,
) -> (quinn::Connection, quinn::SendStream, quinn::RecvStream) {
    let conn = endpoint
        .connect(addr, "localhost")
        .expect("start connect")
        .await
        .expect("native handshake");
    let (mut send, mut recv) = conn.open_bi().await.expect("control stream");
    send_ctrl(
        &mut send,
        &ClientToRelay::Join { conn_id, room_id, display_name: name.into() },
    )
    .await;
    assert!(matches!(recv_ctrl(&mut recv).await, RelayToClient::PresenceSnapshot { .. }));
    (conn, send, recv)
}

fn media_datagram(conn_id: ConnId, stream_id: u8, frame_id: u32) -> Bytes {
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
    buf.resize(MAX_MEDIA_DATAGRAM_LEN, 0xee);
    Bytes::from(buf)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn headless_chromium_joins_over_webtransport() {
    let Some(chromium) = find_chromium() else {
        eprintln!("SKIPPED headless_chromium_joins_over_webtransport: no chromium/google-chrome on PATH");
        return;
    };

    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();

    const SHARER: ConnId = 0x5000_0000_0000_0001;
    const VIEWER: ConnId = 0x6000_0000_0000_0002;
    const BROWSER: ConnId = 0x7000_0000_0000_0003;
    const SHARER_STREAM: u8 = 1;
    let room_id: RoomId = [0x42; 16];

    // --- relay (in-process, real cert) ---
    let registry = Arc::new(Registry::new((Ipv4Addr::LOCALHOST, 0).into()));
    let certs = CertManager::new(CertSource::ephemeral()).unwrap();
    let endpoint =
        build_endpoint_with((IpAddr::V4(Ipv4Addr::LOCALHOST), 0).into(), &certs).unwrap();
    let relay_addr = endpoint.local_addr().unwrap();
    tokio::spawn(serve(registry.clone(), endpoint));
    let hash = encode_cert_hash(&certs.info().hash);

    // --- native peers: a sharer and a viewer already in the room ---
    let native = insecure_client_endpoint();
    let (sharer, mut sharer_send, sharer_recv) =
        join_native(&native, relay_addr, SHARER, room_id, "native-sharer").await;
    send_ctrl(
        &mut sharer_send,
        &ClientToRelay::AnnounceStream {
            stream_id: SHARER_STREAM,
            kind: StreamKind::Video,
            label: "native screen".into(),
            width: 1280,
            height: 720,
        },
    )
    .await;
    await_room_state(&registry, room_id, "native stream announced", |r| {
        r.streams.contains_key(&(SHARER, SHARER_STREAM))
    })
    .await;
    tokio::spawn(async move {
        // Keep the sharer's control stream drained.
        let mut recv = sharer_recv;
        while crate::relay::tests::try_recv_ctrl(&mut recv).await.is_some() {}
    });
    let (viewer, mut viewer_send, viewer_recv) =
        join_native(&native, relay_addr, VIEWER, room_id, "native-viewer").await;
    tokio::spawn(async move {
        let mut recv = viewer_recv;
        while crate::relay::tests::try_recv_ctrl(&mut recv).await.is_some() {}
    });

    // --- page server + headless chromium ---
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let page_port = listener.local_addr().unwrap().port();
    let (result_tx, result_rx) = oneshot::channel();
    let page_done = Arc::new(AtomicBool::new(false));
    tokio::spawn(serve_page(listener, result_tx, page_done.clone()));

    let profile = std::env::temp_dir().join(format!("ssshare-chromium-{}", std::process::id()));
    let page_url = format!(
        "http://127.0.0.1:{page_port}/?port={}&hash={hash}&room={}&me={BROWSER:x}&sharer={SHARER:x}&sharer_stream={SHARER_STREAM}",
        relay_addr.port(),
        room_id.iter().map(|b| format!("{b:02x}")).collect::<String>(),
    );
    let mut chrome = tokio::process::Command::new(&chromium)
        .args([
            "--headless=new",
            "--no-sandbox",
            "--disable-gpu",
            "--no-first-run",
            "--disable-extensions",
            "--disable-background-networking",
        ])
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg(&page_url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("launch chromium");

    // The page announces a stream; only then can the native viewer subscribe.
    await_room_state(&registry, room_id, "browser stream announced", |r| {
        r.streams.contains_key(&(BROWSER, 0))
    })
    .await;
    send_ctrl(
        &mut viewer_send,
        &ClientToRelay::Subscribe { target: BROWSER, stream_id: 0 },
    )
    .await;

    // Once the page subscribed to the native sharer, feed it media + previews.
    await_room_state(&registry, room_id, "browser subscribed to native sharer", |r| {
        r.streams
            .get(&(SHARER, SHARER_STREAM))
            .is_some_and(|s| s.subscribers.contains(&BROWSER))
    })
    .await;
    let stop = Arc::new(AtomicBool::new(false));
    let feeder = {
        let sharer = sharer.clone();
        let stop = stop.clone();
        tokio::spawn(async move {
            let mut frame_id = 0u32;
            while !stop.load(Ordering::Relaxed) {
                let _ = sharer.send_datagram(media_datagram(SHARER, SHARER_STREAM, frame_id));
                frame_id += 1;
                if frame_id % 10 == 1 {
                    let mut blob = PreviewHeader::new(SHARER, SHARER_STREAM, frame_id).pack().to_vec();
                    blob.extend_from_slice(&[0x5a; 300]);
                    if let Ok(mut s) = sharer.open_uni().await {
                        let _ = s.write_all(&blob).await;
                        let _ = s.finish();
                    }
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        })
    };

    // Native viewer: gets the browser's max-size datagram and its preview.
    let from_browser = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let d = viewer.read_datagram().await.expect("viewer datagram");
            if d.len() == MAX_MEDIA_DATAGRAM_LEN {
                return d;
            }
        }
    })
    .await
    .expect("native viewer received a datagram from the browser within 20s");
    let (hdr, _) = MediaHeader::unpack(&from_browser).unwrap();
    assert_eq!(hdr.conn_id, BROWSER, "forwarded verbatim");
    let preview = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let mut r = viewer.accept_uni().await.expect("viewer uni stream");
            let bytes = r.read_to_end(1 << 20).await.expect("preview bytes");
            if PreviewHeader::unpack(&bytes).is_ok_and(|(h, _)| h.conn_id == BROWSER) {
                return bytes;
            }
        }
    })
    .await
    .expect("native viewer received a preview from the browser within 20s");
    assert_eq!(preview.len(), 15 + 300);

    // The native viewer has what it needs: let the page finish and report.
    page_done.store(true, Ordering::Relaxed);

    // Page's own verdict.
    let result = tokio::time::timeout(Duration::from_secs(30), result_rx)
        .await
        .expect("page reported a result within 30s")
        .expect("result channel");
    stop.store(true, Ordering::Relaxed);
    feeder.abort();
    let _ = chrome.kill().await;
    let _ = std::fs::remove_dir_all(&profile);

    println!("browser smoke result: {result}");
    let v: serde_json::Value = serde_json::from_str(&result).expect("result is JSON");
    assert!(v.get("error").is_none(), "page failed: {result}");
    assert_eq!(v["badHashRejected"], true, "a wrong cert hash must be refused");
    assert_eq!(v["snapshotVariant"], 0, "reply to Join is PresenceSnapshot");
    let members = v["snapshotMembers"].as_array().unwrap();
    assert_eq!(members.len(), 3, "sharer + viewer + the browser itself: {members:?}");
    assert_eq!(v["datagramLen"], MAX_MEDIA_DATAGRAM_LEN);
    assert_eq!(v["previewLen"], 15 + 300);
    let max = v["maxDatagramSize"].as_u64().unwrap() as usize;
    assert!(
        max >= MAX_MEDIA_DATAGRAM_LEN,
        "browser max datagram size {max} is below MAX_MEDIA_DATAGRAM_LEN"
    );
}
