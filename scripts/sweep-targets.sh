#!/usr/bin/env bash
# Finds and deletes stale build output of this repository's checkouts (docs/development.md
# "Target directories"): each worktree's target/ and target-*/, and the per-checkout directories
# under STYX_BUILD_ROOT (scripts/dev-env.sh).
#
#   scripts/sweep-targets.sh                  list them with the days since their last build
#   scripts/sweep-targets.sh --sizes          also their sizes (walks every file: slow on a busy disk)
#   scripts/sweep-targets.sh --delete DAYS    delete those not built for DAYS days or more
#   scripts/sweep-targets.sh --delete-path P  delete P (a target directory) now
#
# Deleting renames the directory first (instant; nothing can build into it any more), then
# removes it in the background at idle disk priority (ionice -c3, nice), so other builds keep the
# disk. A directory that is a btrfs subvolume is deleted with `btrfs subvolume delete`, which
# returns at once (it needs root or the user_subvol_rm_allowed mount option; otherwise rm -rf).
# The background deletions log to ${TMPDIR:-/tmp}/styx-sweep-<time>.log.
set -euo pipefail

root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

sizes=0
delete_days=""
delete_path=""
case "${1:-}" in
"") ;;
--sizes) sizes=1 ;;
--delete) delete_days="${2:?days}" ;;
--delete-path) delete_path="${2:?path}" ;;
-h | --help)
    sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'
    exit 0
    ;;
*)
    echo "unknown argument: $1 (--help)" >&2
    exit 2
    ;;
esac

# Deletes a directory: rename aside, then remove at idle priority in the background.
remove() {
    local dir="${1%/}" trash log
    [[ -d "$dir" ]] || return 0
    trash="$(dirname "$dir")/.$(basename "$dir").trash-$(date +%s)-$$"
    mv "$dir" "$trash"
    log="${TMPDIR:-/tmp}/styx-sweep-$(date +%s).log"
    if btrfs subvolume show "$trash" >/dev/null 2>&1 &&
        btrfs subvolume delete "$trash" >>"$log" 2>&1; then
        echo "deleted (btrfs subvolume): $dir"
        return 0
    fi
    nohup ionice -c3 nice -n 19 rm -rf "$trash" >>"$log" 2>&1 &
    echo "deleting in the background: $dir (now $trash; log $log)"
}

if [[ -n "$delete_path" ]]; then
    remove "$delete_path"
    exit 0
fi

# Days since the last build into a target directory: the newest of its profile directories'
# lock and fingerprint entries (no deep walk).
age_days() {
    local newest
    newest="$(find "$1" -maxdepth 3 \( -name .cargo-lock -o -name .cargo-build-lock \
        -o -name .fingerprint \) -printf '%T@\n' 2>/dev/null | sort -n | tail -1)"
    [[ -n "$newest" ]] || newest="$(stat -c %Y "$1")"
    echo $((($(date +%s) - ${newest%.*}) / 86400))
}

declare -a dirs=()
while read -r line; do
    [[ "$line" == worktree\ * ]] || continue
    wt="${line#worktree }"
    for dir in "$wt"/target "$wt"/target-*; do
        [[ -d "$dir" ]] && dirs+=("$dir")
    done
done < <(git -C "$root_dir" worktree list --porcelain)
if [[ -n "${STYX_BUILD_ROOT:-}" && -d "$STYX_BUILD_ROOT" ]]; then
    for dir in "$STYX_BUILD_ROOT"/*/*/; do
        [[ -d "$dir" ]] && dirs+=("${dir%/}")
    done
fi

for dir in "${dirs[@]}"; do
    age="$(age_days "$dir")"
    size=""
    ((sizes)) && size="$(du -sh "$dir" 2>/dev/null | cut -f1)"
    printf '%4s days  %6s  %s\n' "$age" "$size" "$dir"
    if [[ -n "$delete_days" ]] && ((age >= delete_days)); then
        remove "$dir"
    fi
done
