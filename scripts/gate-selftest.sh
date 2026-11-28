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
cp scripts/gate.sh scripts/gate.py scripts/gate-selftest.sh scripts/gate-supervise.py scripts/rustdoc-only.sh scripts/cargo-home.sh "$keyroot/scripts/"
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
# The filter check on the tests step's binaries (test-layout.py STYX_FILTER_REUSE), and the tests
# step's nextest run on them (test-layout.py reuse-checked), refuse stale reuse inputs loudly, never
# list or run them: made for another tree, a binary gone, a binary changed. Inputs that match are
# handed over.
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
  rc=0
  got=$(STYX_GATE_TREE=t1 python3 scripts/test-layout.py reuse-checked "$ri_dir" 2>&1) || rc=$?
  if [ "$rc" = 0 ] || [[ $got != *"$3"* ]]; then
    echo "gate-selftest: FAILED: stale reuse inputs ($3) were not refused for the run: exit $rc: $got" >&2
    rm -rf -- "${ri_dir:?}"; exit 1
  fi
}
ri t0 '{}' "built for tree t0, the tree is t1"
ri t1 "{\"$ri_dir/gone\": [0, 0, 0]}" "is gone since they were made"
ri t1 "{\"$ri_dir/bin\": [0, 1, 1]}" "changed since they were made"
printf '{"tree": "t1", "stamps": {"%s": %s}}' "$ri_dir/bin" \
  "$(python3 -c 'import os,sys; s=os.stat(sys.argv[1]); print([s.st_size, s.st_mtime_ns, s.st_ino])' "$ri_dir/bin")" > "$ri_dir/inputs.json"
got=$(STYX_GATE_TREE=t1 python3 scripts/test-layout.py reuse-checked "$ri_dir" 2>&1) ||
  { echo "gate-selftest: FAILED: matching reuse inputs refused for the run: $got" >&2; rm -rf -- "${ri_dir:?}"; exit 1; }
[ "$got" = "$ri_dir/binaries.json $ri_dir/cargo-metadata.json" ] ||
  { echo "gate-selftest: FAILED: matching reuse inputs not handed over: $got" >&2; rm -rf -- "${ri_dir:?}"; exit 1; }
