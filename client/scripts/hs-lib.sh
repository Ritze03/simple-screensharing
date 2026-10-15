#!/usr/bin/env bash
#
# hs-lib.sh — shared helpers of the HEADLESS test rig (hs-rig.sh, hs-drive.sh,
# hs-scenario.sh, hs-probe.sh). Source it, don't run it.
#
# The rig is a complete private desktop that never touches the user's real one:
#
#   sway (WLR_BACKENDS=headless)      3 virtual outputs, no input devices
#     HEADLESS-1  1920x1080@240Hz     the CAPTURED output: fullscreen vkcube (mailbox) on it
#     HEADLESS-2  1920x1080           client 1 = SHARER
#     HEADLESS-3  1920x1080           client 2 = VIEWER
#   private dbus session bus          $RT/bus
#   private PipeWire + WirePlumber    $RT/pipewire-0   (HS_PIPEWIRE=shared symlinks the user's instead)
#   xdg-desktop-portal-wlr            chooser_type=none, output_name=HEADLESS-1: a ScreenCast
#                                     request is granted with NO picker UI
#   xdg-desktop-portal                private instance on the private bus, ScreenCast=wlr
#   hs-vptr                           a virtual pointer so the seat has a pointer capability
#   relay                             in a uniquely named tmux session (its TUI needs a pty)
#   two egui clients                  scratch XDG_* dirs, auto-connected to the same room,
#                                     SSS_UI_DUMP on, audio sharing off
#
# Everything lives under ONE private XDG_RUNTIME_DIR ($RT = /run/user/<uid>/sss-hs-<hash>), so a
# stray command can never reach the real session: hs_export_env points XDG_RUNTIME_DIR at $RT
# and WAYLAND_DISPLAY at the ABSOLUTE socket path inside it ("wayland-0" and "wayland-1" are also
# names of real compositor sockets, so a relative name would be an accident waiting to happen).
#
# State: $SCRATCH/hs.env (paths, port, join link), $SCRATCH/hs.pids (name pid, one per process
# group started with setsid), logs $SCRATCH/<name>.log.
#
# Env (all optional): SCRATCH, HS_PORT (relay UDP port, default a random free one), PROFILE (release|debug, default release), CARGO_TARGET_DIR,
#   HS_NO_BUILD=1, HS_PIPEWIRE=private|shared, HS_XDPW=stock|patched|<path> (see hs_xdpw_bin), HS_PATTERN=vkcube|gst|none, HS_FPS (capture
#   output refresh, default 240), HS_MAX_FPS (xdpw max_fps, default = HS_FPS), RUST_LOG,
#   START_CODEC/START_FPS/START_KBPS (pre-fill the stream form), SSS_LANGUAGE, HS_TRACE=0 (no
#   GStreamer stats tracer on the sharer).

HS_SCRIPTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# launch-lib.sh: log/die, REPO_ROOT, CARGO_TARGET_DIR, PROFILE, sss_build, sss_make_scratch,
# sss_cert_hash/sss_wait_for_cert, sss_make_join_string, sss_seed_config. Its mmsg/monitor
# functions are for the legacy real-desktop scripts and are never called here.
source "$HS_SCRIPTS_DIR/launch-lib.sh"

HS_UID="$(id -u)"
SCRATCH="${SCRATCH:-$CARGO_TARGET_DIR/hs-scratch/rig}"
mkdir -p "$SCRATCH"
SCRATCH="$(cd "$SCRATCH" && pwd)"
HS_STATE="$SCRATCH/hs.env"
HS_PIDS="$SCRATCH/hs.pids"

# Output layout (global pixel coordinates). Fixed on purpose: hs-drive.sh relies on it.
HS_OUT_CAP=HEADLESS-1   HS_CAP_X=0     HS_CAP_Y=0
HS_OUT_SHARER=HEADLESS-2 HS_SHARER_X=1920 HS_SHARER_Y=0
HS_OUT_VIEWER=HEADLESS-3 HS_VIEWER_X=3840 HS_VIEWER_Y=0
HS_W=1920 HS_H=1080

# --- state ---------------------------------------------------------------------------------

