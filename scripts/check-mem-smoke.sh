#!/usr/bin/env bash
# Run the memory smoke scenarios and fail if any uses more memory than its baseline allows,
# or keeps memory after it stopped (a leak).
set -euo pipefail

root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
baseline_file="${MEM_BASELINE_FILE:-$root_dir/testing/perf/memory-baseline.txt}"
max_retained_kb="${MEM_MAX_RETAINED_KB:-64}"

cd "$root_dir"
output="$(cargo run --release -p styx-examples --no-default-features \
    --features codec-turbojpeg,replay-mcap --bin mem_smoke --quiet)"
printf '%s\n' "$output"

field() {
    sed -n "s/.* $2=\([0-9][0-9]*\).*/\1/p" <<<"$1"
}

failures=0
while read -r metric heap_limit rss_limit _; do
    [[ -z "${metric:-}" || "$metric" =~ ^# ]] && continue
    line="$(grep "^$metric " <<<"$output" || true)"
    if [[ -z "$line" ]]; then
        printf 'missing memory metric: %s\n' "$metric" >&2
        failures=$((failures + 1))
        continue
    fi
    heap="$(field "$line" peak_kb)"
    rss="$(field "$line" rss_peak_kb)"
    retained="$(field "$line" retained_kb)"
    if ((heap > heap_limit)); then
        printf 'memory regression: %s heap peak %s KB > %s KB\n' "$metric" "$heap" "$heap_limit" >&2
        failures=$((failures + 1))
    fi
    if ((rss > rss_limit)); then
        printf 'memory regression: %s resident peak %s KB > %s KB\n' "$metric" "$rss" "$rss_limit" >&2
        failures=$((failures + 1))
    fi
    if ((retained > max_retained_kb)); then
        printf 'memory leak: %s kept %s KB after stopping\n' "$metric" "$retained" >&2
        failures=$((failures + 1))
    fi
done <"$baseline_file"

if [[ "$failures" -gt 0 ]]; then
    exit 1
fi
printf 'memory smoke baseline passed (%s)\n' "$baseline_file"
