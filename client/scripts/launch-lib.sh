#!/usr/bin/env bash
#
# launch-lib.sh — shared helpers for the repo-root launch-test.sh and
# launch-test-tmux.sh. Source it, don't run it.
#
# Everything here is about running the REAL native stack on a dev machine:
# the relay (`simple-screensharing-server`) and N instances of the egui client
# (`simple-screensharing-client`), each with its own scratch config dir, each
# on a chosen monitor, never on one that is (or overlaps) an HDMI output.
#
# Env vars read here (all optional, see launch-test.sh --help for the list):
#   REPO_ROOT, CARGO_TARGET_DIR, PROFILE, SCRATCH, PORT, CLIENTS, CLIENT_MONITORS,
#   SSS_SKIP_MONITOR_CHECK, GUI_LOCK_FILE, RUST_LOG, START_CODEC/FPS/KBPS, CLIENT_ENV_<n>

SSS_SCRIPTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="${REPO_ROOT:-$(cd "$SSS_SCRIPTS_DIR/../.." && pwd)}"

log() { echo ">> $*"; }
die() { echo "ERROR: $*" >&2; exit 1; }

# ---------------------------------------------------------------------------
# Defaults
# ---------------------------------------------------------------------------
PORT="${PORT:-4433}"
CLIENTS="${CLIENTS:-2}"
# One monitor name per client instance, in order (client 1 = sharer by
# convention, client 2 = viewer). Both default to DP-2 so DP-1 stays free for
# whatever you want to capture there; override for your layout.
CLIENT_MONITORS="${CLIENT_MONITORS:-DP-2 DP-2}"
PROFILE="${PROFILE:-release}"            # release (default; 240 fps needs it) or debug
# Target dir: ON DISK inside the checkout (never /tmp, which is a RAM tmpfs).
# A shared target dir can hold a binary some other worktree built more
# recently, which cargo accepts as fresh; an own-checkout target avoids that.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
SCRATCH="${SCRATCH:-}"                   # filled by sss_make_scratch
GUI_LOCK_FILE="${GUI_LOCK_FILE:-}"       # e.g. /tmp/claude-1000/sss-gui.lock when several agents share one desktop

# ---------------------------------------------------------------------------
# Monitor safety: refuse to put anything on HDMI
# ---------------------------------------------------------------------------

# The compositor's monitors as "name x y w h" lines (MangoWC: mmsg).
sss_monitors() {
  command -v mmsg >/dev/null 2>&1 || return 1
  command -v jq >/dev/null 2>&1 || return 1
  mmsg get all-monitors | jq -r '.monitors[] | "\(.name) \(.x) \(.y) \(.width) \(.height)"'
}

# sss_check_monitors NAME...
# Fails unless every NAME is a real monitor that is not an HDMI output and
# whose rectangle does not overlap any HDMI output's rectangle.
# Why geometry as well as the name: a monitor moved on top of the HDMI one must
# still be refused; the rule is "the user's active session is on that screen",
# not the label.
sss_check_monitors() {
  if [[ "${SSS_SKIP_MONITOR_CHECK:-}" == 1 ]]; then
    log "monitor check skipped (SSS_SKIP_MONITOR_CHECK=1)"; return 0
  fi
  local layout
  if ! layout="$(sss_monitors)" || [[ -z "$layout" ]]; then
    echo "ERROR: cannot read the monitor layout (needs mmsg + jq on MangoWC)." >&2
    echo "       Refusing to place windows blind. Set SSS_SKIP_MONITOR_CHECK=1 to override on another compositor." >&2
    return 1
  fi
  local want n x y w h hx hy hw hh hname bad=0
  for want in "$@"; do
    n=""
    read -r n x y w h < <(awk -v m="$want" '$1==m' <<<"$layout") || true
    if [[ -z "$n" ]]; then
      echo "ERROR: monitor '$want' not found. Layout:" >&2; sed 's/^/         /' <<<"$layout" >&2; bad=1; continue
    fi
    if [[ "$want" =~ ^[Hh][Dd][Mm][Ii] ]]; then
      echo "ERROR: monitor '$want' is an HDMI output; refusing to use it." >&2; bad=1; continue
    fi
    while read -r hname hx hy hw hh; do
      [[ "$hname" =~ ^[Hh][Dd][Mm][Ii] ]] || continue
      if (( x < hx + hw && hx < x + w && y < hy + hh && hy < y + h )); then
        echo "ERROR: monitor '$want' ($x,$y ${w}x$h) overlaps HDMI output $hname ($hx,$hy ${hw}x$hh); refusing." >&2; bad=1
      fi
    done <<<"$layout"
  done
  return $bad
}

# Monitor rect "x y w h" for a name (empty if unknown).
sss_monitor_rect() { sss_monitors | awk -v m="$1" '$1==m {print $2, $3, $4, $5}'; }

# Name of the currently focused monitor (so focus can be restored afterwards).
sss_focused_monitor() { mmsg get all-monitors 2>/dev/null | jq -r '.monitors[] | select(.active) | .name' | head -1; }

