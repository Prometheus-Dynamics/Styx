#!/usr/bin/env bash
# The check every PR into next must pass: the lanes a change needs, and a record of what ran.
#   scripts/gate.sh [--lanes LIST] [--drop LANE:REASON]... [--dry-run]
#   scripts/gate.sh --catch-up     the behind lanes, on a clean detached checkout of origin/next
#   scripts/gate.sh --trailer      the commit trailers of this tree's green run
# scripts/gate.py picks the lanes (docs/gate.md has the short form) and runs this script again with
# STYX_GATE_SELECTED set; the steps below run only for the selected lanes. CI runs every lane.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
# Every heavy run is at one nice level across the deps (STYX_NICE, default 10: the coordinator's
# rule of 2026-11-24, since mixed levels starve the lower ones). A process can raise its own nice
# level, not lower it: started above it, the run says so and stays where it is.
want=${STYX_NICE:-10}
if [ "$(nice)" -lt "$want" ]; then
  renice -n "$want" -p $$ > /dev/null
elif [ "$(nice)" -gt "$want" ]; then
  echo "$(basename "$0"): at nice $(nice), above $want: started from a lower-priority shell" >&2
fi
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
# Incremental builds locally: after an edit to a crate low in the graph (styx-frame) every test
# binary rebuilt in 6 s and 68 CPU-s with them, 54-74 s and about 400 CPU-s without (2026-11-24,
# load 5-16). CI builds from a cache without them (less to write). Switching the setting rebuilds
# every workspace crate, so a worktree keeps one.
if [ -n "${GITHUB_ACTIONS:-}" ]; then
  export CARGO_INCREMENTAL="${CARGO_INCREMENTAL:-0}"
else
  export CARGO_INCREMENTAL="${CARGO_INCREMENTAL:-1}"
fi

# Cargo.lock is committed, and every cargo step below builds with --locked: the gate builds exactly
# the versions in it. A manifest changed without its lock fails here, first, with the fix.
# (Not in a dry gate, scripts/gate-selftest.sh: it builds nothing, and its twelve dry gates queued on
# cargo's package cache lock behind a real gate's builds, past the self-test's bound.)
if [ -z "${STYX_GATE_DRY:-}" ] && ! cargo metadata --locked --format-version 1 > /dev/null; then
  echo "gate: Cargo.lock does not match the manifests: run \`cargo update --workspace\` (it changes only what the manifests need) and commit Cargo.lock" >&2
  exit 1
fi

# A target directory belongs to one worktree. In a target shared between worktrees, a workspace
# crate another worktree built from its newer files is taken as current (cargo compares times, and
# its build products carry that worktree's paths), so this tree is tested with that one's code.
# The target records the worktree that first built in it (<target>/.styx-worktree, written after
# the seed restore below) and the gate refuses any other: the rule is enforced, not kept by habit.
target_dir="${CARGO_TARGET_DIR:-$PWD/target}"
owner_file="$target_dir/.styx-worktree"
here="$(pwd -P)"
if [ -f "$owner_file" ] && [ "$(cat "$owner_file")" != "$here" ]; then
  echo "gate: the target directory $target_dir belongs to the worktree $(cat "$owner_file"), not this one ($here): give this worktree a CARGO_TARGET_DIR of its own, and delete that one with its worktree" >&2
  exit 1
fi

