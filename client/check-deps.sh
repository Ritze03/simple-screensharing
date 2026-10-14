#!/usr/bin/env bash
# Check the Linux system-level dependencies for simple-screensharing-client.
# Exits non-zero if anything required is missing. See README.md "Prerequisites".
set -u

miss=0
ok()   { printf '  \033[32m✓\033[0m %s\n' "$1"; }
bad()  { printf '  \033[31m✗\033[0m %s\n' "$1"; miss=1; }
warn() { printf '  \033[33m!\033[0m %s\n' "$1"; }

# $1 = command, $2 = human note
need_cmd() { command -v "$1" >/dev/null 2>&1 && ok "$1" || bad "$1 — $2"; }
# $1 = pkg-config lib, $2 = note
need_lib() { pkg-config --exists "$1" 2>/dev/null && ok "$1 (pkg-config)" || bad "$1 — $2"; }
# $1 = gstreamer element, $2 = note; warn (not fail) — one of a fallback set may be enough
have_gst() { gst-inspect-1.0 "$1" >/dev/null 2>&1 && ok "gst: $1" || warn "gst: $1 missing — $2"; }

echo "== Build toolchain =="
need_cmd cargo "install rustup"
need_cmd cc    "install base-devel"
need_cmd pkg-config "install pkgconf"
need_cmd node  "install nodejs"
need_cmd npm   "install npm"
need_lib libturbojpeg "install libjpeg-turbo (build sets TURBOJPEG_SOURCE=pkg-config)"

echo "== GStreamer core =="
need_cmd gst-inspect-1.0 "install gstreamer"
need_cmd gst-launch-1.0  "install gstreamer"
need_lib gstreamer-1.0       "install gstreamer"
need_lib gstreamer-app-1.0   "install gst-plugins-base"
need_lib gstreamer-video-1.0 "install gst-plugins-base"

echo "== GStreamer elements =="
have_gst ximagesrc     "gst-plugins-good (X11 screen capture)"
have_gst pipewiresrc   "gst-plugins-good (Wayland/audio capture)"
have_gst x264enc       "gst-plugins-ugly (software H.264 fallback)"
have_gst vah264enc     "gst-plugins-bad + VAAPI driver (hardware H.264 — else falls back to x264enc)"
have_gst opusenc       "gst-plugins-base (audio)"

echo "== PipeWire (audio) =="
need_cmd pw-dump     "install pipewire"
need_cmd pw-link     "install pipewire"
need_cmd pw-loopback "install pipewire (pw-loopback tool)"

echo "== Desktop portal / GPU =="
# xdg-desktop-portal installs to /usr/lib, not PATH.
if command -v xdg-desktop-portal >/dev/null 2>&1 || [ -x /usr/lib/xdg-desktop-portal ]; then
  ok "xdg-desktop-portal"
else
  warn "xdg-desktop-portal missing — needed for the Wayland screen picker"
fi
if [ -e /usr/lib/libvulkan.so.1 ] || command -v vulkaninfo >/dev/null 2>&1; then
  ok "vulkan loader"
else
  warn "vulkan loader missing — wgpu renderer needs vulkan-icd-loader + a GPU driver"
fi

echo
[ "$miss" -eq 0 ] && echo "All required dependencies present." \
  || { echo "Missing required dependencies (see ✗ above)."; exit 1; }
