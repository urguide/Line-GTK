#!/usr/bin/env bash
# Build a portable prebuilt tarball for GitHub Releases.
# After this finishes, run the printed gh command yourself.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

VER="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"
ARCH="$(uname -m)"
case "$ARCH" in
  x86_64|aarch64) ;;
  *) echo "unsupported architecture: $ARCH" >&2; exit 1 ;;
esac
DIST_DIR="${LINE_RUST_DIST_DIR:-/tmp}"
mkdir -p "$DIST_DIR"
DIST_DIR="$(cd "$DIST_DIR" && pwd)"
STAGE="line-rust-${VER}-${ARCH}"
OUT_DIR="${DIST_DIR}/${STAGE}"
OUT_TAR="${DIST_DIR}/${STAGE}.tar.gz"

echo "==> building release binary"
cargo build --locked --release

echo "==> staging ${OUT_DIR}"
rm -rf "$OUT_DIR" "$OUT_TAR"
mkdir -p "$OUT_DIR"
cp -a target/release/line-rust "$OUT_DIR/"
mkdir -p "$OUT_DIR/protocol"
# Copy only distributable sources, never local sessions or dependency caches.
cp -a protocol/src protocol/vendor protocol/deno.json protocol/deno.lock "$OUT_DIR/protocol/"
cp -a assets "$OUT_DIR/"
echo "==> compiling offline protocol sidecar"
./scripts/build-sidecar.sh "$OUT_DIR/protocol/line-rust-protocol"
cp -a packaging/io.github.urguide.LineRust.desktop README.md LICENSE "$OUT_DIR/"

echo "==> packing ${OUT_TAR}"
tar -C "$DIST_DIR" -czf "$OUT_TAR" "$STAGE"
sha256sum "$OUT_TAR"
./packaging/check-package.sh "$OUT_TAR"

echo
echo "Done. Asset ready:"
echo "  $OUT_TAR"
echo
echo "If release v${VER} does not exist yet:"
echo "  gh release create v${VER} \"$OUT_TAR\" --verify-tag --title \"v${VER}\" --notes \"Prebuilt ${ARCH} Linux bundle.\""
echo
echo "If release already exists:"
echo "  gh release upload v${VER} \"$OUT_TAR\" --clobber"
