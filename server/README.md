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
Encrypt) no hash is needed and nothing is rotated.

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

## Windows

It builds and runs as is (`cargo build --release -p simple-screensharing-server`): the UDP socket
is bound dual-stack with `socket2` (`set_only_v6(false)`) so IPv4 clients connect, and the SIGTERM
handling is unix-gated. See `superdoc/features/windows.md`.
