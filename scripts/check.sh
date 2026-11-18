#!/usr/bin/env bash
# The inner loop: the gate's checks on the crates a change reaches, and nothing else.
#
#   scripts/check.sh                 the crates whose files differ from origin/next, and their users
#   scripts/check.sh styx-frame ...  the named crates and their users
#
# Every command here is a gate command (scripts/gate.sh) with --workspace narrowed to those crates
# (-p ...), so a red here is red in the gate. Green here is not the gate: features unify across the
# whole workspace in the gate, and the gate's other steps (no_std targets, the source checks) do not
# run here. Run the gate before landing. A change outside every crate (manifests, lock file, config)
# runs the gate itself.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
export CARGO_INCREMENTAL="${CARGO_INCREMENTAL:-0}"

if [ $# -eq 0 ]; then
  mapfile -t crates < <(python3 scripts/affected.py --changed)
else
  mapfile -t crates < <(python3 scripts/affected.py "$@")
fi
if [ "${crates[0]:-}" = "--workspace" ]; then
  echo "a change outside the crates: running the gate"
  exec scripts/gate.sh
fi
if [ ${#crates[@]} -eq 0 ]; then
  echo "no crate changed"
  exit 0
fi
echo "crates: ${crates[*]}"
packages=()
for c in "${crates[@]}"; do packages+=(-p "$c"); done
libs=()
while read -r c; do libs+=(-p "$c"); done < <(python3 scripts/affected.py --libs "${crates[@]}")

# The same needs as the gate: a case that cannot run is reported, and a required one fails.
export STYX_REQUIRE_DMA_HEAP="${STYX_REQUIRE_DMA_HEAP:-1}"
export STYX_REQUIRE_CC="${STYX_REQUIRE_CC:-1}"
export STYX_REQUIRE_PIPEWIRE="${STYX_REQUIRE_PIPEWIRE:-1}"
export STYX_REQUIRE_GPU="${STYX_REQUIRE_GPU:-1}"
export STYX_REQUIRE_VIVID="${STYX_REQUIRE_VIVID:-1}"
export STYX_GPUISP_DEVICE="${STYX_GPUISP_DEVICE:-llvmpipe}"
export STYX_DAEDALUS_ADAPTER="${STYX_DAEDALUS_ADAPTER:-llvmpipe}"
export STYX_NOT_RUN_LOG="${STYX_NOT_RUN_LOG:-/dev/null}"

step() {
  local label="$1" start
  shift
  echo "==> $label"
  start=$SECONDS
  "$@"
  echo "    $label: $((SECONDS - start)) s"
}

tests() {
  if cargo nextest --version > /dev/null 2>&1; then
    cargo nextest run --no-fail-fast --no-tests=pass "${packages[@]}" "$@"
    if [ ${#libs[@]} -gt 0 ]; then
      cargo test --doc "${libs[@]}" "$@"
    fi
  else
    cargo test "${packages[@]}" "$@"
  fi
}

step format cargo fmt --check "${packages[@]}"
step "clippy, default features" cargo clippy "${packages[@]}" --all-targets -- -D warnings
step "tests, default features" tests
step "clippy, all features" cargo clippy "${packages[@]}" --all-targets --all-features -- -D warnings
step "tests, all features" tests --all-features
echo "check passed (not the gate: run scripts/gate.sh before landing)"
