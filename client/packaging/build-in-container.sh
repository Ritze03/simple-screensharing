#!/usr/bin/env bash
# Builds the release client (and optionally the relay), assembles the AppDir and
# packs the AppImage. Runs INSIDE the builder image (client/packaging/Dockerfile),
# started by the repo-root build-appimage.sh (it relies on the image's /opt/tools,
# /opt/libav and apt packages, so it is not meant to run anywhere else).
#
# Env (all set by build-appimage.sh):
#   REPO_ROOT          repo checkout (default: two levels above this script)
#   BUILD_DIR          scratch dir for AppDir / intermediate files
#   CARGO_TARGET_DIR   cargo target dir
#   OUT_DIR            where the finished artifacts go
#   BUILD_SERVER=1     also build the relay and tar it up (default 1)
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="${REPO_ROOT:-$(cd "$HERE/../.." && pwd)}"
BUILD_DIR="${BUILD_DIR:-$REPO_ROOT/target/appimage}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$BUILD_DIR/cargo-target}"
OUT_DIR="${OUT_DIR:-$REPO_ROOT/dist}"
BUILD_SERVER="${BUILD_SERVER:-1}"
APPDIR="$BUILD_DIR/AppDir"
APPIMAGE_NAME="simple-screensharing-client-x86_64.AppImage"

log() { printf '\n>> %s\n' "$*"; }
die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }

# ---------------------------------------------------------------------------
log "cargo build --release (client$([ "$BUILD_SERVER" = 1 ] && echo ' + relay'))"
packages=(-p simple-screensharing-client)
[ "$BUILD_SERVER" = 1 ] && packages+=(-p simple-screensharing-server)
( cd "$REPO_ROOT" && cargo build --release --locked "${packages[@]}" )
CLIENT_BIN="$CARGO_TARGET_DIR/release/simple-screensharing-client"
[ -x "$CLIENT_BIN" ] || die "client binary missing: $CLIENT_BIN"

mkdir -p "$OUT_DIR"
if [ "$BUILD_SERVER" = 1 ]; then
    log "relay tarball"
    SERVER_BIN="$CARGO_TARGET_DIR/release/simple-screensharing-server"
    tmp="$BUILD_DIR/server-pkg/simple-screensharing-server"
    rm -rf "$BUILD_DIR/server-pkg"; mkdir -p "$tmp"
    install -m755 "$SERVER_BIN" "$tmp/simple-screensharing-server"
    strip "$tmp/simple-screensharing-server" || true
    tar -C "$BUILD_DIR/server-pkg" -czf "$OUT_DIR/simple-screensharing-server-x86_64-linux.tar.gz" simple-screensharing-server
fi

# ---------------------------------------------------------------------------
log "assemble AppDir"
rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/lib/gstreamer-1.0" "$APPDIR/usr/libexec/gstreamer-1.0"
install -m755 "$CLIENT_BIN" "$APPDIR/usr/bin/simple-screensharing-client"
strip "$APPDIR/usr/bin/simple-screensharing-client" || true

PLUGIN_DIR="$(pkg-config --variable=pluginsdir gstreamer-1.0)"
SCANNER_DIR="$(pkg-config --variable=pluginscannerdir gstreamer-1.0)"
# /opt/libav: gst-libav built against a minimal FFmpeg; /opt/pipewire-gst: gst-plugin-pipewire with
# the possible_caps fix (see the Dockerfile and README.md); both win over the distro ones.
SEARCH_DIRS=(/opt/libav/lib/gstreamer-1.0 /opt/pipewire-gst/lib/gstreamer-1.0 "$PLUGIN_DIR")
while read -r plugin; do
    f=""
    for d in "${SEARCH_DIRS[@]}"; do [ -f "$d/libgst${plugin}.so" ] && { f="$d/libgst${plugin}.so"; break; }; done
    [ -n "$f" ] || die "GStreamer plugin not found: libgst${plugin}.so (listed in gst-plugins.txt)"
    install -m755 "$f" "$APPDIR/usr/lib/gstreamer-1.0/"
done < <(sed -e 's/#.*//' -e 's/[[:space:]]*$//' "$HERE/gst-plugins.txt" | grep -v '^$')
# The pipewire plugin must be the patched build, never Debian's (its pipewiresrc dies with
# "stream error: unhandled format" on xdg-desktop-portal-hyprland, see README.md).
cmp -s /opt/pipewire-gst/lib/gstreamer-1.0/libgstpipewire.so "$APPDIR/usr/lib/gstreamer-1.0/libgstpipewire.so" \
    || die "bundled libgstpipewire.so is not the patched /opt/pipewire-gst build"
install -m755 "$SCANNER_DIR/gst-plugin-scanner" "$APPDIR/usr/libexec/gstreamer-1.0/"

# Icon: the brand mark (docs/assets/icon.svg), rendered to PNG for the AppDir root.
rsvg-convert -w 256 -h 256 "$REPO_ROOT/docs/assets/icon.svg" -o "$BUILD_DIR/simple-screensharing.png"