rm -rf -- "${ri_dir:?}"
echo "    filter check and tests run: stale reuse inputs refused (another tree, a binary gone, a binary changed); matching ones handed over"
# The restore's mode check (gate.py script_mode_drift): a tracked script whose exec bit differs
# from the index is named, after the hide and restore nothing is, and the check writes nothing to
# the index (`git diff` rewrote all of it at every gate's end). A script added to the index during
# the gate is checked against it; one removed from it is no tracked script (the restore gives it
# 0755 as any untracked one). 2026-11-27 plant: the old `git diff --summary` check, index rewritten.
md=$(mktemp -d "${TMPDIR:-/tmp}/md.XXX")
mkdir "$md/scripts"
printf '#!/bin/sh\n' > "$md/scripts/a.sh"; chmod 755 "$md/scripts/a.sh"
printf 'x = 1\n' > "$md/scripts/b.py"; chmod 644 "$md/scripts/b.py"
printf '#!/bin/sh\n' > "$md/scripts/c.sh"; chmod 755 "$md/scripts/c.sh"
git -C "$md" init -q
git -C "$md" add scripts/a.sh scripts/b.py
git -C "$md" -c user.name=t -c user.email=t@t commit -q -m t
md_fail() { echo "gate-selftest: FAILED: the restore's mode check: $1" >&2; rm -rf -- "${md:?}"; exit 1; }
md_drift() { python3 scripts/gate.py script-mode-drift --root "$md" 2>&1; }
idx() { stat -c '%y %i' "$md/.git/index"; }
sleep 1.1  # an index older than the files' change, so a refresh would write it
before=$(idx)
chmod 0 "$md/scripts/a.sh" "$md/scripts/b.py"; chmod 755 "$md/scripts/a.sh"; chmod 644 "$md/scripts/b.py"
out=$(md_drift) || md_fail "the hide and restore left a drift: $out"
[ "$(idx)" = "$before" ] || md_fail "the check wrote the index"
chmod +x "$md/scripts/b.py"
out=$(md_drift) && md_fail "an exec bit added was not named"
[[ $out == "mode change 100644 => 100755 scripts/b.py" ]] || md_fail "an exec bit added: $out"
chmod 644 "$md/scripts/b.py"; chmod 644 "$md/scripts/a.sh"
out=$(md_drift) && md_fail "an exec bit lost was not named"
[[ $out == "mode change 100755 => 100644 scripts/a.sh" ]] || md_fail "an exec bit lost: $out"
chmod 755 "$md/scripts/a.sh"
git -C "$md" add --chmod=-x scripts/c.sh
out=$(md_drift) && md_fail "a script added to the index during the gate was not checked"
[[ $out == "mode change 100644 => 100755 scripts/c.sh" ]] || md_fail "a script added: $out"
git -C "$md" rm -q -f --cached scripts/c.sh
out=$(md_drift) || md_fail "a script removed from the index was checked: $out"
rm -rf -- "${md:?}"
echo "    restore's mode check: drift named (bit added, lost, a script added); nothing after a restore; no index write"
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
rd skip STYX_DOC_ONLY="$rd_dir/list" STYX_DOC_COLLECT="$rd_dir/none" -- --test --crate-name styx_frame x.rs
rd real STYX_DOC_ONLY="$rd_dir/list" STYX_DOC_COLLECT="$rd_dir/none" -- --crate-name styx_format x.rs
rm -rf -- "${rd_dir:?}"
echo "    rustdoc-only.sh: listed and fall-through runs reach rustdoc; an unlisted crate is skipped and named"
# Collected doctests (rustdoc-only.sh STYX_DOC_COLLECT, gate.py doctests-run): a listed crate's
# call is written, not run; the runner runs every call, prints each output whole in crate order,
# strips cargo's jobserver, and fails on a failing call, a listed crate with no call, and a call
# that cannot be replayed (a record gone, its directory gone, its rustdoc gone). Never a pass.
dc=$(mktemp -d "${TMPDIR:-/tmp}/dc.XXX")
dc_fail() { echo "gate-selftest: FAILED: collected doctests: $1" >&2; rm -rf -- "${dc:?}"; exit 1; }
cat > "$dc/rustdoc" <<'RD'
#!/bin/sh
name=$3
echo "$name one jobserver=${CARGO_MAKEFLAGS:-}${MAKEFLAGS:-}${MFLAGS:-}"
sleep 0.3
echo "$name two"
case " $* " in *" FAIL "*) exit 101 ;; esac
RD
chmod +x "$dc/rustdoc"
printf 'crate_a\ncrate_b\n' > "$dc/list"
collect() { # dir, crate, extra args
  mkdir -p "$1"
  (cd "$dc" && env STYX_RUSTDOC="$dc/rustdoc" STYX_DOC_ONLY="$dc/list" STYX_DOC_COLLECT="$1" \
    CARGO_MAKEFLAGS="-j --jobserver-fds=8,9" MAKEFLAGS="-j --jobserver-auth=fifo:$dc/gone" \
    "$OLDPWD/scripts/rustdoc-only.sh" --test --crate-name "$2" ${3:-}) || dc_fail "collecting $2"
}
runner() { python3 scripts/gate.py doctests-run --calls "$1" --expect "$dc/list" --jobs 2 2>&1; }
collect "$dc/ok" crate_b; collect "$dc/ok" crate_a
[ -s "$dc/ok/crate_a/argv" ] || dc_fail "no call written"
out=$(runner "$dc/ok") || dc_fail "two passing calls failed: $out"
blocks=$(grep -E '^(==>|crate_)' <<< "$out" | tr '\n' '|')
[ "$blocks" = "==> doctests of crate_a:|crate_a one jobserver=|crate_a two|==> doctests of crate_b:|crate_b one jobserver=|crate_b two|" ] \
  || dc_fail "outputs not whole, in crate order, or the jobserver kept: $blocks"
