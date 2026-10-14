#!/usr/bin/env bash
# Dev-only: run a throwaway relay in a detached tmux session (the relay's TUI needs a pty) and print its cert hash.
#
#   docs/dev/relay.sh start   <relay-binary> <scratch-dir> [port]   fresh certificate -> prints "PORT=… HASH=…" (the join link's 4th field)
#   docs/dev/relay.sh restart <relay-binary> <scratch-dir> [port]   graceful stop ('q' in the TUI), start again KEEPING the certificate
#   docs/dev/relay.sh stop    [port]
#
# The relay keeps its self-signed certificate in <scratch-dir>/state, so the hash survives restarts.
set -euo pipefail
cmd="${1:?start|restart|stop}"

stop_session() {
  local port="$1"
  if tmux has-session -t "ssshare-relay-$port" 2>/dev/null; then
    tmux send-keys -t "ssshare-relay-$port" q 2>/dev/null || true # TUI quit = clean shutdown (QUIC CONNECTION_CLOSE to clients)
    for _ in $(seq 1 20); do
      tmux has-session -t "ssshare-relay-$port" 2>/dev/null || return 0
      sleep 0.1
    done
    tmux kill-session -t "ssshare-relay-$port" 2>/dev/null || true
  fi
}

start_session() {
  local bin="$1" dir="$2" port="$3" keep="$4"
  mkdir -p "$dir"
  [ "$keep" = 1 ] || rm -f "$dir/state/cert.meta" # force a fresh certificate
  tmux new-session -d -s "ssshare-relay-$port" "cd '$dir' && exec '$bin' $port"
  for _ in $(seq 1 100); do
    [ -f "$dir/state/cert.meta" ] && break
    sleep 0.1
  done
  [ -f "$dir/state/cert.meta" ] || { echo "relay did not write its certificate" >&2; exit 1; }
  local hash
  hash=$(openssl x509 -in "$dir/state/cert.pem" -outform DER | openssl dgst -sha256 -binary | base64 -w0 | tr '+/' '-_' | tr -d '=')
  echo "PORT=$port HASH=$hash"
}

case "$cmd" in
  start)
    bin="${2:?relay binary}"; dir="${3:?scratch dir}"; port="${4:-4433}"
    stop_session "$port"
    start_session "$bin" "$dir" "$port" 0
    ;;
  restart)
    bin="${2:?relay binary}"; dir="${3:?scratch dir}"; port="${4:-4433}"
    stop_session "$port"
    start_session "$bin" "$dir" "$port" 1
    ;;
  stop)
    stop_session "${2:-4433}"
    ;;
  *) echo "usage: relay.sh start|restart <bin> <dir> [port] | stop [port]" >&2; exit 2 ;;
esac
