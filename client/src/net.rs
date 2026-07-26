//! QUIC networking layer: connect, join a room, send/receive control
//! messages, and encrypt/decrypt/fragment/reassemble media per
//! `superdoc/architecture/protocol-spec.md`.
//!
//! # Join-string format and key encoding
//!
//! This project's fixed link format is `server|room-id|key`:
//!
//! - `server` — `host:port` of the relay, e.g. `relay.example.com:4433`.
//! - `room-id` — the spec's 16-byte `RoomId`, encoded as **32 lowercase hex
//!   characters** (chosen over base64 for the room id specifically because
//!   it's the field most likely to get manually compared/grepped/logged).
//! - `key` — the 32-byte AES-256 room key, encoded as **base64url, no
//!   padding** (`base64::engine::general_purpose::URL_SAFE_NO_PAD`). Chosen
//!   over hex because it's ~33% shorter (43 chars vs 64), which matters more
//!   for a value users paste around. Nothing upstream (spec, server) fixed
//!   this encoding yet — this is the client's decision, documented here so
//!   the server/link-generation side can match it.
//!
//! Example: `127.0.0.1:4433|00112233445566778899aabbccddeeff|Zm9vYmFyYmF6cXV1eDEyMzQ1Njc4OTAxMjM0NTY`
//!
//! # Public API shape
//!
//! [`ScreenshareClient::connect`] does the whole handshake (QUIC connect +
//! send `Join` + await `PresenceSnapshot`/`JoinRejected`, retrying once on
//! `ConnIdCollision` per spec §3.1) and returns three things:
//!
//! 1. The connected `ScreenshareClient` handle, used for outgoing calls
//!    (`announce_stream`, `subscribe`, `unsubscribe`, `stop_stream`, `leave`).
//! 2. A [`PresenceSnapshotState`] with the initial roster/stream list — the
//!    caller (`ui.rs`) seeds its UI state from this synchronously rather than
//!    waiting for the first channel event.
//! 3. A `tokio::sync::mpsc::UnboundedReceiver<ClientEvent>` that the caller
//!    polls (e.g. `try_recv()` from the egui update loop, since eframe's UI
//!    closure can't `.await`) for everything that happens afterward:
//!    presence deltas, subscriber-count/keyframe-request signals (for a
//!    future sender), decrypted incoming media frames, decrypted incoming
//!    previews, and a final "control connection closed" notice.
//!
//! # One endpoint, N connections
//!
//! [`build_client_endpoint`] builds the local QUIC endpoint (a bound UDP
//! socket plus this client's TLS/transport config); [`ScreenshareClient::connect`]
//! takes one by reference rather than building its own.
//!
//! `Why:` a `quinn::Endpoint` is a *local socket*, not a peer — one endpoint
//! can hold any number of outgoing connections to any number of relays, and
//! multiplexes them on a single UDP socket with a single I/O driver task.
//! Building one per connection (which is what this module used to do) burned
//! a socket + driver task per room and made every room's connection
//! independently configurable by accident. The caller (`ui.rs`'s `App`)
//! builds exactly one at construction and hands it to every `connect()`
//! call, which is what makes multi-room joins cheap.
//!
//! `capture.rs` (sending) will use `ScreenshareClient::connection()` +
//! `nonce_sequence()` + `room_key()` together with `encrypt_frame` and
//! `fragment::fragment_frame` to actually transmit; `decode.rs` (receiving)
//! will consume `ClientEvent::MediaFrame`/`ClientEvent::Preview` off the
//! event channel. Neither is wired up here — that's out of scope for this
//! task — but the surface is shaped for them.
//!
//! # Certificate verification (native-only phase)
//!
//! Per spec §11, self-signed cert hash-pinning is explicitly deferred (it
//! would ride the join link's 4th field, which doesn't exist yet in this
//! project's 3-field `server|room-id|key` format). Until that lands, this
//! client skips TLS certificate verification entirely (see
//! `SkipServerVerification` below) — the QUIC transport is still encrypted
//! against passive eavesdroppers, but is not authenticated against an
//! active MITM. That gap is exactly what §11's deferred pinning is meant to
//! close later; it is *separate* from the AES-256-GCM room-key encryption
//! (§5), which already protects the actual audio/video/preview payloads end
//! to end regardless of the transport's cert trust.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use aes_gcm::{
    aead::{Aead, Payload},
    aes::cipher::consts::U12,
    Aes256Gcm, KeyInit, Nonce,
};
use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use tokio::sync::{mpsc, Mutex};