collect "$dc/red" crate_a; collect "$dc/red" crate_b FAIL
out=$(runner "$dc/red") && dc_fail "a failing call passed"
[[ $out == *"doctests failed: crate_b"* ]] || dc_fail "a failing call not named: $out"
collect "$dc/short" crate_a
out=$(runner "$dc/short") && dc_fail "a listed crate with no call passed"
[[ $out == *"no call was collected"*crate_b* ]] || dc_fail "a missing call not named: $out"
collect "$dc/rec" crate_a; collect "$dc/rec" crate_b; rm "$dc/rec/crate_b/env"
out=$(runner "$dc/rec") && dc_fail "a call whose record is gone passed"
[[ $out == *"crate_b: its call cannot be replayed"* ]] || dc_fail "an unreadable record not named: $out"
collect "$dc/dir" crate_a; collect "$dc/dir" crate_b; printf '%s' "$dc/gone" > "$dc/dir/crate_b/cwd"
out=$(runner "$dc/dir") && dc_fail "a call whose directory is gone passed"
[[ $out == *"crate_b: its call cannot be replayed"*"is gone"* ]] || dc_fail "a gone directory not named: $out"
collect "$dc/exe" crate_a; collect "$dc/exe" crate_b; chmod -x "$dc/rustdoc"
out=$(runner "$dc/exe") && dc_fail "a call whose rustdoc cannot start passed"
[[ $out == *"cannot be replayed: no rustdoc"* ]] || dc_fail "a rustdoc gone not named: $out"
rm -rf -- "${dc:?}"
echo "    collected doctests: whole outputs in crate order, no jobserver; a failing, missing or unreplayable call fails"
# The gate's own CARGO_HOME (scripts/cargo-home.sh), in a scratch repository with a lock file and
# no dependencies (nothing downloaded): a fetch records the lock's sha256 and the crate count and
# brings every lane's home to its key; the same lock again fetches nothing and keeps the key; a new
# lock moves the key and brings each home up to it at the same path; an unknown lane, and a lane
# with no home, are refused by name.
ch=$(mktemp -d "${TMPDIR:-/tmp}/ch.XXX")
ch_fail() { echo "gate-selftest: FAILED: cargo home: $1" >&2; rm -rf -- "${ch:?}"; exit 1; }
mkdir -p "$ch/repo/scripts" "$ch/repo/src"
cp scripts/cargo-home.sh "$ch/repo/scripts/"
printf '[package]\nname = "chprobe"\nversion = "0.1.0"\nedition = "2021"\n' > "$ch/repo/Cargo.toml"
: > "$ch/repo/src/lib.rs"
printf '# probe\nversion = 4\n\n[[package]]\nname = "chprobe"\nversion = "0.1.0"\n' > "$ch/repo/Cargo.lock"
git -C "$ch/repo" init -q && git -C "$ch/repo" add -A && git -C "$ch/repo" -c user.name=t -c user.email=t@t commit -qm probe
chf() { (cd "$ch/repo" && env -u GITHUB_ACTIONS STYX_CARGO_HOME_DIR="$ch/home" scripts/cargo-home.sh "$@" 2>&1); }
out=$(chf fetch) || ch_fail "the first fetch failed: $out"
[[ $out == *"lock: Cargo.lock $(sha256sum "$ch/repo/Cargo.lock" | cut -c1-64)"* ]] || ch_fail "the lock's sha256 not recorded: $out"
[[ $out == *"crates: 0 registry crate files"* ]] || ch_fail "the crate count not recorded: $out"
key1=$(cat "$ch/home.lanes/main/.styx-fetch-key" 2> /dev/null) || ch_fail "no main home"
for l in main clippy default bare checks; do
  [ "$(cat "$ch/home.lanes/$l/.styx-fetch-key" 2> /dev/null)" = "$key1" ] || ch_fail "the $l home is not at the fetch"
  grep -q '^offline = true' "$ch/home.lanes/$l/config.toml" || ch_fail "the $l home is not offline"
