#!/usr/bin/env bash
set -euo pipefail

root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
baseline_file="$root_dir/testing/ci/file-size-baseline.txt"
limit="${FILE_SIZE_LINE_LIMIT:-800}"
near_limit="${FILE_SIZE_NEAR_LIMIT:-750}"
exclude_dirs="${FILE_SIZE_EXCLUDE_DIRS:-}"

declare -a scan_roots=()
for candidate in crates plugins testing; do
    if [[ -d "$root_dir/$candidate" ]]; then
        scan_roots+=("$root_dir/$candidate")
    fi
done

if [[ ${#scan_roots[@]} -eq 0 ]]; then
    exit 0
fi

declare -A baseline=()
if [[ -f "$baseline_file" ]]; then
    while IFS= read -r line; do
        [[ -z "$line" || "$line" =~ ^# ]] && continue
        baseline["$line"]=1
    done <"$baseline_file"
fi

is_excluded() {
    local rel_path="$1"
    local raw
    IFS=':' read -r -a raw <<<"$exclude_dirs"
    for entry in "${raw[@]}"; do
        entry="${entry#/}"
        entry="${entry%/}"
        [[ -z "$entry" ]] && continue
        if [[ "$rel_path" == "$entry" || "$rel_path" == "$entry/"* ]]; then
            return 0
        fi
    done
    return 1
}

declare -a over_limit=()
while IFS= read -r record; do
    lines="${record%% *}"
    path="${record#* }"
    rel_path="${path#$root_dir/}"
    over_limit+=("$lines $rel_path")
done < <(find "${scan_roots[@]}" -type f -name '*.rs' -print0 | xargs -0 wc -l | awk -v limit="$limit" '$2 != "total" && $1 > limit { print $1 " " $2 }' | sort -nr)

declare -a near_limit_files=()
while IFS= read -r record; do
    lines="${record%% *}"
    path="${record#* }"
    rel_path="${path#$root_dir/}"
    near_limit_files+=("$lines $rel_path")
done < <(find "${scan_roots[@]}" -type f -name '*.rs' -print0 | xargs -0 wc -l | awk -v near_limit="$near_limit" -v limit="$limit" '$2 != "total" && $1 >= near_limit && $1 <= limit { print $1 " " $2 }' | sort -nr)

declare -a filtered=()
for entry in "${over_limit[@]}"; do
    rel_path="${entry#* }"
    if ! is_excluded "$rel_path"; then
        filtered+=("$entry")
    fi
done
over_limit=("${filtered[@]}")

filtered=()
for entry in "${near_limit_files[@]}"; do
    rel_path="${entry#* }"
    if ! is_excluded "$rel_path"; then
        filtered+=("$entry")
    fi
done
near_limit_files=("${filtered[@]}")

declare -a violations=()
for entry in "${over_limit[@]}"; do
    rel_path="${entry#* }"
    if [[ -z "${baseline[$rel_path]:-}" ]]; then
        violations+=("$entry")
    fi
done

if [[ ${#over_limit[@]} -gt 0 ]]; then
    printf 'Warning: Rust files over %s lines:\n' "$limit"
    printf '  %s\n' "${over_limit[@]}"
fi

if [[ ${#near_limit_files[@]} -gt 0 ]]; then
    printf 'Notice: Rust files at or above %s lines and within the %s-line limit:\n' "$near_limit" "$limit"
    printf '  %s\n' "${near_limit_files[@]}"
fi

if [[ ${#violations[@]} -gt 0 ]]; then
    printf '\nWarning: new files over %s lines:\n' "$limit" >&2
    printf '  %s\n' "${violations[@]}" >&2
    exit 1
fi
