#!/usr/bin/env bash
# The gate's false-pass check: a planted failing step, run through each lane exactly as the gate
# runs it, must leave that lane failed and the gate red. scripts/gate.sh runs dry here
# (STYX_GATE_DRY: every step's command is `true`, the one labelled STYX_GATE_PLANT runs
# planted_failure, a failing command followed by a passing one), with the lanes, subshells, waits
# and pass marks of a real gate, then scripts/gate.py lane-status reads the marks as `gate.py run`
# does. Each lane is planted at its first step and at its last (a red first step with a green last
# one must fail the lane), a step outside every lane fails every lane, and an unplanted run is green
# with every lane passed (the check can pass at all). Nothing is built or recorded. Before that, its
# own reuse key: a change to gate.sh must make it run again (below).
#
#   scripts/gate-selftest.sh
#
# Proof it can be red (2026-11-27, against copies of gate.sh with each fault put back): with step()
# returning 0 after its time line (Atlas's first false pass), all 11 planted cases red; with the
# side lanes' subshells under `set +e` (a lane's status is its last step's: Atlas's second), the 8
# planted in the side lanes red. Green: 5.5-11.8 s at load 52-74, nice 10. The key check
# (2026-11-27): with gate.sh left out of SELFTEST_FILES, red ("a changed gate.sh keeps the key").
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
scratch=$(mktemp -d "${TMPDIR:-/tmp}/gate-selftest.XXX")
trap 'rm -rf -- "${scratch:?}"' EXIT
# Its own reuse key first (gate.py selftest-key): on copies of the files it tests, an unchanged
# copy keys the same, a gate.sh with one line added keys differently, and a green record of the
# old key is reused while the new key runs. CI and reuse-off are left out here: both always run.
keyroot=$scratch/keyroot
mkdir -p "$keyroot/scripts"
cp scripts/gate.sh scripts/gate.py scripts/gate-selftest.sh scripts/gate-supervise.py "$keyroot/scripts/"
k_same=$(python3 scripts/gate.py selftest-key --root "$keyroot")
[ "$k_same" = "$(python3 scripts/gate.py selftest-key)" ] ||
  { echo "gate-selftest: FAILED: an unchanged copy keys $k_same, the tree $(python3 scripts/gate.py selftest-key)"; exit 1; }
echo "# planted by gate-selftest.sh" >> "$keyroot/scripts/gate.sh"
k_changed=$(python3 scripts/gate.py selftest-key --root "$keyroot")
[ "$k_changed" != "$k_same" ] || { echo "gate-selftest: FAILED: a changed gate.sh keeps the key $k_same"; exit 1; }
key_env=(env -u CI -u GITHUB_ACTIONS -u STYX_GATE_NO_REUSE STYX_GATE_RECORDS="$scratch/records")
"${key_env[@]}" python3 scripts/gate.py selftest-record "$k_same"
"${key_env[@]}" python3 scripts/gate.py selftest-reuse "$k_same" > /dev/null ||
  { echo "gate-selftest: FAILED: a green record of $k_same was not reused"; exit 1; }
if "${key_env[@]}" python3 scripts/gate.py selftest-reuse "$k_changed" 2> /dev/null; then
  echo "gate-selftest: FAILED: the changed gate.sh's key $k_changed was reused"; exit 1
fi
echo "    red as it should be: a changed gate.sh keys $k_changed, not $k_same: the self-test runs again"
lanes=(host:checks host:all host:default bare:thumbv7em bare:riscv32imac bare:riscv32imc)
# plant label | the lane it is in ("" for a step outside every lane)
cases=(
  "format|host:checks"
  "one source for every shm crate|host:checks"
  "clippy, all features|host:all"
  "every test group filter matches a test|host:all"
  "clippy, default features|host:default"
  "tests, default features (the default-only crates: build, list, run)|host:default"
  "no_std checks on thumbv7em-none-eabihf|bare:thumbv7em"
  "no_std checks on riscv32imac-unknown-none-elf|bare:riscv32imac"
  "no_std checks on riscv32imc-unknown-none-elf|bare:riscv32imc"
  "styx-frame tests on the portable-atomic path|bare:riscv32imc"
  "cargo seed|"
)
# One dry gate; prints "<exit status> <Gate-Lanes trailer>" and keeps its log as $scratch/<n>.log.
dry_gate() {
  local n=$1 plant=$2 res="$scratch/$1.res" rc=0
  : > "$res"
  mkdir -p "$scratch/tmp-$n"
  env -u STYX_GATE_SUPERVISOR -u STYX_GATE_NARROW -u STYX_GATE_TREE -u STYX_GATE_META \
    -u STYX_GATE_RECORDS -u STYX_GATE_NO_REUSE -u STYX_CHECK_DOCTESTS \
    STYX_GATE_DRY=1 STYX_GATE_PLANT="$plant" STYX_GATE_SELECTED="${lanes[*]}" \
    STYX_GATE_RESULTS="$res" STYX_GATE_SLOTS=0 STYX_REQUIRE_VIVID=0 \
    CARGO_TARGET_DIR="$scratch/target-$n" TMPDIR="$scratch/tmp-$n" \
    scripts/gate.sh > "$scratch/$n.log" 2>&1 || rc=$?
  echo "$rc $(python3 scripts/gate.py lane-status --results "$res" --rc "$rc" "${lanes[@]}" | tail -1)"
}
failed=0
bad() { echo "gate-selftest: FAILED: $1"; sed 's/^/    /' "$scratch/$2.log" | tail -40; failed=1; }
# Every case runs side by side: each is a few processes, mostly waiting on the others' Python.
# Each dry gate says when it ended (printed at once, so a self-test stopped at its bound still
# shows which ones had ended and when).
t0=$SECONDS
pids=()
{ dry_gate clean "" > "$scratch/clean.out"; echo "    dry gate unplanted ended at $((SECONDS - t0)) s"; } & pids+=($!)
for i in "${!cases[@]}"; do
  { dry_gate "$i" "${cases[$i]%%|*}" > "$scratch/$i.out"; echo "    dry gate $i ended at $((SECONDS - t0)) s"; } & pids+=($!)
done
for p in "${pids[@]}"; do wait "$p"; done
read -r rc trailer < "$scratch/clean.out"
for v in "${lanes[@]}"; do
  [[ " ${trailer#Gate-Lanes: } " == *" $v=passed "* ]] || { bad "unplanted: $v is not passed (exit $rc: $trailer)" clean; break; }
done
[ "$rc" = 0 ] || bad "unplanted: the gate exited $rc" clean
for i in "${!cases[@]}"; do
  label=${cases[$i]%%|*} lane=${cases[$i]#*|}
  read -r rc trailer < "$scratch/$i.out"
  [ "$rc" != 0 ] || { bad "\"$label\": the gate exited 0 ($trailer)" "$i"; continue; }
  if grep -q 'planted: ran past its failing command' "$scratch/$i.log"; then
    bad "\"$label\": the step went on past its failing command" "$i"; continue
  fi
  if [ -n "$lane" ]; then
    [[ " ${trailer#Gate-Lanes: } " == *" $lane=failed "* ]] || { bad "\"$label\": $lane is not failed ($trailer)" "$i"; continue; }
  else
    [[ " $trailer " != *"=passed "* ]] || { bad "\"$label\": a lane passed ($trailer)" "$i"; continue; }
  fi
  echo "    red as it should be: \"$label\" -> ${lane:-every lane} ($trailer)"
done
[ "$failed" = 0 ] || exit 1
echo "gate-selftest: every planted failure failed its lane and the gate; the unplanted run passed every lane"