# ---------------------------------------------------------------------------
# Optional shared-desktop GUI lock
# ---------------------------------------------------------------------------
# Several agents/people can share one desktop. When GUI_LOCK_FILE is set, hold
# an exclusive flock on it for the lifetime of this script. Every child is
# started with fd 9 closed (`9>&-`, see sss_spawn_client) so a leftover child
# (the client's pw-loopback!) can never keep the lock after this script is gone.
sss_take_gui_lock() {
  [[ -n "$GUI_LOCK_FILE" ]] || return 0
  mkdir -p "$(dirname "$GUI_LOCK_FILE")"
  exec 9>"$GUI_LOCK_FILE"
  log "waiting for the GUI lock ($GUI_LOCK_FILE) ..."
  flock 9
  log "GUI lock held."
}

# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------
sss_build() {
  local flag=""
  [[ "$PROFILE" == release ]] && flag="--release"
  log "building relay + client ($PROFILE) into $CARGO_TARGET_DIR ..."
  ( cd "$REPO_ROOT" && cargo build $flag -p simple-screensharing-server -p simple-screensharing-client ) \
    || die "build failed."
  RELAY_BIN="$CARGO_TARGET_DIR/$PROFILE/simple-screensharing-server"
  CLIENT_BIN="$CARGO_TARGET_DIR/$PROFILE/simple-screensharing-client"
  [[ -x "$RELAY_BIN" && -x "$CLIENT_BIN" ]] || die "expected binaries missing: $RELAY_BIN / $CLIENT_BIN"
  log "relay:  $RELAY_BIN"
  log "client: $CLIENT_BIN"
}

# ---------------------------------------------------------------------------
# Scratch dir, relay state, join link, per-client config
# ---------------------------------------------------------------------------
sss_make_scratch() {
  if [[ -z "$SCRATCH" ]]; then
    # Inside the (gitignored, on-disk) cargo target dir: not /tmp (RAM tmpfs), and
    # not outside the checkout.
    mkdir -p "$CARGO_TARGET_DIR/launch-scratch"
    SCRATCH="$(mktemp -d -p "$CARGO_TARGET_DIR/launch-scratch" run.XXXXXX)"
  else
    mkdir -p "$SCRATCH"
  fi
  SCRATCH="$(cd "$SCRATCH" && pwd)"      # absolute: the relay and clients run from other cwds
  mkdir -p "$SCRATCH/relay"
  log "scratch dir: $SCRATCH"
}

# Relay certificate hash (SHA-256 of the DER leaf, base64url, unpadded) from
# the relay's state dir. Same recipe as docs/dev/relay.sh.
sss_cert_hash() {
  openssl x509 -in "$SCRATCH/relay/state/cert.pem" -outform DER \
    | openssl dgst -sha256 -binary | base64 -w0 | tr '+/' '-_' | tr -d '='
}

# Wait (up to ~10 s) for the relay to write its certificate.
sss_wait_for_cert() {
  local i
  for i in $(seq 1 100); do
    [[ -f "$SCRATCH/relay/state/cert.meta" && -f "$SCRATCH/relay/state/cert.pem" ]] && return 0
    sleep 0.1
  done
  return 1
}

# Join string: server|room-id|key|certhash
#   room-id: 16 random bytes as 32 hex chars; key: 32 random bytes, base64url, no padding.
sss_make_join_string() {
  local room key
  room="$(openssl rand -hex 16)"
  key="$(openssl rand -base64 32 | tr '+/' '-_' | tr -d '=\n')"
  JOIN_STRING="127.0.0.1:${PORT}|${room}|${key}|$(sss_cert_hash)"
}

# sss_seed_config DIR NAME
# Writes DIR/simple-screensharing/{rooms.toml,settings.toml} so the client
# starts already joined to the test room (auto_connect) under display name NAME.
# Optional env SSS_LANGUAGE (en|de, default en) sets the UI language; START_CODEC (h264|h265|av1), START_FPS, START_KBPS pre-fill the
# stream-settings form ([start_stream_defaults] in settings.toml).
sss_seed_config() {
  local dir="$1/simple-screensharing" name="$2"
  mkdir -p "$dir"
  cat >"$dir/rooms.toml" <<TOML
[[rooms]]
name = "launch-test"
join_string = "$JOIN_STRING"
auto_connect = true
TOML
  {
    echo "language = \"${SSS_LANGUAGE:-en}\""
    echo "display_name = \"$name\""
    if [[ -n "${START_CODEC:-}${START_FPS:-}${START_KBPS:-}" ]]; then
      echo "[start_stream_defaults]"
      [[ -n "${START_FPS:-}" ]] && echo "framerate = $START_FPS"
      [[ -n "${START_KBPS:-}" ]] && echo "bitrate_kbps = $START_KBPS"
      [[ -n "${START_CODEC:-}" ]] && echo "codec = \"$START_CODEC\""
    fi
  } >"$dir/settings.toml"
}

# ---------------------------------------------------------------------------
# Spawning and window placement
# ---------------------------------------------------------------------------