use crate::fragment::Reassembler;
use crate::protocol::{
    decode_control_payload, encode_control_frame, ClientToRelay, ConnId, MediaHeader, Member,
    PresenceEvent, PreviewHeader, RejectReason, RelayToClient, RoomId, StreamId, StreamInfo,
    StreamKind, MAX_CONTROL_FRAME_LEN, PROTOCOL_VERSION, STREAM_TYPE_PREVIEW,
};

/// ALPN protocol identifier for this project's native QUIC transport.
/// RFC 9001 §8.1 requires ALPN in the QUIC/TLS handshake. Matches the
/// server's `ALPN_PROTOCOL` constant in `simple-screensharing-server/src/relay.rs`
/// (`b"ssshare/1"`) — checked directly against that repo's source rather
/// than guessed, since a mismatch here fails the handshake outright.
pub const ALPN: &[u8] = b"ssshare/1";

// ---------------------------------------------------------------------
// Join-string parsing
// ---------------------------------------------------------------------

/// A parsed `server|room-id|key` join string. See module docs for the
/// encoding of each field.
#[derive(Debug, Clone)]
pub struct ParsedJoinString {
    pub server: String,
    pub room_id: RoomId,
    pub room_key: [u8; 32],
}

pub fn parse_join_string(s: &str) -> Result<ParsedJoinString> {
    let s = s.trim();
    let mut parts = s.splitn(3, '|');
    let server = parts
        .next()
        .filter(|p| !p.is_empty())
        .ok_or_else(|| anyhow!("join string missing server address (expected server|room-id|key)"))?;
    let room_id_str = parts
        .next()
        .ok_or_else(|| anyhow!("join string missing room id (expected server|room-id|key)"))?;
    let key_str = parts
        .next()
        .ok_or_else(|| anyhow!("join string missing key (expected server|room-id|key)"))?;

    let room_id = parse_room_id_hex(room_id_str)?;
    let room_key = parse_room_key_base64(key_str)?;

    Ok(ParsedJoinString {
        server: server.to_string(),
        room_id,
        room_key,
    })
}

/// Build a join string from its parts, using this client's chosen encoding.
/// Useful for `rooms.rs` (saved lounges) and for generating new room links.
pub fn format_join_string(server: &str, room_id: &RoomId, room_key: &[u8; 32]) -> String {
    format!(
        "{server}|{}|{}",
        encode_room_id_hex(room_id),
        URL_SAFE_NO_PAD.encode(room_key)
    )
}

fn parse_room_id_hex(s: &str) -> Result<RoomId> {
    if s.len() != 32 {
        bail!("room id must be 32 hex characters (16 bytes), got {} characters", s.len());
    }
    let mut out = [0u8; 16];
    for i in 0..16 {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
            .with_context(|| format!("room id has invalid hex at byte {i}"))?;
    }
    Ok(out)
}

fn encode_room_id_hex(room_id: &RoomId) -> String {
    room_id.iter().map(|b| format!("{b:02x}")).collect()
}

fn parse_room_key_base64(s: &str) -> Result<[u8; 32]> {
    let bytes = URL_SAFE_NO_PAD
        .decode(s)
        .context("room key is not valid base64url (no padding)")?;
    if bytes.len() != 32 {
        bail!(
            "room key must decode to 32 bytes for AES-256, got {} bytes",
            bytes.len()
        );
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

// ---------------------------------------------------------------------
// Nonce sequencing (spec §5.1)
// ---------------------------------------------------------------------

/// Error returned when a connection's 32-bit counter space is exhausted.
/// Per spec §5.2 the connection MUST close in this case and a fresh
/// connection (with a fresh random `ConnId`) is the only reset mechanism.
#[derive(Debug, Clone, Copy)]
pub struct NonceSpaceExhausted;

impl std::fmt::Display for NonceSpaceExhausted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "connection's nonce counter space is exhausted; a new connection (fresh ConnId) is required"
        )
    }
}

impl std::error::Error for NonceSpaceExhausted {}

