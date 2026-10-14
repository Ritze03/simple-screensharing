#!/usr/bin/env bash
#
# launch-test.sh — developer convenience script for manually testing
# simple-screensharing: starts the relay server (if not already running)
# and opens two Electron client instances (each spawning its own Rust
# sidecar backend), so you can quickly try sharing/watching without
# juggling terminals by hand.
#
# No tmux (or any other terminal multiplexer) is used. The relay server's
# ratatui TUI needs a real pty to render, so instead of wrapping it in a
# multiplexer this script starts everything else FIRST and then runs the
# server directly in the FOREGROUND as its own last, blocking step — it
# naturally inherits this terminal's real pty. Ctrl-C on the server (or the
# server exiting on its own) triggers this script's cleanup trap, which
# kills both client process groups. If you want the old tmux-attach
# workflow (e.g. to reuse/view a server this script didn't start), see
# launch-test-tmux.sh.
#
# Usage:
#   ./launch-test.sh
#   PORT=1234 ./launch-test.sh
#   ./launch-test.sh --help
#
# Env var overrides:
#   PORT           relay listen port (default: 4433, matches the server's
#                   own DEFAULT_PORT)
#   SERVER_DIR     path to simple-screensharing-server checkout
#   CLIENT_DIR     path to simple-screensharing-client checkout

set -uo pipefail

if [[ "${1:-}" == "--help" || "${1:-}" == "-h" ]]; then
  cat <<EOF
launch-test.sh — start the relay server (if not already running) and open
two Electron client windows (each with its own release-mode Rust sidecar
backend) for manual testing. No tmux — plain bash job control.

Usage: $0 [--help]

Env var overrides:
  PORT           relay listen port (default: 4433)
  SERVER_DIR     path to simple-screensharing-server checkout
                 (default: ~/GitHubProjects/simple-screensharing/simple-screensharing-server)
  CLIENT_DIR     path to simple-screensharing-client checkout
                 (default: ~/GitHubProjects/simple-screensharing/simple-screensharing-client)

Client output is logged (not attachable) to:
  /tmp/sssclient-1.log
  /tmp/sssclient-2.log

If the server was NOT already running, this script starts it in the
FOREGROUND as its last step (its ratatui TUI needs a real pty, which the
foreground of this terminal provides for free). Pressing Ctrl-C there (or
the server exiting on its own) automatically kills both client instances
(including their spawned Rust sidecars) via a cleanup trap.

If the server WAS already running (detected via a UDP port check), this
script has no server process of its own to foreground/attach to, so it
just launches the clients and exits normally — you won't see the existing
server's live output here. That's a limitation this script accepts in
exchange for dropping tmux; launch-test-tmux.sh can still attach to a named
tmux session if you need that.
EOF
  exit 0
fi

PORT="${PORT:-4433}"
SERVER_DIR="${SERVER_DIR:-$HOME/GitHubProjects/simple-screensharing/simple-screensharing-server}"
CLIENT_DIR="${CLIENT_DIR:-$HOME/GitHubProjects/simple-screensharing/simple-screensharing-client}"

SERVER_BIN_NAME="simple-screensharing-server"
CLIENT_BIN_NAME="simple-screensharing-client"

# The client is now an Electron app (electron/) that spawns a Rust "sidecar"
# binary (the src-tauri backend, running headless) and talks to it. We build
# that sidecar with `cargo build --release` rather than resolving/running a
# stale prebuilt binary, so this script reflects current source. --release
# (not a debug build) is used because this script is specifically for
# exercising real screensharing (live capture/encode/decode), which needs
# release-level performance to be representative — matching how this
# project's testing has consistently used release builds throughout
# development. The tradeoff is a slower first compile; subsequent runs are
# incremental and fast. The sidecar is built once and shared by both client
# instances via the SIDECAR_BIN env var.

log() { echo ">> $*"; }

# --- sanity checks -----------------------------------------------------

if [[ ! -d "$SERVER_DIR" ]]; then
  echo "ERROR: server directory not found: $SERVER_DIR" >&2
  exit 1
fi
if [[ ! -d "$CLIENT_DIR" ]]; then
  echo "ERROR: client directory not found: $CLIENT_DIR" >&2
  exit 1
fi

