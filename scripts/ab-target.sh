#!/usr/bin/env bash
# A/B of where a worktree's target lives, measured on the change that hurts most: a one-line edit
# to styx-frame (low in the graph), then every all-features test binary rebuilt, and the edit
# taken back out and rebuilt again. Each round builds A then B on the same source, so both see
# the same machine load. Prints one line per build: wall, user and sys CPU-s, the load, and the
# I/O stall seconds of the machine and of this cgroup over the build (PSI "some" and "full").
#
#   scripts/ab-target.sh ROUNDS TARGET_A TARGET_B
#
# Both targets must already be warm (one full build each, with the same CARGO_INCREMENTAL).
# The edit is a comment appended to crates/core/frame/src/lib.rs and removed again with git;
# the file is left as committed. Measurement only: it never runs a test.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
rounds=${1:?rounds}; a=${2:?target A}; b=${3:?target B}
file=crates/core/frame/src/lib.rs
git diff --quiet -- "$file" || { echo "ab-target: $file has changes" >&2; exit 1; }
cg=/sys/fs/cgroup$(cut -d: -f3 /proc/self/cgroup)
stall() { # total stall microseconds: <some> <full>
  awk '/^some/{split($5,s,"=")} /^full/{split($5,f,"=")} END{print s[2], f[2]}' "$1"
}
build() {
  local label=$1 target=$2 t0 m0 c0 m1 c1 load
  read -r ms mf <<< "$(stall /proc/pressure/io)"; read -r cs cf <<< "$(stall "$cg/io.pressure")"
  load=$(cut -d' ' -f1 /proc/loadavg)
  local tf; tf=$(mktemp)
  CARGO_TARGET_DIR=$target /usr/bin/time -o "$tf" -f '%e %U %S' taskset -c "${STYX_CPUS:-6-31}" \
    cargo test --locked --workspace --all-features --no-run > /dev/null 2>&1
  local out; out=$(tail -1 "$tf"); rm -f "$tf"
  read -r ms2 mf2 <<< "$(stall /proc/pressure/io)"; read -r cs2 cf2 <<< "$(stall "$cg/io.pressure")"
  read -r wall user sys <<< "$out"
  printf '%-8s %-6s wall %6.1f s  user %6.1f  sys %5.1f  load %6s  io-stall machine some %5.1f full %5.1f s  cgroup some %5.1f full %5.1f s\n' \
    "$label" "$3" "$wall" "$user" "$sys" "$load" \
    "$(echo "($ms2-$ms)/1000000" | bc -l)" "$(echo "($mf2-$mf)/1000000" | bc -l)" \
    "$(echo "($cs2-$cs)/1000000" | bc -l)" "$(echo "($cf2-$cf)/1000000" | bc -l)"
}
trap 'git checkout -q -- "$file"' EXIT
for r in $(seq 1 "$rounds"); do
  # The order alternates by round, so a load that rises or falls over the run favours neither.
  if [ $((r % 2)) = 1 ]; then first=(A "$a"); second=(B "$b"); else first=(B "$b"); second=(A "$a"); fi
  echo "// ab-target probe $r" >> "$file"
  build "${first[@]}" edit; build "${second[@]}" edit
  git checkout -q -- "$file"
  build "${first[@]}" revert; build "${second[@]}" revert
done
