//! QUIC listener and packet relay logic (spec §1, §10).
//!
//! Self-signed cert (native-QUIC-only phase, spec §11 — no CA/domain, no
//! WebTransport cert-hash-pinning yet). One tokio task per accepted
//! connection ("connection actor"): it owns the control bidi stream, and
//! loops `select!`-ing on further control frames, media datagrams, and
//! incoming preview uni-streams from that connection.
//!
//! The relay never decrypts anything — every operation here reads only
//! cleartext header fields (`ConnId`, `stream_id`, frame/preview counters)
//! and forwards opaque ciphertext bytes verbatim.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use quinn::{Connection, Endpoint, RecvStream, SendStream};
use rcgen::CertifiedKey;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::sync::{Mutex, mpsc};

use crate::protocol::{
    ClientToRelay, ConnId, MAX_CONTROL_FRAME_LEN, MAX_NAME_LEN, MAX_ROOM_MEMBERS,
    MediaDatagramHeader, Member, PresenceEvent, PreviewHeader, RejectReason, RelayToClient, RoomId,
    StreamInfo, clamp_utf8, decode_payload, encode_frame,
};
use crate::room::{ConnState, Registry, RoomState, StreamState};

/// ALPN identifier for this native-QUIC-only phase. Not mandated by the
/// wire spec (which is transport-agnostic below the accept layer, §11);
/// picking a concrete value here just keeps client/server handshakes from
/// silently accepting an unrelated ALPN.
pub const ALPN_PROTOCOL: &[u8] = b"ssshare/1";

/// Max bytes accepted for one preview blob (15-byte header + JPEG
/// ciphertext). Generous headroom over a typical ~30s discovery
/// thumbnail; just a sanity backstop against a misbehaving/hostile peer.
const MAX_PREVIEW_BYTES: usize = 2 * 1024 * 1024;

/// Sender-side coalescing window mirrored on the relay for
/// `KeyframeRequest` bursts (spec §9 describes the *sender's* ~100ms
/// coalescing; this is a light relay-side debounce so a burst of
/// `Subscribe`s in the same instant doesn't fan out N keyframe requests).
const KEYFRAME_REQUEST_DEBOUNCE: Duration = Duration::from_millis(100);

/// Build a self-signed rustls/quinn server config. Generates a fresh
/// keypair + cert every startup (no persistence, per spec's native-only
/// phase — no CA, no domain to persist against).
fn build_server_config() -> Result<quinn::ServerConfig> {
    let CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["simple-screenshare.local".to_string()])
            .context("generating self-signed certificate")?;
    let cert_der: CertificateDer<'static> = cert.into();
    let key_der: PrivateKeyDer<'static> =
        PrivatePkcs8KeyDer::from(signing_key.serialize_der()).into();

    let mut server_crypto = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .context("building rustls server config")?;
    server_crypto.alpn_protocols = vec![ALPN_PROTOCOL.to_vec()];

    let quic_crypto = quinn::crypto::rustls::QuicServerConfig::try_from(server_crypto)
        .context("adapting rustls config for QUIC")?;
    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(quic_crypto));

    // Keep idle connections alive across manual-testing pauses (dialogs,
    // UI clicking, etc). Without this, quinn's default ~30s idle timeout
    // drops clients that briefly go quiet. keep_alive_interval must stay
    // comfortably below max_idle_timeout on at least one peer so pings
    // keep the connection from ever reaching the idle deadline, while a
    // truly dead peer still gets reaped in a reasonable time.
    let mut transport_config = quinn::TransportConfig::default();
    transport_config
        .max_idle_timeout(Some(
            quinn::IdleTimeout::try_from(Duration::from_secs(60))
                .expect("60s fits in IdleTimeout"),
        ))
        .keep_alive_interval(Some(Duration::from_secs(15)));
    server_config.transport_config(Arc::new(transport_config));

    Ok(server_config)
}

