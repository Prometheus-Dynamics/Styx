#!/usr/bin/env bash
# The check every PR into next must pass. CI runs exactly this.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
export CARGO_INCREMENTAL="${CARGO_INCREMENTAL:-0}"

# At most two gates of this repository run at once on one machine: every gate links its test
# binaries in the same minutes, and more than two of them starve each other and whatever else
# the machine is doing. A gate waits here for one of two slots. STYX_GATE_SLOTS=0 turns it off.
if [ "${STYX_GATE_SLOTS:-2}" != 0 ] && command -v flock > /dev/null; then
  slots="${TMPDIR:-/tmp}/styx-gate-slots"
  mkdir -p "$slots"
  waited=0
  while :; do
    for n in $(seq 1 "${STYX_GATE_SLOTS:-2}"); do
      exec 8> "$slots/$n"
      if flock -n 8; then
        break 2
      fi
    done
    [ "$waited" = 0 ] && echo "==> waiting for a gate slot"
    waited=1
    read -r -t 10 _ < /dev/zero || true
  done
fi

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

echo "gate passed"
