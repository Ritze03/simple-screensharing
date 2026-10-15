#!/usr/bin/env bash
#
# build-flatpak.sh: build the Flatpak bundle of the egui client from this checkout.
#
#   ./build-flatpak.sh
#
# Output (gitignored):
#   dist/simple-screensharing-client.flatpak   single-file bundle
#
# Install and run:
#   flatpak install --user ./dist/simple-screensharing-client.flatpak
#   flatpak run io.github.ritze03.SimpleScreensharing
#
# Needs flatpak-builder (host package, or `flatpak install --user flathub org.flatpak.Builder`),
# the org.freedesktop.Platform/Sdk 25.08 runtimes and the rust-stable SDK extension:
#   flatpak install --user flathub org.freedesktop.Platform//25.08 org.freedesktop.Sdk//25.08 \
#       org.freedesktop.Sdk.Extension.rust-stable//25.08
# The build downloads crates (--share=network, see the manifest). Build dir, state and repo live
# under target/flatpak-*/ (on disk, never /tmp). See superdoc/features/flatpak.md.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
APP_ID=io.github.ritze03.SimpleScreensharing
MANIFEST="$ROOT/client/packaging/flatpak/$APP_ID.yml"
BUILD="$ROOT/target/flatpak-build" STATE="$ROOT/target/flatpak-state" REPO="$ROOT/target/flatpak-repo"
OUT="$ROOT/dist/simple-screensharing-client.flatpak"

case "${1:-}" in
    "") ;;
    -h|--help) sed -n '2,/^set -e/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $1 (try --help)" >&2; exit 2 ;;
esac

if command -v flatpak-builder >/dev/null; then
    builder=(flatpak-builder)
elif flatpak info org.flatpak.Builder >/dev/null 2>&1; then
    builder=(flatpak run org.flatpak.Builder)
else
    echo "ERROR: need flatpak-builder (or: flatpak install --user flathub org.flatpak.Builder)" >&2; exit 1
fi

mkdir -p "$ROOT/dist" "$STATE"
echo ">> flatpak-builder (first run downloads the crates, takes a few minutes)"
"${builder[@]}" --force-clean --disable-rofiles-fuse --state-dir="$STATE" --repo="$REPO" "$BUILD" "$MANIFEST"

echo ">> bundle"
rm -f "$OUT"
flatpak build-bundle "$REPO" "$OUT" "$APP_ID" --runtime-repo=https://dl.flathub.org/repo/flathub.flatpakrepo

echo
echo ">> Done."
ls -lh "$OUT"