done
out=$(chf fetch) || ch_fail "the second fetch failed: $out"
[[ $out == *"nothing fetched"* ]] || ch_fail "the same lock fetched again: $out"
[ "$(cat "$ch/home.lanes/bare/.styx-fetch-key")" = "$key1" ] || ch_fail "the same lock moved the key"
printf '# probe 2\nversion = 4\n\n[[package]]\nname = "chprobe"\nversion = "0.1.0"\n' > "$ch/repo/Cargo.lock"
git -C "$ch/repo" -c user.name=t -c user.email=t@t commit -qam probe2
out=$(chf fetch) || ch_fail "the fetch of a new lock failed: $out"
key2=$(cat "$ch/home.lanes/default/.styx-fetch-key")
[ -n "$key2" ] && [ "$key2" != "$key1" ] || ch_fail "a new lock kept the key"
[ "$(chf home default)" = "$ch/home.lanes/default" ] || ch_fail "a lane's home moved"
out=$(chf home nope) && ch_fail "an unknown lane was given a home"
[[ $out == *"no lane nope"* ]] || ch_fail "an unknown lane not named: $out"
rm "$ch/home.lanes/clippy/.styx-fetch-key"
out=$(chf home clippy) && ch_fail "a lane with no home was given one"
[[ $out == *"the clippy lane has no home"* ]] || ch_fail "a lane with no home not named: $out"
rm -rf -- "${ch:?}"
echo "    cargo-home.sh: lock and crates recorded; homes at the fetch's key; same lock fetches nothing; a new lock moves the key in place; missing homes named"
# The first-parent log's cache (gate.py first_parent_log), in a scratch repository with a merge:
# every read must equal the whole log read with no cache. Cold reads it all; the same tip reads
# nothing new; new commits and a --no-ff merge are read alone over the cache; a cache whose tip is
# on a merged side line (reachable, not first-parent), a rewound rev, a tip git does not have and
# an unreadable cache each read the whole log again.
fp=$(mktemp -d "${TMPDIR:-/tmp}/fp.XXX")
python3 - "$fp" <<'EOF' || { rm -rf -- "${fp:?}"; echo "gate-selftest: FAILED: first-parent cache" >&2; exit 1; }
import json, os, subprocess, sys
sys.path.insert(0, "scripts")
import gate
d = sys.argv[1]
r, cache = os.path.join(d, "r"), os.path.join(d, "c.json")
def sh(*a): return subprocess.run(["git", "-C", r, "-c", "user.name=t", "-c", "user.email=t@t", *a], check=True, capture_output=True, text=True).stdout.strip()
def commit(*m): sh("commit", "-q", "--allow-empty", *m)
os.makedirs(r)
sh("init", "-q")
commit("-m", "c1"); commit("-m", "c2", "-m", "Gate-Lanes: host:all=passed"); commit("-m", "c3")
def read(rev, want):
    whole, _ = gate.first_parent_log(rev, root=r)
    got, how = gate.first_parent_log(rev, root=r, cache_path=cache)
    if how != want: sys.exit(f"gate-selftest: FAILED: first-parent cache: {rev} read as '{how}', not '{want}'")
    if got != whole: sys.exit(f"gate-selftest: FAILED: first-parent cache: {rev} read over the cache differs from the whole log")
