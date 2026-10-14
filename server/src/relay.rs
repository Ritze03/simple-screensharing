//! Connection accept layer and packet relay logic (spec §1, §10, §11).
//!
//! One UDP port, two kinds of peer, told apart by the ALPN the TLS handshake
//! negotiated (`handle_connection`):
//!
//! - `ssshare/1` — the native desktop client, raw QUIC;
//! - `h3` — a browser, HTTP/3 + WebTransport on path
//!   [`WEBTRANSPORT_PATH`](crate::protocol::WEBTRANSPORT_PATH).
//!
//! Both end up as a [`web_transport_quinn::Session`] (raw QUIC is wrapped with
//! `Session::raw`, which exists precisely to treat the two uniformly), and
//! everything after the accept — the connection actor, rooms, forwarding — is
//! written against that one type. Native and browser members are therefore
//! equal citizens of a room, with no per-transport branching in the relay.
//!
//! `Why web-transport-quinn and not wtransport / h3 + h3-webtransport:` it
//! runs on the quinn `Connection` the endpoint already produced (so the ALPN
//! dispatch is one `if`, and `Endpoint::set_server_config` rotation keeps
//! working), it supports quinn 0.11 and rustls/ring as-is, and its
//! `Session::raw` removes the need for a hand-rolled transport trait. wtransport
//! insists on owning its own endpoint, which can't share the port with the
//! native ALPN; the h3 route means hand-assembling the HTTP/3 session plumbing.
//!
//! One tokio task per accepted connection ("connection actor"): it owns the
//! control bidi stream, and loops `select!`-ing on further control frames,
//! media datagrams, and incoming preview uni-streams from that connection.
//!
//! Each connection additionally gets a **media forwarder task**
//! ([`media_forward_task`]) that owns the fan-out of that connection's
//! media datagrams. The actor only validates a datagram's cleartext
//! header and hands it over a channel; it never waits for the fan-out, and
//! never takes the room lock to do it. See `room.rs`'s module docs for why
//! forwarding had to come off the room mutex (per-*fragment* lock volume
//! from one high-framerate sharer was stalling the whole room).
//!
//! The relay never decrypts anything — every operation here reads only
//! cleartext header fields (`ConnId`, `stream_id`, frame/preview counters)
//! and forwards opaque ciphertext bytes verbatim.
//!
//! # Stream-ordering assumption (browsers and natives alike)
//!
//! The **first bidirectional stream the peer opens is the control stream**,
//! and its first message must be `Join`. (For a browser the HTTP/3 CONNECT
//! stream is consumed by the WebTransport layer and never surfaces here, so
//! "first bidi stream" means the first `createBidirectionalStream()`.) A bidi
//! stream only becomes visible to the relay once the peer writes to it, so the
//! page must write `Join` right after creating it. Preview uni-streams and
//! datagrams may be sent any time after that.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use quinn::Endpoint;
use tokio::sync::{Mutex, mpsc};
use web_transport_quinn::{ReadExactError, RecvStream, SendStream, Session};

use crate::cert::{CertManager, CertSource};
use crate::protocol::{
    ClientToRelay, ConnId, MAX_CONTROL_FRAME_LEN, MAX_NAME_LEN, MAX_ROOM_MEMBERS,
    MediaHeader, Member, PresenceEvent, PreviewHeader, RejectReason, RelayToClient, RoomId,
    STREAM_TYPE_PREVIEW, StreamId, StreamInfo, clamp_utf8, decode_payload, encode_frame,
};
use crate::room::{ConnState, Registry, RoomState, StreamRoute, StreamState, TransportKind};

/// ALPN of the native transport (raw QUIC). Browsers negotiate `h3` instead
/// ([`crate::protocol::WEBTRANSPORT_ALPN`]).
#[cfg(test)]
pub const ALPN_PROTOCOL: &[u8] = crate::protocol::NATIVE_ALPN;

/// How long a freshly handshaken `h3` peer gets to complete the HTTP/3
/// SETTINGS + WebTransport CONNECT exchange before being dropped.
const WEBTRANSPORT_SETUP_TIMEOUT: Duration = Duration::from_secs(10);

/// Max bytes accepted for one preview blob (15-byte header + JPEG
/// ciphertext). Generous headroom over a typical ~30s discovery
/// thumbnail; just a sanity backstop against a misbehaving/hostile peer.
const MAX_PREVIEW_BYTES: usize = 2 * 1024 * 1024;

/// Sender-side coalescing window mirrored on the relay for
/// `KeyframeRequest` bursts (spec §9 describes the *sender's* ~100ms
/// coalescing; this is a light relay-side debounce so a burst of
/// `Subscribe`s in the same instant doesn't fan out N keyframe requests).
const KEYFRAME_REQUEST_DEBOUNCE: Duration = Duration::from_millis(100);

/// Depth, in datagrams, of the queue between a connection actor and its
/// media forwarder task. Sized for a burst of roughly a couple of
/// keyframes' worth of fragments (spec §6.3: a ~1200-byte MTU means an IDR
/// can be hundreds of fragments) so ordinary bursts never touch the bound.
///
/// Why bounded and lossy rather than unbounded: media is best-effort by
/// design — the receiver already discards incomplete frames when a
/// fragment goes missing (spec §6.5 stale-frame discard). If a sender ever
/// outruns the forwarder, dropping the excess keeps memory flat and sheds
/// exactly the load that cannot be delivered anyway, whereas an unbounded
/// queue would grow without limit and add latency to every later fragment.
const MEDIA_QUEUE_DEPTH: usize = 512;

/// Log a summary line once per this many datagrams shed by a full media
/// queue, so overload is visible without one log line per lost fragment.
const MEDIA_DROP_LOG_INTERVAL: u64 = 256;

/// One validated media datagram on its way from a connection actor to
/// that connection's forwarder task.
struct MediaFragment {
    stream_id: StreamId,
    data: Bytes,
}

/// A change to the set of streams a forwarder task routes for. Sent on a
/// separate, unbounded channel from the media fragments themselves: route
/// updates are rare, must never be dropped by media backpressure, and are
/// sent synchronously so the control path never awaits while holding the
/// room lock.
enum RouteUpdate {
    /// This connection announced `stream_id`; start routing its fragments.
    Add {
        stream_id: StreamId,
        route: Arc<StreamRoute>,
    },
    /// This connection stopped `stream_id`; forget it.
    Remove { stream_id: StreamId },
}

/// Per-connection media fan-out task: owns the routing table for the
/// streams that connection publishes, and copies each incoming fragment
/// to the current subscribers of its stream.
///
/// This task takes **no room lock**. It reads only the per-stream
/// [`StreamRoute`] snapshot, which the control plane republishes under the
/// room mutex on every subscriber change. `send_datagram` is non-blocking
/// (it queues into quinn and returns), so the whole loop body is
/// await-free: the task can never wedge and stall its actor.
async fn media_forward_task(
    registry: Arc<Registry>,
    mut route_rx: mpsc::UnboundedReceiver<RouteUpdate>,
    mut media_rx: mpsc::Receiver<MediaFragment>,
) {
    let mut routes: HashMap<StreamId, Arc<StreamRoute>> = HashMap::new();
    loop {
        // `biased` so route updates are applied ahead of queued media:
        // a stream must be registered before its fragments can route, and
        // a removal should take effect promptly. Both senders are dropped
        // together at connection teardown, so exiting on a closed route
        // channel also discards any still-queued fragments — correct, the
        // connection is gone.
        tokio::select! {
            biased;
            update = route_rx.recv() => {
                match update {
                    Some(RouteUpdate::Add { stream_id, route }) => {
                        routes.insert(stream_id, route);
                    }
                    Some(RouteUpdate::Remove { stream_id }) => {
                        routes.remove(&stream_id);
                    }
                    None => return,
                }
            }
            fragment = media_rx.recv() => {
                let Some(MediaFragment { stream_id, data }) = fragment else {
                    return;
                };
                let Some(route) = routes.get(&stream_id) else {
                    continue; // unknown or already-stopped stream: drop
                };
                route
                    .bytes_received
                    .fetch_add(data.len() as u64, std::sync::atomic::Ordering::Relaxed);
                for target in route.targets().iter() {
                    match target.connection.send_datagram(data.clone()) {
                        Ok(()) => registry.bandwidth.add_tx(data.len() as u64),
                        Err(e) => tracing::trace!(
                            "dropping forwarded datagram to {:016x}: {e}",
                            target.conn_id
                        ),
                    }
                }
            }
        }
    }
}