# Libraries that must come from the HOST, on top of linuxdeploy's built-in
# excludelist (glibc, libGL/EGL, libdrm, libX11, wayland-client, libz, ...).
# Reasons, per group, in client/packaging/README.md.
EXCLUDES=(
    # PipeWire client lib: the plugin built here (1.4) talks to the host daemon through
    # the host's libpipewire (linuxdeploy's own excludelist already has it; listed for
    # clarity). SPA plugins, modules and the pw-* tools are the host's too.
    'libpipewire-0.3.so*' 'libspa-*.so*'
    # VA-API: the host's libva matches the host's VA drivers (iHD, radeonsi, ...);
    # a bundled libva would look for drivers in the wrong place/ABI.
    'libva.so*' 'libva-*.so*'
    # GPU stack / windowing: tied to the host's drivers and compositor.
    'libvulkan.so*' 'libdrm*.so*' 'libgbm.so*' 'libGL*.so*' 'libEGL*.so*' 'libOpenGL.so*'
    'libwayland-*.so*' 'libxkbcommon*.so*' 'libX11*.so*' 'libxcb*.so*' 'libXau.so*' 'libXdmcp.so*'
    'libudev.so*' 'libsystemd.so*' 'libdbus-1.so*'
    # The C++/gcc runtimes: host Mesa/Vulkan drivers loaded into our process
    # need the host's (newer) ones; a bundled older libstdc++ would break them.
    'libstdc++.so*' 'libgcc_s.so*'
)
exclude_args=()
for e in "${EXCLUDES[@]}"; do exclude_args+=(--exclude-library "$e"); done

log "linuxdeploy (dependency closure of the client and the bundled plugins)"
export NO_STRIP=1   # the Debian libs are already stripped; old strip chokes on new ELF sections
linuxdeploy --appdir "$APPDIR" \
    --executable "$APPDIR/usr/bin/simple-screensharing-client" \
    --deploy-deps-only "$APPDIR/usr/lib/gstreamer-1.0" \
    --deploy-deps-only "$APPDIR/usr/libexec/gstreamer-1.0" \
    --desktop-file "$REPO_ROOT/simple-screensharing.desktop" \
    --icon-file "$BUILD_DIR/simple-screensharing.png" --icon-filename simple-screensharing \
    --custom-apprun "$HERE/AppRun" \
    "${exclude_args[@]}"
chmod 755 "$APPDIR/AppRun"
desktop-file-validate "$APPDIR/simple-screensharing.desktop"

# ---------------------------------------------------------------------------
log "self-check 1/3: what the AppDir needs from the host"
# DT_NEEDED of every ELF in the AppDir minus what the AppDir itself ships.
find "$APPDIR" -type f -exec sh -c 'file -b "$1" | grep -q "^ELF" && readelf -d "$1" 2>/dev/null' _ {} \; \
    | sed -n 's/.*Shared library: \[\(.*\)\]/\1/p' | sort -u > "$BUILD_DIR/needed.txt"
find "$APPDIR/usr/lib" -maxdepth 1 \( -type f -o -type l \) -printf '%f\n' | sort -u > "$BUILD_DIR/bundled.txt"
comm -23 "$BUILD_DIR/needed.txt" "$BUILD_DIR/bundled.txt" > "$BUILD_DIR/host-libs.txt"
cp "$BUILD_DIR/host-libs.txt" "$OUT_DIR/appimage-host-libs.txt"
sed 's/^/   host: /' "$BUILD_DIR/host-libs.txt"
# Anything outside this list means a library slipped through that should either be
# bundled or deliberately excluded (+ documented in README.md). Fail loudly.
UNEXPECTED="$(grep -vE '^(libc\.so|libm\.so|libdl\.so|libpthread\.so|librt\.so|libutil\.so|libresolv\.so|ld-linux|libstdc\+\+\.so|libgcc_s\.so|libz\.so|libpipewire-0\.3\.so|libva\.so|libva-drm\.so|libvulkan\.so|libdrm\.so|libudev\.so|libsystemd\.so|libdbus-1\.so|libX11|libxcb|libwayland|libxkbcommon|libGL|libEGL|libgbm|libXau|libXdmcp|libOpenGL)' "$BUILD_DIR/host-libs.txt" || true)"
[ -z "$UNEXPECTED" ] || die "AppDir needs host libraries that are neither bundled nor on the expected-host list:
$UNEXPECTED"

log "self-check 2/3: RPATHs"
readelf -d "$APPDIR/usr/bin/simple-screensharing-client" | grep -E 'RPATH|RUNPATH' || die "client has no RPATH/RUNPATH; bundled libs would not be found"
readelf -d "$APPDIR/usr/lib/gstreamer-1.0/libgstpipewire.so" | grep -E 'RPATH|RUNPATH' || die "plugins have no RPATH/RUNPATH"

log "self-check 3/3: bundled plugins provide every required element (host plugins disabled)"
(
    unset GST_PLUGIN_PATH GST_PLUGIN_PATH_1_0
    export GST_PLUGIN_SYSTEM_PATH_1_0="$APPDIR/usr/lib/gstreamer-1.0"
    export GST_PLUGIN_SCANNER_1_0="$APPDIR/usr/libexec/gstreamer-1.0/gst-plugin-scanner"
    export GST_REGISTRY_1_0="$BUILD_DIR/check-registry.bin"; rm -f "$GST_REGISTRY_1_0"
    # gst-inspect is the container's own build of the same GStreamer that was
    # bundled; only the plugin search path is redirected into the AppDir.
    bad=0
    while read -r el; do
        gst-inspect-1.0 "$el" >/dev/null 2>&1 || { echo "   MISSING: $el"; bad=1; }
    done < <(sed -e 's/#.*//' "$HERE/gst-elements.txt" | grep -v '^[[:space:]]*$')
    [ "$bad" = 0 ] || exit 1
) || die "bundled plugin set is missing required elements"

# ---------------------------------------------------------------------------
log "appimagetool"
OUT="$OUT_DIR/$APPIMAGE_NAME"
rm -f "$OUT"
ARCH=x86_64 appimagetool --no-appstream --runtime-file /opt/tools/runtime-x86_64 "$APPDIR" "$OUT"
chmod 755 "$OUT"
ls -lh "$OUT"
du -sh "$APPDIR"
