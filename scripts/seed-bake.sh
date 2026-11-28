#!/usr/bin/env bash
# Bakes the dependency seed of a new toolchain (scripts/cargo-seed.sh), outside the gate. A change
# to rust-toolchain.toml (or a new rustc behind it) leaves no seed any build can use: every
# dependency rebuilds, about 2,300 CPU-s. The bake does that build once, in the background, so
# worktrees after it start warm. The gate never runs it: the gate's supervisor stops whatever a
# step leaves running, and the bake outlasts any gate. scripts/land.sh starts it after a landing.
#
#   scripts/seed-bake.sh                  bake now, in the foreground
#   scripts/seed-bake.sh --background     bake detached, output to <area>/logs/seed-bake-<time>.log
#   scripts/seed-bake.sh --if-due ...     only when no seed has this toolchain (cargo-seed.sh
#                                         toolchain-seeded); otherwise say so and stop
#
# It builds at HEAD, in a detached worktree of its own (<area>/wt/seed-bake) and an empty target
# (<area>/target-sv/seed-bake), what the gate's builds need from the dependencies: clippy and the
# test binaries, with all features and with the defaults. A seed is saved only from a target
# built from empty (cargo-seed.sh save), as this one is. Then the worktree and target go. One bake
# at a time (a lock); at nice STYX_NICE (10) and on STYX_CPUS, in the unit it was started from
# (CPUWeight 100: nothing here changes a weight). It takes no gate slot.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
area=${STYX_AREA:-/mnt/nvme-wd/styx}
wt=$area/wt/seed-bake
target=$area/target-sv/seed-bake
export CARGO_TARGET_DIR=$target

if_due=0 background=0
for a in "$@"; do
  case $a in
    --if-due) if_due=1 ;;
    --background) background=1 ;;
    *) echo "seed-bake: usage: scripts/seed-bake.sh [--if-due] [--background]" >&2; exit 2 ;;
  esac
done
# The gate's own CARGO_HOME (scripts/cargo-home.sh): host:all's, where the seed is restored, since a
# dependency's fingerprint holds its source path under it. STYX_OWN_CARGO_HOME=0 keeps the caller's.
if [ "${STYX_OWN_CARGO_HOME:-1}" != 0 ]; then
  scripts/cargo-home.sh fetch > /dev/null
  CARGO_HOME=$(scripts/cargo-home.sh home main)
  export CARGO_HOME
fi
if [ "$if_due" = 1 ] && scripts/cargo-seed.sh toolchain-seeded; then
  echo "seed-bake: a seed of this toolchain exists: nothing to bake"
  exit 0
fi
if [ "$background" = 1 ]; then
  mkdir -p "$area/logs"
  log=$area/logs/seed-bake-$(date +%Y%m%d-%H%M%S).log
  setsid -f "$0" > "$log" 2>&1 < /dev/null
  echo "seed-bake: baking in the background: $log"
  exit 0
fi

want=${STYX_NICE:-10}
if [ "$(nice)" -lt "$want" ]; then renice -n "$want" -p $$ > /dev/null; fi
exec 9> "$area/target-sv/.seed-bake.lock"
flock -n 9 || { echo "seed-bake: a bake is already running"; exit 0; }
commit=$(git rev-parse HEAD)
root=$(pwd -P)
start=$SECONDS
echo "seed-bake: $commit at nice $(nice), load $(cut -d' ' -f1-3 /proc/loadavg)"

# Our own worktree and target only, by their fixed paths.
cleanup() {
  git -C "$root" worktree remove --force "$wt" 2> /dev/null || true
  [ ! -d "$target" ] || ionice -c3 nice -n19 rm -rf -- "${target:?}"
}
cleanup
trap cleanup EXIT
git -C "$root" worktree add -q --detach "$wt" "$commit"
cd "$wt"
mkdir -p "$target"
echo empty > "$target/.styx-built-from"
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-13} CARGO_INCREMENTAL=${CARGO_INCREMENTAL:-1}
run() {
  local t0=$SECONDS
  taskset -c "${STYX_CPUS:-6-31}" "$@"
  echo "seed-bake: $* : $((SECONDS - t0)) s"
}
run cargo clippy --locked --workspace --all-targets --all-features
run cargo test --locked --workspace --all-features --no-run
run cargo clippy --locked --workspace --all-targets
run cargo test --locked --workspace --no-run
scripts/cargo-seed.sh save
scripts/cargo-seed.sh toolchain-seeded || { echo "seed-bake: no seed of this toolchain after the bake" >&2; exit 1; }
echo "seed-bake: done in $((SECONDS - start)) s, load $(cut -d' ' -f1-3 /proc/loadavg)"