# hs_rt: the private runtime dir for this SCRATCH (stable per scratch dir).
hs_rt() {
  local h; h="$(printf %s "$SCRATCH" | md5sum | head -c 8)"
  echo "/run/user/$HS_UID/sss-hs-$h"
}

# Load the state file of a running/previous rig into the shell (RT, PORT, JOIN_STRING, ...).
hs_load_state() {
  [[ -f "$HS_STATE" ]] || die "no rig state in $SCRATCH (run: hs-rig.sh up). SCRATCH is the scratch dir of the rig."
  # shellcheck disable=SC1090
  source "$HS_STATE"
}

# Point every tool at the PRIVATE desktop. Call before any swaymsg/grim/wtype/client launch.
hs_export_env() {
  [[ -n "${RT:-}" ]] || die "hs_export_env: RT not set (hs_load_state first)"
  export XDG_RUNTIME_DIR="$RT"
  export WAYLAND_DISPLAY="${WAYLAND_SOCK:-$RT/wayland-not-up-yet}"
  export SWAYSOCK="$RT/sway.sock"
  export DBUS_SESSION_BUS_ADDRESS="unix:path=$RT/bus"
  export XDG_CURRENT_DESKTOP=sway
  unset DISPLAY WAYLAND_SOCKET HYPRLAND_INSTANCE_SIGNATURE NIRI_SOCKET I3SOCK DESKTOP_STARTUP_ID
}

# Refuse to continue unless the environment really is the private rig (belt and braces).
hs_assert_private() {
  [[ "${XDG_RUNTIME_DIR:-}" == "$RT" && "${WAYLAND_DISPLAY:-}" == "$RT"/wayland-* && -S "${WAYLAND_DISPLAY:-}" ]] \
    || die "not pointed at the private headless sway (XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-} WAYLAND_DISPLAY=${WAYLAND_DISPLAY:-}); refusing."
}

hs_sway() { swaymsg "$@"; }

# --- process bookkeeping -------------------------------------------------------------------

# hs_spawn NAME CMD...: own session (setsid) so the whole group can be killed, log to
# $SCRATCH/NAME.log, pid recorded in hs.pids. Extra environment: prefix the call with `env`.
hs_spawn() {
  local name="$1"; shift
  setsid "$@" >"$SCRATCH/$name.log" 2>&1 </dev/null &
  echo "$name $!" >>"$HS_PIDS"
  eval "HS_PID_${name//[^A-Za-z0-9_]/_}=$!"
}

hs_pid_of() { awk -v n="$1" '$1==n {p=$2} END{print p}' "$HS_PIDS" 2>/dev/null; }
hs_alive() { local p; p="$(hs_pid_of "$1")"; [[ -n "$p" ]] && kill -0 "$p" 2>/dev/null; }

# Kill every recorded process group, newest first (clients before the compositor).
hs_kill_all() {
  [[ -f "$HS_PIDS" ]] || return 0
  local name pid
  while read -r name pid; do
    [[ -n "$pid" ]] || continue
    kill -TERM -- "-$pid" 2>/dev/null || kill -TERM "$pid" 2>/dev/null || true
  done < <(tac "$HS_PIDS")
  sleep 1.5
  while read -r name pid; do
    [[ -n "$pid" ]] || continue
    kill -KILL -- "-$pid" 2>/dev/null || true
  done < <(tac "$HS_PIDS")
  : >"$HS_PIDS"
}

# --- waiting -------------------------------------------------------------------------------

# hs_wait SECS DESCRIPTION CMD...: poll CMD every 0.2 s until it succeeds.
hs_wait() {
  local secs="$1" what="$2" i; shift 2
  for ((i = 0; i < secs * 5; i++)); do
    if "$@" >/dev/null 2>&1; then return 0; fi
    sleep 0.2
  done
  die "timed out after ${secs}s waiting for $what (logs in $SCRATCH)"
}

# --- sway ----------------------------------------------------------------------------------

