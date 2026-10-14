#!/usr/bin/env bash
#
# build-appimage.sh — build a self-contained Linux AppImage of the client.
# Thin wrapper around `npm run build:appimage` (defined in
# simple-screensharing-client/electron/package.json), which builds the Rust
# sidecar in release mode and packages it with electron-builder.
#
# Usage:
#   ./build-appimage.sh
#   CLIENT_DIR=/path/to/simple-screensharing-client ./build-appimage.sh

set -euo pipefail

CLIENT_DIR="${CLIENT_DIR:-$HOME/GitHubProjects/simple-screensharing/simple-screensharing-client}"
ELECTRON_DIR="$CLIENT_DIR/electron"

if [[ ! -d "$ELECTRON_DIR" ]]; then
  echo "ERROR: electron directory not found: $ELECTRON_DIR" >&2
  exit 1
fi

if [[ ! -d "$ELECTRON_DIR/node_modules" ]]; then
  echo ">> electron/node_modules not found — running npm install ..."
  ( cd "$ELECTRON_DIR" && npm install )
fi

echo ">> Building AppImage (cargo build --release + electron-builder) ..."
( cd "$ELECTRON_DIR" && npm run build:appimage )

echo
echo ">> Done. Output:"
find "$CLIENT_DIR/dist" -maxdepth 1 -name '*.AppImage' -exec echo "   {}" \;
