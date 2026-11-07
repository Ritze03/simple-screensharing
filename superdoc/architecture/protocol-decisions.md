# Protocol Decisions: Where Writing the Spec Changed the Design

Split out of `implementation-notes.md` on 2026-11-07. The wire itself is `protocol-spec.md`; this page keeps the reasoning behind choices that read differently from the original brainstorm.

## Protocol refinements (settled during protocol-spec design)

These aren't bugs — they're places where writing the precise wire spec (needed because the
brainstorm left the nonce/counter discipline underspecified, see the brainstorm's "Final
Verify Results" section) produced a cleaner design than "implement the brainstorm literally"
would have.

- **`ConnId` and the AEAD `client_id` are one 64-bit id, not two.** The brainstorm treated
  "the relay's routing/presence identifier for a connection" and "the AES-GCM nonce prefix"
  as separate concepts. `protocol-spec.md` §3.1 unifies them: a client picks one fresh-random
  `u64` per connection, presented in `Join`, and it does double duty as the relay's `ConnId`
  *and* the nonce's `client_id` prefix. Why: the nonce is transmitted in cleartext anyway
  (nonces are public by design), so there's no confidentiality cost to reusing it as the
  public routing handle, and it removes an entire class of "these two ids drifted out of
  sync" bugs. See `server/src/protocol.rs` (`pub type ConnId = u64`)
  and the identical type in `client/src/protocol.rs`.
- **`frame_id` in the media datagram header is the sender's nonce counter value, not a
  separate counter.** Per spec §6.2/§6.5, each frame is encrypted once with
  `nonce = conn_id ‖ counter`, and that same `counter` is written into the header as
  `frame_id`. This gives fragment-grouping ("which fragments belong to the same frame") and
  nonce reconstruction for free from one field, instead of maintaining two counters that
  would need to stay in lockstep. See `client/src/fragment.rs`
  (`fragment_frame`'s `frame_id: u32` parameter) and `net.rs`'s decrypt path.
- **A 2-byte AAD (`version ‖ stream_id`) authenticates stream identity.** The nonce already
  binds `conn_id` and `counter`, but not `stream_id` (a sender's multiple streams — e.g. two
  monitors, or video + audio — share the same nonce space). Without AAD, a relay could
  relabel a fragment's `stream_id` in the cleartext header and the receiver would decrypt it
  successfully under the wrong stream. Binding `stream_id` (and `version`, for good measure)
  as AAD makes that undetectable relabeling impossible — a mismatch fails AEAD
  authentication outright. Spec §5.3; implemented in both `net.rs`'s `encrypt_frame`/
  `decrypt_frame` and mirrored in `preview.rs`'s `encrypt_preview`.
- **An 8-byte authenticated PTS is prepended to frame plaintext.** The brainstorm explicitly
  called out (§"Final Verify Results") that no A/V-sync timestamp scheme existed and flagged
  it as worth adding, even though "glitches are fine" made it non-blocking. The spec resolved
  it: a `u64` big-endian capture timestamp (microseconds) is prepended to the codec payload
  *before* encryption, so it's authenticated by the same AEAD tag as the frame data — a relay
  can't forge or strip it without breaking authentication. See spec §6.4 and
  `client/src/decode.rs`, which strips the PTS after decrypt.
- **Keyframes are requested on every new subscriber, not just the 0→1 transition**, with a
  ~100ms server-side debounce per stream to collapse a burst of near-simultaneous
  `Subscribe`s into one `KeyframeRequest`. The brainstorm's "SUBSCRIBE-triggered forced
  keyframe" language reads as viewer-count-triggered (0→1); the actual relay logic sends one
  on *every* successful new subscription (a second viewer joining also gets a fresh keyframe
  immediately, rather than waiting for the encoder's normal 2-second GOP boundary), simply
  debounced so a thundering herd of subscribers doesn't cause an IDR burst. See
  `server/src/relay.rs`, `KEYFRAME_REQUEST_DEBOUNCE` (100ms) and the
  `Subscribe` handler's `newly_added`/`send_keyframe` logic.
- **`postcard`, not `bincode`, is the control-message wire encoding — decided mid-scaffold.**
  Both repos were scaffolded in parallel before the protocol spec was written. The client's
  initial scaffold commit pinned `bincode 2.0.1` (bincode 3.0.0 on crates.io is a maintainer
  tombstone — an intentional compile error pointing at the unmaintained crate); the server's
  parallel scaffold had already picked `postcard`. The two were reconciled onto `postcard`
  once the protocol spec settled the wire format (spec §2), and both `Cargo.toml`s now agree
  on `postcard = "1.1.3"`.
