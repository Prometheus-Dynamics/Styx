#!/usr/bin/env bash
set -euo pipefail

root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root_dir"

echo "==> Applying formatting"
cargo fmt \
  -p styx-core-rs \
  -p styx-capture \
  -p styx-codec \
  -p styx-libcamera \
  -p styx \
  -p styx-v4l2 \
  -p styx-examples

echo "==> Applying clippy fixes on the default workspace surface"
cargo clippy --fix --allow-dirty --allow-staged --workspace --all-targets -- -W clippy::all

echo "==> Applying clippy fixes on the example surface"
cargo clippy --fix --allow-dirty --allow-staged -p styx-examples --no-default-features --features "async,file-backend,netcam" -- -W clippy::all

echo "==> Re-applying formatting"
cargo fmt \
  -p styx-core-rs \
  -p styx-capture \
  -p styx-codec \
  -p styx-libcamera \
  -p styx \
  -p styx-v4l2 \
  -p styx-examples

echo "==> Verifying repo state"
"$root_dir/scripts/ci.sh"