# --- cleanup trap (set up before anything is launched in the background) --
# Kills both client process groups. Safe to call more than once (e.g. once
# from the server-already-running branch's normal exit, and never again;
# or once from Ctrl-C on the foregrounded server) — each kill is guarded so
# a client that already exited (or was never launched) doesn't cause noise.

CLIENT_PGID_1=""
CLIENT_PGID_2=""
cleanup_done=0
# Set to 1 right before a deliberate "leave the clients running" exit (the
# server-already-running branch below) — that path has nothing to wait on
# and its normal exit must NOT tear the just-launched clients back down.
# Ctrl-C/TERM during setup still goes through cleanup normally, since this
# flag is only ever set immediately before that specific `exit 0`.
skip_cleanup_on_exit=0

cleanup() {
  [[ "$cleanup_done" -eq 1 ]] && return
  [[ "$skip_cleanup_on_exit" -eq 1 ]] && return
  cleanup_done=1
  if [[ -n "$CLIENT_PGID_1" ]]; then
    log "Stopping client instance #1 (process group $CLIENT_PGID_1) ..."
    kill -TERM -- "-$CLIENT_PGID_1" 2>/dev/null || true
  fi
  if [[ -n "$CLIENT_PGID_2" ]]; then
    log "Stopping client instance #2 (process group $CLIENT_PGID_2) ..."
    kill -TERM -- "-$CLIENT_PGID_2" 2>/dev/null || true
  fi
}
trap cleanup EXIT INT TERM

# --- step 1: is the server already listening on $PORT? -----------------
# The relay speaks QUIC (UDP), not TCP, so we check UDP listeners.

log "Checking whether the relay server is already listening on port $PORT (UDP) ..."

server_already_running=0
if command -v ss >/dev/null 2>&1; then
  if ss -lun 2>/dev/null | awk '{print $4}' | grep -qE "[:.]${PORT}\$"; then
    server_already_running=1
  fi
elif command -v lsof >/dev/null 2>&1; then
  if lsof -iUDP:"$PORT" >/dev/null 2>&1; then
    server_already_running=1
  fi
else
  # Fall back to a process-name check if no port-inspection tool exists.
  if pgrep -f "$SERVER_BIN_NAME" >/dev/null 2>&1; then
    server_already_running=1
  fi
fi

if [[ "$server_already_running" -eq 1 ]]; then
  log "Server already running / port $PORT already in use — skipping server startup."
fi

# --- step 1.5: generate a ready-to-use join link -------------------------
# Join-string format (see simple-screensharing-client/src/net.rs):
#   server|room-id|key
#   - room-id: 16 random bytes, encoded as 32 lowercase hex characters
#   - key:     32 random bytes (AES-256), encoded as base64url, no padding
#
# This doesn't depend on the server already being up (it's generated
# locally, not fetched from the server), so it's safe to do before the
# server exists.

log "Generating a random room id and key for a ready-to-use join link ..."

ROOM_ID_HEX="$(openssl rand -hex 16)"
ROOM_KEY_B64URL="$(openssl rand -base64 32 | tr '+/' '-_' | tr -d '=\n')"

JOIN_STRING="127.0.0.1:${PORT}|${ROOM_ID_HEX}|${ROOM_KEY_B64URL}"

BANNER_LINE="$(printf '=%.0s' $(seq 1 78))"
echo
echo "$BANNER_LINE"
echo "  JOIN LINK (paste into either client's join screen):"
echo
echo "  $JOIN_STRING"
echo
echo "$BANNER_LINE"
echo

if command -v wl-copy >/dev/null 2>&1; then
  printf '%s' "$JOIN_STRING" | wl-copy
  log "Copied to clipboard (wl-copy) — paste into either client's join screen."
else
  log "wl-copy not found — copy the join link above manually."
fi

# --- step 2: build the sidecar, then launch two client instances ---------
# Launched before the server starts (see header comment): the clients only
# need the sidecar binary and the join link to start up and reach their
# join screen — they don't need the relay to already be listening until the
# user actually submits the join link. A brief startup race where a client
# might submit the join link before the relay is up is acceptable for a dev
# script (the old tmux version tolerated the same race via its `sleep 1`
# after starting the server).