/// One shared, atomically-incrementing counter per QUIC connection, per spec
/// §5.1: every video/audio stream and every preview blob on this connection
/// draws from the *same* counter, never a per-stream one, so that
/// `(conn_id, counter)` is globally unique across every plane by
/// construction.
///
/// Internally uses a 64-bit counter (rather than a bare `AtomicU32`) purely
/// so overflow past `u32::MAX` can be *detected* via a plain comparison
/// instead of relying on wrapping-add semantics, which would otherwise
/// silently reuse a `(ConnId, counter)` pair — the exact nonce-reuse bug the
/// spec calls out as catastrophic. The wire nonce itself is always the
/// standard 8-byte `ConnId` + 4-byte counter (12 bytes total).
pub struct NonceSequence {
    conn_id: ConnId,
    next: AtomicU64,
}

impl NonceSequence {
    pub fn new(conn_id: ConnId) -> Self {
        Self {
            conn_id,
            next: AtomicU64::new(0),
        }
    }

    pub fn conn_id(&self) -> ConnId {
        self.conn_id
    }

    /// Atomically consume the next counter value and build this
    /// connection's AES-GCM nonce for it. `stream_id` is accepted (as
    /// requested) for call-site clarity/future use, but per spec §5.1 the
    /// counter is deliberately *not* domain-separated by stream — it does
    /// not influence the nonce or which counter value is drawn.
    pub fn next_nonce(&self, _stream_id: u8) -> Result<([u8; 12], u32), NonceSpaceExhausted> {
        let counter = self.next.fetch_add(1, Ordering::SeqCst);
        if counter > u32::MAX as u64 {
            return Err(NonceSpaceExhausted);
        }
        let counter = counter as u32;
        Ok((build_nonce(self.conn_id, counter), counter))
    }
}

/// `nonce = conn_id.to_be_bytes() ‖ counter.to_be_bytes()` (spec §5).
fn build_nonce(conn_id: ConnId, counter: u32) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[0..8].copy_from_slice(&conn_id.to_be_bytes());
    nonce[8..12].copy_from_slice(&counter.to_be_bytes());
    nonce
}

// ---------------------------------------------------------------------
// AEAD encryption/decryption helpers (spec §5.3, §6.4, §7.1)
// ---------------------------------------------------------------------

fn aead_encrypt(key: &[u8; 32], stream_id: StreamId, nonce: [u8; 12], plaintext: &[u8]) -> Result<Vec<u8>> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| anyhow!("invalid AES-256 key: {e}"))?;
    let nonce: Nonce<U12> = Nonce::from(nonce);
    let aad = [PROTOCOL_VERSION, stream_id];
    cipher
        .encrypt(&nonce, Payload { msg: plaintext, aad: &aad })
        .map_err(|e| anyhow!("AES-GCM encryption failed: {e}"))
}

fn aead_decrypt(key: &[u8; 32], stream_id: StreamId, nonce: [u8; 12], ciphertext: &[u8]) -> Result<Vec<u8>> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| anyhow!("invalid AES-256 key: {e}"))?;
    let nonce: Nonce<U12> = Nonce::from(nonce);
    let aad = [PROTOCOL_VERSION, stream_id];
    cipher
        .decrypt(&nonce, Payload { msg: ciphertext, aad: &aad })
        .map_err(|_| anyhow!("AES-GCM decryption failed: tampered ciphertext, wrong key, or AAD mismatch"))
}

/// Encrypt one whole media frame (spec §6.1/§6.4): prepends the 8-byte
/// big-endian PTS to `payload`, then AEAD-encrypts the result as a single
/// operation with AAD `[version, stream_id]`. The caller (a future
/// `capture.rs`) is responsible for drawing `nonce` from a
/// [`NonceSequence`] shared across everything the connection encrypts, and
/// for splitting the returned ciphertext into datagrams afterward via
/// [`crate::fragment::fragment_frame`] — never re-encrypt per fragment.
pub fn encrypt_frame(key: &[u8; 32], stream_id: StreamId, nonce: [u8; 12], pts: u64, payload: &[u8]) -> Result<Vec<u8>> {
    let mut plaintext = Vec::with_capacity(8 + payload.len());
    plaintext.extend_from_slice(&pts.to_be_bytes());
    plaintext.extend_from_slice(payload);
    aead_encrypt(key, stream_id, nonce, &plaintext)
}