# One gate of this repository runs at a time on one machine: two gates side by side each took
# twice as long (the same CPU, double the wall: 628-827 s against a 320 s median), so a second gate
# waits here for the first to end. STYX_GATE_SLOTS=N allows N; 0 turns the wait off. The slots are
# the user's, not a gate's: in $XDG_RUNTIME_DIR, so gates with their own TMPDIR still share them.
if [ "${STYX_GATE_SLOTS:-1}" != 0 ] && command -v flock > /dev/null; then
  slots="${XDG_RUNTIME_DIR:-/tmp}/styx-gate-slots"
  mkdir -p "$slots"
  waited=0
  while :; do
    for n in $(seq 1 "${STYX_GATE_SLOTS:-1}"); do
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
  local label="$1" bound="$2" start pid timer over status=0 left tmp before after me=$BASHPID
  shift 2
  # The gate's own false-pass check (scripts/gate-selftest.sh) runs it dry: every step runs `true`
  # in place of its command, except the one labelled STYX_GATE_PLANT, which runs planted_failure.
  # Lanes, subshells, waits and pass marks are the gate's own. scripts/gate.py run refuses a dry gate.
  if [ -n "${STYX_GATE_DRY:-}" ]; then
    if [ "$label" = "${STYX_GATE_PLANT:-}" ]; then set -- planted_failure; else set -- true; fi
  fi
  echo "==> $label"
  start=$SECONDS
  step_count=$((step_count + 1))
  tmp="$step_tmp_root/$step_count"
  mkdir "$tmp"
  before=$(child_cpu "$me")
  set -m
  TMPDIR="$tmp" "$@" &
  pid=$!
  set +m
  # The bound: a timer that marks the step over it and stops its group, so the wait below ends.
  # `wait PID`, not `wait -n` on the step and a timer: `wait -n` missed a step that ended within
  # milliseconds of starting (each step of a dry gate, scripts/gate-selftest.sh) and waited out
  # the whole bound. The timer is never signalled: a signal sent between its fork and its exec,
  # or before its trap was set, was lost, and the gate waited on it for the whole bound. It is a
  # new shell (not a subshell, which would hold the gate's EXIT trap) that watches the step every
  # 50 ms and ends by itself once the step has.
  over="$step_tmp_root/$step_count.over"
  bash -c 'end=$((SECONDS + $1)); while kill -0 "$3" 2> /dev/null; do
      if [ "$SECONDS" -ge "$end" ]; then : > "$2"; kill -TERM -- "-$3" 2> /dev/null; exit 0; fi
      sleep 0.05; done' timer "$bound" "$over" "$pid" &
  timer=$!
  wait "$pid" || status=$?
  wait "$timer" 2> /dev/null || true
  if [ -e "$over" ]; then
    rm -f "$over"
    echo "gate: step \"$label\" passed its bound of $bound s: stopping it" >&2
    stop_group "$pid"
    exit 1
  fi
  left=$(pgrep -a -g "$pid" || true)
  if [ -n "$left" ]; then
    echo "gate: step \"$label\" left processes running:" >&2
    echo "$left" | sed 's/^/    /' >&2
    stop_group "$pid"
    exit 1
  fi
  after=$(child_cpu "$me")
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
# A failing command, then a passing one: a step's status must be the failing one's (errexit inside
# the step, and the step returning it), never the last command's.
planted_failure() {
  false
  echo "planted: ran past its failing command"
}
# The CPU seconds (user + system) of a shell's waited-for children so far: cutime and cstime of
# /proc/<pid>/stat. The pid is the shell running the step ($BASHPID there: the bare lanes run in
# a subshell of their own, where $$ is still the gate's).
child_cpu() {
  awk -v hz="$(getconf CLK_TCK)" '{ printf "%.2f", ($16 + $17) / hz }' "/proc/$1/stat"
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
for v in host:all host:default bare:thumbv7em bare:riscv32imac bare:riscv32imc; do
  selected "$v" && cargo_lanes=1
done
{ selected host:all || selected host:default; } && test_lanes=1

# Slowest green: 1.6 s (restore from a reflink seed, 2.1 GB).
[ "$cargo_lanes" = 0 ] || step "cargo seed" 60 scripts/cargo-seed.sh restore
# The scripts that are no test's input (scripts/gate.py test_readable: no crate names them, no
# input script names them) are left out of every reuse key. While the lanes build and test they
# cannot be read at all, so a test that reaches one by a path built at run time fails instead of
# passing on a key without it. A gate stopped by force left them so; this gives them back first.
[ -n "${STYX_GATE_DRY:-}" ] || python3 scripts/gate.py scripts-hidden
# The record, after the restore (which starts only an empty target).
if [ "$cargo_lanes" != 0 ] && [ ! -f "$owner_file" ]; then
  mkdir -p "$target_dir"
  printf '%s\n' "$here" > "$owner_file"
fi

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
  # Only the gate's own shell. A child forked from it (a step's subshell, the timer before its exec)
  # and stopped by a signal runs this trap too, and would remove the gate's files under it.
  [ "$BASHPID" = "$$" ] || return 0
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
  # A lane that ran beside the host lanes and was not printed (the gate ended on a red step
  # first): waited for (each of its steps has its bound) and printed whole, so its result is
  # read, not guessed.
  local pid out
  for pid in ${checks_pid:-} ${default_pid:-} ${bare_pid:-}; do
    wait "$pid" 2> /dev/null || true
  done
  for out in "${checks_out:-}" "${default_out:-}" "${bare_out:-}"; do
    [ -n "$out" ] && [ -f "$out" ] || continue
    echo "==> $(basename "$out" .out) (beside the host lanes; the gate ended before it was printed):"
    cat "$out"
  done
  rm -f "$STYX_NOT_RUN_LOG"
  rm -rf "${lists:-}" "${step_tmp_root:-}"
  [ -n "${STYX_GATE_DRY:-}" ] || python3 scripts/gate.py scripts-hidden
}
trap not_run_report EXIT
[ "$test_lanes" = 0 ] || [ -n "${STYX_GATE_DRY:-}" ] || python3 scripts/gate.py scripts-hidden --on

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
# One test at a time per core. Twice that was faster (26 cores at load 110-215, 2026-11-24: 18.8 s
# against 22.9 s) but not safe when starved: on 5 cores, 52 threads failed TODO Q14 in 1 of 3 runs,
# 26 threads in none of 5. nproc counts the cores this process may use (taskset).
test_threads=$(nproc)
nextest=0
if [ -n "${STYX_GATE_DRY:-}" ] || cargo nextest --version > /dev/null 2>&1; then nextest=1; fi
if [ -n "${STYX_GATE_NARROW+set}" ] && [ "$nextest" = 0 ]; then
  echo "gate: a narrowed check needs cargo-nextest" >&2
  exit 1