SIDECAR_DIR="$CLIENT_DIR/src-tauri"
ELECTRON_DIR="$CLIENT_DIR/electron"
SIDECAR_BIN="${SIDECAR_BIN:-$CLIENT_DIR/target/release/${CLIENT_BIN_NAME}-tauri}"

if [[ ! -d "$SIDECAR_DIR" ]]; then
  echo "ERROR: sidecar directory not found: $SIDECAR_DIR" >&2
  exit 1
fi
if [[ ! -d "$ELECTRON_DIR" ]]; then
  echo "ERROR: electron directory not found: $ELECTRON_DIR" >&2
  exit 1
fi

log "Building the sidecar (cargo build --release) — shared by both client instances ..."
if ! ( cd "$SIDECAR_DIR" && cargo build --release ); then
  echo "ERROR: sidecar release build failed." >&2
  exit 1
fi

if [[ ! -d "$ELECTRON_DIR/node_modules" ]]; then
  log "electron/node_modules not found — running npm install ..."
  ( cd "$ELECTRON_DIR" && npm install )
fi

# Each client is launched with `setsid`, putting it (and everything it
# spawns — npm's `electron .` child, and Electron's own spawned Rust
# sidecar) into a brand new session/process group with the setsid-launched
# process as its leader. `npm start` runs `electron .` as a child process;
# sending a plain `kill` to just the npm PID is not reliable for getting a
# signal to Electron itself — depending on npm's version/signal-forwarding
# behavior, npm may or may not propagate SIGTERM down to its child, and
# Electron's own SIGTERM/SIGHUP/SIGINT cleanup handlers (electron/main.js,
# which kill its spawned Rust sidecar) need to actually receive the signal
# to fire. Killing the whole process GROUP (negative PID) instead delivers
# the signal to every process in that group at once — npm, electron, and
# the sidecar all get it directly, regardless of whether npm forwards
# anything. Because setsid made the launched process its own group leader,
# its PID ($!) IS the process group id, so no extra `ps -o pgid=` lookup is
# needed to find it.

log "Launching client instance #1 (npm start, SIDECAR_BIN=$SIDECAR_BIN) ..."
( cd "$ELECTRON_DIR" && exec setsid env SIDECAR_BIN="$SIDECAR_BIN" npm start ) >/tmp/sssclient-1.log 2>&1 &
CLIENT_PGID_1=$!
log "  -> pid/pgid $CLIENT_PGID_1, log: /tmp/sssclient-1.log"

sleep 1

log "Launching client instance #2 (npm start, SIDECAR_BIN=$SIDECAR_BIN) ..."
( cd "$ELECTRON_DIR" && exec setsid env SIDECAR_BIN="$SIDECAR_BIN" npm start ) >/tmp/sssclient-2.log 2>&1 &
CLIENT_PGID_2=$!
log "  -> pid/pgid $CLIENT_PGID_2, log: /tmp/sssclient-2.log"

sleep 1

# --- summary --------------------------------------------------------------

echo
log "All set. Summary:"
log "  Client windows: 2 Electron instances launched, sidecar: $SIDECAR_BIN"
log "    - instance 1: pgid $CLIENT_PGID_1  (log: /tmp/sssclient-1.log)"
log "    - instance 2: pgid $CLIENT_PGID_2  (log: /tmp/sssclient-2.log)"
log "    - stop them manually: kill -TERM -- -$CLIENT_PGID_1  /  kill -TERM -- -$CLIENT_PGID_2"
echo
log "Use one client to start sharing and the other to watch."
log "Join link: $JOIN_STRING"
echo

if [[ "$server_already_running" -eq 1 ]]; then
  log "Server was already running — nothing to foreground (this script didn't start it,"
  log "so it can't show you its live output, unlike launch-test-tmux.sh's tmux attach)."
  log "The clients above will keep running after this script exits; stop them with the"
  log "kill commands above when you're done (this script's normal exit here deliberately"
  log "does not tear them down — Ctrl-C/TERM during the setup steps above still would)."
  skip_cleanup_on_exit=1
  exit 0
fi

log "Starting the relay server now, in THIS terminal (Ctrl-C here to stop it and clean up"
log "both clients automatically) ..."
echo
( cd "$SERVER_DIR" && SSSHARE_PORT="$PORT" exec cargo run --release )