/// Inverse of [`encrypt_frame`]: decrypts a fully-reassembled
/// `ciphertext_with_tag` and splits the 8-byte PTS back off the front.
pub fn decrypt_frame(key: &[u8; 32], stream_id: StreamId, nonce: [u8; 12], ciphertext: &[u8]) -> Result<(u64, Vec<u8>)> {
    let plaintext = aead_decrypt(key, stream_id, nonce, ciphertext)?;
    if plaintext.len() < 8 {
        bail!("decrypted frame shorter than the mandatory 8-byte PTS prefix");
    }
    let pts = u64::from_be_bytes(plaintext[0..8].try_into().unwrap());
    Ok((pts, plaintext[8..].to_vec()))
}

/// Encrypt a preview JPEG blob (spec §7.1). No PTS prefix — previews are
/// stills.
pub fn encrypt_preview(key: &[u8; 32], stream_id: StreamId, nonce: [u8; 12], jpeg: &[u8]) -> Result<Vec<u8>> {
    aead_encrypt(key, stream_id, nonce, jpeg)
}

/// Inverse of [`encrypt_preview`].
pub fn decrypt_preview(key: &[u8; 32], stream_id: StreamId, nonce: [u8; 12], ciphertext: &[u8]) -> Result<Vec<u8>> {
    aead_decrypt(key, stream_id, nonce, ciphertext)
}

// ---------------------------------------------------------------------
// Events delivered to the caller (ui.rs / future decode.rs)
// ---------------------------------------------------------------------

/// State captured from the `PresenceSnapshot` the relay sends immediately
/// after a successful `Join` (spec §8). Returned synchronously by
/// [`ScreenshareClient::connect`] so the caller doesn't have to race the
/// event channel for it.
#[derive(Debug, Clone)]
pub struct PresenceSnapshotState {
    pub members: Vec<Member>,
    pub streams: Vec<StreamInfo>,
}

/// A fully reassembled and decrypted incoming media frame.
#[derive(Debug, Clone)]
pub struct DecryptedMediaFrame {
    pub conn_id: ConnId,
    pub stream_id: StreamId,
    pub pts: u64,
    pub keyframe: bool,
    /// The sender flagged this frame as a stalled-capture re-send (see
    /// [`crate::protocol::MEDIA_FLAG_STALLED`]).
    ///
    /// `Why:` this comes from the media header's **cleartext, unauthenticated**
    /// `flags` byte — the AAD is only `[version, stream_id]` (see
    /// [`aead_encrypt`]), so unlike every other field on this struct it is not
    /// covered by the AEAD tag and an on-path attacker can flip it freely. It
    /// exists purely so `decode.rs` can hand `ui.rs` a "draw the waiting-for-
    /// capture placeholder" hint; nothing may branch on it that has a security
    /// or protocol consequence (no teardown, no unsubscribe, no key handling).
    pub stalled: bool,
    /// Raw codec bytes (one H.264 access unit, or one Opus packet).
    pub payload: Vec<u8>,
}

/// A decrypted incoming preview JPEG.
#[derive(Debug, Clone)]
pub struct DecryptedPreview {
    pub conn_id: ConnId,
    pub stream_id: StreamId,
    pub jpeg: Vec<u8>,
}

/// Everything that can arrive on a connected [`ScreenshareClient`] after the
/// initial `connect()` handshake, delivered via the `mpsc::UnboundedReceiver`
/// that `connect()` returns.
#[derive(Debug, Clone)]
pub enum ClientEvent {
    PresenceDelta(PresenceEvent),
    /// Sent only to the connection that owns `stream_id` (spec §9).
    SubscriberCount { stream_id: StreamId, count: u32 },
    /// Sent only to the connection that owns `stream_id` (spec §9).
    KeyframeRequest { stream_id: StreamId },
    MediaFrame(DecryptedMediaFrame),
    Preview(DecryptedPreview),
    /// The control stream ended (relay closed it, connection dropped, or we
    /// got rejected after already being joined). Carries a human-readable
    /// reason; no more events will follow.
    ControlClosed(String),
}

// ---------------------------------------------------------------------
// ScreenshareClient
// ---------------------------------------------------------------------

