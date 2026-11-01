#!/usr/bin/env bash
set -euo pipefail

bundle="${1:?usage: check-flatpak.sh BUNDLE.flatpak}"
check_dir="$(mktemp -d /tmp/line-rust-bundle-check.XXXXXX)"
trap 'rm -rf "$check_dir"' EXIT

ostree init --repo="$check_dir/repo" --mode=archive
flatpak build-import-bundle "$check_dir/repo" "$bundle" >/dev/null
ref="app/io.github.urguide.LineRust/$(uname -m)/stable"
ostree --repo="$check_dir/repo" checkout --user-mode "$ref" "$check_dir/checkout"
metadata="$(<"$check_dir/checkout/metadata")"

grep -q '^name=io.github.urguide.LineRust$' <<<"$metadata"
grep -q '^runtime=org.gnome.Platform/' <<<"$metadata"
grep -q '^command=line-rust$' <<<"$metadata"
test -x "$check_dir/checkout/files/bin/line-rust"
test -x "$check_dir/checkout/files/share/line-rust/protocol/line-rust-protocol"

desktop="$check_dir/checkout/files/share/applications/io.github.urguide.LineRust.desktop"
grep -qx 'Icon=io.github.urguide.LineRust' "$desktop"
grep -qx 'StartupWMClass=io.github.urguide.LineRust' "$desktop"
for size in 48x48 64x64 128x128 256x256; do
  test -s "$check_dir/checkout/files/share/icons/hicolor/$size/apps/io.github.urguide.LineRust.png"
done
test -s "$check_dir/checkout/files/share/icons/hicolor/scalable/apps/io.github.urguide.LineRust.svg"

echo "flatpak smoke test passed: $bundle"
