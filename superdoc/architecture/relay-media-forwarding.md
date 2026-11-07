# Relay Media Forwarding: Why the Data Plane Avoids the Room Lock

Split out of `implementation-notes.md` on 2026-11-07 (section "Performance fixes"). Code: `server/src/relay.rs` `media_forward_task`, `server/src/room.rs` `StreamRoute`.

## Performance fixes

- **One high-refresh-rate sharer used to slow down everyone else in the room.** Root cause
  was lock *volume*, not lock duration: media forwarding ran inline in the connection
  actor's `select!` and re-acquired the **room-wide** `tokio::sync::Mutex` once per QUIC
  **fragment**. Since a frame is split into ~1200-byte datagrams (spec §6.3), a 60 fps
  sharer takes that room-wide lock hundreds of times a second — serializing every other
  member's control messages and every other sharer's media behind one busy stream. The
  fan-out work itself was never the problem (`quinn::Connection::send_datagram` is
  non-blocking). Fixed in two halves: (1) each connection got a **media forwarder task**,
  so the actor just validates a header and `try_send`s onto a bounded queue and returns
  immediately; (2) each stream got a `StreamRoute` — an immutable `Arc<[RouteTarget]>`
  fan-out snapshot behind a `std::sync::RwLock`, republished under the room mutex on every
  subscriber change (`RoomState::refresh_route`) and read *without any room lock* per
  fragment. `StreamState::subscribers` stays authoritative; the route is a derived cache.
  See `server/src/room.rs`'s module docs for the lock-ordering rules
  (the route lock is a strict leaf) and `src/relay.rs`'s `media_forward_task`.
- **Why the media queue is bounded and lossy (depth 512, drop-on-full).** The old design
  was lossless but room-stalling; the new one is lossy but isolated. This is safe because
  media is unreliable by construction all the way down: the spec's media plane is
  drop-and-continue with no retransmit (§6.2/§6.5), quinn's own `send_datagram` already
  silently drops the oldest queued datagram when a connection's send buffer overflows, and
  the client's `Reassembler` (`client/src/fragment.rs`) discards an
  incomplete frame as soon as a newer `frame_id` arrives, with a 200 ms backstop. A
  relay-side drop is therefore indistinguishable to the receiver from a drop in transit —
  same missing fragment, same discarded frame, no confusion and no double-counting. An
  unbounded queue would instead grow without limit and add latency to every later fragment.
  Regression coverage: `relay::tests::media_forwarding_follows_subscription_changes` and
  `relay::tests::subscription_churn_under_media_load_keeps_routes_consistent` (the latter
  runs on a multi-threaded runtime so the room mutex and the route lock are genuinely
  contended, and asserts the route snapshot never diverges from the subscriber set).
