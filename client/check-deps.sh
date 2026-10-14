#!/usr/bin/env bash
# Check the Linux system-level dependencies for simple-screensharing-client
# (the native egui app: one Rust process, GStreamer + PipeWire + Vulkan).
# Exits non-zero if anything required is missing; "!" lines are optional or
# have a fallback. See README.md "Prerequisites".
set -u

miss=0
ok()   { printf '  \033[32m✓\033[0m %s\n' "$1"; }
bad()  { printf '  \033[31m✗\033[0m %s\n' "$1"; miss=1; }
warn() { printf '  \033[33m!\033[0m %s\n' "$1"; }

# $1 = command, $2 = human note
need_cmd() { command -v "$1" >/dev/null 2>&1 && ok "$1" || bad "$1 — $2"; }
# $1 = pkg-config lib, $2 = note
need_lib() { pkg-config --exists "$1" 2>/dev/null && ok "$1 (pkg-config)" || bad "$1 — $2"; }
# $1 = gstreamer element, $2 = note; required
need_gst() { gst-inspect-1.0 "$1" >/dev/null 2>&1 && ok "gst: $1" || bad "gst: $1 missing — $2"; }
# $1 = gstreamer element, $2 = note; warn only (a fallback exists, or it is optional)
have_gst() { gst-inspect-1.0 "$1" >/dev/null 2>&1 && ok "gst: $1" || warn "gst: $1 missing — $2"; }

echo "== Build toolchain =="
need_cmd cargo "install rustup"
need_cmd rustc "install rustup"
need_cmd cc    "install base-devel"
need_cmd pkg-config "install pkgconf"

echo "== GStreamer core / base / bad dev libs =="
# The Rust crates (gstreamer, gstreamer-app, gstreamer-video) link these via pkg-config.
need_lib gstreamer-1.0               "install gstreamer (core)"
need_lib gstreamer-base-1.0          "install gstreamer (core)"
need_lib gstreamer-plugins-base-1.0  "install gst-plugins-base"
need_lib gstreamer-app-1.0           "install gst-plugins-base"
need_lib gstreamer-video-1.0         "install gst-plugins-base"
# gst-plugins-good ships no .pc file; its elements are checked below.
need_lib gstreamer-plugins-bad-1.0   "install gst-plugins-bad"

echo "== GStreamer tools =="
need_cmd gst-inspect-1.0 "install gstreamer"
need_cmd gst-launch-1.0  "install gstreamer"

echo "== GStreamer elements (required) =="
need_gst appsink      "gst-plugins-base (app)"
need_gst appsrc       "gst-plugins-base (app)"
need_gst videoconvert "gst-plugins-base"
need_gst videoscale   "gst-plugins-base"
need_gst videorate    "gst-plugins-base"
need_gst audioconvert "gst-plugins-base"
need_gst audioresample "gst-plugins-base"
need_gst valve        "gstreamer core (coreelements)"
need_gst capssetter   "gst-plugins-good"
need_gst level        "gst-plugins-good"
need_gst volume       "gst-plugins-base"
need_gst h264parse    "gst-plugins-bad"
need_gst h265parse    "gst-plugins-bad"
need_gst av1parse     "gst-plugins-bad"

echo "== PipeWire / Opus (screen + audio capture, audio playback) =="
need_gst pipewiresrc  "pipewire gstreamer plugin (Arch: gst-plugin-pipewire)"
need_gst opusenc      "gst-plugins-base (opus)"
need_gst opusdec      "gst-plugins-base (opus)"
need_cmd pw-dump      "install pipewire"
need_cmd pw-link      "install pipewire"
need_cmd pw-loopback  "install pipewire (pw-loopback tool)"

echo "== VA-API hardware codecs (gst-plugins-bad 'va' + a VA driver) =="
# Optional in the sense that software x264enc / avdec_* are used when missing,
# but software cannot sustain the 120-240 fps target: a warning here means
# degraded streaming, not a broken install.
for el in vah264enc vah264dec vah265enc vah265dec vaav1enc vaav1dec vapostproc; do
  have_gst "$el" "install gst-plugins-bad (va) + your GPU's VA driver (mesa / intel-media-driver); falls back to software"
done
have_gst x264enc   "gst-plugins-ugly (software H.264 encode fallback)"
have_gst avdec_h264 "gst-libav (software H.264 decode fallback)"

echo "== Window system / GPU (eframe + wgpu) =="
need_lib xkbcommon      "install libxkbcommon (eframe/winit keyboard input)"
need_lib wayland-client "install wayland (Wayland window backend)"
need_lib x11            "install libx11 (X11 window backend)"
need_lib xcb            "install libxcb (X11 window backend)"
if [ -e /usr/lib/libvulkan.so.1 ] || ldconfig -p 2>/dev/null | grep -q 'libvulkan\.so\.1' || command -v vulkaninfo >/dev/null 2>&1; then
  ok "vulkan loader"
else
  bad "vulkan loader missing — the wgpu renderer needs vulkan-icd-loader + a GPU driver"
fi

echo "== Desktop portal (Wayland screen picker) =="
# xdg-desktop-portal installs to /usr/lib, not PATH.
if command -v xdg-desktop-portal >/dev/null 2>&1 || [ -x /usr/lib/xdg-desktop-portal ]; then
  ok "xdg-desktop-portal"
else
  warn "xdg-desktop-portal missing — needed for the Wayland screen picker"
fi

echo
[ "$miss" -eq 0 ] && echo "All required dependencies present." \
  || { echo "Missing required dependencies (see ✗ above)."; exit 1; }