# Window (container) JSON of a pid, from the sway tree.
hs_win_json() {
  swaymsg -t get_tree | jq -c --argjson p "$1" '[.. | objects | select((.type? == "con" or .type? == "floating_con") and .pid? == $p)] | first // empty'
}
# wtype drops the first key of every run in the headless sway ("Second" arrives as "econd", a lone
# F11 does nothing). Always type through this: a dummy Shift_L press first, then a pause.
hs_type() { wtype -k Shift_L -s 300 "$@"; }
# "x y w h" of the window CONTENT in global pixels (borders are off, so rect == content).
hs_win_rect() {
  local j; j="$(hs_win_json "$1")"
  [[ -n "$j" ]] || return 1
  jq -r '"\(.rect.x + .window_rect.x) \(.rect.y + .window_rect.y) \(.window_rect.width) \(.window_rect.height)"' <<<"$j"
}
hs_win_output() {
  swaymsg -t get_tree | jq -r --argjson p "$1" '.nodes[] | select(.type=="output") | . as $o | [.. | objects | select((.type? == "con" or .type? == "floating_con") and .pid? == $p)] | select(length>0) | $o.name' | head -1
}

# --- build ---------------------------------------------------------------------------------

hs_build() {
  if [[ "${HS_NO_BUILD:-}" == 1 ]]; then
    RELAY_BIN="$CARGO_TARGET_DIR/$PROFILE/simple-screensharing-server"
    CLIENT_BIN="$CARGO_TARGET_DIR/$PROFILE/simple-screensharing-client"
    [[ -x "$RELAY_BIN" && -x "$CLIENT_BIN" ]] || die "HS_NO_BUILD=1 but binaries are missing in $CARGO_TARGET_DIR/$PROFILE"
    log "using existing binaries in $CARGO_TARGET_DIR/$PROFILE"
  else
    sss_build
  fi
}

# hs-vptr: wayland-scanner + gcc, into $SCRATCH/bin (a few KB; rebuilt when the sources change).
hs_build_vptr() {
  mkdir -p "$SCRATCH/bin"
  local out="$SCRATCH/bin/hs-vptr" src="$HS_SCRIPTS_DIR/hs-vptr.c" xml="$HS_SCRIPTS_DIR/hs-vptr-protocol.xml"
  if [[ -x "$out" && "$out" -nt "$src" && "$out" -nt "$xml" ]]; then return 0; fi
  command -v wayland-scanner >/dev/null && command -v gcc >/dev/null || die "need wayland-scanner and gcc to build hs-vptr"
  wayland-scanner client-header "$xml" "$SCRATCH/bin/hs-vptr-protocol.h"
  wayland-scanner private-code "$xml" "$SCRATCH/bin/hs-vptr-protocol.c"
  gcc -O1 -I"$SCRATCH/bin" -o "$out" "$src" "$SCRATCH/bin/hs-vptr-protocol.c" -lwayland-client \
    || die "building hs-vptr failed"
}

# --- config files --------------------------------------------------------------------------

hs_write_sway_conf() {
  cat >"$SCRATCH/sway.conf" <<EOF
# Generated by hs-lib.sh: private headless sway for the screensharing test rig.
xwayland disable
default_border none
default_floating_border none
focus_follows_mouse no
swaybg_command /usr/bin/true
workspace 1 output $HS_OUT_CAP
workspace 2 output $HS_OUT_SHARER
workspace 3 output $HS_OUT_VIEWER
# the test pattern lands on the captured output whatever has focus
for_window [title="vkcube"] move to workspace 1, fullscreen enable
for_window [title="hs-pattern"] move to workspace 1, fullscreen enable
EOF
}

hs_write_portal_conf() {
  local fps="${HS_MAX_FPS:-${HS_FPS:-240}}"
  cat >"$SCRATCH/xdpw.conf" <<EOF
[screencast]
chooser_type=none
output_name=$HS_OUT_CAP
max_fps=$fps
${HS_FORCE_MOD_LINEAR:+force_mod_linear=1}
EOF
  mkdir -p "$SCRATCH/xdgcfg/xdg-desktop-portal"
  cat >"$SCRATCH/xdgcfg/xdg-desktop-portal/portals.conf" <<EOF
[preferred]
default=none
org.freedesktop.impl.portal.ScreenCast=wlr
EOF
  cat >"$SCRATCH/bus.conf" <<EOF
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN" "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path=$RT/bus</listen>
  <auth>EXTERNAL</auth>
  <!-- no <servicedir>: nothing is ever bus-activated, every service is started by hs-rig.sh -->
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
EOF
}

