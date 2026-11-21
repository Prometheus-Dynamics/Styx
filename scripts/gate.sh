#!/usr/bin/env bash
# The check every PR into next must pass: the lanes a change needs, and a record of what ran.
#   scripts/gate.sh [--lanes LIST] [--drop LANE:REASON]... [--dry-run]
#   scripts/gate.sh --catch-up     the behind lanes, on a clean detached checkout of origin/next
#   scripts/gate.sh --trailer      the commit trailers of this tree's green run
# scripts/gate.py picks the lanes (docs/gate.md has the short form) and runs this script again with
# STYX_GATE_SELECTED set; the steps below run only for the selected lanes. CI runs every lane.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
if [ -z "${STYX_GATE_SELECTED:-}" ]; then
  [ "${1:-}" != --trailer ] || exec python3 scripts/gate.py trailer
  exec python3 scripts/gate.py run "$@"
fi
# A lane's steps run when it is selected; the lane is recorded as started, then passed.
selected() {
  case " $STYX_GATE_SELECTED " in *" $1 "*) return 0 ;; esac
  return 1
}
lane_mark() {
  [ -z "${STYX_GATE_RESULTS:-}" ] || echo "$1 $2" >> "$STYX_GATE_RESULTS"
}
# A step of a lane that had nothing to run, and why: printed, and recorded on the gate line as
# <variant>/<step>=not-run(<reason>), never as passed. A step that was meant to run something and
# matched nothing fails instead (nextest's --no-tests=fail, an empty crate list).
step_not_run() {
  echo "    NOT RUN: $1/$2 ($3)"
  [ -z "${STYX_GATE_RESULTS:-}" ] || echo "$1 note $2 not-run($3)" >> "$STYX_GATE_RESULTS"
}
# The gate runs under scripts/gate-supervise.py: whatever a step leaves running is stopped when the
# gate ends and fails it, and the gate's output can't be held open by a process that outlives it.
if [ -z "${STYX_GATE_SUPERVISOR:-}" ]; then
  exec python3 scripts/gate-supervise.py "$0" "$@"
fi
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
# Every step has a hard bound: about three times its slowest green time, which is written beside
# it, and never under 60 s. A step runs as a process group of its own. Past its bound the group
# gets SIGTERM, then SIGKILL, and the gate fails naming the step. A step that leaves a process in
# its group fails too. Slowest green times are local (warm, cold, fresh worktree) and CI, since
# nextest (2026-11-17).
# Each step gets an empty TMPDIR of its own, $TMPDIR/g.XXX/<step number> (short: a test's socket
# path under it must stay under 108 bytes, and some already use 97 of them under /tmp), and
# a step that leaves anything in it fails, naming what: a test that leaks into a shared TMPDIR
# fills it, and a later run fails on something unrelated. The step's wall and CPU time (its
# processes', user + system) are printed beside its name.
step_tmp_root="$(mktemp -d "${TMPDIR:-/tmp}/g.XXX")"
step_count=0
step() {
  local label="$1" bound="$2" start pid timer ended status=0 left tmp before after
  shift 2
  echo "==> $label"
  start=$SECONDS
  step_count=$((step_count + 1))
  tmp="$step_tmp_root/$step_count"
  mkdir "$tmp"
  before=$(child_cpu)
  set -m
  TMPDIR="$tmp" "$@" &
  pid=$!
  set +m
  sleep "$bound" &
  timer=$!
  wait -n -p ended "$pid" "$timer" || status=$?
  if [ "$ended" = "$timer" ]; then
    echo "gate: step \"$label\" passed its bound of $bound s: stopping it" >&2
    stop_group "$pid"
    exit 1
  fi
  kill "$timer" 2> /dev/null || true
  wait "$timer" 2> /dev/null || true
  left=$(pgrep -a -g "$pid" || true)
  if [ -n "$left" ]; then
    echo "gate: step \"$label\" left processes running:" >&2
    echo "$left" | sed 's/^/    /' >&2
    stop_group "$pid"
    exit 1
  fi
  after=$(child_cpu)
  left=$(find "$tmp" -mindepth 1 -maxdepth 1 -printf '%f\n' | sort)
  rm -rf -- "${tmp:?}"
  if [ -n "$left" ]; then
    echo "gate: step \"$label\" left files in its TMPDIR:" >&2
    echo "$left" | sed 's/^/    /' >&2
    exit 1
  fi
  echo "    $label: $((SECONDS - start)) s, $(awk -v a="$before" -v b="$after" 'BEGIN { printf "%.1f", b - a }') CPU-s"
  return "$status"
}
# The CPU seconds (user + system) of this shell's waited-for children so far: cutime and cstime of
# /proc/<this shell>/stat, read in a command substitution ($$ is still this shell there).
child_cpu() {
  awk -v hz="$(getconf CLK_TCK)" '{ printf "%.2f", ($16 + $17) / hz }' "/proc/$$/stat"
}
# SIGTERM to a process group, then SIGKILL to what is left of it 5 s later.
stop_group() {
  kill -TERM -- "-$1" 2> /dev/null || true
  for _ in $(seq 50); do
    kill -0 -- "-$1" 2> /dev/null || break
    sleep 0.1
  done
  kill -KILL -- "-$1" 2> /dev/null || true
  wait "$1" 2> /dev/null || true
}

