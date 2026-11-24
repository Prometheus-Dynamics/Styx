#!/usr/bin/env bash
# The working area outside the repository, made from nothing: worktrees, build output, caches and
# logs, at fixed paths so a worktree and its target stay warm from one item to the next.
#
#   scripts/area.sh setup          make <area>/{wt,target-sv,cache,logs}
#   scripts/area.sh wt NAME        the worktree <area>/wt/NAME on a branch pr/NAME from origin/next:
#                                  made when absent, reused (switched to a new branch) when clean
#   scripts/area.sh env NAME       the environment a build or gate in that worktree runs with, as
#                                  shell lines for `eval "$(scripts/area.sh env NAME)"`
#
# The area is $STYX_AREA, default /mnt/nvme-wd/styx. A worktree's target is <area>/target-sv/NAME
# (one per worktree: docs/gate.md), and the seeds of every target are <area>/target-sv/seeds
# (scripts/cargo-seed.sh). On btrfs, target-sv is a subvolume of its own: with every dep's target
# in one fs tree, the compilers queued on that tree's lock under load (docs/build-storage.md). Nothing in the area is the only copy of anything: tooling and
# evidence live in this repository.
#
# Builds stay within the cores they are given: STYX_CPUS (default 6-31) for taskset, and
# CARGO_BUILD_JOBS (default 13). Every heavy run is at nice STYX_NICE (default 10), one level for
# every dep: scripts/gate.sh and scripts/ab-target.sh set it themselves, styx_run for the rest. GPU tests run on the software Vulkan device only.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
area=${STYX_AREA:-/mnt/nvme-wd/styx}
repo=$(git rev-parse --path-format=absolute --git-common-dir)
repo=${repo%/.git}

die() { echo "area: $*" >&2; exit 1; }
name_ok() { [[ "$1" =~ ^[A-Za-z0-9._-]+$ ]] || die "a worktree name is letters, digits, . _ -: $1"; }

case ${1:-} in
  setup)
    mkdir -p "$area"/wt "$area"/cache "$area"/logs
    if [ ! -e "$area/target-sv" ]; then
      if [ "$(stat -f -c %T "$area")" = btrfs ]; then
        btrfs subvolume create "$area/target-sv" > /dev/null
      else
        mkdir "$area/target-sv"
      fi
    fi
    mkdir -p "$area/target-sv/seeds"
    echo "area: $area ready (wt, target-sv, cache, logs)"
    ;;
  wt)
    name=${2:-}; name_ok "$name"
    "$0" setup > /dev/null
    wt=$area/wt/$name
    git -C "$repo" fetch -q origin
    if [ -d "$wt" ]; then
      [ -z "$(git -C "$wt" status --porcelain)" ] || die "$wt has changes: land or keep them first"
      git -C "$wt" switch -q -C "pr/$name" origin/next
      echo "area: reused $wt on pr/$name at $(git -C "$wt" rev-parse --short HEAD)"
    else
      git -C "$repo" worktree add -q -B "pr/$name" "$wt" origin/next
      echo "area: made $wt on pr/$name at $(git -C "$wt" rev-parse --short HEAD)"
    fi
    ;;
  env)
    name=${2:-}; name_ok "$name"
    lvp=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json
    cat << EOF
export CARGO_TARGET_DIR=$area/target-sv/$name
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-13}
export CARGO_INCREMENTAL=${CARGO_INCREMENTAL:-1}
export VK_DRIVER_FILES=$lvp VK_ICD_FILENAMES=$lvp
export STYX_CPUS=${STYX_CPUS:-6-31}
export STYX_NICE=${STYX_NICE:-10}
styx_run() { taskset -c "\$STYX_CPUS" nice -n "\$(( STYX_NICE > \$(nice) ? STYX_NICE - \$(nice) : 0 ))" "\$@"; }
EOF
    ;;
  *) die "usage: scripts/area.sh setup | wt NAME | env NAME" ;;
esac