/// Bind a QUIC endpoint on `listen_addr` with a throwaway in-memory
/// self-signed cert (default timings, nothing persisted). Split out from
/// [`run`] so tests can bind on an ephemeral port (`:0`) and read back the
/// actual bound address before starting the accept loop.
#[cfg(test)]
pub fn build_endpoint(listen_addr: SocketAddr) -> Result<Endpoint> {
    let certs = CertManager::new(CertSource::ephemeral())?;
    build_endpoint_with(listen_addr, &certs)
}

/// [`build_endpoint`] for a caller-supplied [`CertManager`] (tests that force
/// rotation, `run`). The caller keeps the manager and runs
/// [`CertManager::maintain`] if the cert should rotate.
pub fn build_endpoint_with(listen_addr: SocketAddr, certs: &CertManager) -> Result<Endpoint> {
    // `Endpoint::server` leaves the platform default for IPV6_V6ONLY, which is
    // "on" on Windows: a `[::]` listener would then never see an IPv4 client.
    // Bind the socket ourselves and ask for dual-stack explicitly (what
    // `Endpoint::client` does internally).
    let socket = socket2::Socket::new(
        socket2::Domain::for_address(listen_addr),
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )
    .context("creating the QUIC UDP socket")?;
    if listen_addr.is_ipv6() {
        if let Err(e) = socket.set_only_v6(false) {
            tracing::debug!(%e, "unable to make the QUIC socket dual-stack");
        }
    }
    socket
        .bind(&listen_addr.into())
        .with_context(|| format!("binding QUIC endpoint on {listen_addr}"))?;
    Endpoint::new(
        quinn::EndpointConfig::default(),
        Some(certs.server_config()?),
        socket.into(),
        Arc::new(quinn::TokioRuntime),
    )
    .with_context(|| format!("creating QUIC endpoint on {listen_addr}"))
}

/// Run the accept loop on an already-bound endpoint until the process is
/// torn down. Each accepted connection is spawned as its own task.
pub async fn serve(registry: Arc<Registry>, endpoint: Endpoint) -> Result<()> {
    while let Some(incoming) = endpoint.accept().await {
        let registry = registry.clone();
        tokio::spawn(async move {
            let remote = incoming.remote_address();
            if let Err(e) = handle_connection(registry, incoming).await {
                tracing::info!(%remote, error = %format!("{e:#}"), "connection ended");
            }
        });
    }

    Ok(())
}

/// Bind the QUIC endpoint and spawn the accept loop and cert maintenance.
/// Returns the endpoint so the caller can [`shutdown`] it gracefully.
pub fn start(
    registry: Arc<Registry>,
    listen_addr: SocketAddr,
    cert_source: CertSource,
) -> Result<Endpoint> {
    let certs = CertManager::new(cert_source)?;
    let endpoint = build_endpoint_with(listen_addr, &certs)?;
    let local_addr = endpoint
        .local_addr()
        .context("reading bound local address")?;
    registry.set_cert_info(certs.info());
    tracing::info!(
        %local_addr,
        cert_hash = %crate::protocol::encode_cert_hash(&certs.info().hash),
        "QUIC endpoint listening (ALPN h3 + ssshare/1)"
    );
    tokio::spawn(certs.maintain(endpoint.clone(), registry.clone()));
    let serving = endpoint.clone();
    tokio::spawn(async move {
        if let Err(e) = serve(registry, serving).await {
            tracing::error!("relay listener exited with error: {e:#}");
        }
    });
    Ok(endpoint)
}

/// Read one length-prefixed control frame (spec §2). Returns `Ok(None)`
/// on a clean stream close with no partial frame in flight (peer FIN'd
/// after finishing all messages) — the relay treats this the same as an
/// explicit `Leave` (spec §4.1).
async fn read_control_frame(recv: &mut RecvStream) -> Result<Option<ClientToRelay>> {
    let mut len_buf = [0u8; 4];
    match recv.read_exact(&mut len_buf).await {
        Ok(()) => {}
        Err(ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(e) => bail!("control stream closed reading frame length: {e}"),
    }
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_CONTROL_FRAME_LEN {
        bail!("control frame length {len} exceeds cap {MAX_CONTROL_FRAME_LEN}");
    }
    let mut payload = vec![0u8; len as usize];
    recv.read_exact(&mut payload)
        .await
        .context("control stream closed mid-frame")?;
    let msg = decode_payload(&payload).context("decoding control frame")?;
    Ok(Some(msg))
}

/// Write one length-prefixed control frame directly (used only for the
/// pre-registration handshake, where there's no writer task/channel yet).
async fn write_control_frame(send: &mut SendStream, msg: &RelayToClient) -> Result<()> {
    let framed = encode_frame(msg).map_err(|e| anyhow::anyhow!("{e}"))?;
    send.write_all(&framed)
        .await
        .context("writing control frame")?;
    Ok(())
}

/// Dedicated writer task for one connection's control stream: serializes
/// `RelayToClient` messages arriving on `rx` onto `send`, in order, for
/// the lifetime of the connection.
async fn control_writer_task(mut send: SendStream, mut rx: mpsc::UnboundedReceiver<RelayToClient>) {
    while let Some(msg) = rx.recv().await {
        match encode_frame(&msg) {
            Ok(framed) => {
                if let Err(e) = send.write_all(&framed).await {
                    tracing::debug!("control write failed, stopping writer: {e}");
                    break;
                }
            }
            Err(e) => tracing::warn!("failed to encode outgoing control frame: {e}"),
        }
    }
    let _ = send.finish();
}

/// Everything the join handshake produces once a `Join` is accepted.
struct Joined {
    room: Arc<Mutex<RoomState>>,
    room_id: RoomId,
    conn_id: ConnId,
}

/// Application close code: the first control frame was not a valid `Join`
/// (a `JoinRejected{Malformed}` was sent first).
pub const CLOSE_MALFORMED: u32 = 0x101;
/// Application close code: the room already has `MAX_ROOM_MEMBERS` members
/// (a `JoinRejected{RoomFull}` was sent first).
pub const CLOSE_ROOM_FULL: u32 = 0x102;
/// Application close code: the relay is shutting down; clients should
/// reconnect (to this or a restarted relay).
pub const CLOSE_SHUTDOWN: u32 = 0x200;

/// Upper bound on how long a terminal rejection waits for the peer to
/// receive and read the `JoinRejected` frame before the connection is
/// closed regardless.
const REJECT_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);
/// Upper bound on the graceful shutdown (`CONNECTION_CLOSE` delivery).
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// Make a just-written terminal `JoinRejected` actually reach the peer, then
/// close the session with an application code.
///
/// Why not just close: a `CONNECTION_CLOSE` (or dropping the session)
/// discards stream data the peer has not yet read, so the client saw a bare
/// "connection lost" instead of the rejection. So: `finish()` the control
/// stream (FIN after the frame), wait for `stopped()` (the peer has
/// received everything), then give the peer the remaining budget to read it
/// and hang up on its own (`closed()`), and only then close ourselves. All
/// bounded by [`REJECT_FLUSH_TIMEOUT`] so a silent peer cannot pin the task.
async fn close_after_rejection(
    connection: &Session,
    ctrl_send: &mut SendStream,
    code: u32,
    reason: &[u8],
) {
    let _ = ctrl_send.finish();
    let _ = tokio::time::timeout(REJECT_FLUSH_TIMEOUT, async {
        let _ = ctrl_send.stopped().await;
        connection.closed().await;
    })
    .await;
    connection.close(code, reason);
}

/// Gracefully shut the endpoint down: every connection gets a
/// `CONNECTION_CLOSE` with [`CLOSE_SHUTDOWN`], and we wait (bounded) for it
/// to be sent so peers notice immediately instead of after the QUIC idle
/// timeout (~21 s) and can auto-reconnect.
pub async fn shutdown(endpoint: &Endpoint) {
    endpoint.close(CLOSE_SHUTDOWN.into(), b"relay shutting down");
    let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, endpoint.wait_idle()).await;
}

