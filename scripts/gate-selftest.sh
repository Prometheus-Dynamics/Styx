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
# The alloc key's check: green on this Python's executable; red on each of its four one-line
# mutants and on the old whole-file key (2026-11-27).
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
scratch=$(mktemp -d "${TMPDIR:-/tmp}/gate-selftest.XXX")
trap 'rm -rf -- "${scratch:?}"' EXIT
# Its own reuse key first (gate.py selftest-key): on copies of the files it tests, an unchanged
# copy keys the same, a gate.sh with one line added keys differently, and a green record of the
# old key is reused while the new key runs. CI and reuse-off are left out here: both always run.
keyroot=$scratch/keyroot
mkdir -p "$keyroot/scripts"
cp scripts/gate.sh scripts/gate.py scripts/gate-selftest.sh scripts/gate-supervise.py scripts/rustdoc-only.sh "$keyroot/scripts/"
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
# The reuse key's binary part (gate.py elf_alloc_digest): its mutants on a real ELF, then the same
# check with five broken digests put in its place, each of which it must catch.
python3 scripts/gate.py alloc-selftest
python3 - << 'PY'
import argparse, hashlib, inspect, sys
sys.path.insert(0, "scripts")
import gate
real = gate.elf_alloc_digest
src = inspect.getsource(real)
# Each mutant is the real function with one line changed: the property it breaks must go red.
edits = {
    "non-alloc sections in the key": ("if not flags & SHF_ALLOC:", "if False:"),
    "section bytes left out": ("h.update(data[off:off + size])", "pass"),
    "a section past the end skipped, not refused": ("if off + size > len(data):\n                    return None",
                                                     "if off + size > len(data):\n                    continue"),
    "a parse error keyed as a constant": ("except (struct.error, ValueError):\n        return None",
                                          "except (struct.error, ValueError):\n        return '0' * 64"),
}
mutants = {name: (src.replace(a, b, 1), a in src) for name, (a, b) in edits.items()}
missed = []
for name, (text, found) in mutants.items():
    if not found:
        missed.append(f"{name} (its line is gone from elf_alloc_digest)")
        continue
    space = dict(vars(gate))
    exec(text, space)
    gate.elf_alloc_digest = space["elf_alloc_digest"]
    try:
        gate.cmd_alloc_selftest(argparse.Namespace(elf=None))
        missed.append(name)
    except SystemExit:
        print(f"    red as it should be: an alloc key with {name}")
gate.elf_alloc_digest = lambda d: hashlib.sha256(d).hexdigest()
try:
    gate.cmd_alloc_selftest(argparse.Namespace(elf=None))
    missed.append("the whole file")
except SystemExit:
    print("    red as it should be: the whole file as the key (the old key)")
gate.elf_alloc_digest = real
if missed:
    sys.exit(f"gate-selftest: FAILED: the alloc key self-test passed a broken digest: {'; '.join(missed)}")
PY
# The doctests' list (gate.py doctests_needed): a moved crate's dependents run theirs (a doctest
# can exercise the changed crate; their own keys are reused when only the dependency changed), a
# crate that does not build with it does not, and nothing moved runs nothing. 2026-11-27 plant:
# styx-frame reused by key, its doctest broken by a styx-format change, red.
python3 - <<'PY'
import sys
sys.path.insert(0, "scripts")
import gate
crates = set(gate.metadata())
need = set(gate.doctests_needed(crates - {"styx-format"}))
wrong = []
if not {"styx-format", "styx-frame"} <= need:
    wrong.append(f"a styx-format change leaves out {sorted({'styx-format', 'styx-frame'} - need)}")
if "styx-math" in need:
    wrong.append("a styx-format change runs styx-math's doctests (it does not build with it)")
if gate.doctests_needed(crates):
    wrong.append("nothing moved, yet doctests run")
if wrong:
    sys.exit("gate-selftest: FAILED: the doctests' list: " + "; ".join(wrong))
