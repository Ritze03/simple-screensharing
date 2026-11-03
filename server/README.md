# simple-screensharing-server

The relay for the simple-screensharing platform. It forwards room traffic (control messages and
end-to-end-encrypted video/audio frames) between the members of a room and **never decrypts
anything**: the room key lives only in the clients' join link.

## Transports

One UDP port serves both, told apart by ALPN:

- **QUIC `ssshare/1`** for the native (egui) client.
- **WebTransport (`h3`)** for the browser viewer.

## Certificate

By default the relay generates a self-signed **ECDSA P-256** certificate (valid 13 days, rotated
when fewer than 2 days remain, without dropping existing connections) and keeps it in the state
dir so its hash survives restarts. Browsers pin it with `serverCertificateHashes`, so the SHA-256
hash goes into the join link (`server|room-id|key|certhash`). With `--cert`/`--key` (e.g. Let's
Encrypt) no hash is needed and nothing is rotated; the files are re-checked hourly and a changed
pair is reloaded (a failed reload keeps the old certificate and is retried at the next check).
Browsers retry a link's hash without the pin against such a relay, so links that still carry a hash
keep working.

## Build and run

```
cargo run -p simple-screensharing-server -- --help
cargo run -p simple-screensharing-server -- [PORT] [OPTIONS]
```

The relay shows a ratatui admin TUI, so it needs a real terminal (a tty; under automation use
`tmux`). Logs go to files under `./logs/`, not the terminal. Cert state is written to `./state/` relative to the
current directory, so start it from a directory you own.

| Flag | Meaning |
| --- | --- |
| `PORT` | UDP port (default `$SSSHARE_PORT` or `4433`) |
| `--cert FILE`, `--key FILE` | PEM chain and key from a real CA; must be given together |
| `--state-dir DIR` | where the generated cert is kept (default `./state`) |
| `--san NAME` | extra subject-alternative-name (host or IP) for the generated cert; repeatable |
| `--cert-validity-secs N` | test hook: lifetime of the generated cert (default 13 days) |
| `--cert-rotate-before-secs N` | test hook: rotation lead time (default 2 days) |
| `-h`, `--help` | usage |

## Transport tuning

Every QUIC connection (native and WebTransport) runs **BBR** congestion control, quinn's default
1 MiB datagram send buffer, a **20 s idle timeout** and a **5 s keep-alive** (`src/transport.rs`;
measurements and the why in `superdoc/features/relay-transport.md`). Two environment variables
override the defaults without a rebuild:

| Variable | Meaning |
| --- | --- |
| `SSSHARE_CC` | `bbr` (default), `cubic` (the pre-2026-11-02 behaviour, rollback) or `newreno` |
| `SSSHARE_DGRAM_BUF_KB` | per-connection datagram send buffer in KiB (default 1024); smaller values break keyframes at higher bitrates, see the doc |

The effective values are logged once at start (`QUIC transport tuning ...`), and the relay logs a
`viewer send queue full` line (per viewer, at most every 5 s) when a viewer's send queue is
evicting media datagrams, plus a `connection path stats` line when a connection ends.

## Diagnostics log

Every 60 s the relay logs one `diag stream ...` line per active stream (bytes/s avg and max,
frames, keyframes and their size, viewers, viewer re-subscribes, send-queue evictions) and one
`diag conn ...` line per busy connection (quinn path-stat deltas, rtt, cwnd, evictions), plus a
final `end=1` line when a stream or connection ends. All `key=value`, INFO, nothing for idle rooms.
Fields and how to read them: `superdoc/features/relay-transport.md` "Diagnostics log". Quick look:
`grep -h 'diag ' logs/relay.log.*`.

## Client diagnostics uploads

Clients whose user ticked Settings > Diagnostics > "Send diagnostics to the relay" (off by default) upload their
own log lines here. They land in `logs/clients/<YYYY-MM-DD>/<display-name>-<conn_id>.log` (relative to the working
directory, so `/var/lib/sss/logs/clients/...` on vogelnest), one file per connection, UTC day of the first batch;
the connection id is the `conn=` of that client's `diag conn` lines in `logs/relay.log.*`. Caps: 20 MiB per file,
500 MiB per day directory, directories older than 14 days are deleted (start, then hourly). `SSSHARE_DIAG_UPLOAD=0`
turns it off (the relay then behaves like one that predates it). Read on the box:
`ls /var/lib/sss/logs/clients/$(date -u +%F)/` and `less -R` / `tail -f` / `grep` the file. The relay must be
redeployed for it to work; older relays are unaffected and so are old clients. Wire format and why:
`superdoc/architecture/protocol-spec.md` §6.7, operator view: `superdoc/features/relay-transport.md`
"Client diagnostics upload".

Lab: `cargo test --release -p simple-screensharing-server transport_lab -- --ignored --nocapture`
(see the header of `src/transport_lab.rs`).

## Windows

It builds and runs as is (`cargo build --release -p simple-screensharing-server`): the UDP socket
is bound dual-stack with `socket2` (`set_only_v6(false)`) so IPv4 clients connect, and the SIGTERM
handling is unix-gated. See `superdoc/features/windows.md`.