off = "whole log (the cache's tip is not on its first-parent line)"
read("HEAD", "whole log")
read("HEAD", "0 new over the cache")
commit("-m", "c4"); commit("-m", "c5")
read("HEAD", "2 new over the cache")
sh("checkout", "-q", "-b", "side", "HEAD~1"); commit("-m", "s1"); commit("-m", "s2")
side = sh("rev-parse", "HEAD")
sh("checkout", "-q", "-"); sh("merge", "-q", "--no-ff", "-m", "m1", "side")
read("HEAD", "1 new over the cache")
commit("-m", "c6")
c = json.load(open(cache)); c["tip"] = c["commits"][0][0] = side; json.dump(c, open(cache, "w"))
read("HEAD", off)
read("HEAD~2", off)
read("HEAD", "2 new over the cache")
head = sh("rev-parse", "HEAD")
with open(cache) as f: text = f.read()
with open(cache, "w") as f: f.write(text.replace(head, "e" * 40))
read("HEAD", off)
with open(cache, "w") as f: f.write('{"version": 1, "tip": ')
read("HEAD", "whole log")
read("HEAD", "0 new over the cache")
EOF
rm -rf -- "${fp:?}"
echo "    first-parent cache: equal to the whole log cold, unchanged, appended, merged; side-line tip, rewind, unknown tip, unreadable cache read it all"
# The gate's own index (gate.py worktree_tree), in a scratch repository: every tree from the kept
# index must equal a fresh index's. Missing, unchanged, edited; the racy case 20 times over (a
# same-size edit right after the index write, read again within the same second); a same-size edit
# whose mtime equals the entry's and the index's; new, deleted, chmod +x; an untracked file the
# index held that .gitignore now ignores; a tracked file that is ignored; a new HEAD; an index git
# refuses and a stale index.lock are each made again.
gi=$(mktemp -d "${TMPDIR:-/tmp}/gi.XXX")
python3 - "$gi" <<'EOF' || { rm -rf -- "${gi:?}"; echo "gate-selftest: FAILED: gate index" >&2; exit 1; }
import contextlib, io, os, subprocess, sys, time
sys.path.insert(0, "scripts")
import gate
d = sys.argv[1]
r, index = os.path.join(d, "r"), os.path.join(d, "index")
def sh(*a): subprocess.run(["git", "-C", r, "-c", "user.name=t", "-c", "user.email=t@t", *a], check=True, capture_output=True)
def put(path, text):
    with open(os.path.join(r, path), "w") as f: f.write(text)
os.makedirs(os.path.join(r, "d"))
sh("init", "-q")
# ctime is not compared: the unreadable-file check below changes only a ctime (in a later second
# under load, git then reads the file: f2-g1, 2026-11-27), and without it a same-size edit within
# the second is caught by git's racy-entry check alone.
sh("config", "core.trustctime", "false")
put("a", "one\n"); put("d/b", "two\n"); put("t", "keep\n"); put(".gitignore", "t\n")
sh("add", "-A"); sh("add", "-f", "t"); sh("commit", "-qm", "c1")
for f in ("a", "d/b", "t", ".gitignore"):  # an hour old: not racy, so git trusts their stat data
    os.utime(os.path.join(r, f), (time.time() - 3600,) * 2)
last = [None]
def check(what, moved):
    fresh = gate.worktree_tree(root=r, index="-")
    out = io.StringIO()
    with contextlib.redirect_stdout(out):
        kept = gate.worktree_tree(root=r, index=index)
    if out.getvalue(): sys.exit(f"gate-selftest: FAILED: gate index: {what}: the kept index was not used: {out.getvalue().strip()}")
    if kept != fresh: sys.exit(f"gate-selftest: FAILED: gate index: {what}: the kept index gave {kept}, a fresh index {fresh}")
    if moved and kept == last[0]: sys.exit(f"gate-selftest: FAILED: gate index: {what}: the change was not seen")
    if not moved and kept != last[0]: sys.exit(f"gate-selftest: FAILED: gate index: {what}: the tree moved with nothing changed")
    last[0] = kept
check("a missing index", True)
check("nothing changed", False)
# An unchanged file the kept index has seen is not read again: unreadable, it must not matter
# (a fresh index, or a kept one that lost its stat data, reads it and fails).
if os.geteuid() != 0:
    os.chmod(os.path.join(r, "d/b"), 0)
    try:
        if gate.worktree_tree(root=r, index=index) != last[0]: sys.exit("gate-selftest: FAILED: gate index: an unreadable unchanged file moved the tree")
    except SystemExit as e:
        sys.exit(f"gate-selftest: FAILED: gate index: an unchanged file was read again ({e})")
    finally:
        os.chmod(os.path.join(r, "d/b"), 0o644)