/// Cheap to clone: every field is itself a clonable handle (`quinn::Connection`
/// is refcounted, `nonce_seq`/`control_send` are `Arc`s, the rest are `Copy`),
/// so a clone is just another handle to the same underlying connection/control
/// stream — not a second connection. `capture.rs` clones this to hand an
/// owned handle to its background send task and to `CaptureHandle::stop()`
/// (which needs to call `stop_stream` after the caller's original borrow may
/// have gone out of scope).
#[derive(Clone)]
pub struct ScreenshareClient {
    connection: quinn::Connection,
    conn_id: ConnId,
    room_key: [u8; 32],
    nonce_seq: Arc<NonceSequence>,
    control_send: Arc<Mutex<quinn::SendStream>>,
}

impl ScreenshareClient {
    /// Connect to the relay named in `join_string`, send `Join`, and await
    /// the relay's `PresenceSnapshot`/`JoinRejected` response (retrying once
    /// per `ConnIdCollision`, per spec §3.1). On success, spawns the
    /// background tasks that keep reading the control stream, media
    /// datagrams, and preview uni-streams for the lifetime of the
    /// connection, forwarding everything through the returned channel.
    ///
    /// `endpoint` is the caller's single shared [`build_client_endpoint`]
    /// result — see the module docs' "One endpoint, N connections" section.
    /// Call this once per room; every resulting `ScreenshareClient` is an
    /// independent connection (own `ConnId`, own nonce space, own room key)
    /// riding the same local socket.
    pub async fn connect(
        endpoint: &quinn::Endpoint,
        join_string: &str,
        display_name: &str,
    ) -> Result<(Self, PresenceSnapshotState, mpsc::UnboundedReceiver<ClientEvent>)> {
        let parsed = parse_join_string(join_string)?;
        let remote_addr = resolve_server_addr(&parsed.server).await?;

        let host = parsed
            .server
            .rsplit_once(':')
            .map(|(h, _)| h)
            .unwrap_or(parsed.server.as_str());

        let connection = endpoint
            .connect(remote_addr, host)
            .context("failed to start QUIC connection")?
            .await
            .context("QUIC handshake failed")?;

        let (mut send, mut recv) = connection
            .open_bi()
            .await
            .context("failed to open control stream")?;

        let mut conn_id: ConnId = rand::random();
        let snapshot = loop {
            write_control_message(
                &mut send,
                &ClientToRelay::Join {
                    conn_id,
                    room_id: parsed.room_id,
                    display_name: display_name.to_string(),
                },
            )
            .await
            .context("failed to send Join")?;

            match read_control_message(&mut recv).await.context("failed to read join response")? {
                RelayToClient::PresenceSnapshot { members, streams } => {
                    break PresenceSnapshotState { members, streams };
                }
                RelayToClient::JoinRejected { reason: RejectReason::ConnIdCollision } => {
                    conn_id = rand::random();
                    continue;
                }
                RelayToClient::JoinRejected { reason } => {
                    bail!("join rejected: {reason:?}");
                }
                other => bail!("unexpected message before join acknowledgement: {other:?}"),
            }
        };

        let nonce_seq = Arc::new(NonceSequence::new(conn_id));
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        tokio::spawn(control_reader_loop(recv, event_tx.clone()));
        tokio::spawn(datagram_reader_loop(connection.clone(), parsed.room_key, event_tx.clone()));
        tokio::spawn(preview_uni_loop(connection.clone(), parsed.room_key, event_tx));

        let client = Self {
            connection,
            conn_id,
            room_key: parsed.room_key,
            nonce_seq,
            control_send: Arc::new(Mutex::new(send)),
        };

        Ok((client, snapshot, event_rx))
    }

    pub fn conn_id(&self) -> ConnId {
        self.conn_id
    }

    pub fn room_key(&self) -> [u8; 32] {
        self.room_key
    }

    /// Shared nonce sequence for this connection. A future `capture.rs`
    /// draws from this for every video/audio/preview encryption it does, per
    /// spec §5.1 — never construct a second, independent counter.
    pub fn nonce_sequence(&self) -> Arc<NonceSequence> {
        self.nonce_seq.clone()
    }

    /// The underlying QUIC connection, for a future `capture.rs` to send
    /// media datagrams / open preview uni-streams directly.
    pub fn connection(&self) -> quinn::Connection {
        self.connection.clone()
    }

    pub async fn announce_stream(
        &self,
        stream_id: StreamId,
        kind: StreamKind,
        label: &str,
        width: u16,
        height: u16,
    ) -> Result<()> {
        self.send_control(&ClientToRelay::AnnounceStream {
            stream_id,
            kind,
            label: label.to_string(),
            width,
            height,
        })
        .await
    }