print("    doctests' list: a moved crate and its dependents; not the rest; nothing when nothing moved")
PY
# The filter check on the tests step's binaries (test-layout.py STYX_FILTER_REUSE) refuses stale
# reuse inputs loudly, never lists them: made for another tree, a binary gone, a binary changed.
ri_dir="$(mktemp -d "${TMPDIR:-/tmp}/ri.XXX")"
: > "$ri_dir/bin"
ri() { # tree, stamps json, expected message
  printf '{"tree": "%s", "stamps": %s}' "$1" "$2" > "$ri_dir/inputs.json"
  local got rc=0
  got=$(STYX_GATE_TREE=t1 STYX_FILTER_REUSE="$ri_dir" python3 scripts/test-layout.py filters 2>&1) || rc=$?
  if [ "$rc" = 0 ] || [[ $got != *"$3"* ]]; then
    echo "gate-selftest: FAILED: stale reuse inputs ($3) were not refused: exit $rc: $got" >&2
    rm -rf -- "${ri_dir:?}"; exit 1
  fi
}
ri t0 '{}' "built for tree t0, the tree is t1"
ri t1 "{\"$ri_dir/gone\": [0, 0, 0]}" "is gone since they were made"
ri t1 "{\"$ri_dir/bin\": [0, 1, 1]}" "changed since they were made"
rm -rf -- "${ri_dir:?}"
echo "    filter check: stale reuse inputs refused (another tree, a binary gone, a binary changed)"
# scripts/rustdoc-only.sh, the doctests' rustdoc off CI: a listed crate's --test run and every
# fall-through reach the real rustdoc; an unlisted crate's --test run is skipped and named.
rd_dir="$(mktemp -d "${TMPDIR:-/tmp}/rd.XXX")"
printf '#!/bin/sh\necho REAL "$@"\n' > "$rd_dir/rustdoc"
chmod +x "$rd_dir/rustdoc"
printf 'styx_format\n' > "$rd_dir/list"
rd() { # expectation, env..., -- args...
  local want=$1 got; shift
  local envs=()
  while [ "$1" != -- ]; do envs+=("$1"); shift; done
  shift
  got=$(env STYX_RUSTDOC="$rd_dir/rustdoc" "${envs[@]}" scripts/rustdoc-only.sh "$@" 2>&1) || got="exit $?: $got"
  case $want in
    real) [[ $got == REAL* ]] ;;
    skip) [[ $got == "doctests skipped: "* ]] ;;
  esac || { echo "gate-selftest: FAILED: rustdoc-only.sh ${envs[*]} $*: wanted $want, got: $got" >&2; rm -rf -- "${rd_dir:?}"; exit 1; }
}
rd real -- --test --crate-name styx_frame x.rs
rd real STYX_DOC_ONLY="$rd_dir/missing" -- --test --crate-name styx_frame x.rs
rd real STYX_DOC_ONLY="$rd_dir/list" -- --crate-name styx_frame x.rs
rd real STYX_DOC_ONLY="$rd_dir/list" -- --test x.rs
rd real STYX_DOC_ONLY="$rd_dir/list" -- --test --crate-name styx_format x.rs
rd real STYX_DOC_ONLY="$rd_dir/list" -- --test --crate-name=styx_format x.rs
rd skip STYX_DOC_ONLY="$rd_dir/list" -- --test --crate-name styx_frame x.rs
rd skip STYX_DOC_ONLY="$rd_dir/list" -- --test --crate-name=styx_formats x.rs
rm -rf -- "${rd_dir:?}"
echo "    rustdoc-only.sh: listed and fall-through runs reach rustdoc; an unlisted crate is skipped and named"
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
  "tests, all features|host:all"
  "clippy, all features|host:all|beside-slow"
  "clippy, all features|host:all|serial"
)
# One dry gate; prints "<exit status> <Gate-Lanes trailer>" and keeps its log as $scratch/<n>.log.
# A case's third field: "beside-slow" runs host:all's clippy beside its tests (as locally, even
# on CI) with the tests' step slowed to 3 s, so the planted clippy fails while they still run;
# "serial" runs them one after the other (as CI does, even locally).
dry_gate() {
  local n=$1 plant=$2 mode=${3:-} res="$scratch/$1.res" rc=0 how=()
  : > "$res"
  mkdir -p "$scratch/tmp-$n"
  case "$mode" in
    beside-slow) how=(-u GITHUB_ACTIONS STYX_GATE_CLIPPY_BESIDE=1 "STYX_GATE_DRY_SLOW=tests, all features") ;;
    serial) how=(STYX_GATE_CLIPPY_BESIDE=0) ;;
  esac
  env -u STYX_GATE_SUPERVISOR -u STYX_GATE_NARROW -u STYX_GATE_TREE -u STYX_GATE_META \
    -u STYX_GATE_RECORDS -u STYX_GATE_NO_REUSE -u STYX_CHECK_DOCTESTS "${how[@]}" \
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
  IFS='|' read -r label _ mode <<< "${cases[$i]}"
  { dry_gate "$i" "$label" "$mode" > "$scratch/$i.out"; echo "    dry gate $i ended at $((SECONDS - t0)) s"; } & pids+=($!)
done
for p in "${pids[@]}"; do wait "$p"; done
read -r rc trailer < "$scratch/clean.out"
for v in "${lanes[@]}"; do
  [[ " ${trailer#Gate-Lanes: } " == *" $v=passed "* ]] || { bad "unplanted: $v is not passed (exit $rc: $trailer)" clean; break; }
done
[ "$rc" = 0 ] || bad "unplanted: the gate exited $rc" clean
for i in "${!cases[@]}"; do
  IFS='|' read -r label lane mode <<< "${cases[$i]}"
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
  if [ "$mode" = beside-slow ]; then
    # Named, and only after the tests' step (still running when clippy failed) had ended.
    grep -q 'gate: "clippy, all features" failed: host:all fails' "$scratch/$i.log" ||
      { bad "\"$label\" beside the tests: its failure is not named" "$i"; continue; }
    grep -q '^==> clippy, all features (it ran beside the tests):' "$scratch/$i.log" ||
      { bad "\"$label\" beside the tests: it did not run beside them" "$i"; continue; }
  fi
  echo "    red as it should be: \"$label\"${mode:+ ($mode)} -> ${lane:-every lane} ($trailer)"
done
[ "$failed" = 0 ] || exit 1
echo "gate-selftest: every planted failure failed its lane and the gate; the unplanted run passed every lane"
