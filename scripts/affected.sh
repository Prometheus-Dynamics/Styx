#!/usr/bin/env bash
# The workspace packages a change can affect: the packages whose files changed since the merge
# base with BASE (default `dev`; committed, staged, unstaged and untracked files), plus every
# workspace package that depends on them (normal, dev or build dependency, transitively).
# `styx-fuzz` stands for the fuzz project (fuzz/, not a workspace member).
#
#   scripts/affected.sh [BASE]     one package name per line; `ALL` when everything is affected
#
# Everything is affected when a file outside a package changed that builds can read (Cargo.toml,
# Cargo.lock, rust-toolchain.toml, .cargo/, scripts/, testing/, kernel-modules/, ...), or a file
# inside a package other than its Rust sources, its Cargo.toml and its Markdown (tests read
# other packages' data files: tuning files, sensor descriptions). Nothing is affected by docs/,
# Markdown outside the packages, LICENSE files, .github/ and the packages outside the workspace
# (crates/gst-styx, crates/pipewire-styx), which no local check builds. Without jq, `ALL`.
# scripts/gate.sh --changed uses it.
set -euo pipefail

root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root_dir"

base="${1:-dev}"
if ! command -v jq >/dev/null 2>&1; then
    echo ALL
    exit 0
fi
merge_base="$(git merge-base HEAD "$base" 2>/dev/null)" || {
    echo "affected.sh: no merge base with $base" >&2
    echo ALL
    exit 0
}

mapfile -t files < <({
    git diff --name-only "$merge_base"
    git ls-files --others --exclude-standard
} | sort -u)
((${#files[@]})) || exit 0

metadata="$(cargo metadata --format-version 1 --no-deps --offline 2>/dev/null ||
    cargo metadata --format-version 1 --no-deps)"
# "name<TAB>dir relative to the root" per workspace member, longest directory first.
mapfile -t members < <(jq -r --arg root "$root_dir/" '.packages[]
    | [.name, (.manifest_path | ltrimstr($root) | rtrimstr("Cargo.toml") | rtrimstr("/"))]
    | @tsv' <<<"$metadata" | awk -F'\t' '{ print length($2) "\t" $0 }' | sort -rn | cut -f2-)

declare -A changed=()
fuzz=0
for file in "${files[@]}"; do
    case "$file" in
    docs/* | .github/* | LICENSE* | crates/gst-styx/* | crates/pipewire-styx/*) continue ;;
    fuzz/*)
        fuzz=1
        continue
        ;;
    */*) ;;
    *.md) continue ;;
    esac
    owner=""
    for member in "${members[@]}"; do
        dir="${member#*$'\t'}"
        if [[ -n "$dir" && "$file" == "$dir/"* ]]; then
            owner="${member%%$'\t'*}"
            break
        fi
    done
    case "$file" in
    *.rs | */Cargo.toml | *.md) ;;
    *) owner="" ;;
    esac
    if [[ -z "$owner" ]]; then
        echo ALL
        exit 0
    fi
    changed["$owner"]=1
done

# Reverse path dependencies, to a fixed point.
mapfile -t edges < <(jq -r '.packages[] | .name as $p | .dependencies[] | select(.path)
    | "\(.name)\t\($p)"' <<<"$metadata" | sort -u)
grew=1
while ((grew)); do
    grew=0
    for edge in "${edges[@]}"; do
        dep="${edge%%$'\t'*}"
        user="${edge#*$'\t'}"
        if [[ -n "${changed[$dep]:-}" && -z "${changed[$user]:-}" ]]; then
            changed["$user"]=1
            grew=1
        fi
    done
done

# The fuzz project builds against these packages (its path dependencies).
if ((!fuzz)); then
    while read -r dep; do
        [[ -n "${changed[$dep]:-}" ]] && fuzz=1
    done < <(sed -n 's/^.*package *= *"\([a-z-]*\)".*path *= *"\.\.\/.*/\1/p; s/^\([a-z-]*\) *= *{.*path *= *"\.\.\/.*/\1/p' \
        fuzz/Cargo.toml | sort -u)
fi
((fuzz)) && changed[styx-fuzz]=1

((${#changed[@]})) && printf '%s\n' "${!changed[@]}" | sort
exit 0