    pub async fn stop_stream(&self, stream_id: StreamId) -> Result<()> {
        self.send_control(&ClientToRelay::StopStream { stream_id }).await
    }

    pub async fn subscribe(&self, target: ConnId, stream_id: StreamId) -> Result<()> {
        self.send_control(&ClientToRelay::Subscribe { target, stream_id }).await
    }

    pub async fn unsubscribe(&self, target: ConnId, stream_id: StreamId) -> Result<()> {
        self.send_control(&ClientToRelay::Unsubscribe { target, stream_id }).await
    }

    pub async fn leave(&self) -> Result<()> {
        self.send_control(&ClientToRelay::Leave).await
    }

    async fn send_control(&self, msg: &ClientToRelay) -> Result<()> {
        let mut send = self.control_send.lock().await;
        write_control_message(&mut send, msg).await
    }
}

async fn write_control_message(send: &mut quinn::SendStream, msg: &ClientToRelay) -> Result<()> {
    let framed = encode_control_frame(msg)?;
    send.write_all(&framed).await.context("control stream write failed")
}

async fn read_control_message(recv: &mut quinn::RecvStream) -> Result<RelayToClient> {
    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf)
        .await
        .context("control stream closed or read error while reading length prefix")?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_CONTROL_FRAME_LEN {
        bail!("control frame claims {len} bytes, exceeding the {MAX_CONTROL_FRAME_LEN}-byte cap");
    }
    let mut payload = vec![0u8; len as usize];
    recv.read_exact(&mut payload)
        .await
        .context("control stream closed or read error while reading payload")?;
    decode_control_payload(&payload)
}

async fn control_reader_loop(mut recv: quinn::RecvStream, tx: mpsc::UnboundedSender<ClientEvent>) {
    loop {
        match read_control_message(&mut recv).await {
            Ok(RelayToClient::PresenceDelta { event }) => {
                if tx.send(ClientEvent::PresenceDelta(event)).is_err() {
                    return;
                }
            }
            Ok(RelayToClient::SubscriberCount { stream_id, count }) => {
                if tx.send(ClientEvent::SubscriberCount { stream_id, count }).is_err() {
                    return;
                }
            }
            Ok(RelayToClient::KeyframeRequest { stream_id }) => {
                if tx.send(ClientEvent::KeyframeRequest { stream_id }).is_err() {
                    return;
                }
            }
            Ok(RelayToClient::JoinRejected { reason }) => {
                let _ = tx.send(ClientEvent::ControlClosed(format!(
                    "relay rejected connection after join: {reason:?}"
                )));
                return;
            }
            Ok(RelayToClient::PresenceSnapshot { .. }) => {
                tracing::warn!("received an unexpected second PresenceSnapshot; ignoring");
            }
            Err(e) => {
                let _ = tx.send(ClientEvent::ControlClosed(e.to_string()));
                return;
            }
        }
    }
}

async fn datagram_reader_loop(connection: quinn::Connection, room_key: [u8; 32], tx: mpsc::UnboundedSender<ClientEvent>) {
    let mut reassembler = Reassembler::new();
    loop {
        let datagram = match connection.read_datagram().await {
            Ok(d) => d,
            Err(e) => {
                tracing::debug!("media datagram loop ending: {e}");
                return;
            }
        };
        let (header, fragment_payload) = match MediaHeader::unpack(&datagram) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("dropping malformed media datagram: {e}");
                continue;
            }
        };
        let Some(ciphertext) = reassembler.insert(&header, fragment_payload) else {
            continue;
        };
        let nonce = build_nonce(header.conn_id, header.frame_id);
        match decrypt_frame(&room_key, header.stream_id, nonce, &ciphertext) {
            Ok((pts, payload)) => {
                let frame = DecryptedMediaFrame {
                    conn_id: header.conn_id,
                    stream_id: header.stream_id,
                    pts,
                    keyframe: header.is_keyframe(),
                    stalled: header.is_stalled(),
                    payload,
                };
                if tx.send(ClientEvent::MediaFrame(frame)).is_err() {
                    return;
                }
            }
            Err(e) => tracing::warn!("dropping media frame that failed to decrypt: {e}"),
        }
    }
}