# The lanes that build with cargo, and the ones that run tests.
cargo_lanes=0 test_lanes=0
for v in host:all host:default bare:thumbv7em bare:riscv32imac; do
  selected "$v" && cargo_lanes=1
done
{ selected host:all || selected host:default; } && test_lanes=1

# Slowest green: 1.6 s (restore from a reflink seed, 2.1 GB).
[ "$cargo_lanes" = 0 ] || step "cargo seed" 60 scripts/cargo-seed.sh restore

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
# A gate never opens the desktop's GPU: every GPU test runs on the software Vulkan device
# (lavapipe's llvmpipe). Naming the adapter is not enough, since enumerating adapters loads every
# Vulkan driver and each opens its card, so the Vulkan loader is given lavapipe's driver alone. A
# host without lavapipe then has no Vulkan device, and its GPU cases are not run (a failure where
# STYX_REQUIRE_GPU=1), never run on another device. scripts/gpu-opens.sh scripts/gate.sh proves it:
# it names every GPU device node any process opens, and fails if there is one.
# A real GPU is for a person to choose, outside the gate: styx-daedalus's import checks on one are
#   STYX_DAEDALUS_ADAPTER=<part of the adapter's name> cargo nextest run -p styx-daedalus \
#     --all-features -E 'binary(offset) | test(/^(gpu|kept)::/)'
# (crates/out/daedalus/tests/gpu.rs says what they check). Neither the gate nor CI runs it.
lavapipe=/usr/share/vulkan/icd.d/lvp_icd.$(uname -m).json
[ -e "$lavapipe" ] || [ ! -e /usr/share/vulkan/icd.d/lvp_icd.json ] || lavapipe=/usr/share/vulkan/icd.d/lvp_icd.json
export VK_DRIVER_FILES="$lavapipe" VK_ICD_FILENAMES="$lavapipe"
unset lavapipe
export STYX_GPUISP_DEVICE=llvmpipe STYX_DAEDALUS_ADAPTER=llvmpipe
# vivid is a module, loaded by hand and gone after a reboot: said at once, not after the build.
if [ "$test_lanes" = 1 ] && [ "$STYX_REQUIRE_VIVID" = 1 ] && ! grep -qs '^vivid-' /sys/class/video4linux/*/name; then
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
  rm -rf "${lists:-}" "${step_tmp_root:-}"
}
trap not_run_report EXIT

# Tests run under cargo-nextest: each test in its own process, tests of every binary side by side
# (.config/nextest.toml groups the ones that share a device or a daemon). nextest does not run
# doctests, so they run after it. Without nextest, cargo test runs the same cases.
#
# In host:all each crate's tests are one run with a content key (scripts/gate.py reuse-plan): a
# crate whose key equals a green run's is reused, not run, and printed as REUSED. A crate whose
# tests declare an outside need, or that gets no key, always runs, and says why.
#
# STYX_GATE_NARROW (scripts/check.sh, through scripts/gate.py --narrow; never CI) narrows the tests
# to the crates that reach the named ones, and STYX_CHECK_DOCTESTS=0 leaves out the doctests.
# Everything is still built and checked as in the gate, so a narrowed run is a subset of the gate
# and shares its build. It is not the gate, records nothing, and says so.
narrow_expr=""
if [ -n "${STYX_GATE_NARROW+set}" ]; then
  for c in $STYX_GATE_NARROW; do
    narrow_expr+="${narrow_expr:+ | }rdeps(=$c)"
  done
  echo "==> NOT THE GATE: tests narrowed to ${narrow_expr:-no crate}"
fi
# Tests run once, in the all-features pass, which builds the most code. The default pass builds
# its tests too (a test that does not build without the features fails there) and runs only the
# tests the default build alone has (.config/default-only.txt: code under cfg(not(feature))). The
# two passes' test lists are compared: a default-only test missing from that list fails the gate.
lists=$(mktemp -d "${TMPDIR:-/tmp}/styx-test-lists.XXXXXX")
nextest=0
cargo nextest --version > /dev/null 2>&1 && nextest=1
if [ -n "${STYX_GATE_NARROW+set}" ] && [ "$nextest" = 0 ]; then
  echo "gate: a narrowed check needs cargo-nextest" >&2
  exit 1
fi
default_tests() {
  # The default pass builds in its own target directory, <target>/default. A library with a C
  # crate type (styx-capi, styx-gst) has no feature hash in its file name, so in one directory
  # each pass overwrote the other's and both rebuilt them every gate. The first time, the
  # directory starts as a reflink copy of the target (the same worktree's paths, so cargo finds
  # every unit both passes share current); without reflinks it starts empty.
  local main="${CARGO_TARGET_DIR:-$PWD/target}"
  export CARGO_TARGET_DIR="$main/default"
  if [ ! -d "$CARGO_TARGET_DIR" ]; then
    local entry part="$main/default.part"
    rm -rf -- "${part:?}"
    mkdir "$part"
    for entry in "$main"/* "$main"/.[!.]*; do
      [ -e "$entry" ] || continue
      case "${entry##*/}" in default | default.part | tmp | nextest) continue ;; esac
      cp -a --reflink=always "$entry" "$part/" 2> /dev/null || { rm -rf -- "${part:?}"; mkdir "$part"; break; }
    done
    mv "$part" "$CARGO_TARGET_DIR"
  fi
  if [ "$nextest" = 0 ]; then
    cargo test --locked --workspace
    return
  fi
  cargo nextest list --locked --workspace --message-format json > "$lists/default.json"
  # Every line of the list names a test this build has: a renamed test would otherwise leave its
  # term matching nothing, hidden behind the others (the comparison below runs only beside host:all).
  python3 scripts/test-layout.py default-only-present "$lists/default.json"
  local expr no_tests=fail
  expr=$(python3 scripts/test-layout.py default-only-filter)
  if [ -z "$expr" ]; then
    step_not_run host:default default-only-tests list_empty
    return
  fi
  # A narrowed check may leave none of them (it is not the gate); the gate runs them all.
  [ -z "${STYX_GATE_NARROW+set}" ] || { expr="($expr) & (${narrow_expr:-none()})"; no_tests=pass; }
  cargo nextest run --locked --workspace --no-fail-fast --no-tests="$no_tests" -E "$expr"
}
all_tests() {
  if [ "$nextest" = 0 ]; then
    cargo test --locked --workspace --all-features
    return
  fi
  # The list builds every test binary; the run then builds nothing.
  cargo nextest list --locked --workspace --all-features --message-format json > "$lists/all.json"
  local expr
  expr=$(python3 scripts/gate.py reuse-plan --list "$lists/all.json" --plan "$lists/plan.json")
  # The expression names only crates with a test that is not #[ignore]: matching nothing is a fault.
  if [ -n "$expr" ]; then
    cargo nextest run --locked --workspace --no-fail-fast --no-tests=fail --all-features -E "$expr"
  elif [ -n "${STYX_GATE_NARROW+set}" ]; then
    echo "no crate's tests to run (narrowed)"
  else
    step_not_run host:all tests every_crate_reused
  fi
  # Only after a green run (set -e ends the step on a red one).
  python3 scripts/gate.py reuse-record --plan "$lists/plan.json"
  # nextest does not run doctests.
  if [ "${STYX_CHECK_DOCTESTS:-1}" = 1 ]; then
    cargo test --locked --workspace --doc --all-features
  else
    echo "doctests left out: no changed file holds one (the gate runs them)"
  fi
}
no_std_checks() {
  # A crate that must build without std says so in its manifest:
  # [package.metadata.styx]
  # no_std = true
  # Each crate is checked on its own (its features alone, not unified with the others'). The
  # checks start together: a fresh one is mostly cargo resolving the workspace, which runs side
  # by side; any compiling waits its turn on the target directory's lock. Each reports alone.
  local target="$1" no_std_out no_std_pids=() no_std_labels=() no_std_failed=0 crate i
  local no_std_crates
  mapfile -t no_std_crates < <(grep -l '^no_std = true' crates/*/*/Cargo.toml | xargs -r -n1 dirname | xargs -r -n1 basename)
  # A lane that checks nothing is not a passed lane: no crate found means the marker moved.
  if [ "${#no_std_crates[@]}" = 0 ]; then
    echo "gate: no crate says no_std = true under [package.metadata.styx]: the no_std check matched nothing" >&2
    return 1
  fi
  no_std_out=$(mktemp -d)
  for crate in "${no_std_crates[@]}"; do
    cargo check -q --locked -p "styx-$crate" --no-default-features --target "$target" \
      > "$no_std_out/${#no_std_pids[@]}" 2>&1 &
    no_std_pids+=($!)
    no_std_labels+=("$crate on $target")
  done
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
  [ "$no_std_failed" = 0 ] || { echo "gate: a no_std check failed" >&2; return 1; }
}