put("a", "three\n"); check("an edit", True)
for i in range(20):
    put("a", f"x{i:03d}"); last[0] = gate.worktree_tree(root=r, index=index)  # the index written
    t0 = time.time(); put("a", f"y{i:03d}")
    check(f"racy {i}: a same-size edit within the second of the index write", True)
    if time.time() - t0 >= 1: sys.exit(f"gate-selftest: FAILED: gate index: racy {i}: the read after the edit took 1 s or more")
put("a", "aaaa"); os.utime(os.path.join(r, "a"), (1700000000, 1700000000)); check("a planted mtime (before)", True)
put("a", "bbbb")
for f in (os.path.join(r, "a"), index): os.utime(f, (1700000000, 1700000000))
check("a same-size edit with the entry's and the index's mtime", True)
put("n", "new\n"); check("a new file", True)
os.unlink(os.path.join(r, "d/b")); check("a deleted file", True)
os.chmod(os.path.join(r, "a"), 0o755); check("chmod +x", True)
put(".gitignore", "t\nn\n"); check("an untracked file the index held, ignored now", True)
put("t", "changed\n"); check("a tracked file that is ignored", True)
sh("add", "-A"); sh("commit", "-qm", "c2"); put("a", "four\n"); check("a new HEAD", True)
for what, spoil in (("an index git refuses", lambda: open(index, "wb").write(os.urandom(64))),
                    ("a stale index.lock", lambda: open(index + ".lock", "w").close())):
    spoil()
    out = io.StringIO()
    with contextlib.redirect_stdout(out):
        gate.worktree_tree(root=r, index=index)
    if "is made again from HEAD" not in out.getvalue(): sys.exit(f"gate-selftest: FAILED: gate index: {what} was not made again")
    check(f"after {what}", False)
EOF
rm -rf -- "${gi:?}"
echo "    gate index: equal to a fresh index: missing, unchanged, edits, 20 racy same-second edits, planted mtimes, new/deleted/mode, newly ignored, tracked-ignored, new HEAD; refused index and stale lock made again"
# The record's commit (gate.py head_commit_for), in a scratch repository: with HEAD and its tree
# handed over (STYX_GATE_HEAD, as worktree_tree hashed them) the tree is taken from it, not read (a
# planted tree that is not HEAD's is believed); a HEAD moved since, malformed or no hand-over read
# the tree; worktree_tree records the commit and tree it hashed.
hc=$(mktemp -d "${TMPDIR:-/tmp}/hc.XXX")
python3 - "$hc" <<'EOF' || { rm -rf -- "${hc:?}"; echo "gate-selftest: FAILED: record commit" >&2; exit 1; }
import os, subprocess, sys
sys.path.insert(0, "scripts")
import gate
r = os.path.join(sys.argv[1], "r")
def sh(*a): return subprocess.run(["git", "-C", r, "-c", "user.name=t", "-c", "user.email=t@t", *a], check=True, capture_output=True, text=True).stdout.strip()
os.makedirs(r)
sh("init", "-q"); sh("commit", "-q", "--allow-empty", "-m", "c1")
head, tree = sh("rev-parse", "HEAD"), sh("rev-parse", "HEAD^{tree}")
planted = "0" * 40
def expect(what, env, t, want):
    os.environ.pop("STYX_GATE_HEAD", None)
    if env is not None: os.environ["STYX_GATE_HEAD"] = env
    got = gate.head_commit_for(t, root=r)
    if got != want: sys.exit(f"gate-selftest: FAILED: record commit: {what}: {got}, not {want}")