async fn preview_uni_loop(connection: quinn::Connection, room_key: [u8; 32], tx: mpsc::UnboundedSender<ClientEvent>) {
    loop {
        let recv = match connection.accept_uni().await {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!("preview uni-stream accept loop ending: {e}");
                return;
            }
        };
        tokio::spawn(handle_preview_stream(recv, room_key, tx.clone()));
    }
}

async fn handle_preview_stream(mut recv: quinn::RecvStream, room_key: [u8; 32], tx: mpsc::UnboundedSender<ClientEvent>) {
    // ~30s JPEG previews; 4 MiB is a generous ceiling well above anything a
    // sane preview snapshot should ever be.
    let bytes = match recv.read_to_end(4 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("failed to read preview uni-stream: {e}");
            return;
        }
    };
    let (header, ciphertext) = match PreviewHeader::unpack(&bytes) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("dropping malformed preview stream: {e}");
            return;
        }
    };
    if header.stream_type != STREAM_TYPE_PREVIEW {
        tracing::warn!("uni-stream had unexpected stream_type {:#x}; ignoring", header.stream_type);
        return;
    }
    let nonce = build_nonce(header.conn_id, header.counter);
    match decrypt_preview(&room_key, header.stream_id, nonce, ciphertext) {
        Ok(jpeg) => {
            let _ = tx.send(ClientEvent::Preview(DecryptedPreview {
                conn_id: header.conn_id,
                stream_id: header.stream_id,
                jpeg,
            }));
        }
        Err(e) => tracing::warn!("dropping preview that failed to decrypt: {e}"),
    }
}

async fn resolve_server_addr(server: &str) -> Result<SocketAddr> {
    tokio::net::lookup_host(server)
        .await
        .with_context(|| format!("could not resolve server address {server:?}"))?
        .next()
        .ok_or_else(|| anyhow!("server address {server:?} resolved to no addresses"))
}

/// Build the one local QUIC endpoint every [`ScreenshareClient::connect`]
/// call shares. See the module docs' "One endpoint, N connections".
///
/// Must be called with a Tokio runtime entered (quinn's `runtime-tokio`
/// feature looks the reactor up at bind time) — both front-ends build this
/// from inside `main.rs`'s entered runtime.
pub fn build_client_endpoint() -> Result<quinn::Endpoint> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut crypto = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .context("failed to configure TLS protocol versions")?
        .dangerous()
        .with_custom_certificate_verifier(SkipServerVerification::new(provider))
        .with_no_client_auth();
    crypto.alpn_protocols = vec![ALPN.to_vec()];

    let quic_crypto = quinn::crypto::rustls::QuicClientConfig::try_from(crypto)
        .context("failed to build QUIC TLS config")?;
    let mut client_config = quinn::ClientConfig::new(Arc::new(quic_crypto));

    // Match the server's idle-timeout/keep-alive settings (net.rs). quinn
    // uses the minimum of both peers' max_idle_timeout, and keep-alives
    // only prevent idle disconnects if configured on at least one side,
    // so both client and server set the same values here to avoid drops
    // during manual-testing pauses (dialogs, UI clicking, etc).
    let mut transport_config = quinn::TransportConfig::default();
    transport_config
        .max_idle_timeout(Some(
            quinn::IdleTimeout::try_from(std::time::Duration::from_secs(60))
                .expect("60s fits in IdleTimeout"),
        ))
        .keep_alive_interval(Some(std::time::Duration::from_secs(15)));
    client_config.transport_config(Arc::new(transport_config));

    let mut endpoint = quinn::Endpoint::client("[::]:0".parse().unwrap()).context("failed to bind local UDP socket")?;
    endpoint.set_default_client_config(client_config);
    Ok(endpoint)
}

/// Accepts any server certificate without validation.
///
/// See the module-level "Certificate verification" section: this is a
/// deliberate, spec-acknowledged gap for the native-only phase (§11 defers
/// real cert pinning to a later join-link field), not an oversight. It does
/// NOT weaken the room-key AEAD encryption applied to actual media/preview
/// payloads (§5) — those stay confidential and authenticated end to end
/// regardless of QUIC transport trust.
#[derive(Debug)]
struct SkipServerVerification(Arc<rustls::crypto::CryptoProvider>);