/// Run the join handshake: read `Join` messages off `ctrl_recv` until one
/// succeeds (retrying past `ConnIdCollision`, per spec §3.1/§4.2) or the
/// connection gives up / is rejected terminally. On success, registers
/// the connection in its room, sends the `PresenceSnapshot` + cached
/// previews, and broadcasts `MemberJoined` to the rest of the room.
async fn perform_join(
    registry: &Arc<Registry>,
    connection: &Session,
    transport: TransportKind,
    ctrl_send: &mut SendStream,
    ctrl_recv: &mut RecvStream,
    control_tx: mpsc::UnboundedSender<RelayToClient>,
) -> Result<Option<Joined>> {
    loop {
        let frame = match read_control_frame(ctrl_recv).await? {
            Some(f) => f,
            None => return Ok(None), // hung up before ever joining
        };
        let ClientToRelay::Join {
            conn_id,
            room_id,
            display_name,
        } = frame
        else {
            write_control_frame(
                ctrl_send,
                &RelayToClient::JoinRejected {
                    reason: RejectReason::Malformed,
                },
            )
            .await?;
            close_after_rejection(connection, ctrl_send, CLOSE_MALFORMED, b"malformed join").await;
            return Ok(None);
        };
        let display_name = clamp_utf8(&display_name, MAX_NAME_LEN);

        let room = registry.get_or_create_room(room_id).await;
        let mut guard = room.lock().await;

        if guard.connections.len() >= MAX_ROOM_MEMBERS {
            drop(guard);
            write_control_frame(
                ctrl_send,
                &RelayToClient::JoinRejected {
                    reason: RejectReason::RoomFull,
                },
            )
            .await?;
            registry.drop_room_if_empty(room_id, &room).await;
            close_after_rejection(connection, ctrl_send, CLOSE_ROOM_FULL, b"room full").await;
            return Ok(None);
        }
        if guard.connections.contains_key(&conn_id) {
            drop(guard);
            write_control_frame(
                ctrl_send,
                &RelayToClient::JoinRejected {
                    reason: RejectReason::ConnIdCollision,
                },
            )
            .await?;
            // Spec: client may retry Join with a fresh conn_id on the same connection.
            continue;
        }

        guard.connections.insert(
            conn_id,
            ConnState {
                conn_id,
                display_name: display_name.clone(),
                control_tx: control_tx.clone(),
                connection: connection.clone(),
                transport,
                subscriptions: HashSet::new(),
                stream_count: 0,
            },
        );

        let members: Vec<Member> = guard
            .connections
            .values()
            .map(|c| Member {
                conn_id: c.conn_id,
                display_name: c.display_name.clone(),
                stream_count: c.stream_count,
            })
            .collect();
        let streams: Vec<StreamInfo> = guard
            .streams
            .values()
            .map(|s| StreamInfo {
                owner: s.owner,
                stream_id: s.stream_id,
                kind: s.kind,
                label: s.label.clone(),
                width: s.width,
                height: s.height,
            })
            .collect();
        let cached_previews: Vec<((ConnId, u8), Bytes)> = guard
            .streams
            .values()
            .filter_map(|s| s.last_preview.clone().map(|b| ((s.owner, s.stream_id), b)))
            .collect();

        guard.log.push(format!(
            "{display_name} joined ({conn_id:016x}) via {}",
            transport_label(transport)
        ));

        // §8: snapshot MUST precede the MemberJoined delta to others.
        let _ = control_tx.send(RelayToClient::PresenceSnapshot { members, streams });

        let joined_member = Member {
            conn_id,
            display_name: display_name.clone(),
            stream_count: 0,
        };
        guard.broadcast(
            &RelayToClient::PresenceDelta {
                event: PresenceEvent::MemberJoined(joined_member),
            },
            Some(conn_id),
        );

        drop(guard);

        // Replay cached previews (§8 point 2) to the newcomer, opaque
        // ciphertext, never decrypted. Fire-and-forget per stream.
        for (_key, blob) in cached_previews {
            let conn = connection.clone();
            tokio::spawn(async move {
                if let Ok(mut send) = conn.open_uni().await {
                    let _ = send.write_all(&blob).await;
                    let _ = send.finish();
                }
            });
        }

        return Ok(Some(Joined {
            room,
            room_id,
            conn_id,
        }));
    }
}

fn transport_label(t: TransportKind) -> &'static str {
    match t {
        TransportKind::Native => "native",
        TransportKind::Browser => "browser",
    }
}

/// Decrements the per-transport connection gauge when a connection ends, on
/// every exit path.
struct ConnGauge {
    registry: Arc<Registry>,
    kind: TransportKind,
}

impl ConnGauge {
    fn new(registry: &Arc<Registry>, kind: TransportKind) -> Self {
        Self::counter(registry, kind).fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self {
            registry: registry.clone(),
            kind,
        }
    }
    fn counter(registry: &Registry, kind: TransportKind) -> &std::sync::atomic::AtomicUsize {
        match kind {
            TransportKind::Native => &registry.native_connections,
            TransportKind::Browser => &registry.browser_connections,
        }
    }
}

