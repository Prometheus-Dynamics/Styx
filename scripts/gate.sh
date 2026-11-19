#!/usr/bin/env bash
# The check every PR into next must pass. CI runs exactly this.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
export CARGO_INCREMENTAL="${CARGO_INCREMENTAL:-0}"

# Cargo.lock is committed, and every cargo step below builds with --locked: the gate builds exactly
# the versions in it. A manifest changed without its lock fails here, first, with the fix.
if ! cargo metadata --locked --format-version 1 > /dev/null; then
  echo "gate: Cargo.lock does not match the manifests: run \`cargo update --workspace\` (it changes only what the manifests need) and commit Cargo.lock" >&2
  exit 1
fi

# At most two gates of this repository run at once on one machine: every gate links its test
# binaries in the same minutes, and more than two of them starve each other and whatever else
# the machine is doing. A gate waits here for one of two slots. STYX_GATE_SLOTS=0 turns it off.
# The slots are the user's, not a gate's: in $XDG_RUNTIME_DIR, so gates with their own TMPDIR
# still share them.
if [ "${STYX_GATE_SLOTS:-2}" != 0 ] && command -v flock > /dev/null; then
  slots="${XDG_RUNTIME_DIR:-/tmp}/styx-gate-slots"
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

# A fresh worktree's empty target directory starts from the dependencies of an earlier green
# gate (scripts/cargo-seed.sh: on for a CARGO_TARGET_DIR outside the worktree, never in CI).
scripts/cargo-seed.sh restore

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
# GPU tests run on the software Vulkan device (lavapipe's llvmpipe), not the desktop's GPU, unless
# the caller names a device. The exception is styx-daedalus's import checks (tests/gpu.rs,
# kept.rs, offset.rs): on a CPU adapter their graphs give no shader output, so they open a real GPU
# (STYX_DAEDALUS_IMPORT_ADAPTER, unset = Daedalus's default). They are sub-second conformance and
# import checks; no stress or soak test may join them.
export STYX_GPUISP_DEVICE="${STYX_GPUISP_DEVICE:-llvmpipe}"
export STYX_DAEDALUS_ADAPTER="${STYX_DAEDALUS_ADAPTER:-llvmpipe}"
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

# Tests run under cargo-nextest: each test in its own process, tests of every binary side by side
# (.config/nextest.toml groups the ones that share a device or a daemon). nextest does not run
# doctests, so they run after it. Without nextest, cargo test runs the same cases.
#
# STYX_TEST_FILTER (set by scripts/check.sh, never by CI) narrows the tests nextest runs to a
# filterset, and only that: everything is built and checked as in the gate, so a narrowed run is
# a subset of the gate and shares its build. It is not the gate, and says so.
#
# scripts/check.sh also leaves out whole passes, never parts of one: every command it runs is the
# gate's, on the gate's build. STYX_CHECK_PASS (default or all) is the one feature pass it runs,
# STYX_CHECK_DOCTESTS=0 leaves out the doctests, and STYX_CHECK_NO_STD lists the only no_std crates
# it checks (directory names). Each pass left out is printed at the end: the gate runs it.
filter=()
check_pass="${STYX_CHECK_PASS:-}"
skipped=()
if [ -n "${STYX_TEST_FILTER:-}" ]; then
  cargo nextest --version > /dev/null 2>&1 || { echo "gate: STYX_TEST_FILTER needs cargo-nextest" >&2; exit 1; }
  filter=(-E "$STYX_TEST_FILTER" --no-tests=pass)
  echo "==> NOT THE GATE: tests narrowed to $STYX_TEST_FILTER"
elif [ -n "$check_pass" ]; then
  echo "gate: STYX_CHECK_PASS is scripts/check.sh's, with STYX_TEST_FILTER" >&2
  exit 1
fi
case "$check_pass" in
  "" | default | all) ;;
  *) echo "gate: STYX_CHECK_PASS is default or all, not $check_pass" >&2; exit 1 ;;
