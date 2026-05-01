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

echo "==> Running tests"
cargo test --workspace

echo "==> Building example surface"
cargo check -p styx-examples --no-default-features --features "async,file-backend,netcam,codec-jpeg-decoder"

echo "==> Running perf smoke baseline"
"$root_dir/scripts/check-perf-smoke.sh"