impl Drop for ConnGauge {
    fn drop(&mut self) {
        Self::counter(&self.registry, self.kind).fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The accept layer: turn a handshaken QUIC connection into a [`Session`]
/// according to the negotiated ALPN.
///
/// - `ssshare/1` -> `Session::raw` (no HTTP/3 at all);
/// - `h3` -> HTTP/3 SETTINGS + the WebTransport CONNECT; only
///   `https://.../ssshare` is accepted, anything else gets a 404.
async fn accept_session(connection: quinn::Connection) -> Result<(Session, TransportKind)> {
    let alpn = connection
        .handshake_data()
        .and_then(|d| d.downcast::<quinn::crypto::rustls::HandshakeData>().ok())
        .and_then(|d| d.protocol.clone());
    match alpn.as_deref() {
        Some(a) if a == crate::protocol::NATIVE_ALPN => {
            Ok((Session::raw(connection), TransportKind::Native))
        }
        Some(a) if a == crate::protocol::WEBTRANSPORT_ALPN => {
            let request = tokio::time::timeout(
                WEBTRANSPORT_SETUP_TIMEOUT,
                web_transport_quinn::Request::accept(connection),
            )
            .await
            .context("timed out waiting for the WebTransport CONNECT")?
            .context("WebTransport handshake failed")?;
            if request.url.path() != crate::protocol::WEBTRANSPORT_PATH {
                let path = request.url.path().to_string();
                let _ = request
                    .reject(web_transport_quinn::http::StatusCode::NOT_FOUND)
                    .await;
                bail!("WebTransport request for unknown path {path:?}");
            }
            let session = tokio::time::timeout(WEBTRANSPORT_SETUP_TIMEOUT, request.ok())
                .await
                .context("timed out answering the WebTransport CONNECT")?
                .context("accepting WebTransport session")?;
            Ok((session, TransportKind::Browser))
        }
        other => bail!("unsupported ALPN {:?}", other.map(String::from_utf8_lossy)),
    }
}

/// Drive one accepted QUIC connection end to end: join handshake, then
/// the steady-state control/datagram/uni-stream loop, then cleanup.
async fn handle_connection(registry: Arc<Registry>, incoming: quinn::Incoming) -> Result<()> {
    let quic = incoming.await.context("QUIC handshake failed")?;
    let remote = quic.remote_address();
    let (connection, transport) = accept_session(quic).await?;
    let _gauge = ConnGauge::new(&registry, transport);
    tracing::debug!(%remote, transport = transport_label(transport), max_datagram = connection.max_datagram_size(), "connection established, awaiting control stream");

    let (mut ctrl_send, mut ctrl_recv) = connection
        .accept_bi()
        .await
        .context("waiting for control stream")?;

    let (control_tx, control_rx) = mpsc::unbounded_channel::<RelayToClient>();

    let joined = match perform_join(
        &registry,
        &connection,
        transport,
        &mut ctrl_send,
        &mut ctrl_recv,
        control_tx.clone(),
    )
    .await?
    {
        Some(j) => j,
        None => return Ok(()),
    };
    let Joined {
        room,
        room_id,
        conn_id,
    } = joined;

    tracing::info!(%remote, transport = transport_label(transport), conn_id = format!("{conn_id:016x}"), room_id = %hex_room(&room_id), "member joined");

    let writer = tokio::spawn(control_writer_task(ctrl_send, control_rx));

    // Data plane for this connection: the actor validates and enqueues,
    // this task fans out. Keeping them separate is what stops one busy
    // stream from delaying the actor's own next read.
    let (route_tx, route_rx) = mpsc::unbounded_channel::<RouteUpdate>();
    let (media_tx, media_rx) = mpsc::channel::<MediaFragment>(MEDIA_QUEUE_DEPTH);
    let forwarder = tokio::spawn(media_forward_task(registry.clone(), route_rx, media_rx));

    let run_result = connection_loop(
        &registry,
        &room,
        conn_id,
        &connection,
        &mut ctrl_recv,
        &media_tx,
        &route_tx,
    )
    .await;

    // Shut the data plane down *before* cleanup: dropping both senders
    // ends the forwarder, so no fragment of this connection can still be
    // in flight while the room state is being torn down.
    drop(media_tx);
    drop(route_tx);
    let _ = forwarder.await;

    // Cleanup: remove from room, drop owned streams, unregister
    // subscriptions elsewhere, broadcast MemberLeft.
    cleanup_connection(&registry, &room, room_id, conn_id).await;

    drop(control_tx);
    let _ = writer.await;

    run_result
}

/// The steady-state per-connection loop: reads further control frames,
/// incoming media datagrams, and incoming preview uni-streams from this
/// connection, until the connection closes or sends `Leave`.
async fn connection_loop(
    registry: &Arc<Registry>,
    room: &Arc<Mutex<RoomState>>,
    conn_id: ConnId,
    connection: &Session,
    ctrl_recv: &mut RecvStream,
    media_tx: &mpsc::Sender<MediaFragment>,
    route_tx: &mpsc::UnboundedSender<RouteUpdate>,
) -> Result<()> {
    let mut dropped_datagrams: u64 = 0;
    loop {
        tokio::select! {
            frame = read_control_frame(ctrl_recv) => {
                match frame? {
                    Some(ClientToRelay::Leave) => {
                        tracing::debug!(conn_id = format!("{conn_id:016x}"), "explicit Leave");
                        return Ok(());
                    }
                    Some(msg) => handle_client_message(room, conn_id, msg, route_tx).await,
                    None => {
                        tracing::debug!(conn_id = format!("{conn_id:016x}"), "control stream closed (implicit leave)");
                        return Ok(());
                    }
                }
            }
            dgram = connection.read_datagram() => {
                match dgram {
                    Ok(bytes) => enqueue_media_datagram(
                        registry,
                        media_tx,
                        conn_id,
                        bytes,
                        &mut dropped_datagrams,
                    ),
                    Err(e) => {
                        tracing::debug!(conn_id = format!("{conn_id:016x}"), "datagram read error: {e}");
                        return Ok(());
                    }
                }
            }
            uni = connection.accept_uni() => {
                match uni {
                    Ok(recv) => {
                        let room = room.clone();
                        tokio::spawn(handle_preview_uni(room, conn_id, recv));
                    }
                    Err(e) => {
                        tracing::debug!(conn_id = format!("{conn_id:016x}"), "uni accept error: {e}");
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// Handle one control-plane message from an already-joined connection
/// (everything except `Join`/`Leave`, which are handled by the callers).
async fn handle_client_message(
    room: &Arc<Mutex<RoomState>>,
    conn_id: ConnId,
    msg: ClientToRelay,
    route_tx: &mpsc::UnboundedSender<RouteUpdate>,
) {
    match msg {
        ClientToRelay::Join { .. } => {
            tracing::debug!(
                conn_id = format!("{conn_id:016x}"),
                "ignoring duplicate Join after handshake"
            );
        }
        ClientToRelay::Leave => unreachable!("handled by caller"),
        ClientToRelay::AnnounceStream {
            stream_id,
            kind,
            label,
            width,
            height,
        } => {
            let label = clamp_utf8(&label, MAX_NAME_LEN);
            let route = Arc::new(StreamRoute::new());
            let mut guard = room.lock().await;
            let replaced = guard.streams.insert(
                (conn_id, stream_id),
                StreamState {
                    owner: conn_id,
                    stream_id,
                    kind,
                    label: label.clone(),
                    width,
                    height,
                    subscribers: HashSet::new(),
                    route: route.clone(),
                    last_preview: None,
                    last_keyframe_request: None,
                },
            );
            // Re-announcing an existing stream_id resets its subscriber
            // set, so retire the superseded route too: fragments still
            // queued against it fan out to nobody rather than to the
            // stale set.
            if let Some(replaced) = replaced {
                replaced.route.publish(Vec::new());
            }
            // Register the (empty) route with this connection's forwarder.
            // Synchronous send on an unbounded channel: never awaits under
            // the room lock, and can never be shed by media backpressure.
            let _ = route_tx.send(RouteUpdate::Add { stream_id, route });
            if let Some(c) = guard.connections.get_mut(&conn_id) {
                c.stream_count = c.stream_count.saturating_add(1);
            }
            guard.log.push(format!(
                "{conn_id:016x} started stream {stream_id} ({label})"
            ));
            let info = StreamInfo {
                owner: conn_id,
                stream_id,
                kind,
                label,
                width,
                height,
            };
            guard.broadcast(
                &RelayToClient::PresenceDelta {
                    event: PresenceEvent::StreamStarted(info),
                },
                Some(conn_id),
            );
        }
        ClientToRelay::StopStream { stream_id } => {
            let mut guard = room.lock().await;
            if let Some(stopped) = guard.streams.remove(&(conn_id, stream_id)) {
                // Empty the route *synchronously*, under the room lock:
                // any fragment already queued for the forwarder now fans
                // out to nobody, even before the Remove below is applied.
                stopped.route.publish(Vec::new());
                let _ = route_tx.send(RouteUpdate::Remove { stream_id });
                if let Some(c) = guard.connections.get_mut(&conn_id) {
                    c.stream_count = c.stream_count.saturating_sub(1);
                }
                for other in guard.connections.values_mut() {
                    other.subscriptions.remove(&(conn_id, stream_id));
                }
                guard
                    .log
                    .push(format!("{conn_id:016x} stopped stream {stream_id}"));
                guard.broadcast(
                    &RelayToClient::PresenceDelta {
                        event: PresenceEvent::StreamStopped { conn_id, stream_id },
                    },
                    Some(conn_id),
                );
            }
        }
        ClientToRelay::Subscribe { target, stream_id } => {
            let mut guard = room.lock().await;
            let Some(stream) = guard.streams.get_mut(&(target, stream_id)) else {
                tracing::debug!(
                    conn_id = format!("{conn_id:016x}"),
                    "Subscribe to unknown stream ({target:016x}, {stream_id})"
                );
                return;
            };
            let newly_added = stream.subscribers.insert(conn_id);
            let count = stream.subscribers.len() as u32;
            let send_keyframe = if newly_added {
                let now = Instant::now();
                let debounced = stream
                    .last_keyframe_request
                    .is_some_and(|last| now.duration_since(last) < KEYFRAME_REQUEST_DEBOUNCE);
                if !debounced {
                    stream.last_keyframe_request = Some(now);
                }
                !debounced
            } else {
                false
            };
            if let Some(c) = guard.connections.get_mut(&conn_id) {
                c.subscriptions.insert((target, stream_id));
            }
            // Republish the fan-out snapshot while still holding the room
            // lock, so the new viewer starts receiving from the very next
            // fragment (and before the KeyframeRequest below can produce
            // an IDR the route would otherwise miss).
            guard.refresh_route((target, stream_id));
            guard.log.push(format!(
                "{conn_id:016x} subscribed to ({target:016x}, {stream_id}) -> count={count}"
            ));
            guard.send_to(target, RelayToClient::SubscriberCount { stream_id, count });
            if send_keyframe {
                guard.send_to(target, RelayToClient::KeyframeRequest { stream_id });
            }
        }
        ClientToRelay::Unsubscribe { target, stream_id } => {
            let mut guard = room.lock().await;
            if let Some(c) = guard.connections.get_mut(&conn_id) {
                c.subscriptions.remove(&(target, stream_id));
            }
            if let Some(stream) = guard.streams.get_mut(&(target, stream_id)) {
                stream.subscribers.remove(&conn_id);
                let count = stream.subscribers.len() as u32;
                guard.refresh_route((target, stream_id));
                guard.log.push(format!(
                    "{conn_id:016x} unsubscribed from ({target:016x}, {stream_id}) -> count={count}"
                ));
                guard.send_to(target, RelayToClient::SubscriberCount { stream_id, count });
            }
        }
    }
}

/// Validate one incoming media datagram (spec §6.2) and hand it to this
/// connection's forwarder task. Drops it if malformed or spoofed.
///
/// Deliberately **synchronous and non-blocking**: the connection actor
/// must be able to read its next datagram or control frame immediately,
/// regardless of how long the fan-out for the previous one takes. This is
/// the half of the fix that keeps a busy stream from stalling its own
/// connection's control plane; `room.rs`'s [`crate::room::StreamRoute`] is
/// the half that keeps it from stalling the rest of the room.
fn enqueue_media_datagram(
    registry: &Arc<Registry>,
    media_tx: &mpsc::Sender<MediaFragment>,
    conn_id: ConnId,
    data: Bytes,
    dropped: &mut u64,
) {
    let Ok((header, _)) = MediaHeader::unpack(&data) else {
        tracing::debug!(
            conn_id = format!("{conn_id:016x}"),
            "dropping undersized media datagram"
        );
        return;
    };
    if header.conn_id != conn_id {
        // The relay SHOULD validate the source conn_id matches the
        // connection it arrived on (spec §6.2) to reject spoofing.
        tracing::warn!(
            conn_id = format!("{conn_id:016x}"),
            claimed = format!("{:016x}", header.conn_id),
            "dropping media datagram with spoofed conn_id"
        );
        return;
    }

    registry.bandwidth.add_rx(data.len() as u64);

    let fragment = MediaFragment {
        stream_id: header.stream_id,
        data,
    };
    if media_tx.try_send(fragment).is_err() {
        // Full (sender outrunning fan-out) or closed (forwarder gone at
        // teardown). Media is best-effort; shed it rather than blocking
        // the read loop, and summarize periodically instead of logging
        // every lost fragment.
        *dropped += 1;
        if dropped.is_multiple_of(MEDIA_DROP_LOG_INTERVAL) {
            tracing::debug!(
                conn_id = format!("{conn_id:016x}"),
                total_dropped = *dropped,
                "media forward queue saturated, shedding datagrams"
            );
        }
    }
}

/// Read one preview uni-stream to completion (spec §7.1), cache it as
/// opaque ciphertext on the stream state, and fan it out to every other
/// room member (broadcast, no subscription required — previews are the
/// discovery mechanism, §7.2).
async fn handle_preview_uni(
    room: Arc<Mutex<RoomState>>,
    sender_conn_id: ConnId,
    mut recv: RecvStream,
) {
    let data = match recv.read_to_end(MAX_PREVIEW_BYTES).await {
        Ok(d) => d,
        Err(e) => {
            tracing::debug!(
                conn_id = format!("{sender_conn_id:016x}"),
                "preview stream read failed: {e}"
            );
            return;
        }
    };
    let Ok((header, _)) = PreviewHeader::unpack(&data) else {
        tracing::debug!(
            conn_id = format!("{sender_conn_id:016x}"),
            "dropping malformed preview stream"
        );
        return;
    };
    if header.stream_type != STREAM_TYPE_PREVIEW {
        tracing::debug!("dropping uni stream with unknown stream type {:#x}", header.stream_type);
        return;
    }
    if header.conn_id != sender_conn_id {
        tracing::warn!(
            conn_id = format!("{sender_conn_id:016x}"),
            claimed = format!("{:016x}", header.conn_id),
            "dropping preview with spoofed conn_id"
        );
        return;
    }

    let blob = Bytes::from(data);

    let mut guard = room.lock().await;
    let Some(stream) = guard.streams.get_mut(&(sender_conn_id, header.stream_id)) else {
        tracing::debug!(
            conn_id = format!("{sender_conn_id:016x}"),
            "preview for unknown/stopped stream {}, dropping",
            header.stream_id
        );
        return;
    };
    stream.last_preview = Some(blob.clone());

    let targets: Vec<Session> = guard
        .connections
        .values()
        .filter(|c| c.conn_id != sender_conn_id)
        .map(|c| c.connection.clone())
        .collect();
    drop(guard);

    for conn in targets {
        let blob = blob.clone();
        tokio::spawn(async move {
            if let Ok(mut send) = conn.open_uni().await {
                let _ = send.write_all(&blob).await;
                let _ = send.finish();
            }
        });
    }
}

/// Tear down a departed connection: remove it from the room, drop any
/// streams it owned (broadcasting `StreamStopped`), remove it from every
/// other stream's subscriber set (notifying those owners of the new
/// count), broadcast `MemberLeft`, and drop the room if it's now empty
/// (spec §10).
async fn cleanup_connection(
    registry: &Arc<Registry>,
    room: &Arc<Mutex<RoomState>>,
    room_id: RoomId,
    conn_id: ConnId,
) {
    let mut guard = room.lock().await;

    let owned_streams: Vec<u8> = guard
        .streams
        .keys()
        .filter(|(owner, _)| *owner == conn_id)
        .map(|(_, sid)| *sid)
        .collect();
    for stream_id in owned_streams {
        if let Some(stopped) = guard.streams.remove(&(conn_id, stream_id)) {
            // Belt and braces: this connection's forwarder is already
            // gone by the time cleanup runs, but emptying the route makes
            // "removed from the room" and "routes nothing" atomic under
            // the room lock regardless of call order.
            stopped.route.publish(Vec::new());
        }
        guard.broadcast(
            &RelayToClient::PresenceDelta {
                event: PresenceEvent::StreamStopped { conn_id, stream_id },
            },
            Some(conn_id),
        );
    }

    let mut owner_notifications = Vec::new();
    let mut stale_routes = Vec::new();
    for stream in guard.streams.values_mut() {
        if stream.subscribers.remove(&conn_id) {
            owner_notifications.push((
                stream.owner,
                stream.stream_id,
                stream.subscribers.len() as u32,
            ));
            stale_routes.push((stream.owner, stream.stream_id));
        }
    }
    // Republish every route the departing member was in, so no snapshot
    // keeps forwarding to (and holding a handle on) a dead connection.
    for key in stale_routes {
        guard.refresh_route(key);
    }
    for (owner, stream_id, count) in owner_notifications {
        guard.send_to(owner, RelayToClient::SubscriberCount { stream_id, count });
    }

    guard.connections.remove(&conn_id);
    guard.log.push(format!("{conn_id:016x} left"));
    guard.broadcast(
        &RelayToClient::PresenceDelta {
            event: PresenceEvent::MemberLeft { conn_id },
        },
        None,
    );

    drop(guard);
    registry.drop_room_if_empty(room_id, room).await;
    tracing::info!(
        conn_id = format!("{conn_id:016x}"),
        "member left, cleaned up"
    );
}

fn hex_room(room_id: &RoomId) -> String {
    room_id.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
pub(crate) mod tests {
    //! End-to-end smoke test: bind the real relay on an ephemeral port,
    //! connect with a throwaway quinn client (self-signed cert accepted
    //! without pinning, matching this native-only phase's threat model
    //! for a test harness), send `Join`, and assert a `PresenceSnapshot`
    //! comes back. This exercises the core "the whole system depends on
    //! this" loop rather than just type-checking the protocol structs.

    use super::*;
    use crate::protocol::{MEDIA_HEADER_LEN, RelayToClient, StreamKind, WIRE_VERSION};
    // The test peers are plain quinn clients (they are what the native client
    // is), so shadow the Session-flavoured stream types imported above.
    use quinn::{Connection, RecvStream, SendStream};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use std::net::{IpAddr, Ipv4Addr};

    #[derive(Debug)]
    struct SkipServerVerification(Arc<rustls::crypto::CryptoProvider>);

    impl SkipServerVerification {
        fn new() -> Arc<Self> {
            Arc::new(Self(Arc::new(rustls::crypto::ring::default_provider())))
        }
    }

    impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp: &[u8],
            _now: UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls12_signature(
                message,
                cert,
                dss,
                &self.0.signature_verification_algorithms,
            )
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls13_signature(
                message,
                cert,
                dss,
                &self.0.signature_verification_algorithms,
            )
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            self.0.signature_verification_algorithms.supported_schemes()
        }
    }

    pub(crate) fn insecure_client_endpoint() -> Endpoint {
        let mut endpoint = Endpoint::client((IpAddr::V4(Ipv4Addr::LOCALHOST), 0).into())
            .expect("bind client endpoint");
        let mut crypto = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(SkipServerVerification::new())
            .with_no_client_auth();
        crypto.alpn_protocols = vec![ALPN_PROTOCOL.to_vec()];
        let quic_crypto =
            quinn::crypto::rustls::QuicClientConfig::try_from(crypto).expect("adapt client crypto");
        endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(quic_crypto)));
        endpoint
    }

    /// Send one length-prefixed control frame from a test client.
    pub(crate) async fn send_ctrl(send: &mut SendStream, msg: &ClientToRelay) {
        let framed = encode_frame(msg).expect("encode control frame");
        send.write_all(&framed).await.expect("write control frame");
    }

    /// Read one length-prefixed control frame on a test client.
    pub(crate) async fn recv_ctrl(recv: &mut RecvStream) -> RelayToClient {
        let mut len_buf = [0u8; 4];
        recv.read_exact(&mut len_buf).await.expect("read frame len");
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut payload = vec![0u8; len];
        recv.read_exact(&mut payload).await.expect("read frame body");
        decode_payload(&payload).expect("decode frame")
    }

    /// [`recv_ctrl`] that returns `None` instead of panicking when the stream ends.
    pub(crate) async fn try_recv_ctrl(recv: &mut RecvStream) -> Option<RelayToClient> {
        let mut len_buf = [0u8; 4];
        recv.read_exact(&mut len_buf).await.ok()?;
        let mut payload = vec![0u8; u32::from_be_bytes(len_buf) as usize];
        recv.read_exact(&mut payload).await.ok()?;
        decode_payload(&payload).ok()
    }

    /// Build one well-formed single-fragment media datagram (spec §6.2)
    /// with an opaque body — the relay never decrypts, so any bytes do.
    fn media_datagram(conn_id: ConnId, stream_id: StreamId, frame_id: u32) -> Bytes {
        let header = MediaHeader {
            version: WIRE_VERSION,
            flags: 0,
            conn_id,
            stream_id,
            frame_id,
            fragment_index: 0,
            fragment_count: 1,
        };
        let mut buf = Vec::with_capacity(MEDIA_HEADER_LEN + 32);
        buf.extend_from_slice(&header.pack());
        buf.extend_from_slice(&[0xAB; 32]);
        Bytes::from(buf)
    }

    /// Poll room state until `pred` holds, or panic after ~2s. Used to
    /// synchronize on the *server* having applied a control message,
    /// rather than sleeping and hoping.
    pub(crate) async fn await_room_state(
        registry: &Arc<Registry>,
        room_id: RoomId,
        label: &str,
        mut pred: impl FnMut(&RoomState) -> bool,
    ) {
        for _ in 0..200 {
            let room = registry.rooms.read().await.get(&room_id).cloned();
            if let Some(room) = room
                && pred(&*room.lock().await)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for room state: {label}");
    }

    /// The media data plane end to end: a subscriber receives forwarded
    /// datagrams, and stops receiving once it unsubscribes.
    ///
    /// This is the regression test for the fan-out rework (forwarding
    /// moved off the room mutex onto per-stream `StreamRoute` snapshots):
    /// it exercises the Subscribe -> forward -> Unsubscribe sequence that
    /// the route snapshot has to keep consistent with the authoritative
    /// subscriber set.
    #[tokio::test]
    async fn media_forwarding_follows_subscription_changes() {
        const SHARER: ConnId = 0xA000_0000_0000_0001;
        const VIEWER: ConnId = 0xB000_0000_0000_0002;
        const STREAM: StreamId = 0;

        let registry = Arc::new(Registry::new((Ipv4Addr::LOCALHOST, 0).into()));
        let server_endpoint = build_endpoint((IpAddr::V4(Ipv4Addr::LOCALHOST), 0).into())
            .expect("bind server endpoint");
        let local_addr = server_endpoint.local_addr().expect("local addr");
        tokio::spawn(serve(registry.clone(), server_endpoint));

        let client_endpoint = insecure_client_endpoint();
        let room_id: RoomId = [9u8; 16];

        // --- sharer joins and announces a stream ---
        let sharer = client_endpoint
            .connect(local_addr, "localhost")
            .expect("start connect")
            .await
            .expect("sharer handshake");
        let (mut sharer_send, mut sharer_recv) =
            sharer.open_bi().await.expect("sharer control stream");
        send_ctrl(
            &mut sharer_send,
            &ClientToRelay::Join {
                conn_id: SHARER,
                room_id,
                display_name: "sharer".into(),
            },
        )
        .await;
        assert!(matches!(
            recv_ctrl(&mut sharer_recv).await,
            RelayToClient::PresenceSnapshot { .. }
        ));
        send_ctrl(
            &mut sharer_send,
            &ClientToRelay::AnnounceStream {
                stream_id: STREAM,
                kind: StreamKind::Video,
                label: "Monitor 1".into(),
                width: 1920,
                height: 1080,
            },
        )
        .await;
        await_room_state(&registry, room_id, "stream announced", |r| {
            r.streams.contains_key(&(SHARER, STREAM))
        })
        .await;

        // --- viewer joins and subscribes ---
        let viewer = client_endpoint
            .connect(local_addr, "localhost")
            .expect("start connect")
            .await
            .expect("viewer handshake");
        let (mut viewer_send, mut viewer_recv) =
            viewer.open_bi().await.expect("viewer control stream");
        send_ctrl(
            &mut viewer_send,
            &ClientToRelay::Join {
                conn_id: VIEWER,
                room_id,
                display_name: "viewer".into(),
            },
        )
        .await;
        assert!(matches!(
            recv_ctrl(&mut viewer_recv).await,
            RelayToClient::PresenceSnapshot { .. }
        ));
        send_ctrl(
            &mut viewer_send,
            &ClientToRelay::Subscribe {
                target: SHARER,
                stream_id: STREAM,
            },
        )
        .await;
        await_room_state(&registry, room_id, "viewer subscribed", |r| {
            r.streams
                .get(&(SHARER, STREAM))
                .is_some_and(|s| s.subscribers.contains(&VIEWER))
        })
        .await;

        // The owner must still be asked for a keyframe on first subscribe
        // (the debounce path), ahead of any other pending control frame.
        let keyframe_requested = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let RelayToClient::KeyframeRequest { stream_id } =
                    recv_ctrl(&mut sharer_recv).await
                {
                    return stream_id;
                }
            }
        })
        .await
        .expect("keyframe request within 2s");
        assert_eq!(keyframe_requested, STREAM);

        // --- forwarding while subscribed ---
        for frame_id in 0..20 {
            sharer
                .send_datagram(media_datagram(SHARER, STREAM, frame_id))
                .expect("send media datagram");
        }
        let forwarded = tokio::time::timeout(Duration::from_secs(2), viewer.read_datagram())
            .await
            .expect("subscriber receives a forwarded datagram within 2s")
            .expect("datagram read ok");
        let (header, _) = MediaHeader::unpack(&forwarded).expect("forwarded header parses");
        assert_eq!(header.conn_id, SHARER, "forwarded verbatim, no rewrite");
        assert_eq!(header.stream_id, STREAM);

        // --- unsubscribe, then confirm the fan-out really stopped ---
        send_ctrl(
            &mut viewer_send,
            &ClientToRelay::Unsubscribe {
                target: SHARER,
                stream_id: STREAM,
            },
        )
        .await;
        await_room_state(&registry, room_id, "viewer unsubscribed", |r| {
            r.streams
                .get(&(SHARER, STREAM))
                .is_some_and(|s| s.subscribers.is_empty())
        })
        .await;

        // Drain anything already in flight when the unsubscribe landed —
        // an in-flight fragment being delivered is expected, forwarding
        // *new* fragments is not.
        while tokio::time::timeout(Duration::from_millis(200), viewer.read_datagram())
            .await
            .is_ok()
        {}

        for frame_id in 100..120 {
            sharer
                .send_datagram(media_datagram(SHARER, STREAM, frame_id))
                .expect("send media datagram");
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(500), viewer.read_datagram())
                .await
                .is_err(),
            "no datagrams may be forwarded to a connection that unsubscribed"
        );

        // The sender's own control plane is still responsive after all
        // that media traffic (the property the rework exists to protect).
        send_ctrl(
            &mut sharer_send,
            &ClientToRelay::StopStream { stream_id: STREAM },
        )
        .await;
        await_room_state(&registry, room_id, "stream stopped", |r| {
            !r.streams.contains_key(&(SHARER, STREAM))
        })
        .await;
    }

    /// Read and discard control frames until the stream errors out. Used
    /// so a test peer that is deliberately not inspecting its control
    /// plane still drains it, instead of letting the relay's writer task
    /// block on QUIC flow control.
    async fn drain_ctrl(mut recv: RecvStream) {
        let mut len_buf = [0u8; 4];
        while recv.read_exact(&mut len_buf).await.is_ok() {
            let len = u32::from_be_bytes(len_buf) as usize;
            let mut payload = vec![0u8; len];
            if recv.read_exact(&mut payload).await.is_err() {
                return;
            }
        }
    }

    /// Concurrency stress for the fan-out rework: three viewers hammer
    /// Subscribe/Unsubscribe on one stream, and a fourth vanishes
    /// mid-stream, while the sharer floods media datagrams — on a real
    /// **multi-threaded** runtime, so the room mutex, the per-stream
    /// `StreamRoute` snapshot and the per-connection forwarder tasks are
    /// genuinely contended rather than cooperatively interleaved.
    ///
    /// Three properties are asserted:
    /// 1. no panic (a stale `RouteTarget` must yield `Err`, never a crash),
    /// 2. no deadlock — a route lock taken out of order, or taken while
    ///    the room mutex is held across an await, would hang here,
    /// 3. **the settled state is correct**: once the churn stops with all
    ///    viewers subscribed, every viewer receives again. A dropped or
    ///    misordered `refresh_route` would leave a viewer permanently
    ///    starved, which "it still forwards to somebody" would not catch.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn subscription_churn_under_media_load_keeps_routes_consistent() {
        use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

        const SHARER: ConnId = 0xC000_0000_0000_0001;
        const VIEWER_BASE: ConnId = 0xD000_0000_0000_0000;
        const QUITTER: ConnId = 0xE000_0000_0000_0009;
        const STREAM: StreamId = 3;
        const VIEWERS: u64 = 3;
        const CHURN_ROUNDS: usize = 120;

        let registry = Arc::new(Registry::new((Ipv4Addr::LOCALHOST, 0).into()));
        let server_endpoint = build_endpoint((IpAddr::V4(Ipv4Addr::LOCALHOST), 0).into())
            .expect("bind server endpoint");
        let local_addr = server_endpoint.local_addr().expect("local addr");
        tokio::spawn(serve(registry.clone(), server_endpoint));

        let client_endpoint = insecure_client_endpoint();
        let room_id: RoomId = [11u8; 16];

        /// Connect + Join one test client, returning its connection and
        /// control stream halves once the `PresenceSnapshot` has landed.
        async fn join_client(
            endpoint: &Endpoint,
            addr: SocketAddr,
            conn_id: ConnId,
            room_id: RoomId,
            name: &str,
        ) -> (Connection, SendStream, RecvStream) {
            let conn = endpoint
                .connect(addr, "localhost")
                .expect("start connect")
                .await
                .expect("handshake");
            let (mut send, mut recv) = conn.open_bi().await.expect("control stream");
            send_ctrl(
                &mut send,
                &ClientToRelay::Join {
                    conn_id,
                    room_id,
                    display_name: name.into(),
                },
            )
            .await;
            assert!(matches!(
                recv_ctrl(&mut recv).await,
                RelayToClient::PresenceSnapshot { .. }
            ));
            (conn, send, recv)
        }

        // --- sharer joins and announces ---
        let (sharer, mut sharer_send, sharer_recv) =
            join_client(&client_endpoint, local_addr, SHARER, room_id, "sharer").await;
        send_ctrl(
            &mut sharer_send,
            &ClientToRelay::AnnounceStream {
                stream_id: STREAM,
                kind: StreamKind::Video,
                label: "Monitor 1".into(),
                width: 1920,
                height: 1080,
            },
        )
        .await;
        await_room_state(&registry, room_id, "stream announced", |r| {
            r.streams.contains_key(&(SHARER, STREAM))
        })
        .await;
        // The sharer is about to be sent ~1k SubscriberCount frames; drain
        // them so its control writer never blocks on flow control.
        let sharer_drain = tokio::spawn(drain_ctrl(sharer_recv));

        // --- viewers join ---
        let mut viewers = Vec::new();
        for i in 0..VIEWERS {
            let conn_id = VIEWER_BASE | (i + 1);
            let (conn, send, recv) = join_client(
                &client_endpoint,
                local_addr,
                conn_id,
                room_id,
                &format!("viewer-{i}"),
            )
            .await;
            tokio::spawn(drain_ctrl(recv));
            viewers.push((conn_id, conn, send));
        }

        // A viewer that subscribes and then dies mid-forward, exercising
        // cleanup_connection's route republish against a live fan-out.
        let (quitter_conn, mut quitter_send, quitter_recv) =
            join_client(&client_endpoint, local_addr, QUITTER, room_id, "quitter").await;
        tokio::spawn(drain_ctrl(quitter_recv));
        send_ctrl(
            &mut quitter_send,
            &ClientToRelay::Subscribe {
                target: SHARER,
                stream_id: STREAM,
            },
        )
        .await;
        await_room_state(&registry, room_id, "quitter subscribed", |r| {
            r.streams
                .get(&(SHARER, STREAM))
                .is_some_and(|s| s.subscribers.contains(&QUITTER))
        })
        .await;

        // --- media flood ---
        let stop = Arc::new(AtomicBool::new(false));
        let sender = {
            let sharer = sharer.clone();
            let stop = stop.clone();
            tokio::spawn(async move {
                let mut frame_id = 0u32;
                while !stop.load(AtomicOrdering::Relaxed) {
                    for _ in 0..16 {
                        let _ = sharer.send_datagram(media_datagram(SHARER, STREAM, frame_id));
                        frame_id = frame_id.wrapping_add(1);
                    }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
        };

        // Kill the quitter partway through, while fan-out is in flight.
        let quitter_killer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            drop(quitter_send);
            quitter_conn.close(0u32.into(), b"bye");
        });

        // --- subscription churn, all viewers concurrently ---
        let churn: Vec<_> = viewers
            .into_iter()
            .map(|(conn_id, conn, mut send)| {
                tokio::spawn(async move {
                    for _ in 0..CHURN_ROUNDS {
                        send_ctrl(
                            &mut send,
                            &ClientToRelay::Subscribe {
                                target: SHARER,
                                stream_id: STREAM,
                            },
                        )
                        .await;
                        tokio::time::sleep(Duration::from_millis(1)).await;
                        send_ctrl(
                            &mut send,
                            &ClientToRelay::Unsubscribe {
                                target: SHARER,
                                stream_id: STREAM,
                            },
                        )
                        .await;
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                    // Settle subscribed, so the end-state assertion below
                    // measures a route that *should* be forwarding.
                    send_ctrl(
                        &mut send,
                        &ClientToRelay::Subscribe {
                            target: SHARER,
                            stream_id: STREAM,
                        },
                    )
                    .await;
                    (conn_id, conn, send)
                })
            })
            .collect();

        let mut settled = Vec::new();
        for handle in churn {
            settled.push(
                tokio::time::timeout(Duration::from_secs(30), handle)
                    .await
                    .expect("churn task finished (deadlock if this times out)")
                    .expect("churn task did not panic"),
            );
        }
        quitter_killer.await.expect("quitter killer finished");

        // --- everything must have converged ---
        await_room_state(&registry, room_id, "quitter cleaned up", |r| {
            !r.connections.contains_key(&QUITTER)
        })
        .await;
        await_room_state(&registry, room_id, "all viewers subscribed", |r| {
            r.streams
                .get(&(SHARER, STREAM))
                .is_some_and(|s| s.subscribers.len() == VIEWERS as usize)
        })
        .await;

        // The derived route snapshot must agree with the authoritative
        // subscriber set — this is the invariant the whole design rests on.
        {
            let rooms = registry.rooms.read().await;
            let room = rooms.get(&room_id).expect("room still live").clone();
            drop(rooms);
            let guard = room.lock().await;
            let stream = guard.streams.get(&(SHARER, STREAM)).expect("stream live");
            let targets = stream.route.targets();
            let routed: HashSet<ConnId> = targets.iter().map(|t| t.conn_id).collect();
            assert_eq!(
                routed, stream.subscribers,
                "route snapshot diverged from the authoritative subscriber set"
            );
            assert!(
                !routed.contains(&QUITTER),
                "route still holds a handle on a departed connection"
            );
        }

        // Quiesce the sender and drain each viewer, so what we assert on
        // next is definitely a *newly* forwarded datagram.
        stop.store(true, AtomicOrdering::Relaxed);
        sender.await.expect("sender task finished");
        for (_, conn, _) in &settled {
            while tokio::time::timeout(Duration::from_millis(150), conn.read_datagram())
                .await
                .is_ok()
            {}
        }

        // --- the real correctness bar: everyone is receiving again ---
        for frame_id in 900_000..900_400 {
            let _ = sharer.send_datagram(media_datagram(SHARER, STREAM, frame_id));
        }
        for (conn_id, conn, _) in &settled {
            let forwarded = tokio::time::timeout(Duration::from_secs(5), conn.read_datagram())
                .await
                .unwrap_or_else(|_| {
                    panic!("viewer {conn_id:016x} is subscribed but receives nothing")
                })
                .expect("datagram read ok");
            let (header, _) = MediaHeader::unpack(&forwarded).expect("header parses");
            assert_eq!(header.conn_id, SHARER);
            assert_eq!(header.stream_id, STREAM);
        }

        sharer_drain.abort();
    }

    #[tokio::test]
    async fn join_yields_presence_snapshot() {
        let registry = Arc::new(Registry::new((Ipv4Addr::LOCALHOST, 0).into()));
        let server_endpoint = build_endpoint((IpAddr::V4(Ipv4Addr::LOCALHOST), 0).into())
            .expect("bind server endpoint");
        let local_addr = server_endpoint.local_addr().expect("local addr");
        tokio::spawn(serve(registry.clone(), server_endpoint));

        let client_endpoint = insecure_client_endpoint();
        let connection = client_endpoint
            .connect(local_addr, "localhost")
            .expect("start connect")
            .await
            .expect("QUIC handshake");

        let (mut send, mut recv) = connection.open_bi().await.expect("open control stream");

        let room_id: RoomId = [7u8; 16];
        let join = ClientToRelay::Join {
            conn_id: 0xDEAD_BEEF_0000_0001,
            room_id,
            display_name: "smoke-test".into(),
        };
        let framed = encode_frame(&join).expect("encode join");
        send.write_all(&framed).await.expect("write join");

        let mut len_buf = [0u8; 4];
        recv.read_exact(&mut len_buf)
            .await
            .expect("read snapshot length");
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut payload = vec![0u8; len];
        recv.read_exact(&mut payload)
            .await
            .expect("read snapshot payload");
        let msg: RelayToClient = decode_payload(&payload).expect("decode snapshot");

        match msg {
            RelayToClient::PresenceSnapshot { members, streams } => {
                assert_eq!(members.len(), 1);
                assert_eq!(members[0].conn_id, 0xDEAD_BEEF_0000_0001);
                assert_eq!(members[0].display_name, "smoke-test");
                assert_eq!(members[0].stream_count, 0);
                assert!(streams.is_empty());
            }
            other => panic!("expected PresenceSnapshot, got {other:?}"),
        }

        // Also exercise AnnounceStream -> StreamStarted broadcast doesn't
        // panic / the connection stays healthy end-to-end.
        let announce = ClientToRelay::AnnounceStream {
            stream_id: 0,
            kind: StreamKind::Video,
            label: "Monitor 1".into(),
            width: 1920,
            height: 1080,
        };
        let framed = encode_frame(&announce).expect("encode announce");
        send.write_all(&framed).await.expect("write announce");

        // Give the relay a moment to process, then confirm room state.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let rooms = registry.rooms.read().await;
        let room = rooms.get(&room_id).expect("room exists");
        let guard = room.lock().await;
        assert_eq!(guard.streams.len(), 1);
        assert_eq!(guard.connections.len(), 1);
    }

    /// Connect a native test client and send one `Join`.
    async fn connect_and_join(
        endpoint: &Endpoint,
        addr: SocketAddr,
        conn_id: ConnId,
        room_id: RoomId,
    ) -> (Connection, SendStream, RecvStream) {
        let conn = endpoint.connect(addr, "localhost").unwrap().await.expect("handshake");
        let (mut send, recv) = conn.open_bi().await.expect("control stream");
        send_ctrl(
            &mut send,
            &ClientToRelay::Join { conn_id, room_id, display_name: format!("c{conn_id}") },
        )
        .await;
        (conn, send, recv)
    }

    /// The 9th joiner must receive `JoinRejected{RoomFull}` (not a bare
    /// connection loss), followed by an application close with
    /// [`CLOSE_ROOM_FULL`].
    #[tokio::test]
    async fn full_room_delivers_join_rejected_then_closes() {
        let registry = Arc::new(Registry::new((Ipv4Addr::LOCALHOST, 0).into()));
        let server = build_endpoint((IpAddr::V4(Ipv4Addr::LOCALHOST), 0).into()).unwrap();
        let addr = server.local_addr().unwrap();
        tokio::spawn(serve(registry, server));
        let client = insecure_client_endpoint();
        let room_id: RoomId = [9u8; 16];

        let mut members = Vec::new();
        for id in 1..=MAX_ROOM_MEMBERS as u64 {
            let (conn, send, mut recv) = connect_and_join(&client, addr, id, room_id).await;
            assert!(matches!(recv_ctrl(&mut recv).await, RelayToClient::PresenceSnapshot { .. }));
            members.push((conn, send, recv));
        }

        let (conn, _send, mut recv) = connect_and_join(&client, addr, 99, room_id).await;
        let reply = tokio::time::timeout(Duration::from_secs(5), recv_ctrl(&mut recv))
            .await
            .expect("JoinRejected arrives promptly");
        assert!(matches!(
            reply,
            RelayToClient::JoinRejected { reason: RejectReason::RoomFull }
        ));
        // Peer hangs up (as a real client would); the relay then closes too.
        conn.close(0u32.into(), b"ok");
    }

    /// With a silent peer the relay itself closes with `CLOSE_ROOM_FULL`
    /// within the bounded flush window.
    #[tokio::test]
    async fn full_room_relay_closes_with_code() {
        let registry = Arc::new(Registry::new((Ipv4Addr::LOCALHOST, 0).into()));
        let server = build_endpoint((IpAddr::V4(Ipv4Addr::LOCALHOST), 0).into()).unwrap();
        let addr = server.local_addr().unwrap();
        tokio::spawn(serve(registry, server));
        let client = insecure_client_endpoint();
        let room_id: RoomId = [8u8; 16];
        let mut members = Vec::new();
        for id in 1..=MAX_ROOM_MEMBERS as u64 {
            let (conn, send, mut recv) = connect_and_join(&client, addr, id, room_id).await;
            recv_ctrl(&mut recv).await;
            members.push((conn, send, recv));
        }
        let (conn, _send, mut recv) = connect_and_join(&client, addr, 99, room_id).await;
        assert!(matches!(
            recv_ctrl(&mut recv).await,
            RelayToClient::JoinRejected { reason: RejectReason::RoomFull }
        ));
        let err = tokio::time::timeout(Duration::from_secs(5), conn.closed())
            .await
            .expect("relay closes the connection");
        match err {
            quinn::ConnectionError::ApplicationClosed(c) => {
                assert_eq!(u64::from(c.error_code), CLOSE_ROOM_FULL as u64);
                assert_eq!(&c.reason[..], b"room full");
            }
            other => panic!("unexpected close: {other:?}"),
        }
    }

    /// `shutdown` closes live connections at once with `CLOSE_SHUTDOWN`.
    #[tokio::test]
    async fn shutdown_closes_connections_promptly() {
        let registry = Arc::new(Registry::new((Ipv4Addr::LOCALHOST, 0).into()));
        let server = build_endpoint((IpAddr::V4(Ipv4Addr::LOCALHOST), 0).into()).unwrap();
        let addr = server.local_addr().unwrap();
        tokio::spawn(serve(registry, server.clone()));
        let client = insecure_client_endpoint();
        let (conn, _send, mut recv) = connect_and_join(&client, addr, 1, [7u8; 16]).await;
        assert!(matches!(recv_ctrl(&mut recv).await, RelayToClient::PresenceSnapshot { .. }));

        let started = Instant::now();
        shutdown(&server).await;
        let err = tokio::time::timeout(Duration::from_secs(3), conn.closed())
            .await
            .expect("client sees close promptly");
        assert!(started.elapsed() < Duration::from_secs(3));
        match err {
            quinn::ConnectionError::ApplicationClosed(c) => {
                assert_eq!(u64::from(c.error_code), CLOSE_SHUTDOWN as u64);
                assert_eq!(&c.reason[..], b"relay shutting down");
            }
            other => panic!("unexpected close: {other:?}"),
        }
    }
}