# host:checks: a few seconds at most (slowest green 21 s); each is bound at 60 s.
lane_mark host:checks started
if compgen -G "crates/*/*/Cargo.toml" > /dev/null; then
  # cargo fmt reads no dependency (and takes no --locked). Slowest green: 13 s.
  step format 60 cargo fmt --all --check
fi
# Styx's code names no product, board or camera module. Those live in data.
if compgen -G "crates/*/*/src" > /dev/null; then
  step "no product names in code" 60 bash -c '! grep -rn -i -E "\b(raze|helios|photonvision|systemcore|roborio|ov9782|cm5)\b" crates/*/*/src'
fi
# One lock per outside repository: crates name git dependencies through the workspace only.
step "git dependencies are named in the workspace only" 60 bash -c '! grep -n -E "(git|rev) = " crates/*/*/Cargo.toml'
# One simulated camera: the sensor, receiver and ISP drivers are defined in styx-sim only (S0/X15);
# a test needs one from `styx_sim::paced` (free-running) or `styx_sim::SimCamera` (lock-step).
step "one definition of the simulated camera drivers" 60 bash -c '! grep -rn -E "#\[driver\(\"sim-(sensor|receiver|isp)\"" crates --include=*.rs | grep -v "^crates/camera/sim/src/"'
# Every tests/*.rs file is built: a crate that lists its test binaries (tests/all.rs) leaves none out.
step "every test file is in a test binary" 60 python3 scripts/test-layout.py files
# A merged test binary runs its tests as threads of one process under cargo test. A file in one
# that installs a global allocator, reads /proc/self, or reaches a sim or test-support module's
# process-wide static is its own [[test]], or listed in .config/shared-state.txt with its reason.
step "merged test files share no process state unlisted" 60 python3 scripts/test-layout.py shared-state
# One copy of the shared ring: every shm-* crate from one version and one source.
step "one source for every shm crate" 60 python3 scripts/one-shm.py
lane_mark host:checks passed

