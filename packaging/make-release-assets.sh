#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VER="$(grep -m1 '^version' "$ROOT/Cargo.toml" | cut -d'"' -f2)"
ARCH="$(uname -m)"
DIST_DIR="${LINE_GTK_DIST_DIR:-/tmp}"
mkdir -p "$DIST_DIR"
DIST_DIR="$(cd "$DIST_DIR" && pwd)"
export LINE_GTK_DIST_DIR="$DIST_DIR"

env DENO="${DENO:-deno}" "$ROOT/packaging/make-prebuild.sh"
env DENO="${DENO:-deno}" "$ROOT/packaging/make-flatpak.sh"

PREBUILD="line-gtk-${VER}-${ARCH}.tar.gz"
FLATPAK="line-gtk-${VER}-${ARCH}.flatpak"

cd "$DIST_DIR"
sha256sum "$PREBUILD" "$FLATPAK" > "SHA256SUMS-${ARCH}"

echo
echo "Release assets are ready:"
cat "SHA256SUMS-${ARCH}"
echo
echo "After committing, pushing, and tagging, publish with:"
echo "  gh release create v${VER} \"$DIST_DIR/$PREBUILD\" \"$DIST_DIR/$FLATPAK\" \"$DIST_DIR/SHA256SUMS-${ARCH}\" --verify-tag --title \"v${VER}\" --generate-notes"
