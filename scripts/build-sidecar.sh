#!/usr/bin/env bash
set -euo pipefail

project_root="$(cd "$(dirname "$0")/.." && pwd)"
deno_bin="${DENO:-deno}"
output="${1:-$project_root/protocol/line-rust-protocol}"

"$deno_bin" compile \
  --config "$project_root/protocol/deno.json" \
  --frozen \
  --unstable-worker-options \
  --include "$project_root/protocol/src/runtime.ts" \
  --no-prompt \
  --allow-read \
  --allow-write \
  --allow-net \
  --allow-env=HOME,PATH,DENO_DIR,Q_DEBUG,NODE_DEBUG,NO_COLOR,LINE_RUST_DATA,LINE_RUST_LANG,LINE_RUST_CACHE_RETENTION,LINE_RUST_AUDIO_INPUT,LINE_RUST_AUDIO_OUTPUT,LINE_DEVICE,LINE_VERSION,LINE_CALL_DEVNAME,LINE_CALL_DEVICE_INFO,LINE_CALL_OPUS_SIGNAL,LINE_CALL_DEBUG \
  --output "$output" \
  "$project_root/protocol/src/launcher.ts"

chmod 0755 "$output"
