#!/usr/bin/env bash
# A/B of where a worktree's target lives, measured on the change that hurts most: a one-line edit
# to styx-frame (low in the graph), then every all-features test binary rebuilt, and the edit
# taken back out and rebuilt again. Each round builds every target on the same source, so all see
# the same machine load; the order rotates by round, so a load that rises or falls over the run
# favours none. Prints one line per build: wall, user and sys CPU-s, the load, and the I/O stall
# seconds of the machine and of this cgroup over the build (PSI "some" and "full"); with
# STYX_AB_WAITS=1, also what the build's threads waited on (scripts/dstate-sample.sh).
#
#   scripts/ab-target.sh ROUNDS TARGET...      (two or more; labelled A, B, C, ... in that order)
#
# Every target must already be warm (one full build each, with the same CARGO_INCREMENTAL).
# The edit is a comment appended to crates/core/frame/src/lib.rs and removed again with git;
# the file is left as committed. Measurement only: it never runs a test.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
rounds=${1:?rounds}; shift
[ $# -ge 2 ] || { echo "ab-target: give two or more targets" >&2; exit 1; }
targets=("$@") labels=()
for i in "${!targets[@]}"; do labels+=("$(printf "\\$(printf '%03o' $((65 + i)))")"); done
file=crates/core/frame/src/lib.rs
git diff --quiet -- "$file" || { echo "ab-target: $file has changes" >&2; exit 1; }
cg=/sys/fs/cgroup$(cut -d: -f3 /proc/self/cgroup)
stall() { # total stall microseconds: <some> <full>
  awk '/^some/{split($5,s,"=")} /^full/{split($5,f,"=")} END{print s[2], f[2]}' "$1"
}
build() {
  local label=$1 target=$2 tf wf pid load out wall user sys
  read -r ms mf <<< "$(stall /proc/pressure/io)"; read -r cs cf <<< "$(stall "$cg/io.pressure")"
  load=$(cut -d' ' -f1 /proc/loadavg)
  tf=$(mktemp); wf=$(mktemp)
  CARGO_TARGET_DIR=$target /usr/bin/time -o "$tf" -f '%e %U %S' taskset -c "${STYX_CPUS:-6-31}" \
    cargo test --locked --workspace --all-features --no-run > /dev/null 2>&1 &
  pid=$!
  if [ "${STYX_AB_WAITS:-0}" = 1 ]; then scripts/dstate-sample.sh "$pid" > "$wf" & fi
  wait "$pid"
  wait
  out=$(tail -1 "$tf"); rm -f "$tf"
  read -r ms2 mf2 <<< "$(stall /proc/pressure/io)"; read -r cs2 cf2 <<< "$(stall "$cg/io.pressure")"
  read -r wall user sys <<< "$out"
  printf '%-3s %-6s wall %6.1f s  user %6.1f  sys %5.1f  load %6s  io-stall machine some %5.1f full %5.1f s  cgroup some %5.1f full %5.1f s  %s\n' \
    "$label" "$3" "$wall" "$user" "$sys" "$load" \
    "$(echo "($ms2-$ms)/1000000" | bc -l)" "$(echo "($mf2-$mf)/1000000" | bc -l)" \
    "$(echo "($cs2-$cs)/1000000" | bc -l)" "$(echo "($cf2-$cf)/1000000" | bc -l)" "$target"
  if [ -s "$wf" ]; then sed 's/^/      /' "$wf"; fi
  rm -f "$wf"
}
for i in "${!targets[@]}"; do echo "${labels[$i]} = ${targets[$i]}"; done
trap 'git checkout -q -- "$file"' EXIT
n=${#targets[@]}
for r in $(seq 1 "$rounds"); do
  order=()
  for k in $(seq 0 $((n - 1))); do order+=($(((k + r - 1) % n))); done
  echo "// ab-target probe $r" >> "$file"
  for i in "${order[@]}"; do build "${labels[$i]}" "${targets[$i]}" edit; done
  git checkout -q -- "$file"
  for i in "${order[@]}"; do build "${labels[$i]}" "${targets[$i]}" revert; done
done
