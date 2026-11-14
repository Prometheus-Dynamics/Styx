#!/usr/bin/env bash
# The check every PR into next must pass. CI runs exactly this.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
export CARGO_INCREMENTAL="${CARGO_INCREMENTAL:-0}"

step() {
  local label="$1" start
  shift
  echo "==> $label"
  start=$SECONDS
  "$@"
  echo "    $label: $((SECONDS - start)) s"
}

# The workspace glob matches nothing until the first crate lands.
if compgen -G "crates/*/*/Cargo.toml" > /dev/null; then
  step format cargo fmt --all --check
  # Default features: no libcamera, no FFmpeg, no Orion, no Daedalus. Styx stands alone.
  step "clippy, default features" cargo clippy --workspace --all-targets -- -D warnings
  step "tests, default features" cargo test --workspace
  step "clippy, all features" cargo clippy --workspace --all-targets --all-features -- -D warnings
  step "tests, all features" cargo test --workspace --all-features

  # A crate that must build without std says so in its manifest:
  # [package.metadata.styx]
  # no_std = true
  mapfile -t no_std_crates < <(grep -l '^no_std = true' crates/*/*/Cargo.toml | xargs -r -n1 dirname | xargs -r -n1 basename)
  for target in thumbv7em-none-eabihf riscv32imac-unknown-none-elf; do
    for crate in "${no_std_crates[@]}"; do
      step "$crate on $target" cargo check -q -p "styx-$crate" --no-default-features --target "$target"
    done
  done
else
  echo "no crates yet"
fi

# Styx's code names no product, board or camera module. Those live in data.
if compgen -G "crates/*/*/src" > /dev/null; then
  step "no product names in code" bash -c '! grep -rn -i -E "\b(raze|helios|photonvision|systemcore|roborio|ov9782|cm5)\b" crates/*/*/src'
fi

# One lock per outside repository: crates name git dependencies through the workspace only.
step "git dependencies are named in the workspace only" bash -c '! grep -n -E "(git|rev) = " crates/*/*/Cargo.toml'

# One copy of the shared ring: every shm-* crate from one version and one source.
step "one source for every shm crate" python3 scripts/one-shm.py

# One simulated camera: the sensor, receiver and ISP drivers live in styx-sim only.
step "one simulated camera" python3 scripts/one-sim.py

echo "gate passed"