expect("no hand-over, HEAD's tree", None, tree, head)
expect("no hand-over, another tree", None, planted, "-")
expect("handed over: its tree is not read", f"{head} {planted}", planted, head)
expect("handed over: its tree, not the one asked", f"{head} {planted}", tree, "-")
expect("malformed hand-over", head, tree, head)
sh("commit", "-q", "--allow-empty", "-m", "c2")
expect("HEAD moved since the hand-over", f"{head} {planted}", planted, "-")
expect("HEAD moved since the hand-over, its own tree", f"{head} {planted}", tree, sh("rev-parse", "HEAD"))
gate.worktree_tree(root=r, index="-")
if gate._head_at_hash.get(r) != (sh("rev-parse", "HEAD"), tree):
    sys.exit(f"gate-selftest: FAILED: record commit: worktree_tree recorded {gate._head_at_hash.get(r)}")
EOF
rm -rf -- "${hc:?}"
echo "    record commit: a handed-over HEAD's tree is used unread; moved, malformed or none read it; worktree_tree records what it hashed"
# The gate's fetch (gate.py fetch_upstream) starts no background maintenance: in a clone that git
# judges due (gc.auto=1, loose objects fetched), a plain fetch starts `maintenance run --auto` and
# the gate's does not (GIT_TRACE2_EVENT names each child).
mf=$(mktemp -d "${TMPDIR:-/tmp}/mf.XXX")
python3 - "$mf" <<'EOF' || { rm -rf -- "${mf:?}"; echo "gate-selftest: FAILED: gate fetch" >&2; exit 1; }
import os, subprocess, sys
sys.path.insert(0, "scripts")
import gate
d = sys.argv[1]
up, r = os.path.join(d, "up"), os.path.join(d, "r")
def sh(where, *a): subprocess.run(["git", "-C", where, "-c", "user.name=t", "-c", "user.email=t@t", *a], check=True, capture_output=True)
os.makedirs(up); sh(up, "init", "-q"); sh(up, "commit", "-q", "--allow-empty", "-m", "c1")
subprocess.run(["git", "clone", "-q", up, r], check=True, capture_output=True)
sh(r, "config", "gc.auto", "1"); sh(r, "config", "fetch.unpackLimit", "1000")
def fetched(how):
    for i in range(2):
        with open(os.path.join(up, f"f{how}{i}"), "w") as f: f.write(f"{how} {i}\n")
        sh(up, "add", "-A"); sh(up, "commit", "-q", "-m", f"{how}{i}")
    trace = os.path.join(d, f"{how}.trace")
    os.environ["GIT_TRACE2_EVENT"] = trace
    try:
        if how == "gate": gate.fetch_upstream(root=r)
        else: subprocess.run(["git", "-C", r, "fetch", "-q", "origin"], check=True, capture_output=True)
    finally:
        del os.environ["GIT_TRACE2_EVENT"]
    return '"maintenance","run","--auto"' in open(trace).read()
