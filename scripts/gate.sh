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

# Cases that cannot run here (styx_frame::not_run): no camera, no GPU, no dma-heap, no PipeWire
# daemon, no test driver. A case whose need is required (STYX_REQUIRE_<NEED>=1) fails the gate.
# Every other one is counted and reported here and in CI's job summary, by need: never green as
# tested. The local gate requires a dma-heap, a C compiler, a PipeWire daemon, a GPU (a software
# adapter counts) and the vivid test driver; a host without one declares it with
# STYX_REQUIRE_<NEED>=0. A camera is not required: those cases are reported as not run, with the
# reason.
export STYX_REQUIRE_DMA_HEAP="${STYX_REQUIRE_DMA_HEAP:-1}"
export STYX_REQUIRE_CC="${STYX_REQUIRE_CC:-1}"
export STYX_REQUIRE_PIPEWIRE="${STYX_REQUIRE_PIPEWIRE:-1}"
export STYX_REQUIRE_GPU="${STYX_REQUIRE_GPU:-1}"
export STYX_REQUIRE_VIVID="${STYX_REQUIRE_VIVID:-1}"
# vivid is a module, loaded by hand and gone after a reboot: said at once, not after the build.
if [ "$STYX_REQUIRE_VIVID" = 1 ] && ! grep -qs '^vivid-' /sys/class/video4linux/*/name; then
  echo "gate: vivid not loaded (sudo modprobe vivid), or set STYX_REQUIRE_VIVID=0 on a host without it" >&2
  exit 1
fi
STYX_NOT_RUN_LOG="$(mktemp "${TMPDIR:-/tmp}/styx-not-run.XXXXXX")"
export STYX_NOT_RUN_LOG
not_run_report() {
  local n dma
  n=$(sort -u "$STYX_NOT_RUN_LOG" | grep -c . || true)
  dma=$(sort -u "$STYX_NOT_RUN_LOG" | grep -c '^NOT RUN \[dma-heap\]' || true)
  echo "dma-buf cases SKIPPED: $dma"
  if [ "$n" != 0 ]; then
    echo "cases NOT RUN: $n"
    sort -u "$STYX_NOT_RUN_LOG" | sed -E 's/^NOT RUN \[([^]]*)\].*/\1/' | sort | uniq -c | sed 's/^/    /'
    sort -u "$STYX_NOT_RUN_LOG" | sed 's/^/    /'
    if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
      {
        echo "### Cases NOT RUN: $n"
        echo
        echo "These cases did not run on this runner (what each needs is in brackets). They are not covered by this run."
        echo
        echo "dma-buf cases SKIPPED: $dma"
        echo
        sort -u "$STYX_NOT_RUN_LOG" | sed 's/^/- /'
      } >> "$GITHUB_STEP_SUMMARY"
    fi
    if [ -n "${GITHUB_ACTIONS:-}" ]; then
      echo "::warning title=Cases NOT RUN::$n cases did not run on this runner ($dma of them dma-buf cases): see the job summary"
    fi
  else
    echo "cases NOT RUN: 0"
  fi
  rm -f "$STYX_NOT_RUN_LOG"
}
trap not_run_report EXIT

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

# One simulated camera: the sensor, receiver and ISP drivers are defined in styx-sim only (S0/X15);
# a test needs one from `styx_sim::paced` (free-running) or `styx_sim::SimCamera` (lock-step).
step "one definition of the simulated camera drivers" bash -c '! grep -rn -E "#\[driver\(\"sim-(sensor|receiver|isp)\"" crates --include=*.rs | grep -v "^crates/camera/sim/src/"'

# One copy of the shared ring: every shm-* crate from one version and one source.
step "one source for every shm crate" python3 scripts/one-shm.py

echo "gate passed"