/// Bind a QUIC endpoint on `listen_addr` with a freshly generated
/// self-signed cert. Split out from [`run`] so tests can bind on an
/// ephemeral port (`:0`) and read back the actual bound address before
/// starting the accept loop.
pub fn build_endpoint(listen_addr: SocketAddr) -> Result<Endpoint> {
    let server_config = build_server_config()?;
    Endpoint::server(server_config, listen_addr)
        .with_context(|| format!("binding QUIC endpoint on {listen_addr}"))
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

/// Start the QUIC endpoint and run the accept loop until the process is
/// torn down.
pub async fn run(registry: Arc<Registry>, listen_addr: SocketAddr) -> Result<()> {
    let endpoint = build_endpoint(listen_addr)?;
    let local_addr = endpoint
        .local_addr()
        .context("reading bound local address")?;
    tracing::info!(%local_addr, "QUIC endpoint listening");
    serve(registry, endpoint).await
}

/// Read one length-prefixed control frame (spec §2). Returns `Ok(None)`
/// on a clean stream close with no partial frame in flight (peer FIN'd
/// after finishing all messages) — the relay treats this the same as an
/// explicit `Leave` (spec §4.1).
async fn read_control_frame(recv: &mut RecvStream) -> Result<Option<ClientToRelay>> {
    let mut len_buf = [0u8; 4];
    match recv.read_exact(&mut len_buf).await {
        Ok(()) => {}
        Err(quinn::ReadExactError::FinishedEarly(0)) => return Ok(None),
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

/// Run the join handshake: read `Join` messages off `ctrl_recv` until one
/// succeeds (retrying past `ConnIdCollision`, per spec §3.1/§4.2) or the
/// connection gives up / is rejected terminally. On success, registers
/// the connection in its room, sends the `PresenceSnapshot` + cached
/// previews, and broadcasts `MemberJoined` to the rest of the room.
async fn perform_join(
    registry: &Arc<Registry>,
    connection: &Connection,
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

        guard
            .log
            .push(format!("{display_name} joined ({conn_id:016x})"));

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

/// Drive one accepted QUIC connection end to end: join handshake, then
/// the steady-state control/datagram/uni-stream loop, then cleanup.
async fn handle_connection(registry: Arc<Registry>, incoming: quinn::Incoming) -> Result<()> {
    let connection = incoming.await.context("QUIC handshake failed")?;
    let remote = connection.remote_address();
    tracing::debug!(%remote, "connection established, awaiting control stream");

    let (mut ctrl_send, mut ctrl_recv) = connection
        .accept_bi()
        .await
        .context("waiting for control stream")?;

    let (control_tx, control_rx) = mpsc::unbounded_channel::<RelayToClient>();

    let joined = match perform_join(
        &registry,
        &connection,
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

    tracing::info!(%remote, conn_id = format!("{conn_id:016x}"), room_id = %hex_room(&room_id), "member joined");

    let writer = tokio::spawn(control_writer_task(ctrl_send, control_rx));

    let run_result = connection_loop(&registry, &room, conn_id, &connection, &mut ctrl_recv).await;

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
    connection: &Connection,
    ctrl_recv: &mut RecvStream,
) -> Result<()> {
    loop {
        tokio::select! {
            frame = read_control_frame(ctrl_recv) => {
                match frame? {
                    Some(ClientToRelay::Leave) => {
                        tracing::debug!(conn_id = format!("{conn_id:016x}"), "explicit Leave");
                        return Ok(());
                    }
                    Some(msg) => handle_client_message(room, conn_id, msg).await,
                    None => {
                        tracing::debug!(conn_id = format!("{conn_id:016x}"), "control stream closed (implicit leave)");
                        return Ok(());
                    }
                }
            }
            dgram = connection.read_datagram() => {
                match dgram {
                    Ok(bytes) => forward_media_datagram(registry, room, conn_id, bytes).await,
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
async fn handle_client_message(room: &Arc<Mutex<RoomState>>, conn_id: ConnId, msg: ClientToRelay) {
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
            let mut guard = room.lock().await;
            guard.streams.insert(
                (conn_id, stream_id),
                StreamState {
                    owner: conn_id,
                    stream_id,
                    kind,
                    label: label.clone(),
                    width,
                    height,
                    subscribers: HashSet::new(),
                    bytes_received: std::sync::atomic::AtomicU64::new(0),
                    last_preview: None,
                    last_keyframe_request: None,
                },
            );
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
            if guard.streams.remove(&(conn_id, stream_id)).is_some() {
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
                guard.log.push(format!(
                    "{conn_id:016x} unsubscribed from ({target:016x}, {stream_id}) -> count={count}"
                ));
                guard.send_to(target, RelayToClient::SubscriberCount { stream_id, count });
            }
        }
    }
}

/// Route one media datagram (spec §6.2) to every current subscriber of
/// its `(conn_id, stream_id)`, verbatim, no header rewrite. Drops the
/// datagram if malformed, spoofed, or unknown/unsubscribed.
async fn forward_media_datagram(
    registry: &Arc<Registry>,
    room: &Arc<Mutex<RoomState>>,
    conn_id: ConnId,
    data: Bytes,
) {
    let Some(header) = MediaDatagramHeader::unpack(&data) else {
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

    let guard = room.lock().await;
    let Some(stream) = guard.streams.get(&(conn_id, header.stream_id)) else {
        return; // no such stream (or already stopped): drop
    };
    stream
        .bytes_received
        .fetch_add(data.len() as u64, std::sync::atomic::Ordering::Relaxed);
    if stream.subscribers.is_empty() {
        return;
    }
    for sub in &stream.subscribers {
        if let Some(target) = guard.connections.get(sub) {
            match target.connection.send_datagram(data.clone()) {
                Ok(()) => registry.bandwidth.add_tx(data.len() as u64),
                Err(e) => tracing::debug!("dropping forwarded datagram to {sub:016x}: {e}"),
            }
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
    let Some(header) = PreviewHeader::unpack(&data) else {
        tracing::debug!(
            conn_id = format!("{sender_conn_id:016x}"),
            "dropping malformed preview stream"
        );
        return;
    };
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

    let targets: Vec<Connection> = guard
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
        guard.streams.remove(&(conn_id, stream_id));
        guard.broadcast(
            &RelayToClient::PresenceDelta {
                event: PresenceEvent::StreamStopped { conn_id, stream_id },
            },
            Some(conn_id),
        );
    }

    let mut owner_notifications = Vec::new();
    for stream in guard.streams.values_mut() {
        if stream.subscribers.remove(&conn_id) {
            owner_notifications.push((
                stream.owner,
                stream.stream_id,
                stream.subscribers.len() as u32,
            ));
        }
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
mod tests {
    //! End-to-end smoke test: bind the real relay on an ephemeral port,
    //! connect with a throwaway quinn client (self-signed cert accepted
    //! without pinning, matching this native-only phase's threat model
    //! for a test harness), send `Join`, and assert a `PresenceSnapshot`
    //! comes back. This exercises the core "the whole system depends on
    //! this" loop rather than just type-checking the protocol structs.

    use super::*;
    use crate::protocol::{RelayToClient, StreamKind};
    use rustls::pki_types::{ServerName, UnixTime};
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

    fn insecure_client_endpoint() -> Endpoint {
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
}