# The workspace glob matches nothing until the first crate lands.
if compgen -G "crates/*/*/Cargo.toml" > /dev/null; then
  # host:default. Default features: no libcamera, no FFmpeg, no Orion, no Daedalus. Styx stands alone.
  if selected host:default; then
    lane_mark host:default started
    # Slowest green: 76 s (cold). Tests: 425 s (cold, building every default test binary).
    step "clippy, default features" 230 cargo clippy --locked --workspace --all-targets -- -D warnings
    step "tests, default features (build; the default-only tests)" 1275 default_tests
    lane_mark host:default passed
  fi
  if selected host:all; then
    lane_mark host:all started
    # Slowest green: 94 s (cold). Tests: 267 s (fresh worktree), with the doctests.
    step "clippy, all features" 285 cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
    step "tests, all features" 800 all_tests
    # A test group's filter (.config/nextest.toml), term by term, still names tests. Slowest green: 6 s.
    step "every test group filter matches a test" 60 python3 scripts/test-layout.py filters --all-features
    lane_mark host:all passed
  fi
  if [ -f "$lists/default.json" ] && [ -f "$lists/all.json" ]; then
    lane_mark host:default started
    step "every default-only test is in the default-only run" 60 \
      python3 scripts/test-layout.py default-only "$lists/default.json" "$lists/all.json"
    lane_mark host:default passed
  fi
  # bare: each no_std crate on its own, per target. Slowest green: 28 s (22 checks, both targets).
  for target in thumbv7em-none-eabihf riscv32imac-unknown-none-elf; do
    variant="bare:${target%%-*}"
    if selected "$variant"; then
      lane_mark "$variant" started
      step "no_std checks on $target" 90 no_std_checks "$target"
      lane_mark "$variant" passed
    fi
  done
else
  echo "no crates yet"
fi

if [ -n "${STYX_GATE_NARROW+set}" ]; then
  echo "check passed, tests narrowed to ${narrow_expr:-no crate}: not the gate"
else
  echo "gate passed: $STYX_GATE_SELECTED"
  if [ "$cargo_lanes" = 1 ]; then
    # Slowest green: 13 s (a new seed: the dependencies' files copied by reflink, 2.3 GB).
    step "cargo seed save" 60 bash -c 'scripts/cargo-seed.sh save || echo "cargo-seed: no seed kept (the reason is above); the gate passed"'
  fi
fi
