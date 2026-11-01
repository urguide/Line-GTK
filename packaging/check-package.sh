#!/usr/bin/env bash
set -euo pipefail

archive="${1:?usage: check-package.sh ARCHIVE.tar.gz}"
listing="$(tar -tzf "$archive")"

for required in \
  /line-rust \
  /protocol/line-rust-protocol \
  /assets/lang/eng.json \
  /assets/lang/thai.json \
  /io.github.urguide.LineRust.desktop
do
  if ! grep -q "${required}$" <<<"$listing"; then
    echo "missing package entry: $required" >&2
    exit 1
  fi
done

echo "package smoke test passed: $archive"
