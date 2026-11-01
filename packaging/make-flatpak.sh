#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VER="$(grep -m1 '^version' "$ROOT/Cargo.toml" | cut -d'"' -f2)"
ARCH="$(uname -m)"
case "$ARCH" in
  x86_64|aarch64) ;;
  *) echo "unsupported architecture: $ARCH" >&2; exit 1 ;;
esac
DIST_DIR="${LINE_RUST_DIST_DIR:-/tmp}"
mkdir -p "$DIST_DIR"
DIST_DIR="$(cd "$DIST_DIR" && pwd)"
STAGE="${DIST_DIR}/line-rust-${VER}-${ARCH}"
OUT="${DIST_DIR}/line-rust-${VER}-${ARCH}.flatpak"

command -v flatpak >/dev/null || {
  echo "flatpak is required" >&2
  exit 1
}
command -v flatpak-builder >/dev/null || {
  echo "flatpak-builder is required (Arch: yay -S flatpak-builder)" >&2
  exit 1
}

if [[ ! -x "$STAGE/line-rust" || ! -x "$STAGE/protocol/line-rust-protocol" ]]; then
  env DENO="${DENO:-deno}" "$ROOT/packaging/make-prebuild.sh"
fi

WORK="$(mktemp -d /tmp/line-rust-flatpak.XXXXXX)"
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/payload"
cp -a "$STAGE"/. "$WORK/payload/"
cp "$ROOT/packaging/flatpak/io.github.urguide.LineRust.yml" "$WORK/"
cp "$ROOT/packaging/flatpak/io.github.urguide.LineRust.metainfo.xml" "$WORK/"

MANIFEST="$WORK/io.github.urguide.LineRust.yml"
BUILD_DIR="$WORK/build"
REPO_DIR="$WORK/repo"
STATE_DIR="$WORK/state"

flatpak-builder \
  --force-clean \
  --user \
  --arch="$ARCH" \
  --state-dir="$STATE_DIR" \
  --install-deps-from=flathub \
  --default-branch=stable \
  --repo="$REPO_DIR" \
  "$BUILD_DIR" \
  "$MANIFEST"

SMOKE_OUT="$WORK/sidecar.out"
set +e
timeout 5 flatpak-builder --run "$BUILD_DIR" "$MANIFEST" \
  env LINE_RUST_DATA=/tmp/line-rust-flatpak-smoke \
  /app/share/line-rust/protocol/line-rust-protocol >"$SMOKE_OUT"
smoke_status=$?
set -e
[[ "$smoke_status" == 0 || "$smoke_status" == 124 ]]
grep -q '"event":"ready"' "$SMOKE_OUT"

rm -f "$OUT"
flatpak build-bundle \
  --arch="$ARCH" \
  "$REPO_DIR" \
  "$OUT" \
  io.github.urguide.LineRust \
  stable \
  --runtime-repo=https://flathub.org/repo/flathub.flatpakrepo

sha256sum "$OUT"
"$ROOT/packaging/check-flatpak.sh" "$OUT"
echo "Flatpak bundle ready: $OUT"
