#!/usr/bin/env bash
set -euo pipefail

root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
baseline_file="${PERF_BASELINE_FILE:-$root_dir/testing/perf/baseline.txt}"
package="styx-examples"

if [[ ! -f "$baseline_file" ]]; then
    printf 'missing perf baseline: %s\n' "$baseline_file" >&2
    exit 1
fi

declare -A p95_by_metric=()
declare -A limit_by_metric=()

record_output() {
    local output="$1"
    local metric p95
    while IFS= read -r line; do
        [[ -z "$line" ]] && continue
        metric="${line%% *}"
        p95="$(sed -n 's/.*p95_ms=\([0-9.][0-9.]*\).*/\1/p' <<<"$line")"
        if [[ -n "$metric" && -n "$p95" ]]; then
            p95_by_metric["$metric"]="$p95"
        fi
    done <<<"$output"
}

run_and_record() {
    local label="$1"
    shift
    printf '==> %s\n' "$label"
    local output
    output="$("$@")"
    printf '%s\n' "$output"
    record_output "$output"
}

while read -r metric limit _; do
    [[ -z "${metric:-}" || "$metric" =~ ^# ]] && continue
    limit_by_metric["$metric"]="$limit"
done <"$baseline_file"

cd "$root_dir"

# Three release builds instead of one per binary. Each group keeps what its binaries measure:
# - libjpeg-turbo + MCAP: luma_perf (its decoder named explicitly) and shared_perf (the planner
#   picks the decoder, so exactly its own features); check-mem-smoke.sh builds mem_smoke with
#   the same features, so CI shares this build.
# - jpeg-decoder + file backend: perf_smoke (its decoder named explicitly) and file_replay_perf
#   (the file backend decodes with `image`/`jpeg-decoder` itself, not through the registry).
# - mozjpeg on its own: it and libjpeg-turbo do not link into one binary.
build() {
    cargo build --release -q -p "$package" --no-default-features --features "$1" "${@:2}"
}
build codec-turbojpeg,replay-mcap --bin luma_perf --bin shared_perf
build codec-jpeg-decoder,file-backend --bin perf_smoke --bin file_replay_perf
build codec-mozjpeg --bin encode_perf
bin_dir="${CARGO_TARGET_DIR:-$root_dir/target}/release"

run_and_record "pipeline decode/transform perf smoke" "$bin_dir/perf_smoke"
run_and_record "MJPEG luma perf smoke (C270 fixture)" "$bin_dir/luma_perf"
run_and_record "shared capture and frame server perf smoke (C270 fixture)" "$bin_dir/shared_perf"
run_and_record "file replay perf smoke" "$bin_dir/file_replay_perf"
run_and_record "mozjpeg encode perf smoke" "$bin_dir/encode_perf"

failures=0
for metric in "${!limit_by_metric[@]}"; do
    if [[ -z "${p95_by_metric[$metric]:-}" ]]; then
        printf 'missing perf metric: %s\n' "$metric" >&2
        failures=$((failures + 1))
        continue
    fi
    limit="${limit_by_metric[$metric]}"
    p95="${p95_by_metric[$metric]}"
    if ! awk -v p95="$p95" -v limit="$limit" 'BEGIN { exit !(p95 <= limit) }'; then
        printf 'perf regression: %s p95_ms=%s limit=%s\n' "$metric" "$p95" "$limit" >&2
        failures=$((failures + 1))
    fi
done

if [[ "$failures" -gt 0 ]]; then
    exit 1
fi

printf 'perf smoke baseline passed (%s)\n' "$baseline_file"