fi
# A target directory of its own inside the main one, <main>/<name>, made the first time as a reflink
# copy of the main one (the same worktree's paths, so cargo finds every unit they share current);
# without reflinks it starts empty.
start_from() {
  local main="$1" name="$2" entry part="$1/$2.part"
  [ ! -d "$main/$name" ] || return 0
  rm -rf -- "${part:?}"
  mkdir "$part"
  for entry in "$main"/* "$main"/.[!.]*; do
    [ -e "$entry" ] || continue
    case "${entry##*/}" in default | default.part | bare | bare.part | tmp | nextest) continue ;; esac
    cp -a --reflink=always "$entry" "$part/" 2> /dev/null || { rm -rf -- "${part:?}"; mkdir "$part"; break; }
  done
  mv "$part" "$main/$name"
}
default_tests() {
  # The default pass builds in its own target directory, <target>/default. A library with a C
  # crate type (styx-capi, styx-gst) has no feature hash in its file name, so in one directory
  # each pass overwrote the other's and both rebuilt them every gate. The first time, the
  # directory starts as a reflink copy of the target (the same worktree's paths, so cargo finds
  # every unit both passes share current); without reflinks it starts empty.
  # The lane's clippy runs in it too (start_default).
  # Only the crates whose tests can differ under default features build here (test-layout.py
  # default-only-crates: a not(feature) cfg, a build script reading CARGO_FEATURE_*, or a line of
  # .config/default-only.txt). Every other crate's tests are the all-features pass's, and the
  # clippy step above has type-checked them under default features. Each crate is built as itself
  # (-p), with its own default features.
  # An assignment, not `< <(...)`: a failing list must fail the step, not leave it with no crates.
  local crate crates=() names
  names=$(python3 scripts/test-layout.py default-only-crates)
  for crate in $names; do crates+=(-p "$crate"); done
  if [ "${#crates[@]}" = 0 ]; then
    step_not_run host:default tests no_default_only_code
    return
  fi
  if [ "$nextest" = 0 ]; then
    cargo test --locked "${crates[@]}"
    return
  fi
  cargo nextest list --locked "${crates[@]}" --message-format json > "$lists/default.json"
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
  cargo nextest run --locked "${crates[@]}" --no-fail-fast --no-tests="$no_tests" \
    --test-threads "$test_threads" -E "$expr"
}
all_tests() {
  if [ "$nextest" = 0 ]; then
    cargo test --locked --workspace --all-features
    return
  fi
  # styx-capi's c_proof links the C static library with the package's default features, built in
  # a target of its own (<profile>/c-proof, crates/bridge/capi/tests/c_proof.rs). Built here, beside
  # the test binaries, the test finds it current; built in the test, it is a cargo build at the
  # tail of the run that every other test has finished waiting for. A different target directory:
  # no lock is shared. A failure here is the test's to report, with its own message.
  # Each part's time from the step's start, printed at its end (the step's line has the total).
  local t0=$SECONDS
  part() { echo "    part: $1 ends at $((SECONDS - t0)) s"; }
  local cproof_pid cproof_out="$lists/c-proof.out"
  CARGO_TARGET_DIR="$target_dir/debug/c-proof" cargo build --locked -p styx-capi --lib > "$cproof_out" 2>&1 &
  cproof_pid=$!
  # The list builds every test binary; the run then builds nothing.
  cargo nextest list --locked --workspace --all-features --message-format json > "$lists/all.json"
  part "nextest list (builds the test binaries)"
  wait "$cproof_pid" || { echo "==> the C library build for c_proof failed (the test reports it):"; cat "$cproof_out"; }
  part "the C library for c_proof"
  local expr doc_pid="" doc_out="$lists/doctests.out" doc_status=0 crate doc_crates=()
  expr=$(python3 scripts/gate.py reuse-plan --list "$lists/all.json" --plan "$lists/plan.json" \
    --doc "$lists/doc-crates")
  part "reuse plan"
  # nextest does not run doctests. They run beside the nextest run, not after it: rustdoc compiles
  # and runs each crate's examples one crate after another, mostly waiting (33 s of wall for 9 CPU-s
  # at load 200), and it holds no lock on the build directory while it does. Their output is
  # printed when they end. They follow the crates' reuse keys (the plan's --doc list): a crate
  # reused by key reuses its doctest verdict; every other crate with a doctest target runs them.
  # 5 s of a core change's 40 s test step went to the doctests of 52 crates, 47 with none.
  while read -r crate; do doc_crates+=(-p "$crate"); done < "$lists/doc-crates"
  if [ "${STYX_CHECK_DOCTESTS:-1}" != 1 ]; then
    echo "doctests left out: no changed file holds one (the gate runs them)"
  elif [ "${#doc_crates[@]}" = 0 ]; then
    step_not_run host:all doctests every_crate_reused
  else
    cargo test --locked "${doc_crates[@]}" --doc --all-features > "$doc_out" 2>&1 &
    doc_pid=$!
  fi
  # The expression names only crates with a test that is not #[ignore]: matching nothing is a fault.
  if [ -n "$expr" ]; then
    cargo nextest run --locked --workspace --no-fail-fast --no-tests=fail --all-features \
      --test-threads "$test_threads" -E "$expr"
  elif [ -n "${STYX_GATE_NARROW+set}" ]; then
    echo "no crate's tests to run (narrowed)"
  else
    step_not_run host:all tests every_crate_reused
  fi
  part "nextest run"
  if [ -n "$doc_pid" ]; then
    wait "$doc_pid" || doc_status=$?
    part "doctests"
    echo "==> doctests (they ran beside the nextest run):"
    cat "$doc_out"
    [ "$doc_status" = 0 ] || { echo "gate: the doctests failed" >&2; return 1; }
  fi
  # Only after a green nextest run and green doctests (set -e ends the step on a red one): a key
  # recorded here reuses both verdicts.
  python3 scripts/gate.py reuse-record --plan "$lists/plan.json"
  part "reuse record"
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
  # A target with no compare-and-swap (rustc leaves target_has_atomic unset): a crate with a
  # `critical-section` feature is checked with it, every other one as it is.
  local no_cas=0 features
  rustc --print cfg --target "$target" | grep -q '^target_has_atomic="ptr"$' || no_cas=1
  for crate in "${no_std_crates[@]}"; do
    features=()
    if [ "$no_cas" = 1 ] && grep -q '^critical-section = ' crates/*/"$crate"/Cargo.toml; then
      features=(--features critical-section)
    fi
    cargo check -q --locked -p "styx-$crate" --no-default-features "${features[@]}" --target "$target" \
      > "$no_std_out/${#no_std_pids[@]}" 2>&1 &
    no_std_pids+=($!)
    no_std_labels+=("$crate on $target${features:+ with critical-section}")
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

# host:checks: each a few seconds of CPU at most, bound at 60 s. They read the tree only and build
# nothing, so they run beside the builds in a subshell of their own: one after the other they took
# 28 s of wall for 6 CPU-s at load 140 (2026-11-24), all of it before the first build began. Their
# output is printed when the host lanes end; a failure fails the gate there.
# Passes when grep finds nothing (status 1). A match (0) fails, and so does an error (2: a file it
# could not read, a bad pattern), which `! grep` would have taken as a pass.
no_match() {
  local s=0
  grep "$@" || s=$?
  [ "$s" = 1 ] || { [ "$s" = 0 ] || echo "gate: grep failed (status $s)" >&2; return 1; }
}
# Every simulated camera driver definition outside styx-sim (the grep's errors fail it, as above).
sim_drivers_outside_sim() {
  local out s=0
  out=$(grep -rn -E '#\[driver\("sim-(sensor|receiver|isp)"' crates --include=*.rs) || s=$?
  [ "$s" -le 1 ] || { echo "gate: grep failed (status $s)" >&2; return 1; }
  out=$(awk '!/^crates\/camera\/sim\/src\// && NF' <<< "$out")
  [ -z "$out" ] || { echo "$out"; return 1; }
}
gate_selftest() {
  local key
  key=$(python3 scripts/gate.py selftest-key)
  if python3 scripts/gate.py selftest-reuse "$key"; then
    [ -z "${STYX_GATE_RESULTS:-}" ] || echo "host:checks note gate-selftest reused($key)" >> "$STYX_GATE_RESULTS"
    return 0
  fi
  scripts/gate-selftest.sh
  python3 scripts/gate.py selftest-record "$key"
}
checks_lane() {
  lane_mark host:checks started
  if compgen -G "crates/*/*/Cargo.toml" > /dev/null; then
    # cargo fmt reads no dependency (and takes no --locked). Slowest green: 13 s.
    step format 60 cargo fmt --all --check
  fi
  # Styx's code names no product, board or camera module. Those live in data.
  if compgen -G "crates/*/*/src" > /dev/null; then
    step "no product names in code" 60 no_match -rn -i -E '\b(raze|helios|photonvision|systemcore|roborio|ov9782|cm5)\b' crates/*/*/src
  fi
  # One lock per outside repository: crates name git dependencies through the workspace only.
  step "git dependencies are named in the workspace only" 60 no_match -n -E '(git|rev) = ' crates/*/*/Cargo.toml
  # One simulated camera: the sensor, receiver and ISP drivers are defined in styx-sim only (S0/X15);
  # a test needs one from `styx_sim::paced` (free-running) or `styx_sim::SimCamera` (lock-step).
  step "one definition of the simulated camera drivers" 60 sim_drivers_outside_sim
  # Every tests/*.rs file is built: a crate that lists its test binaries (tests/all.rs) leaves none out.
  step "every test file is in a test binary" 60 python3 scripts/test-layout.py files
  # A merged test binary runs its tests as threads of one process under cargo test. A file in one
  # that installs a global allocator, reads /proc/self, or reaches a sim or test-support module's
  # process-wide static is its own [[test]], or listed in .config/shared-state.txt with its reason.
  step "merged test files share no process state unlisted" 60 python3 scripts/test-layout.py shared-state
  # One copy of the shared ring: every shm-* crate from one version and one source.
  step "one source for every shm crate" 60 python3 scripts/one-shm.py
  # The gate itself: a planted failing step fails its lane and the gate, in every lane, at its
  # first step and at its last (Atlas's two false passes, 2026-11-24). Slowest green: 18 s. Keyed
  # on the files it tests (gate.py SELFTEST_FILES): reused after a green run with the same key,
  # never in CI or with reuse off.
  step "a failing step fails its lane (scripts/gate-selftest.sh)" 60 gate_selftest
  lane_mark host:checks passed
}
checks_pid="" checks_out=""
if [ "$cargo_lanes" = 1 ]; then
  checks_out="$step_tmp_root/checks.out"
  (
    trap - EXIT
    step_tmp_root="$step_tmp_root/checks"
    step_count=0
    mkdir "$step_tmp_root"
    checks_lane
  ) > "$checks_out" 2>&1 &
  checks_pid=$!