# --- bring-up pieces ------------------------------------------------------------------------

hs_start_sway() {
  hs_write_sway_conf
  log "starting headless sway (log $SCRATCH/sway.log)"
  # The IPC socket name is fixed via SWAYSOCK so no scanning of the runtime dir is needed.
  hs_spawn sway env -u WAYLAND_DISPLAY XDG_RUNTIME_DIR="$RT" SWAYSOCK="$RT/sway.sock" WLR_BACKENDS=headless \
    WLR_LIBINPUT_NO_DEVICES=1 sway -c "$SCRATCH/sway.conf"
  # The compositor picks its own socket name (wayland-1 here, not -0): find it.
  hs_wait 20 "sway's wayland socket" bash -c "compgen -G '$RT/wayland-[0-9]' >/dev/null"
  WAYLAND_SOCK="$(compgen -G "$RT/wayland-[0-9]" | head -1)"
  echo "WAYLAND_SOCK='$WAYLAND_SOCK'" >>"$HS_STATE"
  hs_export_env
  hs_wait 20 "sway's IPC socket" swaymsg -t get_version
  # Outputs: the backend starts with HEADLESS-1; create the other two.
  local n
  for n in 2 3; do swaymsg create_output >/dev/null; done
  hs_wait 10 "three headless outputs" bash -c "[[ \$(swaymsg -t get_outputs | jq length) -ge 3 ]]"
  local fps="${HS_FPS:-240}"
  swaymsg "output $HS_OUT_CAP mode ${HS_W}x${HS_H}@${fps}Hz pos $HS_CAP_X $HS_CAP_Y" >/dev/null
  swaymsg "output $HS_OUT_SHARER mode ${HS_W}x${HS_H}@60Hz pos $HS_SHARER_X $HS_SHARER_Y" >/dev/null
  swaymsg "output $HS_OUT_VIEWER mode ${HS_W}x${HS_H}@60Hz pos $HS_VIEWER_X $HS_VIEWER_Y" >/dev/null
  log "outputs: $(swaymsg -t get_outputs | jq -r '[.[] | "\(.name) \(.current_mode.width)x\(.current_mode.height)@\(.current_mode.refresh/1000|floor)Hz at \(.rect.x),\(.rect.y)"] | join("; ")')"
}

hs_start_bus() {
  log "starting private dbus session bus"
  hs_spawn bus dbus-daemon --config-file="$SCRATCH/bus.conf" --nofork --nopidfile
  hs_wait 10 "the private bus socket" test -S "$RT/bus"
}

hs_start_pipewire() {
  if [[ "${HS_PIPEWIRE:-private}" == shared ]]; then
    # The user's PipeWire through symlinks in the private runtime dir. Screencast nodes then show
    # up in the user's graph; and a stuck native stream there (task #48) wedges linking for us too.
    log "using the user's PipeWire (HS_PIPEWIRE=shared)"
    ln -sfn "/run/user/$HS_UID/pipewire-0" "$RT/pipewire-0"
    ln -sfn "/run/user/$HS_UID/pipewire-0-manager" "$RT/pipewire-0-manager"
    return 0
  fi
  log "starting private PipeWire + WirePlumber (policy profile: no audio/bluetooth/v4l2 devices touched)"
  mkdir -p "$SCRATCH/xdgcfg" "$SCRATCH/xdgstate"
  hs_spawn pipewire env XDG_CONFIG_HOME="$SCRATCH/xdgcfg" XDG_STATE_HOME="$SCRATCH/xdgstate" pipewire
  hs_wait 10 "private PipeWire" test -S "$RT/pipewire-0"
  hs_spawn wireplumber env XDG_CONFIG_HOME="$SCRATCH/xdgcfg" XDG_STATE_HOME="$SCRATCH/xdgstate" wireplumber -p policy
  hs_wait 15 "WirePlumber's linking policy" bash -c "pw-dump | jq -e '.[] | select(.type==\"PipeWire:Interface:Metadata\")' >/dev/null"
}

