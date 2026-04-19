#!/usr/bin/env bash
set -euo pipefail

root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root_dir"

echo "==> Checking formatting"
cargo fmt --all -- --check

echo "==> Checking file sizes"
"$root_dir/scripts/check-file-sizes.sh"

echo "==> Running clippy"
cargo clippy --workspace --all-targets -- -D warnings

echo "==> Running tests"
cargo test --workspace

echo "==> Building example surface"
cargo check -p styx --examples --features "examples,file-backend,netcam"
