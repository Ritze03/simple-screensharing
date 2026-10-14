# Builder image for the Linux AppImage (see ../../build-appimage.sh and README.md).
#
# Why a container at all: an AppImage that bundles GStreamer/glib can only run
# on hosts whose glibc is at least as new as the one it was built against.
# Building on the developer's rolling distro (Arch, glibc 2.44) would produce
# an AppImage that only runs on equally fresh systems. Debian 13 (glibc 2.41,
# GStreamer 1.26, PipeWire 1.4, libva 2.22) is the floor: every distro released
# since early 2025 (Fedora 41+, Ubuntu 25.04+/26.04, Debian 13, Arch) satisfies it.
# Override with --build-arg BASE_IMAGE=... (Debian/Ubuntu derivatives only; the
# GStreamer there must have the `va` plugin: >= 1.22, and 1.26 for vaav1enc).
ARG BASE_IMAGE=debian:trixie
FROM ${BASE_IMAGE}

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential pkg-config curl ca-certificates git file patchelf binutils \
        desktop-file-utils librsvg2-bin squashfs-tools \
        libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev libgstreamer-plugins-bad1.0-dev \
        libwayland-dev libxkbcommon-dev libx11-dev libxcb1-dev \
        gstreamer1.0-tools gstreamer1.0-plugins-base gstreamer1.0-plugins-good \
        gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly gstreamer1.0-pipewire \
        dpkg-dev meson ninja-build nasm \
    && rm -rf /var/lib/apt/lists/*

# gst-libav (avdec_h264/avdec_h265, the software decode fallback) built against a
# minimal FFmpeg instead of using Debian's gstreamer1.0-libav.
# Why: Debian's libavcodec links ~100 libraries (x265, SvtAv1, codec2, flite,
# gnutls, krb5, pango, cairo, librsvg, ...): +150 MB, and several of those are on
# the AppImage excludelist (fontconfig, freetype, harfbuzz, ...), i.e. the plugin
# would silently fail to load on a host without them. A decoder-only FFmpeg with
# nothing but H.264 + HEVC and no external deps is ~3 MB and loads anywhere.
# Sources come from Debian's signed deb-src (apt verifies them), which also makes
# gst-libav match the bundled GStreamer core exactly (1.26.2 on trixie). Debian's
# security patches are applied, except its 00_plugin-dependencies (it adds a registry
# dependency on Debian's libavcodec.so.60 path, meaningless here and not buildable).
RUN sed -i 's/^Types: deb$/Types: deb deb-src/' /etc/apt/sources.list.d/*.sources \
    && apt-get update \
    && mkdir /tmp/libav-src && cd /tmp/libav-src \
    && apt-get source --download-only ffmpeg gst-libav1.0 \
    && dpkg-source -x ffmpeg_*.dsc ffmpeg-src \
    && dpkg-source -x --skip-patches gst-libav1.0_*.dsc gst-libav-src \
    && cd ffmpeg-src \
    && ./configure --prefix=/opt/libav --enable-shared --disable-static --enable-pic \
        --disable-programs --disable-doc --disable-debug --disable-autodetect --disable-everything \
        --disable-avdevice --enable-pthreads \
        --enable-avcodec --enable-avformat --enable-avfilter --enable-swresample --enable-swscale \
        --enable-decoder=h264,hevc --enable-parser=h264,hevc \
    && make -j"$(nproc)" && make install \
    && echo /opt/libav/lib > /etc/ld.so.conf.d/libav.conf && ldconfig
RUN cd /tmp/libav-src/gst-libav-src \
    && for p in $(grep -v -e plugin-dependencies -e '^#' -e '^$' debian/patches/series | cut -d' ' -f1); do patch -p1 < "debian/patches/$p"; done \
    && PKG_CONFIG_PATH=/opt/libav/lib/pkgconfig meson setup build --prefix=/opt/libav --libdir=lib \
        --buildtype=release -Dtests=disabled -Ddoc=disabled \
    && ninja -C build && ninja -C build install \
    && rm -rf /tmp/libav-src /var/lib/apt/lists/* \
    && ls -la /opt/libav/lib/gstreamer-1.0/ && ls /opt/libav/lib

# Rust: rustup-init pinned by version + published sha256, then a pinned toolchain.
ARG RUSTUP_VERSION=1.29.1
ARG RUSTUP_SHA256=dda7234360b7f578ca8b0ddcb80145646fa61a67c1720a5abc7051b35c9fcb71
ARG RUST_VERSION=1.99.0
ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo PATH=/opt/cargo/bin:$PATH
RUN curl -fsSL -o /tmp/rustup-init \
        "https://static.rust-lang.org/rustup/archive/${RUSTUP_VERSION}/x86_64-unknown-linux-gnu/rustup-init" \
    && echo "${RUSTUP_SHA256}  /tmp/rustup-init" | sha256sum -c - \
    && chmod +x /tmp/rustup-init \
    && /tmp/rustup-init -y --profile minimal --default-toolchain "${RUST_VERSION}" \
    && rm /tmp/rustup-init && chmod -R a+rX /opt/rustup /opt/cargo

# AppImage tooling: pinned releases, sha256 from the GitHub release asset digest.
# They are AppImages themselves; extract them (no FUSE needed in a container).
ARG LINUXDEPLOY_URL=https://github.com/linuxdeploy/linuxdeploy/releases/download/1-alpha-20251107-1/linuxdeploy-x86_64.AppImage
ARG LINUXDEPLOY_SHA256=c20cd71e3a4e3b80c3483cef793cda3f4e990aca14014d23c544ca3ce1270b4d
ARG APPIMAGETOOL_URL=https://github.com/AppImage/appimagetool/releases/download/1.9.1/appimagetool-x86_64.AppImage
ARG APPIMAGETOOL_SHA256=ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0
# type2-runtime: the small static (libfuse3 built in) stub prepended to the
# squashfs, so the resulting AppImage needs no libfuse2 on the host.
ARG RUNTIME_URL=https://github.com/AppImage/type2-runtime/releases/download/20251108/runtime-x86_64
ARG RUNTIME_SHA256=2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d
RUN set -e; mkdir -p /opt/tools && cd /opt/tools \
    && curl -fsSL -o linuxdeploy.AppImage "${LINUXDEPLOY_URL}" \
    && echo "${LINUXDEPLOY_SHA256}  linuxdeploy.AppImage" | sha256sum -c - \
    && curl -fsSL -o appimagetool.AppImage "${APPIMAGETOOL_URL}" \
    && echo "${APPIMAGETOOL_SHA256}  appimagetool.AppImage" | sha256sum -c - \
    && curl -fsSL -o runtime-x86_64 "${RUNTIME_URL}" \
    && echo "${RUNTIME_SHA256}  runtime-x86_64" | sha256sum -c - \
    && chmod +x linuxdeploy.AppImage appimagetool.AppImage \
    && ./linuxdeploy.AppImage --appimage-extract >/dev/null && mv squashfs-root linuxdeploy \
    && ./appimagetool.AppImage --appimage-extract >/dev/null && mv squashfs-root appimagetool \
    && rm linuxdeploy.AppImage appimagetool.AppImage \
    && ln -s /opt/tools/linuxdeploy/AppRun /usr/local/bin/linuxdeploy \
    && ln -s /opt/tools/appimagetool/AppRun /usr/local/bin/appimagetool \
    && chmod -R a+rX /opt/tools
