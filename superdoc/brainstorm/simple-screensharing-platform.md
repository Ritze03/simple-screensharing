# Simple Screensharing Platform — Brainstorm & Design Plan

**Session metadata:**

- Command: `/teamlead brainstorm 5 3` (5 independent agents per round, 3 rounds, +1 final verify round)
- Worker model for brainstorm rounds: Sonnet 5 (High effort)
- Final verify round model: Opus 4.8 (High effort)
- Lenses used every round (each agent's *starting* angle, but all agents ranged across the whole topic): Protocol & Networking Architecture, Media Pipeline & Performance, Cross-Platform Desktop Client, Security & Privacy, Web Viewer & Ops
- Reference repos consulted throughout: `~/Documents/Programming/ScreegoElectronScreenshare/` (PipeWire audio-capture reference) and `~/Documents/Programming/CloudflaredScreenshare/` (room/join-string logic reference; its screensharing "didn't work properly" per the project owner — root-caused during the brainstorm to software-only x264 encoding + Cloudflare quick-tunnel fragility + TCP/WebSocket head-of-line blocking, NOT its crypto/relay design, which was sound and partly reused as inspiration)
- Status at time of writing: Round 1, 2, and 3 complete and fully answered by the project owner. A final Opus verify pass was dispatched to check consistency/gaps across the whole design; **results were still pending when this document was first saved** — whoever reads this should check with the project owner or the session log for whether the verify pass surfaced any gaps that were subsequently resolved, and update this document's "Final Verify Results" section accordingly once available (currently marked PENDING below).

## Original Brief (verbatim intent from the project owner)

A super simple screensharing platform for use between friends. Two reference projects were provided: `~/Documents/Programming/ScreegoElectronScreenshare/` for audio capturing technique, and `~/Documents/Programming/CloudflaredScreenshare/` for room creation and join-string logic (noted as having broken screensharing logic). The platform needed: a GitHub Pages-hosted, bundled-JS, VIEW-ONLY browser client in a separate repo (`~/GitHubProjects/simple-screensharing-website/`); a Rust server in `~/Documents/Programming/simple-screensharing-server`; a Rust+egui desktop client supporting Windows (blind/untested by owner), Linux Wayland (with PipeWire audio), Linux X11 (with PipeWire audio — later dropped, see Round 2), bitrate selection, audio source selection, GPU-accelerated encoding with CPU fallback. All repos private on GitHub for now. Streaming target: up to 240Hz for desktop clients (browser can be capped ~60Hz, acceptable for now). Join/share link format (fixed from the start): `<SERVER(IP/Domain)>|<ROOM-ID>|<SYMMETRICAL-ENCRYPTION-KEY>`. Server: simple broadcast-relay rooms, UUID per joining client for connect/disconnect tracking, single port, ratatui admin status TUI (bandwidth, rooms, users). Owner explicitly noted "everything here, especially the server and technical details, are subject to change" — the load-bearing priorities were: high refresh rate, audio support, single-port server, Wayland+PipeWire support, and a working GitHub Pages browser viewer.

## Round 1 — Initial Exploration

Five agents (one per lens above) read the two reference repos and proposed initial architecture directions. Key findings and recommendations that shaped the rest of the brainstorm:

- CloudflaredScreenshare's relay/crypto design (content-blind broadcast relay, client-side AES-256-GCM, `client_id‖counter` nonces) was sound and worth reusing as a pattern; its failure was specifically in capture pipeline immaturity (HW encoder arms were unfinished `todo!()` stubs, forcing CPU-bound software x264 encoding) and Cloudflare Quick Tunnel fragility/ephemeral-DNS issues — not the crypto or relay-routing design.
- Because the new design uses a real self-hosted relay (not a P2P/tunnel setup), NAT traversal is a non-issue — clients only ever make outbound connections to a known server, no STUN/TURN/hole-punching needed.
- TCP/WebSocket (used by Cloudflared) has head-of-line-blocking that's a poor fit for real-time video where a stale frame should be dropped, not retransmitted — this became a recurring theme favoring UDP-based transport.
- H.264 was flagged early as the safest codec choice (universal hardware encode coverage + universal browser WebCodecs decode support), versus AV1 (efficiency gains not worth the risk of falling back to software encoding on older friend hardware) or HEVC (weak/inconsistent browser support).
- The PipeWire audio-tee pattern from ScreegoElectronScreenshare (`pw-loopback` + `pw-link`, additive/non-destructive per-application audio capture) was confirmed as directly portable to the new Rust client and works identically on Wayland and X11 since it operates at the PipeWire-graph level, not the compositor level.
- Security review confirmed the fixed join-link format (symmetric key embedded in a shareable link) was an acceptable design for a friends-only tool, recommended the relay be genuinely content-blind (never decrypt, E2E only), and flagged that room-IDs should be high-entropy since knowing a room-ID alone (without the key) lets a stranger connect to the relay and consume bandwidth/appear in presence, even though they couldn't decrypt anything.
- Web/ops review confirmed a browser viewer could realistically connect via WebSocket + WebCodecs decode (as CloudflaredScreenshare's browser code already partially proved), and flagged that GitHub Pages' HTTPS-only hosting would block any plaintext `ws://` connection to the relay (mixed-content restriction) — an early version of the TLS/domain problem that Round 2 fully resolved via WebTransport + self-signed cert hash-pinning instead of WebSocket + real TLS/domain.

### Round 1 Q&A (17 distilled questions asked, all answered by the project owner)

1. **Server hosting** → Home lab, 600 Mbit down / 400 Mbit up.
2. **Transport for v1** → Owner wants UDP (not WebSocket/TCP).
3. **LAN direct-connect fast path** → Rejected — relay-always is a hard rule, no exceptions even for same-LAN peers.
4. **GStreamer dependency** → Accepted ("that's fine").
5. **240Hz — motion vs static content, and real bandwidth** → Primarily for gaming/fast-motion content; framed as "more of a nice to have, that we can just unlock." (Home connection: 600 down / 400 up, per Q1.)
6. **Room size** → At most 8 concurrent participants.
7. **Audio scope** → Per-application picker, no microphone capture. Viewers get 0-200% volume control.
8. **Bitrate control** → Adaptive-to-content "maybe, if not too big a hassle" — otherwise user-selectable, up to 40 Mbit/s ceiling.
9. **X11 support** → Dropped entirely ("I won't test it and none of my friends uses it").
10. **Relay trust model (server sees plaintext?)** → No — the server should never know what's being streamed (true E2E).
11. **Room-ID generation & multi-stream** → Client-generated, and the client should locally SAVE multiple rooms so users have reusable "private lounges." A single room can contain MULTIPLE simultaneous streams; viewers select which stream(s) to watch rather than receiving all of them.
12. **Room revocation** → Not needed — rooms are purely on-demand/ephemeral, created when someone joins; "stop sharing" is sufficient.
13. **Admin TUI usernames** → Yes, show usernames (overriding an initial suggestion to keep the admin view anonymized to UUID-only).
14. **TLS/domain** → No TLS — "super simple," symmetric encryption alone is considered sufficient. (This set up the Round 2 tension that WebTransport/browser support still requires QUIC's inherent transport encryption, resolved via self-signed hash-pinned certs rather than a real domain/CA.)
15. **Codec** → Pick one codec that also works in browsers; sharing must support GPU acceleration with CPU fallback.
16. **Reuse CloudflaredScreenshare's browser viewer code?** → No — rewrite it from scratch (though its WebCodecs decode pattern was noted as a useful reference).
17. **Future scope: browser sharing** → Not now, "could be added in the future."

Additional owner-specified requirements added at the end of Round 1: the desktop client should support "popping out" a stream into its own window; popped-out windows should be borderless (for fullscreen use); and there should be an F11 fullscreen keybind (especially for Windows users).

## Round 2 — Resolving the Core Tensions

The headline tension flagged going into Round 2: the owner wanted UDP transport with no TLS, but **browsers cannot open raw UDP sockets at all** — the only browser-accessible UDP-like transport is WebTransport (built on HTTP/3/QUIC), and QUIC is *always* encrypted at the protocol level (not optional). The group's resolution, which held for the rest of the brainstorm:

- **Use QUIC (via the Rust `quinn` library) for native clients AND WebTransport for the browser — one single protocol/relay implementation, not two.** This actually satisfies the "avoid TCP head-of-line-blocking" goal better than raw UDP would have, since QUIC streams are independently ordered.
- **Avoid a real domain/CA entirely** by using WebTransport's `serverCertificateHashes` self-signed-certificate mode: the browser trusts the relay's cert if its hash matches a value supplied out-of-band, with no Certificate Authority or Let's Encrypt involved. Chrome enforces a ≤14-day validity window on such certs, so the relay auto-rotates its cert roughly every 10-12 days.
- **The cert hash is delivered via the join link itself** as a 4th field for browser links only: `server|room|key|certhash`. Native links stay 3 fields (`server|room|key`) since native `quinn` clients don't perform any certificate validation at all — they rely purely on the E2E AES-GCM payload encryption for security, making native links immune to cert rotation entirely.
- **The room KEY itself never changes/rotates** — only a browser join-link's embedded cert-hash can go stale if the link sits unused across a rotation cycle. Native "lounge" links saved by the desktop client don't carry a cert hash at all, so they remain valid indefinitely as long as the server address is still reachable.
- Multi-stream rooms: the relay does **selective per-subscriber forwarding** (a viewer must explicitly SUBSCRIBE to a given sharer's stream; the relay only forwards data for streams someone is actually watching), keeping bandwidth proportional to actual viewership rather than broadcasting every stream to everyone.
- Media rides **unreliable QUIC datagrams** (drop-and-continue on loss, no retransmission — confirmed acceptable by the owner: "brief glitches are fine"); control/presence messages ride reliable QUIC streams.
- Codec settled: **H.264 only for launch** (universal hardware encode coverage across "friends' random hardware" vs. AV1's spotty HW-encode support risking a CPU-bound fallback — the same failure mode that broke CloudflaredScreenshare); **Opus** for audio. Bitrate "adaptive to content" is satisfied cheaply via **encoder-internal rate control** (CBR/VBV-constrained mode reacting to scene complexity automatically) rather than building real network-congestion-adaptive bitrate renegotiation, which the owner agreed was the right level of effort ("yep, that's smart").
- **Idle-stream optimization**: when a stream has 0 subscribers, the sender keeps capturing but pauses/reduces encoding, resuming with an instant forced keyframe when someone subscribes — also confirmed smart by the owner.
- Client identity: **client_id is fresh-random per CONNECTION, never persisted per device** — confirmed explicitly by the owner ("Yeah, it should always be fresh... especially since usernames don't have to be unique"), meaning there is no cross-reconnect identity system at all, just ephemeral per-session UUIDs plus self-reported display names.
- The client should show live usernames in its own in-session UI (not just the server's admin TUI), so users can visually notice an unexpected/uninvited guest themselves — confirmed by the owner.
- A "regenerate this lounge" one-click key-rotation feature was proposed as a cheap mitigation for a leaked room link (since there's no formal revocation mechanism) — the owner said **not needed**.

### Round 2 Q&A (13 distilled questions, all answered)

1. Cert-hash delivery via the join link's 4th field, refreshed live by the desktop host at link-creation time → **Confirmed fine.**
2. Offline link generation needed? → **No.**
3. UUID/reconnect: fresh-per-connection identity vs. stable per-device identity → **Always fresh, that's what I meant — especially since usernames don't have to be unique.**
4. Codec: H.264-only for launch, AV1 deferred → **Yes, fine.**
5. Cert rotation: automatic vs. manual/TUI-triggered → **(See Round 3 — owner later clarified certs shouldn't need rotation from a security standpoint; the technical need for periodic rotation is a browser-enforced constraint, not a design choice — see "Cert rotation clarification" note below.)**
6. Idle streams: pause encoding but keep capturing when 0 viewers → **Yep, that's smart.**
7. In-session client UI shows live usernames → **Yes, of course.**
8. "Regenerate this lounge" feature → **Not needed.**
9. Pop-out windows: independent per-viewport refresh pacing (240Hz pop-out not throttled by a slower main window) → **Nope [i.e., don't ship synced-repaint-first] — use per-viewport refresh pacing** [confirmed as a hard requirement].
10. Stream picker: thumbnails → **The server could send a single frame every 30 seconds, so clients not actively watching can have previews/thumbnails. Or do it differently if there's a better way.** (This became the preview/thumbnail feature designed in Round 3.)
11. Cap on concurrent sharers per room → **No cap for now.**
12. Packet loss handling → **Brief glitches are fine.**
13. Time-to-first-frame / GOP sizing → **Be sane** (resolved in Round 3 as a 2-second fixed GOP interval plus a SUBSCRIBE-triggered forced keyframe).

**Cert rotation clarification (important, resolved via direct owner Q&A after Round 2):** The owner initially said "Certs shouldn't need to be rotated... it shouldn't be super secure, it's enough when users are able to share this key once" — referring to the ROOM KEY, which indeed never rotates by design. However, the relay's self-signed TLS certificate (used only for the WebTransport/browser leg) DOES still need to auto-rotate roughly every 10-12 days purely because browsers enforce a hard ~14-day validity cap on self-signed WebTransport certificates — this is a browser API constraint, not a project design choice, and it does not affect the room key, native links, or native clients in any way (native `quinn` clients don't validate the relay's certificate at all). This was explained directly to the owner and confirmed understood.

## Round 3 — Closing Remaining Gaps

Five agents did a final substantive pass, resolving the last open design items:

- **Preview/thumbnail mechanism (final design):** Since the relay is content-blind and cannot decode encrypted media, the SENDER (not the relay) produces a second, independent output: every ~30 seconds, it grabs a raw frame straight from its own capture buffer (fully decoupled from the main H.264 encoder's state, so this keeps working even while the main stream's encoder is paused due to 0 subscribers), downscales it (~320px wide), encodes it as a standalone JPEG (~10-30KB), encrypts it with the same room key, and sends it over a **reliable** QUIC stream (unlike main media, a dropped preview means a 30-second-stale thumbnail rather than a momentary glitch, so reliability matters here). Previews are **broadcast to all room members regardless of subscription state** — no SUBSCRIBE needed to receive them, since they're the discovery mechanism that gets someone to subscribe to a stream in the first place. Cost is negligible (~5KB/s worst case across 8 simultaneous sharers). Browser decode of previews is trivial — plain `createImageBitmap()`, no WebCodecs needed, a completely separate and much simpler code path from the main video decoder.
- **Keyframe/GOP interval:** settled at a fixed **2-second** interval (a "self-heal" ceiling bounding how long a decode glitch from a lost datagram can visibly persist, given no retransmission), plus a **SUBSCRIBE-triggered forced keyframe request** so a newly joining viewer gets video immediately rather than waiting up to 2 seconds.
- **Join-proof (cryptographic room-admission hardening):** explicitly evaluated and **skipped** — the security-lens analysis concluded the whole design consistently substitutes *social/human verification* for *cryptographic enforcement* (ephemeral rooms self-expire instead of needing revocation; visible presence/username lists let people self-police for strangers instead of needing a challenge-response join-proof), and this is judged the correct, deliberate philosophy for a friends-only hobby tool rather than a gap.
- **Windows packaging:** recommended building natively via a **GitHub Actions `windows-latest` CI runner** with the MSVC toolchain, rather than cross-compiling from Linux — GStreamer's Windows DLL-bundling story is fragile enough that debugging it blind (the owner cannot test Windows personally) would be painful. Recommendation: get a minimal "window opens" CI build passing and friend-tested early, well before the full feature set is complete.
- **Stream picker UI:** a grid of tiles (not a list), each showing the live-refreshing preview thumbnail (crossfade on update, "updated Ns ago" indicator) plus a username overlay. Non-unique username collisions (evaluated only among currently active tiles) get a short disambiguating suffix appended automatically. Single-click views inline; a corner icon/double-click/right-click opens a pop-out window.
- **Main UI viewing mode:** confirmed by the owner to support displaying **multiple streams simultaneously inline** (mosaic-style), in addition to the separate pop-out-to-new-window option — both are required, not alternatives.
- **Multi-monitor / multi-stream-per-sender:** confirmed in scope by the owner ("It would be cool, implement it, if it isn't too big of a hassle") — flagged risk: NVENC caps concurrent hardware encode sessions per GPU (historically ~3); additional streams beyond that cap on one machine should degrade to CPU encoding.
- **Background preview caching:** confirmed by the owner to keep receiving/caching previews even when the stream-picker view isn't actively open, "if it's not too hard."
- **GitHub Pages + private repo:** GitHub Pages hosting from a private repository requires a paid GitHub Pro plan (~$4/month) — the Free tier only serves Pages from public repos. The owner confirmed: **keep the website repo private for now regardless, and test locally rather than deploying to Pages** until/unless that changes.
- **Admin TUI final layout:** tab-based (ratatui `Tabs`), three tabs — Overview (total bandwidth, room count, uptime, cert rotation status), Rooms (table of rooms with per-participant username + role, sortable by bandwidth), Room Detail (per-stream bandwidth split main-vs-preview, subscriber counts, scrolling event log). No dedicated "preview health" indicator (redundant with the existing per-stream bandwidth panel). No historical persistence/graphing — real-time snapshot only, explicitly scoped as a "Saturday afternoon hobby project."

### Round 3 Q&A (5 distilled questions, all answered)

1. Multi-monitor/multi-stream-per-sender in scope for launch? → **"It would be cool, implement it, if it isn't too big of a hassle."**
2. Background preview caching while picker is closed? → **"Would be smart, implement it, if it's not too hard."**
3. Inline viewing: single-slot or mosaic? → **"The main UI should be able to display multiple streams."**
4. GitHub Pages + private repo billing conflict → **"It will be private for now, but you can test locally."**
5. Stale cert-hash handling in the browser viewer → Initially: **"The browser will need fresh links every time anyway, so it doesn't matter, I guess?"** — the owner then asked for this to be explained in detail (see "Post-Round-3 clarifications" below), and after the explanation, confirmed the specific error-message behavior: **the browser viewer should show "This link has expired — ask your friend for a new one"** on a stale/mismatched cert hash, rather than a cryptic raw connection failure.

## Post-Round-3 Clarifications (Direct Q&A With the Project Owner, After All Formal Rounds)

These were explained directly by the team lead (not a dispatched agent) to help the owner make final calls, and are recorded here for completeness:

1. **"Will saved `server|room|key` links work in a year without a problem?"** — Yes, confirmed. Native desktop "lounge" links never depend on the relay's rotating TLS certificate at all (native `quinn` clients don't validate certs), and the room key itself never changes by design. The only real-world caveat: if the `SERVER` field is a raw home IP address and that IP ever changes, the link breaks — same as any bookmarked IP would. A dynamic-DNS hostname (e.g. DuckDNS) was suggested as a hedge against this, though it's a networking-hygiene detail, not a protocol requirement.
2. **"Why does the browser need the extra [cert-hash] information?"** — Explained: browsers cannot open raw UDP sockets (a sandboxing restriction to prevent arbitrary network abuse from web pages); the only browser path to UDP-like transport is WebTransport, which is built on QUIC, which is *always* encrypted at the protocol level with no way to disable it. This means the browser must always verify the identity of whatever it's connecting to, or the connection is refused outright with no click-through option. Normally that verification means a CA-signed certificate (requiring a domain + Let's Encrypt), which the owner wanted to avoid; the `serverCertificateHashes` self-signed-cert mode is WebTransport's one escape hatch from that requirement, but it needs the browser to be told the expected hash value in advance — hence the 4th join-link field. Native clients never hit this because they aren't inside the browser sandbox.
3. Owner's response to the above: **"Okay, then it is fine, if the desktop client has to generate something, for browsers to join properly."** (Confirms the whole cert-hash-in-link mechanism.)
4. **New requirement added after all formal rounds:** the desktop client must support **English and German (EN/DE) translations/localization**. No specific i18n crate or architecture has been chosen yet — understood as a build-phase implementation detail (e.g. a standard Rust i18n crate such as `rust-i18n` or `fluent-rs` with locale resource files and a language toggle in settings), not something requiring further brainstorming.

## Final Consolidated Design Plan

*(This is the complete, load-bearing design state as of the end of Round 3 plus all post-round clarifications — the version sent to the final Opus verify pass.)*

### Repos (all GitHub, private initially)

- **Desktop client**: Rust + egui, location TBD, single binary.
- **Server**: `~/Documents/Programming/simple-screensharing-server` (Rust).
- **Browser viewer**: `~/GitHubProjects/simple-screensharing-website/` (bundled JS, GitHub Pages) — staying private for now; will test locally rather than deploy, since GitHub Pages on a private repo requires a paid GitHub Pro plan (~$4/month).

### Reference Repos (Inspiration Only, Not Copied Wholesale)

- `~/Documents/Programming/ScreegoElectronScreenshare/` — PipeWire audio-tee pattern (`pw-loopback` + `pw-link`, additive, per-application audio picker).
- `~/Documents/Programming/CloudflaredScreenshare/` — room/join-string logic and content-blind E2E relay design were sound and used as inspiration; its actual screensharing was broken by software-only x264 encoding + Cloudflare quick-tunnel fragility + TCP/WebSocket head-of-line blocking. Its browser viewer will be rewritten from scratch (not reused), though its WebCodecs decode pattern is a useful reference.

### Join/Share Link Formats

- **Native** (desktop, saved as reusable "lounges" in a local TOML file): `server|room-id|key` — permanent; works indefinitely as long as the server address stays reachable and the room key is unchanged. Native clients never pin the relay's TLS cert, relying purely on E2E AES-GCM encryption for security.
- **Browser** (view-only, freshly generated per share by the desktop host, not saved/reused): `server|room-id|key|certhash` — the 4th field is the relay's *current* self-signed cert hash, fetched live by the desktop host from the relay at link-creation time (no offline link generation supported). On a stale-hash mismatch, the browser viewer shows: **"This link has expired — ask your friend for a new one."**

### Transport & Protocol

- QUIC via `quinn` for native clients; WebTransport for the browser — same relay endpoint, single UDP port, ALPN-dispatched.
- Control-plane (JOIN/LEAVE/SUBSCRIBE/UNSUBSCRIBE/ANNOUNCE/presence) over reliable QUIC streams.
- Media-plane (video/audio frames) over unreliable QUIC datagrams — drop-and-continue on loss, no retransmission.
- Preview/thumbnail-plane: periodic (~30s) still-frame previews per active stream, over a reliable stream, broadcast to all room members regardless of subscription.
- Room-IDs: client-generated, meant to be high-entropy (128-bit).
- Rooms: no persistent server-side registry — exist only while occupied.
- Multi-stream rooms: up to 8 total participants per room, no cap on concurrent sharers, a single sender may run multiple simultaneous streams (e.g. dual-monitor).
- Relay does selective per-subscriber forwarding for main streams; previews broadcast to everyone.
- Keyframe/GOP: 2-second fixed interval + SUBSCRIBE-triggered forced keyframe.
- No join-proof / HMAC challenge-response — explicitly skipped.

### Security & Encryption

- E2E symmetric encryption: AES-256-GCM, key embedded in the join link. Relay is 100% content-blind.
- Nonces: `client_id ‖ counter`, with `client_id` fresh-random per CONNECTION (never persisted per device).
- No TLS/CA/domain/reverse-proxy required. The relay's self-signed cert auto-rotates ~10-12 days purely because browsers enforce a ~14-day cap — the room key itself never changes, and native links/clients are entirely unaffected by cert rotation.
- Explicitly skipped: room revocation, "regenerate lounge" feature, cap on concurrent sharers, join-proof hardening.
- Design philosophy: the system consistently substitutes social/human verification for cryptographic enforcement (ephemeral rooms, visible presence/username lists in both the admin TUI and the in-session client UI).
- Usernames are not unique, not authenticated — purely cosmetic self-report. Room-key possession proves group membership, not individual identity.
- Preview thumbnails add minor metadata visibility (approximate sharer count) but no content leak.

### Media Pipeline

- Video codec: **H.264 only for launch** (AV1 deferred as a future opportunistic upgrade).
- Audio codec: **Opus**, 48kHz low-latency mode.
- Encoding: GStreamer-based pipeline, GPU/hardware-accelerated encoder elements (`nvh264enc`/`vah264enc`/`mfvideoenc`) required, CPU (`libx264` zerolatency) fallback required.
- Bitrate: user-settable up to 40 Mbit/s ceiling; "adaptive to content" via encoder-internal rate control (CBR/VBV-constrained) only — no network-adaptive bitrate renegotiation.
- Idle-stream optimization: 0 subscribers → sender pauses/reduces main-stream encoding (keeps capturing), resumes with an instant forced keyframe on subscribe. Preview generation is exempt from this pause.
- Preview mechanism: standalone ~30s JPEG snapshot from the live capture buffer, fully decoupled from the main H.264 encoder state.
- Multi-monitor/multi-stream-per-sender: in scope; NVENC's concurrent-session cap (~3) means extra streams beyond that should degrade to CPU encoding.
- Audio idle-pause rides the same subscription gate as video.

### Desktop Client (Rust + egui)

- Single binary, runtime toggle between share/view mode.
- Platforms: **Windows** (owner cannot self-test; build natively via GitHub Actions `windows-latest` CI, MSVC toolchain — do not cross-compile) and **Linux Wayland only** (X11 dropped entirely).
- Capture/encode pipeline: GStreamer-based (runtime bundling accepted), reusing/extending CloudflaredScreenshare's `capture.rs` skeleton pattern (ashpd portal + `pipewiresrc`) but filling in the HW encoder arms it left unfinished.
- Audio: per-application picker via PipeWire tee method, no microphone capture. Viewers get per-stream volume control, 0-200%.
- Local room storage: simple TOML file listing saved "lounges" (name, server, room-id, key — no cert hash needed). Key stored in plaintext on disk (acceptable, should be documented).
- Multi-viewport pop-out: borderless pop-out windows, F11 fullscreen toggle per-viewport. **Hard requirement:** independent per-viewport refresh pacing (a 240Hz pop-out must not be throttled by a slower main window) — proposed implementation via GStreamer appsink's `new-sample` callback pushing frames into a per-viewport channel + `ctx.request_repaint_of(viewport_id)`.
- Main UI: supports displaying **multiple streams simultaneously inline** (mosaic), in addition to the separate pop-out option.
- Stream picker UI: grid of tiles, each showing a ~30s-refreshing preview thumbnail (crossfade on update, "updated Ns ago" indicator) + username overlay (short disambiguator suffix for active-tile name collisions). Click = view inline; pop-out via corner icon/double-click/right-click. Previews use a separate lightweight JPEG-decode path, not the main GStreamer video pipeline.
- Preview delivery when picker closed: keep receiving/caching in the background, best-effort.
- **Localization**: English and German translations required (implementation detail for the build phase — e.g. `rust-i18n` or `fluent-rs` with locale files and a settings toggle).

### Server (Rust)

- Home-lab hosted, 600 Mbit down / 400 Mbit up available.
- Simple broadcast relay: rooms exist only while occupied, no persistent registry.
- UUID assigned per CONNECTION (not per device) for presence tracking within that session only.
- Single port, QUIC/WebTransport dual-protocol via ALPN dispatch.
- Self-signed cert, auto-rotates ~10-12 days (browser requirement only).
- Admin interface: ratatui TUI, tab-based — Overview (bandwidth, room count, uptime, cert rotation status), Rooms (table with usernames + roles, sortable by bandwidth), Room Detail (per-stream bandwidth split main vs. preview, subscriber counts, scrolling log tail). No historical persistence/graphing — real-time snapshot only.

### Browser Viewer (Bundled JS, Static Site, GitHub Pages)

- View-only for now (no browser-side sharing; could be added later without protocol changes).
- Rewritten from scratch, not reusing CloudflaredScreenshare's browser code (though its WebCodecs decode pattern is a useful reference).
- Connects via WebTransport using the 4-field join link; decrypts via WebCrypto AES-GCM; decodes main video via WebCodecs `VideoDecoder` (H.264); decodes previews via simple `createImageBitmap()`.
- On stale cert-hash: shows "This link has expired — ask your friend for a new one."
- Deployment: GitHub Actions native Pages-from-Actions (esbuild/vite build → `upload-pages-artifact` → `deploy-pages`).
- Repo stays private for now; test locally rather than deploying to Pages (private-repo Pages requires GitHub Pro).

### Explicitly Deferred / Out of Scope for Launch

- AV1 codec support
- Network-adaptive/congestion-based bitrate renegotiation
- Room revocation feature
- "Regenerate this lounge" one-click key rotation feature
- Join-proof / cryptographic room-admission hardening
- LAN direct-connect fast path bypassing the relay
- Linux X11 support
- Browser-side sharing capability
- Any cap on concurrent sharers per room
- Actual GitHub Pages deployment/going public

## Final Verify Results

**Status: COMPLETE.** A `tl-opus-high` verifier reviewed the full design plan above for internal consistency, answer fidelity (does the plan honor every owner decision from Rounds 1-3), and load-bearing gaps. Verdict: **the design is internally consistent and correctly honors every owner answer — ready to build after a short pre-build spec pass on 4 protocol mechanics**, not another brainstorm round. Findings:

### One inconsistency found (now noted as a spec item)

**Client-side presence roster vs. security design.** The SECURITY section states presence/username lists are visible in both the admin TUI and the in-session client UI, and the whole "social verification instead of cryptographic enforcement" philosophy leans on this (people notice unexpected guests by seeing them). But the DESKTOP CLIENT section only specifies a stream-picker grid of tiles keyed to *active streamers* — a silent lurker who joined with just the room-id but never shares would produce no tile and be invisible. **Resolution needed:** the client UI should include an explicit "everyone currently in the room" roster panel (username + role: streaming/viewing), separate from the streamer-tile grid, so the self-policing security model is actually delivered by the UI as designed.

### Four load-bearing protocol gaps (need pinning down before/during build, not new features)

1. **Nonce/counter discipline (highest priority — security-critical, not cosmetic).** The design uses `nonce = client_id ‖ counter` with one shared AES-256-GCM room key and a fresh-random `client_id` per connection. Three things were never pinned down: (a) `client_id`'s bit-width, which determines collision resistance across connections; (b) for multi-stream-per-sender (dual monitor, confirmed in scope), the counter must be a single monotonic space across all of that connection's streams, or `client_id` must fold in a stream-id, or two streams on one connection can reuse `(client_id, counter)`; (c) video, audio, and preview channels all encrypt under the same room key and must have domain-separated counters so they never collide with each other. Getting this wrong causes silent AES-GCM nonce reuse, which breaks the encryption's security guarantee entirely — this needs to be nailed down precisely before implementation, not improvised.
2. **Join-time state sync.** A client that just joined has no specified way to learn the current set of active sharers/streams or see any preview until the next ~30s preview cycle happens to land. Needs: on JOIN, the relay sends a state snapshot (active streams + full presence roster) and immediately forwards the last-cached encrypted preview blob per active stream (relay stays content-blind — it just caches/forwards opaque ciphertext, never decrypts).
3. **Media-frame fragmentation/reassembly.** QUIC/WebTransport datagrams are MTU-bounded (~1200 bytes); H.264 frames — especially the 2-second-interval keyframes and subscribe-triggered forced IDRs — are many KB. Nothing currently specifies an application-level frame→datagram fragmentation scheme. Needs: a datagram header carrying (frame-id, fragment-index, fragment-count) and a reassembly buffer that discards an entire frame if any fragment is missing (consistent with the existing "drop-and-continue" model). Previews are unaffected — they ride the reliable stream, which already handles framing.
4. **Relay→sender subscriber-count signaling.** Two already-agreed features depend on the *sender* knowing its live subscriber count: idle-pause (0 subscribers → pause encoding) and forced-keyframe-on-subscribe (instant keyframe when someone subscribes). The control-plane message list covers SUBSCRIBE/UNSUBSCRIBE from viewers but never specified the relay pushing that information back to the sharer. Needs: a relay→sender control message carrying per-stream subscriber count / keyframe-request.

### Lower-priority notes (not blockers)

- **Stable relay hostname.** Native "permanent" links and both link formats embed a literal `server` field. A residential home-lab IP can change, silently breaking every saved lounge and issued link. Recommendation: front the relay with a dynamic-DNS hostname (e.g. DuckDNS) rather than a raw IP.
- No A/V sync/timestamp (PTS) scheme was specified — acceptable given the owner's "glitches are fine" tolerance, but worth adding a timestamp field to frames for correctness.
- Uplink bandwidth math (40 Mbit/s × multiple simultaneous sharers) can exceed the 400 Mbit/s home-lab uplink under heavy simultaneous use — this is a capacity/tuning reality given the owner's explicit choices (no adaptive bitrate, no sharer cap), not a design flaw.
- Reconnection UX after a mid-session QUIC drop is unspecified — fine to leave best-effort/undefined for now.
- Browser audio playback requires a user gesture to start (standard browser autoplay-policy constraint) — minor UX note for the browser viewer.