esac
tests() {
  if cargo nextest --version > /dev/null 2>&1; then
    cargo nextest run --locked --workspace --no-fail-fast "${filter[@]}" "$@"
    # Doctests run once, in the all-features pass.
    if [ "$*" = --all-features ]; then
      if [ "${STYX_CHECK_DOCTESTS:-1}" = 1 ]; then
        cargo test --locked --workspace --doc "$@"
      else
        skipped+=("doctests: no changed file holds one")
      fi
    fi
  else
    cargo test --locked --workspace "$@"
  fi
}

# The workspace glob matches nothing until the first crate lands.
if compgen -G "crates/*/*/Cargo.toml" > /dev/null; then
  # cargo fmt reads no dependency (and takes no --locked).
  step format cargo fmt --all --check
  # Default features: no libcamera, no FFmpeg, no Orion, no Daedalus. Styx stands alone.
  if [ "$check_pass" != all ]; then
    step "clippy, default features" cargo clippy --locked --workspace --all-targets -- -D warnings
    step "tests, default features" tests
  else
    skipped+=("clippy and tests, default features")
  fi
  if [ -z "$check_pass" ] || [ "$check_pass" = all ]; then
    step "clippy, all features" cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
    step "tests, all features" tests --all-features
    # A test group's filter (.config/nextest.toml), term by term, still names tests.
    step "every test group filter matches a test" python3 scripts/test-layout.py filters --all-features
  else
    skipped+=("clippy and tests, all features")
  fi

  # A crate that must build without std says so in its manifest:
  # [package.metadata.styx]
  # no_std = true
  # Each crate is checked on its own (its features alone, not unified with the others'). The
  # checks start together: a fresh one is mostly cargo resolving the workspace, which runs side
  # by side; any compiling waits its turn on the target directory's lock. Each reports alone.
  mapfile -t no_std_crates < <(grep -l '^no_std = true' crates/*/*/Cargo.toml | xargs -r -n1 dirname | xargs -r -n1 basename)
  if [ -n "$check_pass" ]; then
    all_no_std=${#no_std_crates[@]}
    read -r -a no_std_crates <<< "${STYX_CHECK_NO_STD:-}"
    [ "$all_no_std" = "${#no_std_crates[@]}" ] || skipped+=("no_std checks of the $((all_no_std - ${#no_std_crates[@]})) unchanged no_std crates")
  fi
  no_std_out=$(mktemp -d)
  no_std_start=$SECONDS
  no_std_pids=()
  no_std_labels=()
  for target in thumbv7em-none-eabihf riscv32imac-unknown-none-elf; do
    for crate in "${no_std_crates[@]}"; do
      cargo check -q --locked -p "styx-$crate" --no-default-features --target "$target" \
        > "$no_std_out/${#no_std_pids[@]}" 2>&1 &
      no_std_pids+=($!)
      no_std_labels+=("$crate on $target")
    done
  done
  no_std_failed=0
  for i in "${!no_std_pids[@]}"; do
    if wait "${no_std_pids[$i]}"; then
      echo "==> ${no_std_labels[$i]}: ok"
    else
      echo "==> ${no_std_labels[$i]}: FAILED"
      no_std_failed=1
    fi
    cat "$no_std_out/$i"
  done
  rm -rf "$no_std_out"
  echo "    no_std checks (${#no_std_pids[@]}): $((SECONDS - no_std_start)) s"
  [ "$no_std_failed" = 0 ] || { echo "gate: a no_std check failed" >&2; exit 1; }
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

# Every tests/*.rs file is built: a crate that lists its test binaries (tests/all.rs) leaves none out.
step "every test file is in a test binary" python3 scripts/test-layout.py files

# One copy of the shared ring: every shm-* crate from one version and one source.
step "one source for every shm crate" python3 scripts/one-shm.py

if [ ${#filter[@]} -gt 0 ]; then
  for s in "${skipped[@]}"; do
    echo "check skipped: $s (the gate runs it)"
  done
  echo "check passed, tests narrowed to $STYX_TEST_FILTER: not the gate"
else
  echo "gate passed"
  scripts/cargo-seed.sh save || echo "cargo-seed: no seed kept (the reason is above); the gate passed"
fi