if not fetched("plain"): sys.exit("gate-selftest: FAILED: gate fetch: the planted clone is not due (a plain fetch started no maintenance)")
if fetched("gate"): sys.exit("gate-selftest: FAILED: gate fetch: the gate's fetch started maintenance run --auto")
EOF
rm -rf -- "${mf:?}"
echo "    gate fetch: a due clone's plain fetch starts maintenance; the gate's does not"
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
  "format|host:checks|leftover"
  "format|host:checks|orphan"
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
    nohome) how=(STYX_GATE_CARGO_HOMES="$scratch/no-homes") ;;
    leftover) how=(STYX_GATE_PLANT_KIND=leftover) ;;
    orphan) how=(STYX_GATE_PLANT_KIND=orphan) ;;
    fixture) how=("STYX_GATE_DRY_STAT_DIR=$scratch/stat") ;;
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
# Two steps of the unplanted run read fixture stat files before and after (gate.sh child_cpu): the
# children's ticks (fields 16 and 17) grow by 175 for "format" and by 172 for the shm check (one
# rounds up, one down: a truncation and an always-up both show); a command name with spaces and
# parentheses; the process's own ticks (14 and 15) differ, so reading the wrong fields shows.
clk=$(getconf CLK_TCK)
fixture() { # label, children's ticks after (from 100 + 23)
  mkdir -p "$scratch/stat/$1"
  echo '4242 (a (b) c) S 1 4242 4242 0 -1 0 0 0 0 0 900 77 100 23 20 0 1 0 1 0 0' > "$scratch/stat/$1/before"
  echo "4242 (a (b) c) S 1 4242 4242 0 -1 0 0 0 0 0 990 88 $2 48 20 0 1 0 1 0 0" > "$scratch/stat/$1/after"
  local t=$(( (($2 + 48 - 123) * 10 + clk / 2) / clk ))
  echo "$((t / 10)).$((t % 10))"
}
fixture_format=$(fixture format 250)
fixture_shm=$(fixture "one source for every shm crate" 247)
{ dry_gate clean "" fixture > "$scratch/clean.out"; echo "    dry gate unplanted ended at $((SECONDS - t0)) s"; } & pids+=($!)
{ dry_gate nohome "" nohome > "$scratch/nohome.out"; echo "    dry gate with no lane homes ended at $((SECONDS - t0)) s"; } & pids+=($!)
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
# step()'s own lines, from builtins (gate.sh child_cpu, us2): every step's CPU-s (from the real
# /proc: a number, never negative) and epilogue line in its format, and the fixture step's exact.
steps_seen=$(grep -c '^==> .*[^:]$' "$scratch/clean.log")  # a lane's heading ends in ":"
[ "$(grep -cE '^    .+: [0-9]+ s, [0-9]+\.[0-9] CPU-s$' "$scratch/clean.log")" = "$steps_seen" ] ||
  bad "unplanted: not every step printed its time and CPU-s line" clean
[ "$(grep -cE '^    epilogue: body end to wait -?[0-9]+\.[0-9]{2} s, timer wait -?[0-9]+\.[0-9]{2}, pgrep -?[0-9]+\.[0-9]{2}, child_cpu -?[0-9]+\.[0-9]{2}, TMPDIR check -?[0-9]+\.[0-9]{2}, rm -?[0-9]+\.[0-9]{2}$' "$scratch/clean.log")" = "$steps_seen" ] ||
  bad "unplanted: not every step printed its epilogue line" clean
for fx in "format|$fixture_format" "one source for every shm crate|$fixture_shm"; do
  IFS='|' read -r fx_label fx_cpu <<< "$fx"
  fx_seen=$(grep -F "    $fx_label: " "$scratch/clean.log" | sed -n 's/^.*: [0-9]* s, \([0-9]*\.[0-9]\) CPU-s$/\1/p')
  [ "$fx_seen" = "$fx_cpu" ] ||
    bad "unplanted: the fixture step \"$fx_label\" reported ${fx_seen:-no} CPU-s, not $fx_cpu (child_cpu)" clean
done
read -r rc trailer < "$scratch/nohome.out"
if [ "$rc" = 0 ]; then bad "no lane homes: the gate exited 0 ($trailer)" nohome
elif ! grep -q 'gate: the checks lane has no CARGO_HOME' "$scratch/nohome.log"; then bad "no lane homes: the lane is not named" nohome
else echo "    red as it should be: no lane homes -> the lanes that need one fail by name ($trailer)"; fi
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
  if [ "$mode" = leftover ]; then
    grep -q "gate: step \"$label\" left files in its TMPDIR:" "$scratch/$i.log" && grep -q '^    planted-leftover$' "$scratch/$i.log" ||
      { bad "\"$label\" leaving a file in its TMPDIR: not named" "$i"; continue; }
  fi
  if [ "$mode" = orphan ]; then
    grep -A1 "gate: step \"$label\" left processes running:" "$scratch/$i.log" | grep -qE '^    ([0-9]+ sleep 30|process group [0-9]+ \(pgrep listed none\))$' ||
      { bad "\"$label\" leaving a process running: not named" "$i"; continue; }
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