# Which xdg-desktop-portal-wlr to run. Default: the stock /usr/lib/xdg-desktop-portal-wlr, what
# ordinary sway users run (0.8.4 captures through ext-image-copy-capture), so the rig covers it.
# HS_XDPW=patched picks the user's own build ~/.local/bin/xdg-desktop-portal-wlr-patched (wlr-screencopy
# only); HS_XDPW=<path> runs any binary; HS_XDPW=stock is the default, spelled out.
# Why stock is the default: it used to crash the share, because pipewiresrc takes its stream
# streaming -> paused -> streaming at start and, behind the client's heavy GPU tail, the resume
# landed after the portal had asked for its first frame; stock xdpw then creates a second frame object
#   "ext_image_copy_capture_session_v1#18: error 1: session already has a frame object"
# (wl_display_dispatch failed: Protocol error), the portal node is gone ("target not found",
# "GPU capture path failed to start ... StateChangeError"). Since #52 the client puts a queue behind
# pipewiresrc (capture::decoupled_source), the flap lands before any frame request and xdpw survives.
# portal-probe.py never flapped because its tail is cheap (no heavy chain to delay the caps push).
hs_xdpw_bin() {
  local alt="$HOME/.local/bin/xdg-desktop-portal-wlr-patched"
  case "${HS_XDPW:-}" in
    ""|stock) echo /usr/lib/xdg-desktop-portal-wlr ;;
    patched)  [[ -x "$alt" ]] || die "HS_XDPW=patched but $alt is missing"; echo "$alt" ;;
    *)        echo "$HS_XDPW" ;;
  esac
}

hs_start_portal() {
  log "starting $(hs_xdpw_bin) (chooser_type=none) and xdg-desktop-portal on the private bus"
  hs_spawn xdpw env XDG_CONFIG_HOME="$SCRATCH/xdgcfg" XDG_STATE_HOME="$SCRATCH/xdgstate" \
    "$(hs_xdpw_bin)" -c "$SCRATCH/xdpw.conf" -l INFO
  hs_wait 10 "xdpw on the bus" bash -c "busctl --address='unix:path=$RT/bus' list | grep -q '^org.freedesktop.impl.portal.desktop.wlr '"
  hs_spawn xdp env XDG_CONFIG_HOME="$SCRATCH/xdgcfg" XDG_STATE_HOME="$SCRATCH/xdgstate" \
    /usr/lib/xdg-desktop-portal -v
  hs_wait 15 "xdg-desktop-portal on the bus" bash -c "busctl --address='unix:path=$RT/bus' list | grep -q '^org.freedesktop.portal.Desktop '"
}

hs_start_vptr() {
  hs_build_vptr
  hs_spawn vptr "$SCRATCH/bin/hs-vptr"
  hs_wait 10 "a seat with a pointer capability" bash -c "swaymsg -t get_seats | jq -e '(.[0].capabilities % 2) == 1' >/dev/null"
}

hs_start_pattern() {
  case "${HS_PATTERN:-vkcube}" in
    none) return 0 ;;
    vkcube)
      # MAILBOX present mode: renders unthrottled, the compositor shows the newest frame at every
      # refresh, so the captured output has new content at its full refresh rate (an idle output
      # delivers no frames to the portal: capture is damage-driven).
      swaymsg "workspace number 1" >/dev/null
      hs_spawn pattern vkcube --wsi wayland --present_mode 1 --width "$HS_W" --height "$HS_H" --suppress_popups ;;
    gst)
      swaymsg "workspace number 1" >/dev/null
      hs_spawn pattern gst-launch-1.0 videotestsrc pattern=ball '!' "video/x-raw,width=$HS_W,height=$HS_H,framerate=300/1" \
        '!' timeoverlay '!' waylandsink fullscreen=true sync=false ;;
  esac
  hs_wait 20 "the test pattern window on $HS_OUT_CAP" bash -c "[[ \"\$(swaymsg -t get_tree | jq -r '[.nodes[] | select(.name==\"$HS_OUT_CAP\") | .. | objects | select(.type? == \"con\" and .pid? != null)] | length')\" -ge 1 ]]"
  sleep 1
}