else
  checks_lane
fi

# bare: each no_std crate on its own, per target. Slowest green: 28 s (22 checks, both targets).
bare_lanes() {
  for target in thumbv7em-none-eabihf riscv32imac-unknown-none-elf riscv32imc-unknown-none-elf; do
    variant="bare:${target%%-*}"
    if selected "$variant"; then
      lane_mark "$variant" started
      step "no_std checks on $target" 90 no_std_checks "$target"
      if [ "$variant" = bare:riscv32imc ]; then
        # The portable-atomic path's tests, on the host: its own target directory (an explicit
        # --target), so the flag rebuilds nothing else. portable-atomic uses the host's native
        # atomics here: this runs the lease, pool and Arc code on the portable types, not the
        # critical section itself, which is the library's.
        step "styx-frame tests on the portable-atomic path" 300 \
          env RUSTFLAGS="--cfg styx_portable_atomic" cargo test -q --locked -p styx-frame \
          --features critical-section --target "$(rustc -vV | sed -n 's/^host: //p')"
      fi
      lane_mark "$variant" passed
    fi
  done
}
# The bare lanes run beside the host lanes, in a subshell with a target directory of their own
# (<target>/bare: cargo's lock on the host one would make them wait for every host build) and two
# of the build jobs; the host lanes take the rest. Their output is printed when the host lanes end.
bare_pid="" bare_out=""
bare_selected=0
for v in bare:thumbv7em bare:riscv32imac bare:riscv32imc; do selected "$v" && bare_selected=1; done
start_bare() {
  bare_out="$step_tmp_root/bare.out"
  (
    trap - EXIT
    step_tmp_root="$step_tmp_root/bare"
    step_count=0
    mkdir "$step_tmp_root"
    start_from "$target_dir" bare
    export CARGO_TARGET_DIR="$target_dir/bare" CARGO_BUILD_JOBS=2
    bare_lanes
  ) > "$bare_out" 2>&1 &
  bare_pid=$!
}
# host:default: both its steps build default features in a target directory of its own
# (<target>/default; a library with a C crate type has no feature hash in its file name, so in
# one directory the two passes overwrote each other's). Beside host:all it runs in a subshell
# with part of the build jobs, as the bare lanes do: the two passes share no unit and no lock, and
# one after the other the gate used 2.3 of the cores it could get (2026-11-24: 89 s then 77 s,
# load 134-151). Its output is printed when host:all ends.
default_lane() {
  lane_mark host:default started
  # Slowest green: 76 s (cold). Tests: 425 s (cold) when it built every default test binary;
  # since it builds only the default-only crates' tests, the bound stays until a slowest green.
  step "clippy, default features" 230 cargo clippy --locked --workspace --all-targets -- -D warnings
  step "tests, default features (the default-only crates: build, list, run)" 1275 default_tests
  lane_mark host:default passed
}
default_pid="" default_out=""
start_default() {
  default_out="$step_tmp_root/default.out"
  local jobs=${CARGO_BUILD_JOBS:-$(nproc)}
  (
    trap - EXIT
    step_tmp_root="$step_tmp_root/default"
    step_count=0
    mkdir "$step_tmp_root"
    start_from "$target_dir" default
    export CARGO_TARGET_DIR="$target_dir/default" CARGO_BUILD_JOBS=$(( jobs > 3 ? jobs / 2 : 1 ))
    default_lane
  ) > "$default_out" 2>&1 &
  default_pid=$!
  export CARGO_BUILD_JOBS=$(( jobs > 3 ? jobs - jobs / 2 : 1 ))
}
# The workspace glob matches nothing until the first crate lands.
if compgen -G "crates/*/*/Cargo.toml" > /dev/null; then
  if [ "$bare_selected" = 1 ]; then
    if [ "$test_lanes" = 1 ]; then
      start_bare
      export CARGO_BUILD_JOBS=$(( ${CARGO_BUILD_JOBS:-$(nproc)} > 3 ? ${CARGO_BUILD_JOBS:-$(nproc)} - 2 : 1 ))
    else
      bare_lanes
    fi
  fi
  # host:default. Default features: no libcamera, no FFmpeg, no Orion, no Daedalus. Styx stands alone.
  if selected host:default; then
    if selected host:all; then
      start_default
    else
      ( start_from "$target_dir" default; export CARGO_TARGET_DIR="$target_dir/default"; default_lane )
    fi
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
  if [ -n "$default_pid" ]; then
    echo "==> host:default (it ran beside host:all):"
    status=0
    wait "$default_pid" || status=$?
    cat "$default_out"; default_out=""
    [ "$status" = 0 ] || { echo "gate: host:default failed" >&2; exit 1; }
  fi
  if [ -f "$lists/default.json" ] && [ -f "$lists/all.json" ]; then
    lane_mark host:default started
    step "every default-only test is in the default-only run" 60 \
      python3 scripts/test-layout.py default-only "$lists/default.json" "$lists/all.json"
    lane_mark host:default passed
  fi
  if [ -n "$bare_pid" ]; then
    echo "==> the bare lanes (they ran beside the host lanes):"
    status=0
    wait "$bare_pid" || status=$?
    cat "$bare_out"; bare_out=""
    [ "$status" = 0 ] || { echo "gate: the bare lanes failed" >&2; exit 1; }
  fi
else
  echo "no crates yet"
fi
if [ -n "$checks_pid" ]; then
  echo "==> host:checks (they ran beside the builds):"
  status=0
  wait "$checks_pid" || status=$?
  cat "$checks_out"; checks_out=""
  [ "$status" = 0 ] || { echo "gate: host:checks failed" >&2; exit 1; }
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
