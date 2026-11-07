# WebTransport Leg and the Rotating Pinned Certificate: Decisions

Split out of `implementation-notes.md` on 2026-11-07. The binding spec is `protocol-spec.md` §11; this page keeps what was decided while building it and why.

## WebTransport leg and the rotating pinned certificate (2026-10-14)

Spec: `protocol-spec.md` §11. What was decided while building it, and why:

- **`web-transport-quinn` 0.13 (kixelated/moq-dev), not `wtransport` or `h3` +
  `h3-webtransport`.** It runs on a `quinn::Connection` the relay's own endpoint already
  accepted (quinn 0.11, rustls/ring — `default-features = false, features = ["ring"]` to
  avoid dragging in aws-lc), so ALPN dispatch is one `match` after the handshake and
  `Endpoint::set_server_config` rotation keeps working. `wtransport` wants to own its
  endpoint (cannot share the port with the native ALPN); the `h3` route means hand-assembling
  the session plumbing. Its `Session::raw(conn)` ("pretend a QUIC connection is a WebTransport
  session so raw QUIC and WebTransport are supported simultaneously") made the planned
  transport trait unnecessary: **both transports are the one `Session` type** from
  `accept_session` on, and `ConnState`/`RouteTarget` simply hold a `Session`.
- **Datagram cap 1000, measured.** The first guess (1100, derived from QUIC MTU arithmetic)
  was wrong for browsers: headless Chromium 153 reports `datagrams.maxDatagramSize === 1024`
  against this relay while quinn would allow ~1162 natively. Since the relay cannot re-fragment
  (no key), the cap is applied by every sender in `fragment.rs`
  (`MAX_MEDIA_DATAGRAM_LEN` in the protocol crate).
- **Cert lifecycle.** 13-day ECDSA P-256 (browsers: ≤ 14 days, P-256 only), `notBefore`
  back-dated by 1 h *inside* the 13 days; rotation at < 2 days left; persisted to
  `state/` (gitignored, key file 0600) so a restart keeps the hash. A companion `cert.meta` file
  stores `notAfter` and the SAN list because reading `notAfter` back out of the PEM would need
  an X.509 parser dependency; missing/mismatched meta simply regenerates. Test hooks:
  `--cert-validity-secs` / `--cert-rotate-before-secs`, and `CertSource::SelfSigned`'s fields.
- **Hash encoding: base64url, no padding** — shortest, and the same codec as the key field,
  so the website decodes both with one helper. 4th, optional join-link field.
- **Provided-cert reload and the browser's plain fallback (2026-11-03).** `CertManager::poll_provided`
  advances its remembered mtime only after a *successful* reload, and `reload_provided` proves the new
  material builds a `quinn::ServerConfig` before it replaces `current`. *Why:* the mtime used to be
  remembered first, so a reload that failed once (cert and key briefly out of sync while being
  distributed) was never retried until the files changed again, and `current` could hold a cert/key pair
  that does not match while the endpoint kept serving the old config. Browser side: `RelaySession.open`
  retries without `serverCertificateHashes` after a failed pinned attempt, because a CA relay's
  certificate can never match a pin (see `protocol-spec.md` §11.4, `docs/js/core/README.md`).
  Native `client/src/net.rs` was deliberately left untouched.
- **Native client does not enforce the link's hash.** Rotation every ~11 days would turn every
  saved link into a connect failure. It logs the mismatch and records the presented hash for
  `current_cert_hash()`. Known limitation: a native connection that outlives a rotation keeps
  reporting the old hash until it reconnects (no protocol message announces rotation).
- **Relay cleanup done in passing:** `server/src/protocol.rs` no longer wraps
  `MediaHeader`/`PreviewHeader` — the relay uses the shared crate's types directly.
- **Verification gap noticed, since closed:** `end_to_end_test.rs::server_repo_candidates` looked for the
  relay binary under the old multi-repo layout (the old per-repo `simple-screensharing-server/target/...`), so
  its `--ignored` tests could not find the monorepo's `target/` binary. It no longer exists: `server_binary_path`
  takes `SSSHARE_SERVER_BIN` if set, otherwise the newer of `release`/`debug` under `server_target_dir`
  (`CARGO_TARGET_DIR`, else `<workspace>/target`), and fails with a "build it first" error.