hs_start_relay() {
  command -v tmux >/dev/null || die "tmux is needed (the relay's TUI wants a pty)"
  sss_make_scratch >/dev/null            # SCRATCH is set: just mkdir $SCRATCH/relay
  [[ -f "$SCRATCH/relay/state/cert.pem" ]] && rm -rf "$SCRATCH/relay/state"   # fresh cert per run
  TMUX_SESSION="$(basename "$RT")-$$"
  echo "$TMUX_SESSION" >"$SCRATCH/tmux.session"
  log "starting the relay on UDP $PORT in tmux session $TMUX_SESSION"
  tmux new-session -d -s "$TMUX_SESSION" -c "$SCRATCH/relay" "exec '$RELAY_BIN' $PORT 2>>'$SCRATCH/relay.err'"
  sss_wait_for_cert || die "the relay wrote no certificate in 10 s (see $SCRATCH/relay.err)"
  sss_make_join_string
}

# hs_free_port: a UDP port nobody listens on.
hs_free_port() {
  local p i
  for i in $(seq 1 50); do
    p=$((20000 + RANDOM % 20000))
    ss -lun | awk '{print $5}' | grep -q ":$p\$" || { echo "$p"; return; }
  done
  die "no free UDP port found"
}

# hs_start_client N WORKSPACE OUTPUT
hs_start_client() {
  local n="$1" ws="$2" out="$3" cfg extra_var name_var extra=() pid i
  cfg="$SCRATCH/cfg$n"; extra_var="CLIENT_ENV_$n"; name_var="HS_NAME_$n"
  sss_seed_config "$cfg" "${!name_var:-client-$n}"   # HS_NAME_1/HS_NAME_2 override the display name
  mkdir -p "$SCRATCH/data$n"
  rm -f "$SCRATCH/ui$n.txt"
  [[ -n "${!extra_var:-}" ]] && read -r -a extra <<<"${!extra_var}"
  swaymsg "workspace number $ws" >/dev/null     # a new window opens on the focused workspace
  log "starting client #$n on $out (config $cfg, log $SCRATCH/client$n.log)"
  hs_spawn "client$n" env XDG_CONFIG_HOME="$cfg" XDG_DATA_HOME="$SCRATCH/data$n" \
    RUST_LOG="${RUST_LOG:-info}" SSS_UI_DUMP="$SCRATCH/ui$n.txt" \
    PULSE_SERVER="unix:$RT/no-pulse" "${extra[@]}" "$CLIENT_BIN"
  pid="$(hs_pid_of "client$n")"
  for ((i = 0; i < 150; i++)); do
    [[ -n "$(hs_win_json "$pid")" ]] && break
    kill -0 "$pid" 2>/dev/null || die "client #$n died on start (see $SCRATCH/client$n.log)"
    sleep 0.2
  done
  [[ -n "$(hs_win_json "$pid")" ]] || die "client #$n opened no window in 30 s"
  [[ "$(hs_win_output "$pid")" == "$out" ]] || die "client #$n is on '$(hs_win_output "$pid")', wanted $out"
  log "  client #$n window up on $out: $(hs_win_rect "$pid")"
}

hs_summary() {
  echo
  log "Rig is up. Scratch dir: $SCRATCH   private runtime dir: $RT"
  log "  sharer = client 1 on $HS_OUT_SHARER, viewer = client 2 on $HS_OUT_VIEWER, captured output $HS_OUT_CAP (test pattern)"
  log "  join link (already saved as an auto-connect room in both clients):"
  echo "    $JOIN_STRING"
  log "  next:  hs-drive.sh share|setup|golive|watch|hover|shot ...   (SCRATCH=$SCRATCH)"
  log "  run any command inside the rig's environment:  SCRATCH=$SCRATCH hs-rig.sh run -- grim -o $HS_OUT_VIEWER /tmp/x.png"
  log "  tear down:  SCRATCH=$SCRATCH hs-rig.sh down"
}
