#!/usr/bin/env bash
# The manual gate before merging to dev, with as little compile work as covers it
# (docs/development.md#the-gate):
#
#   scripts/gate.sh            the gate
#   scripts/gate.sh --full     also the rarely needed extras: zero_alloc repeated
#                              (GATE_STRESS_RUNS, default 10), every release feature set of styx
#                              (check-feature-combinations.sh) and the all-feature workspace
#                              clippy (both need FFmpeg's development files), the memory smoke,
#                              and the tests in one process per test binary (cargo test)
#                              besides nextest's one per test
#   scripts/gate.sh --list     the steps, without running them
#   GATE_SKIP="fuzz cross"     skip steps by name (each skipped step is reported at the end)
#
# How it keeps the work small: styx and the crates under it are compiled for three feature sets
# (the workspace's defaults, linted and tested; one set linted; one set tested) instead of five;
# nextest runs every test binary at once; the same RUSTFLAGS everywhere (the script sets none,
# so no step rebuilds another's artifacts); zero_alloc once (its counts are exact, a repeat
# proves nothing new; --full repeats it); scripts/mcu-size.sh --check only inside check-nostd.sh.
#
# Optional pieces, skipped with a note when missing: cargo-nextest (else cargo test), the nightly
# toolchain (the fuzz project's check), and a linker for aarch64-unknown-linux-gnu with a glibc
# sysroot (CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER, e.g. the HeliOS buildroot's
# aarch64-linux-gcc) for the glibc cross build. The aarch64 musl clippy needs only the rustup
# target.
set -euo pipefail

root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root_dir"

full=0
list=0
for arg in "$@"; do
    case "$arg" in
    --full) full=1 ;;
    --list) list=1 ;;
    -h | --help)
        sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'
        exit 0
        ;;
    *)
        echo "unknown argument: $arg (--full, --list)" >&2
        exit 2
        ;;
    esac
done

# styx's feature-set clippy: the native stack, V4L2, libcamera, Daedalus, the frame socket,
# previews with libjpeg-turbo; styx-record-cli with all its features (a subset of these).
clippy_features="styx/native,styx/v4l2,styx/async,styx/libcamera,styx/daedalus,styx/frame-socket,styx/preview,styx/codec-turbojpeg,styx-record-cli/libcamera,styx-record-cli/native,styx-record-cli/v4l2"
# styx's feature tests in one build: the native features (every styx test, zero_alloc without
# Daedalus included), zero_alloc with Daedalus, styx-record-cli with all its features, and the
# integration tests that need libjpeg-turbo, MCAP replay or previews (camera_service,
# scaled_planning, shared_planning, preview). The tests that need libjpeg-turbo absent run in
# the workspace's default build (`test`).
test_features="styx/native,styx/daedalus,styx/codec-turbojpeg,styx/replay-mcap,styx/preview,styx-record-cli/libcamera,styx-record-cli/native,styx-record-cli/v4l2"
# The aarch64 glibc cross build: what goes to a Raspberry Pi CM5.
cross_features="styx-record-cli/native,styx-record-cli/v4l2"

have_nextest=0
cargo nextest --version >/dev/null 2>&1 && have_nextest=1

# Runs the tests of the given packages: nextest when installed (every test binary in parallel,
# each test in its own process), then the doctests, which nextest does not run.
run_tests() {
    if ((have_nextest)); then
        cargo nextest run --no-fail-fast --cargo-quiet --status-level slow "$@"
        cargo test --doc -q "$@"
    else
        cargo test --no-fail-fast -q "$@"
    fi
}

declare -a names=() skipped=() timings=()
step() {
    local name="$1"
    shift
    names+=("$name")
    if ((list)); then
        printf '%-16s %s\n' "$name" "$*"
        return 0
    fi
    if [[ " ${GATE_SKIP:-} " == *" $name "* ]]; then
        skipped+=("$name (GATE_SKIP)")
        return 0
    fi
    printf '\n==> [%s] %s\n' "$name" "$*"
    local start=$SECONDS
    "$@"
    timings+=("$(printf '%-16s %4ds' "$name" $((SECONDS - start)))")
}

skip() {
    names+=("$1")
    ((list)) && printf '%-16s (skipped: %s)\n' "$1" "$2" && return 0
    skipped+=("$1 ($2)")
}

musl_clippy() {
    # Lint only: nothing links, so no musl toolchain is needed beyond the rustup target.
    cargo clippy -q --target aarch64-unknown-linux-musl -p styx --features native -- -D warnings
    cargo clippy -q --target aarch64-unknown-linux-musl -p styx-native -p native-spike \
        --all-targets -- -D warnings
}

zero_alloc_stress() {
    local runs="${GATE_STRESS_RUNS:-10}" i
    for ((i = 1; i <= runs; i++)); do
        echo "    zero_alloc run $i/$runs"
        # Both packages, as in test-features: the same build, nothing recompiled.
        cargo test -q -p styx -p styx-record-cli --features "$test_features" --test zero_alloc
    done
}

step fmt cargo fmt --all --check
step file-sizes ./scripts/check-file-sizes.sh
step test-targets ./scripts/check-test-targets.sh
step clippy cargo clippy --workspace --all-targets -- -D warnings
step clippy-features cargo clippy -p styx -p styx-record-cli --all-targets \
    --features "$clippy_features" -- -D warnings
step test run_tests --workspace
step test-features run_tests -p styx -p styx-record-cli --features "$test_features"
# check-nostd.sh ends with scripts/mcu-size.sh --check.
step nostd ./scripts/check-nostd.sh
step perf-smoke ./scripts/check-perf-smoke.sh

if cargo +nightly --version >/dev/null 2>&1; then
    step fuzz bash -c 'cd fuzz && cargo +nightly check -q'
else
    skip fuzz "no nightly toolchain"
fi

if rustup target list --installed 2>/dev/null | grep -qx aarch64-unknown-linux-musl; then
    step musl-clippy musl_clippy
else
    skip musl-clippy "rustup target add aarch64-unknown-linux-musl"
fi

if [[ -n "${CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER:-}" ]]; then
    step cross cargo build -q --release --target aarch64-unknown-linux-gnu \
        -p styx -p styx-record-cli --features "$cross_features"
else
    skip cross "set CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER to an aarch64 glibc linker"
fi

if ((full)); then
    step zero-alloc-stress zero_alloc_stress
    # Both build FFmpeg's bindings (codec-ffmpeg), which need its development files.
    if pkg-config --exists libavcodec libavformat libswscale 2>/dev/null; then
        step feature-sets bash ./scripts/check-feature-combinations.sh
        step clippy-all cargo clippy --workspace --all-targets --all-features -- -D warnings
    else
        skip feature-sets "no FFmpeg development files (pkg-config libavcodec)"
        skip clippy-all "no FFmpeg development files (pkg-config libavcodec)"
    fi
    step mem-smoke ./scripts/check-mem-smoke.sh
    step test-libtest cargo test -q --workspace
fi

((list)) && exit 0

printf '\n==> gate passed%s\n' "$( ((full)) && echo ' (--full)')"
printf '    %s\n' "${timings[@]}"
if ((${#skipped[@]})); then
    printf 'skipped:\n'
    printf '    %s\n' "${skipped[@]}"
fi
