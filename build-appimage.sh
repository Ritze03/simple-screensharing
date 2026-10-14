#!/usr/bin/env bash
#
# build-appimage.sh: build the Linux AppImage of the egui client (and a tarball
# of the relay) from this checkout.
#
#   ./build-appimage.sh              # client AppImage + relay tarball
#   ./build-appimage.sh --no-server  # client AppImage only
#
# Output (gitignored):
#   dist/simple-screensharing-client-x86_64.AppImage
#   dist/simple-screensharing-server-x86_64-linux.tar.gz   (unless --no-server)
#   dist/appimage-host-libs.txt   libraries the AppImage expects from the host
#
# Everything is built inside a Debian 13 container (docker or podman, no root
# needed beyond access to the container engine) so the AppImage's glibc floor is
# 2.41 instead of whatever this machine runs. Intermediate files and the cargo
# target live under target/appimage/. See client/packaging/README.md.
#
# Env: CONTAINER_ENGINE=docker|podman (default: first one found)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BUILD_SERVER=1
for arg in "$@"; do
    case "$arg" in
        --no-server) BUILD_SERVER=0 ;;
        -h|--help) sed -n '2,/^set -e/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown argument: $arg (try --help)" >&2; exit 2 ;;
    esac
done

ENGINE="${CONTAINER_ENGINE:-$(command -v docker || command -v podman || true)}"
[ -n "$ENGINE" ] || { echo "ERROR: need docker or podman to build the AppImage" >&2; exit 1; }
ENGINE_NAME="$(basename "$ENGINE")"

# The image is tagged with a hash of its Dockerfile, so editing it rebuilds it
# and an unchanged one is a no-op (layer cache).
IMAGE="sss-appimage-builder:$(sha256sum "$ROOT/client/packaging/Dockerfile" | cut -c1-12)"
echo ">> builder image $IMAGE"
"$ENGINE" build -q -t "$IMAGE" "$ROOT/client/packaging" >/dev/null

mkdir -p "$ROOT/target/appimage/home" "$ROOT/target/appimage/cargo-home" "$ROOT/dist"

user_args=(--user "$(id -u):$(id -g)")
[ "$ENGINE_NAME" = podman ] && user_args=(--userns=keep-id --user "$(id -u):$(id -g)")

echo ">> building inside the container (first run downloads the crates, takes a few minutes)"
"$ENGINE" run --rm "${user_args[@]}" \
    -v "$ROOT":/work -w /work \
    -e HOME=/work/target/appimage/home \
    -e CARGO_HOME=/work/target/appimage/cargo-home \
    -e CARGO_TARGET_DIR=/work/target/appimage/cargo-target \
    -e REPO_ROOT=/work -e BUILD_DIR=/work/target/appimage -e OUT_DIR=/work/dist \
    -e BUILD_SERVER="$BUILD_SERVER" \
    "$IMAGE" bash client/packaging/build-in-container.sh

echo
echo ">> Done. Output in $ROOT/dist:"
ls -lh "$ROOT/dist"
