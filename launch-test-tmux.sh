#!/usr/bin/env bash
#
# launch-test-tmux.sh — tmux-based variant of launch-test.sh, preserved as-is
# for anyone who wants the tmux-attach workflow (including the ability to
# view a server this script didn't start). See launch-test.sh for the
# current default (no tmux, plain bash job control).
#
# launch-test.sh — developer convenience script for manually testing
# simple-screensharing: starts the relay server (if not already running)
# and opens two Electron client instances (each spawning its own Rust
# sidecar backend), so you can quickly try sharing/watching without
# juggling terminals by hand.
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
#   TMUX_SESSION   tmux session name used for the server (default: sssrelay)

set -uo pipefail

if [[ "${1:-}" == "--help" || "${1:-}" == "-h" ]]; then
  cat <<EOF
launch-test.sh — start the relay server (in tmux, if not already running)
and open two Electron client windows (each with its own release-mode Rust
sidecar backend) for manual testing.

Usage: $0 [--help]

Env var overrides:
  PORT           relay listen port (default: 4433)
  SERVER_DIR     path to simple-screensharing-server checkout
                 (default: ~/GitHubProjects/simple-screensharing/simple-screensharing-server)
  CLIENT_DIR     path to simple-screensharing-client checkout
                 (default: ~/GitHubProjects/simple-screensharing/simple-screensharing-client)
  TMUX_SESSION   tmux session name for the server (default: sssrelay)

Each of the two Electron clients runs in its own tmux session named
<TMUX_SESSION>-client-1 / <TMUX_SESSION>-client-2 (e.g. sssrelay-client-1),
so they can be killed reliably (including their spawned Rust sidecar) with:
  tmux kill-session -t sssrelay-client-1   # stop client 1
  tmux kill-session -t sssrelay-client-2   # stop client 2
  tmux kill-session -t sssrelay            # stop the relay server
  tmux ls                                  # see all sessions at a glance

Client sessions are automatically killed when the relay server session ends
(watchdog: tmux session '<TMUX_SESSION>-watchdog', e.g. sssrelay-watchdog),
whether the server is stopped from inside the attached TUI or from another
terminal after detaching.

The script ends by attaching to the relay server's tmux session; detach
with Ctrl-B then D to leave everything running in the background.
EOF
  exit 0
fi

PORT="${PORT:-4433}"
SERVER_DIR="${SERVER_DIR:-$HOME/GitHubProjects/simple-screensharing/simple-screensharing-server}"
CLIENT_DIR="${CLIENT_DIR:-$HOME/GitHubProjects/simple-screensharing/simple-screensharing-client}"
TMUX_SESSION="${TMUX_SESSION:-sssrelay}"

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
if ! command -v tmux >/dev/null 2>&1; then
  echo "ERROR: tmux is required (the server's ratatui TUI needs a real pty)." >&2
  exit 1
fi

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
else
  log "No server detected on port $PORT. Preparing to start it."

  if tmux has-session -t "$TMUX_SESSION" 2>/dev/null; then
    log "A tmux session named '$TMUX_SESSION' already exists but the port check found nothing listening."
    log "Reusing the existing tmux session as-is rather than starting a second server."
  else
    log "Starting relay server (cargo run --release) in tmux session '$TMUX_SESSION' on port $PORT ..."
    ( cd "$SERVER_DIR" && tmux new-session -d -s "$TMUX_SESSION" "SSSHARE_PORT=$PORT cargo run --release" )
    sleep 1
    log "Server launched. View/attach to its TUI with:"
    log "    tmux attach -t $TMUX_SESSION"
    log "  (Detach again with Ctrl-B then D — this leaves the server running in the background.)"
  fi
fi

# --- step 1.5: generate a ready-to-use join link -------------------------
# Join-string format (see simple-screensharing-client/src/net.rs):
#   server|room-id|key
#   - room-id: 16 random bytes, encoded as 32 lowercase hex characters
#   - key:     32 random bytes (AES-256), encoded as base64url, no padding

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

CLIENT_SESSION_1="${TMUX_SESSION}-client-1"
CLIENT_SESSION_2="${TMUX_SESSION}-client-2"

log "Launching client instance #1 (npm start, SIDECAR_BIN=$SIDECAR_BIN) in tmux session '$CLIENT_SESSION_1' ..."
tmux new-session -d -s "$CLIENT_SESSION_1" "cd '$ELECTRON_DIR' && SIDECAR_BIN='$SIDECAR_BIN' npm start 2>&1 | tee /tmp/sssclient-1.log"

sleep 1

log "Launching client instance #2 (npm start, SIDECAR_BIN=$SIDECAR_BIN) in tmux session '$CLIENT_SESSION_2' ..."
tmux new-session -d -s "$CLIENT_SESSION_2" "cd '$ELECTRON_DIR' && SIDECAR_BIN='$SIDECAR_BIN' npm start 2>&1 | tee /tmp/sssclient-2.log"

sleep 1

# --- step 2.5: watchdog — kill client sessions once the server session ends ---
# Runs as its own detached tmux session (not a bare background process, to stay
# consistent with this script's tmux-for-everything approach) so it keeps
# working even if the user detaches from the server's TUI (Ctrl-B D) and this
# script's own process exits — the watchdog session lives on independently and
# still cleans up the clients whenever/however the server session disappears.

WATCHDOG_SESSION="${TMUX_SESSION}-watchdog"
log "Starting watchdog (tmux session '$WATCHDOG_SESSION') to kill client sessions once '$TMUX_SESSION' ends ..."
tmux kill-session -t "$WATCHDOG_SESSION" 2>/dev/null || true
tmux new-session -d -s "$WATCHDOG_SESSION" "while tmux has-session -t '$TMUX_SESSION' 2>/dev/null; do sleep 2; done; tmux kill-session -t '$CLIENT_SESSION_1' 2>/dev/null; tmux kill-session -t '$CLIENT_SESSION_2' 2>/dev/null; echo 'Relay server session ended — killed client sessions.'"

sleep 1

# --- summary --------------------------------------------------------------

echo
log "All set. Summary:"
log "  Relay server: listening on port $PORT"
log "    - attach to its TUI:  tmux attach -t $TMUX_SESSION"
log "    - stop it (from another terminal): tmux kill-session -t $TMUX_SESSION"
log "  Client windows: 2 Electron instances launched, sidecar: $SIDECAR_BIN"
log "    - session 1: tmux attach -t $CLIENT_SESSION_1  (log: /tmp/sssclient-1.log)"
log "    - session 2: tmux attach -t $CLIENT_SESSION_2  (log: /tmp/sssclient-2.log)"
log "    - stop them: tmux kill-session -t $CLIENT_SESSION_1  /  tmux kill-session -t $CLIENT_SESSION_2"
log "  Client sessions are automatically killed when the relay server session ends"
log "    (watchdog: tmux session '$WATCHDOG_SESSION')"
log "  See all sessions at a glance with: tmux ls"
echo
log "Use one client to start sharing and the other to watch."
log "Join link: $JOIN_STRING"
echo
log "Attaching to the relay server's tmux session now (Ctrl-B then D to detach) ..."
tmux attach -t "$TMUX_SESSION"
