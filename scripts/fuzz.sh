#!/usr/bin/env bash
# Runs the cargo-fuzz targets in fuzz/ for a while each (docs/fuzzing.md).
#
#   scripts/fuzz.sh                    # every target, 60 s each (the CI smoke run)
#   scripts/fuzz.sh -t 1800 -j 8       # 30 min each, 8 targets at a time
#   scripts/fuzz.sh uvc_descriptors    # some targets only
#   scripts/fuzz.sh -l                 # list the targets
#
# Needs a nightly toolchain and cargo-fuzz (`cargo install cargo-fuzz`). Corpora grow in
# fuzz/corpus/<target> (not committed); the committed seeds in fuzz/seeds/<target> and the
# repository's own data files (sensor descriptions, tuning files, the C270 frame) are read as
# extra inputs. Exits non-zero when any target finds a crash, a timeout, an out-of-memory input
# or a leak; the input is in fuzz/artifacts/<target>/.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fuzz="$root/fuzz"
seconds="${FUZZ_SECONDS:-60}"
jobs=1
toolchain="${FUZZ_TOOLCHAIN:-nightly}"

usage() {
    sed -n '2,15p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

all_targets() {
    sed -n 's/^name = "\(.*\)"$/\1/p' "$fuzz/Cargo.toml" | grep -v '^styx-fuzz$'
}

while getopts "t:j:lh" opt; do
    case "$opt" in
        t) seconds="$OPTARG" ;;
        j) jobs="$OPTARG" ;;
        l) all_targets; exit 0 ;;
        h) usage; exit 0 ;;
        *) usage >&2; exit 2 ;;
    esac
done
shift $((OPTIND - 1))
if [[ $# -gt 0 ]]; then
    targets=("$@")
else
    mapfile -t targets < <(all_targets)
fi

# Options per target: dictionary, largest input, and the repository files to start from.
options() {
    case "$1" in
        uvc_descriptors) echo "-dict=$fuzz/dicts/uvc.dict" ;;
        sensor_description | sensor_subdev) echo "-dict=$fuzz/dicts/sensor.dict -max_len=65536" ;;
        algo_tuning) echo "-dict=$fuzz/dicts/algo_tuning.dict -max_len=262144" ;;
        pisp_stats) echo "-max_len=32768" ;;
        replay_reader) echo "-max_len=65536" ;;
        mjpeg_decode) echo "-max_len=262144" ;;
        *) echo "" ;;
    esac
}

seed_from_repo() {
    local target="$1" corpus="$2"
    case "$target" in
        sensor_description)
            cp "$root"/crates/sensor/sensors/ov9782.toml "$corpus/repo-ov9782.toml"
            cp "$root"/examples/06_new_camera/example_sensor.toml "$corpus/repo-example.toml"
            ;;
        algo_tuning)
            cp "$root"/crates/pipeline/tuning/ov9782.json "$corpus/repo-ov9782.json"
            cp "$root"/crates/pipeline/tuning/generic.toml "$corpus/repo-generic.toml"
            cp "$root"/crates/algo/tests/data/rpi-minimal.json "$corpus/repo-rpi-minimal.json"
            cp "$root"/crates/algo/tests/data/sim.toml "$corpus/repo-sim.toml"
            ;;
        pisp_stats)
            # One statistics buffer (23200 bytes): all zero, and random.
            head -c 23200 /dev/zero >"$corpus/repo-zero"
            [[ -f "$corpus/repo-random" ]] || head -c 23200 /dev/urandom >"$corpus/repo-random"
            ;;
        mjpeg_decode)
            cp "$root"/testing/fixtures/c270_720p_rst.mjpeg "$corpus/repo-c270.mjpeg"
            ;;
    esac
}

cd "$fuzz"
echo "building the fuzz targets (cargo +$toolchain fuzz build, debug assertions on)"
cargo "+$toolchain" fuzz build -O --debug-assertions

run() {
    local target="$1"
    local corpus="$fuzz/corpus/$target" artifacts="$fuzz/artifacts/$target"
    mkdir -p "$corpus" "$artifacts"
    seed_from_repo "$target" "$corpus"
    local inputs=("$corpus")
    [[ -d "$fuzz/seeds/$target" ]] && inputs+=("$fuzz/seeds/$target")
    # Requests are client messages: start from those.
    [[ $target == ipc_request ]] && inputs+=("$fuzz/seeds/ipc_messages")
    local log="$fuzz/artifacts/$target.log"
    # shellcheck disable=SC2046 # options are split on purpose
    if cargo "+$toolchain" fuzz run -O --debug-assertions "$target" "${inputs[@]}" -- \
        -max_total_time="$seconds" -timeout=10 -rss_limit_mb=2048 -malloc_limit_mb=512 \
        -artifact_prefix="$artifacts/" -print_final_stats=1 $(options "$target") >"$log" 2>&1; then
        local execs
        execs="$(sed -n 's/^stat::number_of_executed_units: *//p' "$log" | tail -1)"
        echo "ok    $target (${seconds}s, ${execs:-?} execs)"
    else
        echo "FAIL  $target: see $log and $artifacts/"
        grep -E "^(==[0-9]+==ERROR|thread .* panicked|SUMMARY)" "$log" | head -5 || true
        return 1
    fi
}

failed=0
running=0
pids=()
for target in "${targets[@]}"; do
    run "$target" &
    pids+=($!)
    running=$((running + 1))
    if [[ $running -ge $jobs ]]; then
        wait -n || failed=1
        running=$((running - 1))
    fi
done
while [[ $running -gt 0 ]]; do
    wait -n || failed=1
    running=$((running - 1))
done
exit "$failed"
