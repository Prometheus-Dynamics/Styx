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

run_and_record \
    "pipeline decode/transform perf smoke" \
    cargo run --release -p "$package" --no-default-features --features codec-jpeg-decoder --bin perf_smoke --quiet

run_and_record \
    "file replay perf smoke" \
    cargo run --release -p "$package" --no-default-features --features file-backend --bin file_replay_perf --quiet

run_and_record \
    "mozjpeg encode perf smoke" \
    cargo run --release -p "$package" --no-default-features --features codec-mozjpeg --bin encode_perf --quiet

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
