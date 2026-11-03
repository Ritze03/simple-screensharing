# Relay / client QUIC transport (congestion control, datagram queue, idle timeout)

Task #86, 2026-11-02. Code: `server/src/transport.rs` (`TransportTuning`, relay side),
`client/src/net.rs::build_client_endpoint` (the native client's mirror), `server/src/transport_lab.rs`
(the measuring lab). Env overrides on the relay and the client: `SSSHARE_CC=bbr|cubic|newreno`
(relay and client), `SSSHARE_DGRAM_BUF_KB` (relay only).

## Decision

| Setting | Before | Now | Where |
| --- | --- | --- | --- |
| Congestion controller | quinn default (Cubic) | **BBR** (`quinn::congestion::BbrConfig`, labelled experimental upstream) | relay (relay -> viewer) **and** native client (sharer uplink) |
| Datagram send buffer | quinn default, 1 MiB | **unchanged, 1 MiB** (a smaller one was rejected, see below) | relay, client |
| Max idle timeout | 60 s | **20 s** | both |
| Keep-alive | 15 s | **5 s** | both |
| Deadline drop / "never evict the newest IDR" | n/a | **not built** (reasoning below) | |
| Eviction counter + path-stats logging (WIP ad14e6a) | n/a | **kept** (`relay.rs`: `viewer send queue full` every <= 5 s per viewer, `connection path stats` at the end of a connection, `Registry::datagrams_forwarded/evicting`) | relay |

The lab knobs of the WIP patch became the documented `SSSHARE_CC` / `SSSHARE_DGRAM_BUF_KB`
overrides: one env var is the cheapest rollback if BBR ever misbehaves on someone's path, and the
relay logs the effective tuning at start (`QUIC transport tuning ...`).

**Compatibility.** Nothing here is negotiated except the idle timeout, and quinn takes the minimum
of both peers' values, so every old/new combination works: lab `lab_idle_compat` keeps a quiet joined
connection open for 45 s with old (60 s/15 s) and new (20 s/5 s) on either end, all four combinations.
A new client against the old relay still gets its 20 s (the minimum) and its own 5 s pings; an old
client against the new relay gets the relay's 20 s and 5 s pings. The congestion controller is purely
sender-local. The **relay change only takes effect when the user redeploys the relay**
(vogelnest.any64.de:41000 runs the old build): the client change alone already fixes the sharer's
uplink (BBR) and shortens dead-path detection, but relay -> viewer keeps Cubic (and its collapse
under loss) until then.

## Why: what went wrong

`quinn::Connection::send_datagram` never fails when its queue is full: it silently drops the
**oldest** queued datagrams (`datagram_send_buffer_size`, 1 MiB). The relay forwards every media
fragment with it. Cubic treats ~1 % random loss as congestion and shrinks the window below the
stream bitrate; the queue then fills and evicts from the head, which breaks almost every frame and
delivers a keyframe seconds late (a Cubic send queue at 4 Mbit/s holds ~2 s, at 1 Mbit/s ~8 s of
stale video). Nothing in any socket or NIC counter shows that loss: it is why the eviction counter
and the path-stats logging stay.

## Method

`transport_lab.rs`: per cell a real relay (in-process, real `serve`, `TransportTuning::build`), a
synthetic sharer and viewer (real quinn clients, the native client's transport settings) each
behind a UDP impairment proxy on loopback (random loss, one-way delay, a bottleneck rate with a
tail-drop queue, blackouts). The sharer sends frames shaped like the native client's (<= 1000 B
datagrams with the 19 B header, 2 s GOP, IDR = 12 x a delta, so ~93 fragments at 4 Mbit/s 60 fps,
all of a frame sent back-to-back). The viewer reassembles and tracks decodability **strictly**: a
delta frame counts only if it directly follows a decodable frame, a keyframe always does. This is
pessimistic (the real viewer also decodes frames with holes, see the rig result), but it is the
number that shows the keyframe problem. `rec` = the viewer re-subscribes (rate-limited to 1/s)
after 400 ms without a decodable frame, which makes the relay ask the sharer for a keyframe
(the recovery #101 adds); `raw` = no recovery. 20 s measured per cell after a 4 s warm-up, 6-8
cells in parallel on loopback.

Reproduce: `cargo test --release -p simple-screensharing-server transport_lab -- --ignored --nocapture`
(`LAB_FILTER=a,b` substrings, `LAB_SECS`, `LAB_PAR`, `LAB_OUT=file`). The impairment proxy first
written for this (a Python one that spawned a task per packet) reordered packets and made a clean
path look broken: if you write another, keep one ordered queue per direction (the lab's `pipe`).

Whole matrix: 7 configurations x 34 cells (+ high bitrate, paced and uplink cells). Cell format
below is **arrived/decodable fps** (arrived = every fragment of the frame came in).

### Random loss on the viewer leg (both directions), 4 Mbit/s 60 fps, `rec`

| config | 0.5 % rtt20 | 1 % rtt20 | 2 % rtt20 | 0.5 % rtt80 | 1 % rtt80 | 2 % rtt80 |
|---|---|---|---|---|---|---|
| cubic/1M (before) | 57/14 | 51/5 | 38/1 | **7**/0 | **5**/0 | **1**/0 |
| newreno/1M | 56/19 | 53/4 | 30/0 | 11/0 | 6/0 | 1/0 |
| **bbr/1M (chosen)** | 58/23 | 56/12 | 50/1 | **57**/17 | 55/3 | 49/0 |
| bbr/512K | 56/14 | 54/6 | 51/1 | 56/13 | 55/8 | 50/3 |
| bbr/256K | 57/20 | 54/8 | 50/2 | 57/18 | 54/6 | 50/0 |
| bbr/128K | 57/16 | 55/8 | 50/0 | 57/19 | 55/8 | 50/2 |

Cubic and NewReno **collapse at 80 ms RTT from 0.5 % loss** (7 fps arrive; 50-80 % of the
forwarded datagrams evict older ones, the `ev` column of the raw output) and the keyframe request
never succeeds. BBR keeps ~55 fps arriving at every loss level and RTT with zero evictions, and
the buffer size makes no difference to that. The low *decodable* numbers for BBR are not the
controller: at 1 % loss an ~93-fragment IDR completes only (0.99)^93 = 39 % of the time (15 % at
2 %), and in the strict model nothing decodes until one does. That is the keyframe-delivery
problem #100/#101 address (repeats, re-requests), not transport. A 12 Mbit/s IDR (~300 fragments)
at 1 % loss never completes whatever the controller: 0.2 decodable fps for BBR/1M, 0 for Cubic,
which additionally loses all arrivals (1 fps).

### Sharer uplink (impairment on the sharer's leg; the client's controller)

| config | 1 % loss | 2 % loss | link 4200k q150 | link 5000k q100 | link 8000k q50 |
|---|---|---|---|---|---|
| cubic/1M | 55/4 | 31/1 | 56/42 | 58/37 | 58/30 |
| newreno/1M | 55/6 | 28/0 | 55/35 | 60/58 | 60/58 |
| bbr/1M | 55/9 | **52**/2 | 58/58 | 55/1 | 58/0 |
| bbr/512K | 55/8 | 51/1 | 57/52 | 60/60 | 58/2 |

### Narrow / near-rate bottleneck on the viewer leg (rtt 40 ms), stream 4 Mbit/s

Cell: arrived/decodable fps / median capture-to-complete latency ms.

| config | 4200k q50 | 4200k q150 | 4200k q300 | 5000k q50 | 5000k q150 | 8000k q50 | 8000k q150 |
|---|---|---|---|---|---|---|---|
| cubic/1M | 51/17 / 1286 | 57/49 / 586 | 55/43 / 910 | 58/46 / 27 | 60/60 / 26 | 59/**30** / 21 | 60/60 / 21 |
| newreno/1M | - | 56/41 / 1002 | 57/51 / 640 | 59/40 / 82 | 60/58 / 25 | 60/60 / 20 | 60/60 / 20 |
| bbr/1M | 58/**58** / 657 | 58/**58** / 698 | 58/58 / 684 | 60/51 / 26 | 60/60 / 26 | 58/**0** | 60/60 / 21 |
| bbr/512K | 41/4 / 167 | 44/2 / 432 | 58/58 / 635 | 56/6 / 166 | 59/43 / 27 | 58/0 | 60/60 / 21 |
| bbr/256K | 52/12 / 458 | 53/35 / 533 | 54/42 / 516 | 59/44 / 26 | 60/60 / 26 | 58/0 | 60/60 / 20 |
| bbr/128K | 56/28 / 262 | 57/54 / 276 | 56/53 / 262 | 56/0 | 60/60 / 26 | 58/0 | 60/60 / 21 |

A link **below** the stream rate (2500k, 3500k, any queue) leaves nobody with a usable picture
(0-18 decodable fps for every controller and buffer): the stream does not fit, only the
encoder adapting its bitrate helps, and that is not a transport matter.

**What was measured about "BBR collapses on narrow links"** (the board row cites it; no earlier
numbers survive in the repo, so it was re-measured): BBR is as good or better than Cubic from
~5 % above the stream rate up with a >= 150 ms queue and clearly better at 4200k, but with a
**shallow (50 ms) queue and a link well above the stream rate (8000k, also 12 Mbit/s on 15 Mbit/s,
8 Mbit/s 120 fps on 10 Mbit/s)** BBR's *keyframes never get through* (0 of 29, arrival stays
58 fps) while Cubic/NewReno lose them only some of the time. Mechanism: the sharer sends an IDR as
one burst (~93 KB at 4 Mbit/s) and quinn's pacer allows bursts of up to ~256 packets at BBR's
large window; a 50 ms queue on an 8 Mbit/s link holds ~50 KB, the burst tail is dropped, and BBR,
being loss-agnostic, repeats exactly the same burst for every retry, whereas Cubic shrinks its
window after the loss. **A paced sender fixes it**: with the IDR spread over 60 ms
(`pace_idr_ms`), 8000k q50 goes from 0 to 58 decodable fps for BBR (and 30 -> 58 for Cubic);
5000k q50 is not fixed by that spread because 93 KB cannot cross a 5 Mbit/s link in 60 ms. This
is the case to hand #100: pace the fragments of a keyframe (and large frames) instead of sending
them back-to-back; it is the highest-value remaining lever and makes the controller choice nearly
irrelevant for this failure mode.

### Datagram send buffer

| config | 12 Mbit/s 60 fps clean | 12 Mbit/s, link 15 M q150 | 8 Mbit/s 120 fps, link 10 M q150 | 6 s blackout: dead s / longest gap | blackout evictions |
|---|---|---|---|---|---|
| bbr/1M | 60/60 | 60/56 | 120/120 | 7 / 8.1 s | 24 % |
| bbr/512K | 60/60 | 60/50 | 120/120 | 5 / 6.0 s | 0 % |
| bbr/256K | 58/**0** | 57/**0** | 119/104 | 6 / 7.8 s | 31 % |
| bbr/128K | 58/**0** | 58/**0** | 120/120 | 5 / 6.0 s | 0 % |

The suggested ~128 KiB (a 250 ms backlog at 4 Mbit/s) was **rejected**: the relay and the sharer
forward a keyframe as one burst, so the queue must hold a whole IDR (~290 KB at 12 Mbit/s,
~480 KB at 20 Mbit/s, ~1 MB at 40 Mbit/s: IDR = 12 x a delta). With <= 256 KiB, a 12 Mbit/s stream
gets **no keyframe through even on a clean, unimpaired path** (0 decodable fps), and 128 KiB is
marginal for 8 Mbit/s 120 fps. The benefit of a small buffer (shorter stale backlog: median
latency 262 ms vs 657-700 ms at a 4200k link; blackout recovery at the blackout length instead of
~2 s later) is real but smaller than that failure, and a byte limit cannot be right for every
bitrate. 1 MiB (quinn's default) also fails for a 40 Mbit/s keyframe on a path slower than the
burst; not measured here and not changed.

**Deadline drop / newest-IDR priority: not built.** With BBR the queue evicts ~0 % under random
loss at every RTT (the case the user complained about); eviction only shows up where the link
cannot carry the stream (nothing helps) and after a blackout (a 1-2 s backlog that arrives as a
continuous delta chain, so the picture resumes without a keyframe). A real deadline drop would
need a relay-side per-viewer queue with timestamps in front of quinn (`send_datagram_wait` plus a
small quinn buffer), a per-viewer task and frame awareness; the measurements do not call for it.
If one is ever built, the first move is a time-based bound (drop non-keyframe fragments older than
~250 ms), not a smaller byte buffer.

### Idle timeout

`lab_idle_blackhole`: a connection whose path goes dark is declared dead after **72.0 s** with
60 s/15 s and **22.0 s** with 20 s/5 s (the idle timeout plus the last PTO) in the lab. The viewer
used to freeze for up to a minute before the reconnect logic ran. Blackout recovery after the
path returns was 6-8 s for a 6 s blackout in the lab at every setting (PTO backoff; quinn has no
knob for the backoff cap). 20 s means a >20 s network outage now drops the connection and the
client's suspended-room reconnect takes over (instead of waiting up to 60 s for the QUIC layer).

## Real stack check (headless rig, release build, the proxies in front of both clients)

H.264 60 fps 4000 kbps, 1920x1080, `HS_XDPW=patched`; viewer leg impaired: 20 ms each way, 20 Mbit/s
cap with a 150 ms queue; the viewer tile's pixels are sampled 5 x/s (a 0.8 s unchanged picture =
frozen). A = this change (BBR on relay and both clients), B = the same binaries with
`SSSHARE_CC=cubic` on relay and clients (the old behaviour).

| phase | A: BBR/20 s/5 s | B: Cubic |
|---|---|---|
| clean | no freeze, 60 fps | no freeze |
| viewer leg 1 % loss, 40 s, then 2 % loss, 40 s | **0 freezes**; at 2 % the viewer tooltip read "Decoded 52 fps / shown 52 fps, 0 dropped" (3.4 Mbit/s) | **5 freezes, 61 s frozen of the 80 s**: 25.8, 20.6, 1.5, 2.0, 11.3 s; at 2 % the tooltip read "Decoded 7 fps / shown 6 fps", 411 kbps, a corrupted picture |
| 8 s blackout of the viewer leg | frozen exactly the 8 s, moving again at once | frozen 9.1 s (about 1 s past the end) |
| sharer leg 1 % loss, 40 s | 0 freezes | 0 freezes |
| sharer leg 2 % loss, 40 s | 0 freezes, "Decoded 50 fps" | 1 freeze of 1.4 s |

(Under 2 % loss BBR's picture has visible block artefacts: the real decoder shows frames with
holes. Concealing or re-requesting those is the viewer-side work of #101.) Settings-change
transitions (the 12-transition test of the investigators) were not re-run here.

## Selective retransmission (NACK, #104, 2026-11-03)

BBR (above) makes the transport keep delivering under random loss, but a frame still dies with any
one of its fragments (a 1 MB IDR is ~1000 of them). The relay therefore also answers retransmit
requests. Wire format, capability probe, compatibility matrix and the *why* of every choice:
`architecture/protocol-spec.md` section 6.6. Relay side, in short:

- **Cache.** Per *video* stream, `StreamRoute::cache` (`protocol::FragmentCache<Bytes>`) holds the
  datagrams the forwarder sent in the last 2 s, at most 8 MiB, only while the stream has
  subscribers. `Bytes` clones: the cache costs the refcount of buffers quinn already holds, not a copy.
- **Request** (viewer, subscribed to that stream, datagram `0xE2`): hits go back to that viewer only,
  byte for byte; the missing runs go to the owner as a Request if the owner said Hello (and the
  viewer is remembered in `StreamRoute::nackers` for 1 s), else back to the viewer as `Miss`.
- **Owner re-send** (media datagram with `flags` bit2): inserted in the cache and sent only to the
  `nackers`, never to every subscriber (duplicates would open ghost frames in old reassemblers).
- **Hello** marks `ConnState::nack` and is acked. A relay with `SSSHARE_NACK=0` ignores all of it
  (no cache, no HelloAck): behaves exactly like a relay that predates #104, which is how the
  mixed-version checks get an "old relay" from one binary.
- Counters (`Registry::nack`) are logged every 10 s while they move:
  `NACK activity in the last 10 s requests=.. from_cache=.. forwarded_to_owner=.. miss_replies=..
  owner_resends_relayed=..`.
- Tests: `server/src/nack_tests.rs`.
- **Deploy note:** an old relay (the vogelnest one until redeployed) simply drops Hello as an
  undersized datagram, so new clients never NACK against it. The relay must be redeployed for the
  feature to do anything.

## Risks

- BBR in quinn is labelled experimental. It ran clean for every lab cell and the rig run; the
  rollback is `SSSHARE_CC=cubic` on the relay (and on the client for testing).
- The shallow-queue keyframe-burst case above is BBR's weak spot until the sharer paces keyframes.
- 20 s idle: a laptop that stalls the network for >20 s now reconnects instead of resuming.
- Browsers keep their own QUIC stack (Chromium: BBR already): the relay's BBR governs relay ->
  browser, the browser's own governs browser -> relay; WebTransport peers also pick up the
  20 s/5 s via the minimum rule (not verified with a real browser in this task).

## Diagnostics log (task #109, 2026-11-03)

Code: `server/src/diag.rs`. Why: a friend's stall used to mean "relaunch from a terminal and send me
the client log". The relay sees both legs of every stream, so a periodic summary in its own log is
usually enough (the field case that motivated it was read off the TUI: a viewer doing
unsubscribe -> subscribe every ~1.5 s, the L1 keyframe request of `features/video-pipeline.md`
"Viewer recovery", while the stream rate swung 1 KB/s <-> 1 MB/s). Server only: no protocol or client
change.

**Cadence.** One background task ticks every second, takes each room mutex briefly, and prints a line
when a stream's / connection's 60 s window is due (`DEFAULT_WINDOW`; a window is "at least 60 s", so
`win_s` reads 60-61), and a final partial-window line (`end=1`) when the stream stops, is re-announced
or the connection leaves (the sampler keeps its own `Arc`s, so there is no hook in the stop/leave
paths). Nothing is printed when there is nothing to say: no rooms or no streams = no lines; a stream
line is skipped only if no bytes arrived, nobody watches and nobody (un)subscribed (a stream with
viewers but 0 bytes **is** printed, that is the stall); a connection line is skipped below 16 KiB of
UDP traffic per window (keep-alives are ~1 KB) unless it evicted datagrams. INFO level, in the normal
relay log. The owner's display name is logged (friends-only test system, the owner agreed); it stays in
the relay log and is never sent anywhere.

**Stream line**, one per announced stream:

```
diag stream room=7aa8249c owner=45917ef453d85c50 owner_name="client-1" stream=0 kind=Video label="Screen" win_s=61 end=0 viewers=1 bytes_s_avg=961627 bytes_s_max=983575 dgrams=60498 frames=3693 keyframes=35 stalled_frames=0 kf_max_dgrams=34 kf_max_bytes=33746 subs=2 unsubs=1 resubs=1 evicted=0
```

| field | meaning |
| --- | --- |
| `room` | first 4 bytes of the room id, hex |
| `owner`, `owner_name`, `stream`, `kind`, `label` | the sharer's conn id and display name (from the join), stream id, kind, label |
| `win_s`, `end` | window length in seconds; `end=1` = final partial window |
| `viewers` | subscriber count at the last tick (an `end=1` line shows the last seen value) |
| `bytes_s_avg`, `bytes_s_max` | media bytes/s that **arrived from the sharer** over the window; max = highest per-tick (1 s) rate. Header bytes included, so about the encoder bitrate plus 19 B per datagram |
| `dgrams`, `frames` | datagrams, and distinct `frame_id`s (a new frame starts when `frame_id` changes), so `frames / win_s` = the sharer's delivered fps |
| `keyframes`, `kf_max_dgrams`, `kf_max_bytes` | frames with header flag bit0; the largest one as the fragment count its header declares and as bytes that actually arrived (a big gap = fragments lost on the sharer's uplink) |
| `stalled_frames` | frames flagged `MEDIA_FLAG_STALLED` (sharer re-sending its cached IDR while its capture source is down) |
| `subs`, `unsubs`, `resubs` | subscribes / unsubscribes in the window, and unsubscribe -> subscribe of the same viewer within 5 s (`RESUB_WITHIN`) = a native viewer's keyframe request. Dozens of `resubs` per window = a viewer that cannot decode |
| `evicted` | forwards toward this stream's viewers that evicted older queued datagrams (the viewer's send queue was full) |

**Connection line**, one per joined connection with traffic:

```
diag conn room=7aa8249c conn=8aa89bb5723cfbcd name="client-2" transport=native win_s=61 end=0 shares=0 watching=1 tx_bytes_s=990926 rx_bytes_s=3170 sent=60255 lost=0 congestion=0 rtt_ms=0 cwnd=13392 mtu=1452 evicted=0
```

`shares` / `watching` = streams it owns / subscribes to. `tx_bytes_s` (relay -> peer, a viewer's
downlink) and `rx_bytes_s` (peer -> relay, a sharer's uplink) are quinn UDP byte deltas, so they
include keep-alives and control traffic. `sent`, `lost`, `congestion` are deltas of quinn's path stats
over the window; `rtt_ms`, `cwnd`, `mtu` are the values at the end of the window; `evicted` = datagrams
the relay queued toward this connection into a full send queue (the same event as the rate-limited
`viewer send queue full` line, counted per window instead of sampled).

**Reading them.** Sharer-side problem: `frames` below the configured fps or `bytes_s_avg` low while the
sharer's conn line shows `lost` > 0, or `stalled_frames` > 0, or `kf_max_bytes` far below
`kf_max_dgrams` x ~1000 (keyframes arriving incomplete). Relay -> viewer problem: the viewer's conn line
has `lost` / `congestion` / `evicted` > 0 while the stream line is healthy. Viewer cannot decode:
`resubs` high. Everything healthy but the viewer still sees stutter: the problem is in the viewer's
decode/render (ask for that client's log).

**On vogelnest.** `grep -h 'diag ' /var/lib/sss/logs/relay.log.*`; narrow with `diag stream` / `diag conn`,
`owner_name="Name"` / `name="Name"`, `end=1`, `evicted=[1-9]`, or e.g.
`grep -h 'diag stream' /var/lib/sss/logs/relay.log.2026-11-03 | grep -v 'resubs=0'`.

**Cost.** Per media datagram the forwarder takes one uncontended `std::sync::Mutex` (the stream's
counters) and parses the 19 B header a second time; no allocation, plus one relaxed `fetch_add` per
*evicting* forward. The 1 s tick allocates a few small `String`s per stream / connection and does nothing
with no rooms. Verified in the headless rig (60 fps H.264, one sharer, one viewer, ~1 MB/s): one stream
and two conn lines per minute, `frames=3693` over 61 s (60 fps), the viewer's `unwatch` + `watch` shows
as `subs=2 unsubs=1 resubs=1`, and `stopshare` produced the `end=1` line.

## Client diagnostics upload (task #110, 2026-11-03)

Code: relay `server/src/client_logs.rs` + `relay.rs` (`handle_uni`, `handle_diag_uni`); client
`client/src/diag_upload.rs`. Wire format, capability handshake, cadence and the *why*:
`architecture/protocol-spec.md` §6.7. This page is the operator's view.

**What it is.** A friend ticks Settings > Diagnostics > "Send diagnostics to the relay" (off by default,
persisted as `send_diagnostics` in `settings.toml`); from then on their client sends its log ring to the
relay it is connected to, and the relay writes it to `logs/clients/<YYYY-MM-DD>/<display-name>-<conn_id>.log`
in its working directory, one file per connection (a restart of the app or a reconnect is a new file).
The first lines of a file are a `# relay:` line (name, connection id, start time) and the client's
header (version + commit, OS, GPU, VA driver); then the client's INFO+ lines (WARN+ for libraries) exactly as
Settings > Logs > Copy shows them, UTC timestamps; a `# relay: connection closed` line ends a clean session.
The same connection id appears in the relay's own `diag conn ... conn=<id>` lines (#109), so one grep joins
the two views. A tick while the app runs only uploads lines logged from then on.

**Switches.** Relay: `SSSHARE_DIAG_UPLOAD=0` (no ack, nothing stored; clients behave as with an older relay).
Client: untick the box. There is no client env switch (the setting is the switch).

**Limits.** 256 KiB per batch (one in flight per connection), 20 MiB per file, 500 MiB per day directory
(over-cap batches are dropped whole; one `# relay:` note in the file says so), directories older than 14 days
deleted at start and then hourly (`client_logs::prune_at`; only directories named `YYYY-MM-DD`, nothing else
under `logs/clients` is touched). The relay logs `dropping client diagnostics: limit reached` (WARN) and
`client diagnostics uploads (...) enabled=... dir=...` at start.

**On vogelnest.** `ls /var/lib/sss/logs/clients/$(date -u +%F)/`, then e.g.
`less -R /var/lib/sss/logs/clients/2026-11-03/Name-<conn id>.log`,
`grep -h 'WARN\|recover' /var/lib/sss/logs/clients/2026-11-03/Name-*.log`, or follow a live session with
`tail -f`. The relay has to be redeployed for any of this to work (an older relay never acks).

**Cost.** Relay: a 1-byte datagram compare per datagram (`bytes == [0xE4]`, before the NACK check), one
`spawn_blocking` file append per batch (<= 256 KiB, every 30 s per ticked client), a 1 h prune task. Client:
a 1 s timer per connection that does nothing while the box is off; with it on, one mutex lock on the log ring
per tick that has something to send. Nothing sits in the media path.

**Verified** in the headless rig (2026-11-03, 60 fps H.265 share + watch, ~3 min): the sharer ticked via
`HS_SEND_DIAG_1=1` (settings seeded on), the viewer ticked in the real Settings modal while the stream ran;
the relay's rig cwd held `Rig_Sharer-<id>.log` with header, startup WARNs, `capture started in the Gpu domain`,
the keyframe-pacing lines and `Rig_Viewer-<id>.log` with `H.265 decoding via vah265dec (hardware)`; no `|` (join
link) anywhere in either; a relay built from `facc373` with both clients ticked: share and watch worked
(`Decoded 60 fps / shown 60 fps, 0 dropped`), no file written, no client error.
