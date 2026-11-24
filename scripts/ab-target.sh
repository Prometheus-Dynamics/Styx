#!/usr/bin/env bash
# A/B of where a worktree's target lives, measured on the change that hurts most: a one-line edit
# to styx-frame (low in the graph), then every all-features test binary rebuilt, and the edit
# taken back out and rebuilt again. Each round builds every target on the same source, so all see
# the same machine load; the order rotates by round, so a load that rises or falls over the run
# favours none. Prints one line per build: wall, user and sys CPU-s, the load, and the I/O stall
# seconds of the machine and of this cgroup over the build (PSI "some" and "full"); with
# STYX_AB_WAITS=1, also what the build's threads waited on (scripts/dstate-sample.sh). With
# STYX_AB_RUN=1, each round ends with every target's all-features nextest run, timed the same way
# (the run's result is printed; a failure stops the A/B).
#
#   scripts/ab-target.sh ROUNDS TARGET...      (two or more; labelled A, B, C, ... in that order)
#
# A TARGET is a directory, or DIR@VAR=VALUE[,VAR=VALUE...] to build it with those variables set
# (e.g. DIR@CARGO_PROFILE_DEV_OPT_LEVEL=0).
#
# Every target must already be warm (one full build each, with the same CARGO_INCREMENTAL).
# The edit is a comment appended to crates/core/frame/src/lib.rs and removed again with git;
# the file is left as committed. Measurement only: it runs tests only with STYX_AB_RUN=1.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
# Every heavy run is at one nice level across the deps (STYX_NICE, default 10: the coordinator's
# rule of 2026-11-24, since mixed levels starve the lower ones). A process can raise its own nice
# level, not lower it: started above it, the run says so and stays where it is.
want=${STYX_NICE:-10}
if [ "$(nice)" -lt "$want" ]; then
  renice -n "$want" -p $$ > /dev/null
elif [ "$(nice)" -gt "$want" ]; then
  echo "$(basename "$0"): at nice $(nice), above $want: started from a lower-priority shell" >&2
fi
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
  local label=$1 spec=$2 target tf wf pid load out wall user sys vars=() cmd
  target=${spec%%@*}
  [ "$target" = "$spec" ] || IFS=, read -r -a vars <<< "${spec#*@}"
  if [ "$3" = run ]; then
    cmd=(cargo nextest run --locked --workspace --all-features --no-fail-fast)
  else
    cmd=(cargo test --locked --workspace --all-features --no-run)
  fi
  read -r ms mf <<< "$(stall /proc/pressure/io)"; read -r cs cf <<< "$(stall "$cg/io.pressure")"
  load=$(cut -d' ' -f1 /proc/loadavg)
  tf=$(mktemp); wf=$(mktemp)
  env CARGO_TARGET_DIR="$target" "${vars[@]}" /usr/bin/time -o "$tf" -f '%e %U %S' \
    taskset -c "${STYX_CPUS:-6-31}" "${cmd[@]}" > "$tf.out" 2>&1 &
  pid=$!
  if [ "${STYX_AB_WAITS:-0}" = 1 ]; then scripts/dstate-sample.sh "$pid" > "$wf" & fi
  if ! wait "$pid"; then
    echo "ab-target: $label $3 failed ($spec):" >&2; tail -30 "$tf.out" >&2; exit 1
  fi
  wait
  [ "$3" != run ] || grep -E '^ +Summary' "$tf.out" | sed 's/^ */      /'
  rm -f "$tf.out"
  out=$(tail -1 "$tf"); rm -f "$tf"
  read -r ms2 mf2 <<< "$(stall /proc/pressure/io)"; read -r cs2 cf2 <<< "$(stall "$cg/io.pressure")"
  read -r wall user sys <<< "$out"
  printf '%-3s %-6s wall %6.1f s  user %6.1f  sys %5.1f  load %6s  io-stall machine some %5.1f full %5.1f s  cgroup some %5.1f full %5.1f s  %s\n' \
    "$label" "$3" "$wall" "$user" "$sys" "$load" \
    "$(echo "($ms2-$ms)/1000000" | bc -l)" "$(echo "($mf2-$mf)/1000000" | bc -l)" \
    "$(echo "($cs2-$cs)/1000000" | bc -l)" "$(echo "($cf2-$cf)/1000000" | bc -l)" "$spec"
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
  if [ "${STYX_AB_RUN:-0}" = 1 ]; then
    for i in "${order[@]}"; do build "${labels[$i]}" "${targets[$i]}" run; done
  fi
done
