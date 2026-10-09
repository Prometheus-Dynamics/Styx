#!/usr/bin/env bash
# Fails when a target marked `test = false` holds tests: `cargo test` would silently skip them.
# Binaries without tests are marked so `cargo test` does not build and link an empty test
# harness for each (docs/development.md#the-gate); this keeps the marking honest. Each such
# target's source is searched with the modules it declares (`mod name;`).
set -euo pipefail

root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root_dir"

# The files of a module tree: the root, then each `mod name;` it (or a module) declares.
module_files() {
    local -a queue=("$1")
    local file dir name
    while ((${#queue[@]})); do
        file="${queue[0]}"
        queue=("${queue[@]:1}")
        [[ -f "$file" ]] || continue
        echo "$file"
        case "$(basename "$file")" in
        main.rs | lib.rs | mod.rs) dir="$(dirname "$file")" ;;
        *) dir="${file%.rs}" ;;
        esac
        # A bin's root in a shared directory (examples/) declares its modules next to it.
        [[ -d "$dir" ]] || dir="$(dirname "$file")"
        while IFS= read -r name; do
            if [[ -f "$dir/$name.rs" ]]; then
                queue+=("$dir/$name.rs")
            else
                queue+=("$dir/$name/mod.rs")
            fi
        done < <(sed -nE 's/^[[:space:]]*(pub(\([a-z]+\))?[[:space:]]+)?mod[[:space:]]+([a-z_0-9]+);.*/\3/p' "$file")
    done
}

failures=0
while IFS= read -r manifest; do
    dir="$(dirname "$manifest")"
    # Each `test = false` table's `path` ([[bin]], [[example]], [lib]).
    while IFS= read -r path; do
        [[ -z "$path" ]] && continue
        while IFS= read -r file; do
            if grep -qE '#\[(test|cfg\(test\))\]' "$file"; then
                echo "$file has tests, but its target is marked test = false ($manifest)" >&2
                failures=$((failures + 1))
            fi
        done < <(module_files "$dir/$path")
    done < <(awk '
        /^\[/ { if (off && path != "") print path; off = 0; path = "" }
        /^path = "/ { p = $0; sub(/^path = "/, "", p); sub(/".*/, "", p); path = p }
        /^test = false/ { off = 1 }
        END { if (off && path != "") print path }
    ' "$manifest")
done < <(git ls-files '*Cargo.toml')

if ((failures)); then
    exit 1
fi
