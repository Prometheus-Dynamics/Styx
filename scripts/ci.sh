#!/usr/bin/env bash
set -euo pipefail

root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root_dir"

echo "==> Checking formatting"
cargo fmt \
  -p styx-core-rs \
  -p styx-capture \
  -p styx-codec \
  -p styx-libcamera \
  -p styx \
  -p styx-v4l2 \
  -p styx-examples \
  -- --check

echo "==> Checking file sizes"
"$root_dir/scripts/check-file-sizes.sh"

echo "==> Running clippy"
cargo clippy --workspace --all-targets -- -D warnings

echo "==> Running all-feature workspace check"
cargo check --workspace --all-targets --all-features

echo "==> Running all-feature clippy"
cargo clippy --workspace --all-targets --all-features -- -D warnings

echo "==> Checking release feature combinations"
bash "$root_dir/scripts/check-feature-combinations.sh"

echo "==> Checking duplicate dependency surface"
cargo tree -d --workspace --no-default-features

echo "==> Checking the no_std crates (bare-metal and wasm targets, smoke test)"
"$root_dir/scripts/check-nostd.sh"

echo "==> Running tests"
cargo test --workspace

echo "==> Running the recording tests (MCAP and .styxrec are opt-in features)"
cargo test -p styx --lib --features replay-mcap,replay-styxrec replay

echo "==> Building example surface"
cargo check -p styx-examples --no-default-features --features "async,file-backend,netcam,codec-jpeg-decoder"

echo "==> Building the camera examples (native stack, V4L2; no libcamera)"
cargo check -p styx-examples --no-default-features --features "native,v4l2,async,hotplug,replay-mcap" --all-targets

echo "==> Running perf smoke baseline"
"$root_dir/scripts/check-perf-smoke.sh"