impl SkipServerVerification {
    fn new(provider: Arc<rustls::crypto::CryptoProvider>) -> Arc<Self> {
        Arc::new(Self(provider))
    }
}

impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> [u8; 32] {
        [0x42u8; 32]
    }

    #[test]
    fn join_string_round_trip() {
        let room_id: RoomId = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        let key = test_key();
        let joined = format_join_string("relay.example.com:4433", &room_id, &key);
        let parsed = parse_join_string(&joined).unwrap();
        assert_eq!(parsed.server, "relay.example.com:4433");
        assert_eq!(parsed.room_id, room_id);
        assert_eq!(parsed.room_key, key);
    }

    #[test]
    fn join_string_rejects_wrong_field_count() {
        assert!(parse_join_string("only-one-field").is_err());
        assert!(parse_join_string("server|only-two-fields").is_err());
    }

    #[test]
    fn nonce_sequence_increments_and_matches_layout() {
        let seq = NonceSequence::new(0x0102_0304_0506_0708);
        let (nonce0, c0) = seq.next_nonce(0).unwrap();
        let (nonce1, c1) = seq.next_nonce(5).unwrap();
        assert_eq!(c0, 0);
        assert_eq!(c1, 1);
        assert_eq!(&nonce0[0..8], &0x0102_0304_0506_0708u64.to_be_bytes());
        assert_eq!(&nonce0[8..12], &0u32.to_be_bytes());
        assert_eq!(&nonce1[8..12], &1u32.to_be_bytes());
        // stream_id must NOT affect the nonce (spec §5.1: single shared counter).
        assert_eq!(&nonce0[0..8], &nonce1[0..8]);
    }

    #[test]
    fn nonce_sequence_detects_exhaustion() {
        let seq = NonceSequence {
            conn_id: 1,
            next: AtomicU64::new(u32::MAX as u64),
        };
        let (_, last) = seq.next_nonce(0).unwrap();
        assert_eq!(last, u32::MAX);
        assert!(seq.next_nonce(0).is_err());
    }

    #[test]
    fn encrypt_then_decrypt_frame_round_trips() {
        let key = test_key();
        let nonce = build_nonce(0xAABB_CCDD_EEFF_0011, 42);
        let payload = b"fake h264 access unit bytes";
        let ciphertext = encrypt_frame(&key, 3, nonce, 123_456_789, payload).unwrap();
        let (pts, decrypted) = decrypt_frame(&key, 3, nonce, &ciphertext).unwrap();
        assert_eq!(pts, 123_456_789);
        assert_eq!(decrypted, payload);
    }

    #[test]
    fn decrypt_frame_rejects_tampered_ciphertext() {
        let key = test_key();
        let nonce = build_nonce(1, 1);
        let mut ciphertext = encrypt_frame(&key, 0, nonce, 1, b"hello").unwrap();
        let last = ciphertext.len() - 1;
        ciphertext[last] ^= 0xFF; // flip a bit in the GCM tag
        assert!(decrypt_frame(&key, 0, nonce, &ciphertext).is_err());
    }

    #[test]
    fn decrypt_frame_rejects_wrong_stream_id_aad() {
        let key = test_key();
        let nonce = build_nonce(1, 1);
        let ciphertext = encrypt_frame(&key, 5, nonce, 1, b"hello").unwrap();
        // Decrypting with a different stream_id changes the AAD -> must fail.
        assert!(decrypt_frame(&key, 6, nonce, &ciphertext).is_err());
    }

    #[test]
    fn decrypt_frame_rejects_wrong_key() {
        let key = test_key();
        let mut wrong_key = key;
        wrong_key[0] ^= 0xFF;
        let nonce = build_nonce(1, 1);
        let ciphertext = encrypt_frame(&key, 0, nonce, 1, b"hello").unwrap();
        assert!(decrypt_frame(&wrong_key, 0, nonce, &ciphertext).is_err());
    }

    #[test]
    fn encrypt_then_decrypt_preview_round_trips() {
        let key = test_key();
        let nonce = build_nonce(9, 2);
        let jpeg = b"\xff\xd8\xff\xe0 pretend jpeg bytes \xff\xd9";
        let ciphertext = encrypt_preview(&key, 4, nonce, jpeg).unwrap();
        let decrypted = decrypt_preview(&key, 4, nonce, &ciphertext).unwrap();
        assert_eq!(decrypted, jpeg);
    }
}