sss_client_id_for_pid() { mmsg get all-clients | jq -r --argjson p "$1" '.clients[] | select(.pid==$p) | .id' | head -1; }
sss_client_monitor() { mmsg get all-clients | jq -r --argjson i "$1" '.clients[] | select(.id==$i) | .monitor'; }

# sss_place PID MONITOR
# Waits for PID's window; if it did not open on MONITOR, floats it and moves it
# there (absolute global coordinates inside the monitor rect). Fails if the
# window ends up anywhere but MONITOR.
sss_place() {
  local pid="$1" mon="$2" id="" i x y w h
  for i in $(seq 1 100); do
    id="$(sss_client_id_for_pid "$pid")"
    [[ -n "$id" ]] && break
    sleep 0.2
  done
  [[ -n "$id" ]] || { echo "ERROR: no window for pid $pid after 20 s" >&2; return 1; }
  if [[ "$(sss_client_monitor "$id")" != "$mon" ]]; then
    read -r x y w h <<<"$(sss_monitor_rect "$mon")"
    mmsg dispatch togglefloating client,"$id" >/dev/null
    mmsg dispatch movewin,$((x + 60)),$((y + 60)) client,"$id" >/dev/null
    mmsg dispatch resizewin,$((w - 120)),$((h - 120)) client,"$id" >/dev/null
    sleep 0.5
  fi
  [[ "$(sss_client_monitor "$id")" == "$mon" ]] || { echo "ERROR: window $id (pid $pid) is on '$(sss_client_monitor "$id")', wanted $mon" >&2; return 1; }
  log "  window $id (pid $pid) is on $mon"
}

# sss_spawn_client N MONITOR -> CLIENT_PIDS[N]. Own session (setsid) so the whole
# process group can be killed; fd 9 closed so it never holds the GUI lock.
CLIENT_PIDS=()
sss_spawn_client() {
  local n="$1" mon="$2" cfg="$SCRATCH/cfg$n"
  sss_seed_config "$cfg" "client-$n"
  mmsg dispatch focusmon,"$mon" >/dev/null 2>&1 || true     # new windows open on the focused monitor
  log "starting client #$n on $mon (XDG_CONFIG_HOME=$cfg, log $SCRATCH/client$n.log)"
  # CLIENT_ENV_<n> ("A=1 B=2") adds environment for client n only, e.g. GST_DEBUG_FILE
  # for the sharer (two clients must not write the same file).
  local extra_var="CLIENT_ENV_$n" extra=()
  [[ -n "${!extra_var:-}" ]] && read -r -a extra <<<"${!extra_var}"
  env XDG_CONFIG_HOME="$cfg" RUST_LOG="${RUST_LOG:-info}" "${extra[@]}" setsid "$CLIENT_BIN" 9>&- >"$SCRATCH/client$n.log" 2>&1 &
  CLIENT_PIDS[$n]=$!
  echo "$!" >>"$SCRATCH/client.pids"       # so a different process (trap, watchdog) can stop them
  sss_place "${CLIENT_PIDS[$n]}" "$mon"
}

# Start every client, then give focus back to the monitor that had it.
sss_spawn_clients() {
  local mons=($CLIENT_MONITORS) n prev
  prev="$(sss_focused_monitor)"
  for n in $(seq 1 "$CLIENTS"); do
    sss_spawn_client "$n" "${mons[$(( (n - 1) % ${#mons[@]} ))]}" || return 1
    sleep 1
  done
  [[ -n "$prev" ]] && mmsg dispatch focusmon,"$prev" >/dev/null 2>&1
  return 0
}

# Kill every client (and its pw-loopback / helper children, same process group)
# this script started, read from $SCRATCH/client.pids so it also works from a
# trap or watchdog that never saw the spawn. Only the groups we created: never a
# blanket pkill.
sss_kill_clients() {
  local pid pids=()
  [[ -f "$SCRATCH/client.pids" ]] || return 0
  mapfile -t pids <"$SCRATCH/client.pids"
  for pid in "${pids[@]}"; do
    kill -TERM -- "-$pid" 2>/dev/null || kill -TERM "$pid" 2>/dev/null || true
  done
  sleep 1
  for pid in "${pids[@]}"; do
    kill -KILL -- "-$pid" 2>/dev/null || true
  done
  : >"$SCRATCH/client.pids"
}

sss_print_summary() {
  echo
  log "Join link (already saved as an auto-connect room in each client's scratch config):"
  echo "   $JOIN_STRING"
  if [[ "${COPY_LINK:-}" == 1 ]] && command -v wl-copy >/dev/null 2>&1; then   # opt-in: it overwrites your clipboard
    printf '%s' "$JOIN_STRING" | wl-copy 2>/dev/null && log "(copied to the clipboard)"
  fi
  log "Share a screen in one client; watch it in the other."
  log "Real portal picker: type 'dp' + Enter (never pick an HDMI output)."
  log "Scratch dir (configs, logs, relay state): $SCRATCH"
}
