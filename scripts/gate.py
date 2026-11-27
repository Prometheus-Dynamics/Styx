#!/usr/bin/env python3
"""The landing gate's rules: which lanes a change runs, which test runs are reused, and the record
of what ran. scripts/gate.sh runs the steps; docs/gate.md is the short form.

Entry points: scripts/gate.sh, scripts/check.sh, scripts/lanes-behind.sh,
scripts/release-check.sh, scripts/land.sh. Lanes and their variants:

  host:checks    format, no product names, git dependencies, one sim, test layout, one shm source
  host:all       clippy and tests with all features (nextest, then the doctests), the filter check
  host:default   clippy and tests with default features (the build, and the default-only tests)
  bare:thumbv7em, bare:riscv32imac   the no_std check of every no_std crate for that target
  bare:riscv32imc   the same with no compare-and-swap: each crate with a `critical-section`
                    feature checked with it, and styx-frame's tests on its portable-atomic path

A lane name alone means all its variants; `all` means every variant. When host:default and
host:all both run, the default-only list check runs too.

Selection. Every file changed since `git merge-base origin/next HEAD` (committed, staged,
unstaged, untracked) selects lanes: by scripts/gate-lanes.map (first matching pattern), else a
file of a crate selects host:all; host:default as well when it holds code compiled only without a
feature (scripts/affected.py's rule), or when the crate or one built with it builds with a crate whose
code or dependencies change with a feature (builds_differently_by_feature); bare as well when the crate is no_std or a no_std crate
depends on it. Documentation that no code or script reads selects host:checks; any other file
host:all. host:checks always runs. The default also holds every variant more than DRIFT_COMMITS
commits or DRIFT_DAYS days behind on origin/next, and every blocked variant the change selects
(a red catch-up): those cannot be dropped. `--lanes` adds lanes; `--drop LANE:REASON` removes a
default lane and records it as not run with the reason. Unknown lane names fail.

Reuse. In host:all each crate's tests are one run with a content key (`key_of`): its test
binaries' bytes, the shared libraries they load, every file a test can read (the repository's
files other than other crates' Rust sources, manifests and documentation), the environment the
crate's sources and its workspace dependencies' read, the tools, the gate's own scripts and the
worktree's path, and the machine a test with an outside need meets (machine_facts: the C
compiler, the PipeWire version and socket, the software Vulkan driver, the vivid driver's build,
the dma-heaps, the kernel). A crate whose key equals a green run's is reused, not run: its result
is known. A crate that gets no key always runs, printed with the reason. Keys are recorded only
after a green run.

Record. A run writes, per variant, passed, failed, or not-run (and why) to the records dir
(STYX_GATE_RECORDS, default ~/.local/state/styx-gate), one line per gate in gates.tsv keyed by
the tested tree. `gate.sh --trailer` prints the commit trailers of the current tree's green run
(or of the newest green tree whose code is the same: documentation edits need no new run):

  Gate-Lanes: host:checks=passed host:all=passed ... bare:riscv32imac=not-run(behind)
  Gate-Tree: <tree id>

`gate.py verify-push` (scripts/land.sh) compares code only: documentation no code or script
names is left out of every comparison, and a push that changes only such documentation needs no
gate and no trailers. Otherwise the newest commit since origin/next with trailers carries the gate;
it refuses when there is none, when its trailers differ from the green record of their tree, or
when the pushed tree differs from that tree in code other than files origin/next moved and the
change did not touch.

No gate runs when its result is known: with only such documentation changed since the base, or
nothing, `gate.sh` (and check.sh) says so and exits 0 with no run and no record. `--lanes` and
`--catch-up` still run.

Behind. `lanes-behind.sh [REV]` reads the trailers of REV's first-parent history and the
catch-up records: per variant, the last commit it passed at and how far REV is past it.
`gate.sh --catch-up`, on a clean detached checkout of origin/next while nothing lands, runs the
behind variants; a red one is bisected over its behind range and blocked until a gate passes it.

Release. `release-check.sh COMMIT` refuses unless every variant passed at exactly COMMIT. It
never runs a gate.

Continuous integration (CI set) runs every lane, reuses nothing and records nothing.
"""

import argparse
import concurrent.futures
import datetime
import fcntl
import fnmatch
import hashlib
import json
import os
import re
import stat
import struct
import subprocess
import sys
import tempfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(ROOT, "scripts"))
import affected  # noqa: E402  (the not(feature) and doctest rules, shared with check.sh)

LANES = {
    "host": ["checks", "all", "default"],
    "bare": ["thumbv7em", "riscv32imac", "riscv32imc"],
}
ALL = [f"{lane}:{v}" for lane, vs in LANES.items() for v in vs]
UPSTREAM = os.environ.get("STYX_GATE_UPSTREAM", "origin/next")
DRIFT_COMMITS = 10
DRIFT_DAYS = 3
STATUS_OK = ("passed", "reused")
CI = bool(os.environ.get("CI"))
# The gate's own files: a record or key made by other gate rules is not trusted.
GATE_FILES = ["scripts/gate.sh", "scripts/gate.py", "scripts/gate-lanes.map", "scripts/affected.py",
              "scripts/test-layout.py", "scripts/gate-supervise.py", "scripts/rustdoc-only.sh",
              ".config/nextest.toml", ".config/default-only.txt"]
# How a test says it needs something outside its process (styx_frame::not_run, dma_case).
OUTSIDE_NEED = re.compile(r"\bnot_run\b|\bdma_case\b")
# Variables a test may read: the ones its crate's sources name, and these prefixes. Not the gate's
# bookkeeping (where records and logs go, a step's scratch folder, the build's job count).
ENV_PREFIXES = ("STYX_", "VK_", "CARGO_", "RUST", "NEXTEST_", "LD_", "LIBCAMERA_", "PIPEWIRE_")
# The gate's own variables, by prefix: none may reach a key (Eidos keyed its log folder and lane
# list, and no run ever matched). reuse-plan fails when a crate's sources read one.
# Directories a test reads that are keyed by their value: what the tests use in them is in the
# machine facts (the PipeWire socket) or is a lock file whose content means nothing.
DIRS_KEYED_BY_VALUE = ("XDG_RUNTIME_DIR",)
GATE_ENV_PREFIXES = ("STYX_GATE_", "STYX_CHECK_", "STYX_NOT_RUN_LOG", "STYX_TEST_FILTER")
ENV_SKIP = {"CARGO_BUILD_JOBS", "CARGO_MAKEFLAGS", "CARGO_TARGET_DIR", "TMPDIR"}


def gate_var(name):
    return name.startswith(GATE_ENV_PREFIXES) or name in ENV_SKIP
ENV_READ = re.compile(rb"var(?:_os)?\(\s*\"([A-Za-z0-9_]+)\"")


def git(*args, check=True, env=None, cwd=ROOT):
    r = subprocess.run(["git", "-C", cwd, *args], capture_output=True, text=True, env=env)
    if check and r.returncode:
        sys.exit(f"gate: git {' '.join(args)}: {r.stderr.strip()}")
    return r.stdout


def records_dir():
    d = os.environ.get("STYX_GATE_RECORDS") or os.path.expanduser("~/.local/state/styx-gate")
    os.makedirs(os.path.join(d, "blocks"), exist_ok=True)
    return d


def expand(names):
    out = []
    for n in names:
        if n == "all":
            out += ALL
        elif n in LANES:
            out += [f"{n}:{v}" for v in LANES[n]]
        elif n in ALL:
            out.append(n)
        else:
            sys.exit(f"gate: unknown lane {n!r} (lanes: all, {', '.join(LANES)}, {', '.join(ALL)})")
    return out


def ordered(variants):
    return [v for v in ALL if v in set(variants)]


def digest(path, cache):
    st = os.stat(path)
    tag = (path, st.st_size, st.st_mtime_ns, st.st_ino)
    if tag not in cache:
        h = hashlib.sha256()
        with open(path, "rb") as f:
            for chunk in iter(lambda: f.read(1 << 20), b""):
                h.update(chunk)
        cache[tag] = h.hexdigest()
    return cache[tag]


SHF_ALLOC, SHT_NOBITS = 0x2, 8


def elf_alloc_digest(data):
    """The sha256 of what a program loads: every SHF_ALLOC section's name, address, flags and bytes
    (a NOBITS section by its size), without .note.gnu.build-id. None when DATA is not a well-formed
    64-bit little-endian ELF: the caller then keys the whole file, so a file this cannot read never
    reuses a run it should not. Debug info, symbol tables and comments are not loaded: they moved
    on 68 of 131 test binaries rebuilt from an identical source after other edits (incremental
    codegen), which keyed on the whole file reran 39 crates that had not changed (2026-11-27)."""
    try:
        if len(data) < 64 or data[:6] != b"\x7fELF\x02\x01":
            return None
        shoff, = struct.unpack_from("<Q", data, 0x28)
        shentsize, shnum, shstrndx = struct.unpack_from("<HHH", data, 0x3A)
        if shentsize != 64 or shnum == 0 or shstrndx >= shnum or shoff + shnum * 64 > len(data):
            return None
        secs = [struct.unpack_from("<IIQQQQ", data, shoff + i * 64) for i in range(shnum)]
        stroff, strsize = secs[shstrndx][4], secs[shstrndx][5]
        if stroff + strsize > len(data):
            return None
        h, alloc = hashlib.sha256(), 0
        for name, typ, flags, addr, off, size in secs:
            if not flags & SHF_ALLOC:
                continue
            if name >= strsize:
                return None
            end = data.index(b"\0", stroff + name, stroff + strsize)
            n = data[stroff + name:end]
            if n == b".note.gnu.build-id":
                continue
            h.update(n + b"\0" + struct.pack("<QQ", addr, flags))
            if typ == SHT_NOBITS:
                h.update(struct.pack("<Q", size))
            else:
                if off + size > len(data):
                    return None
                h.update(data[off:off + size])
            alloc += 1
        return h.hexdigest() if alloc else None
    except (struct.error, ValueError):
        return None


def binary_digest(path, cache):
    """A test binary's part of a key: "alloc:" and its loaded sections' digest, or "file:" and the
    whole file's when it is not an ELF this reads (the two never match each other)."""
    st = os.stat(path)
    tag = (path, st.st_size, st.st_mtime_ns, st.st_ino, "alloc")
    if tag not in cache:
        with open(path, "rb") as f:
            data = f.read()
        a = elf_alloc_digest(data)
        cache[tag] = "alloc:" + a if a else "file:" + hashlib.sha256(data).hexdigest()
    return cache[tag]


def scripts_hash():
    h = hashlib.sha256()
    for rel in GATE_FILES:
        full = os.path.join(ROOT, rel)
        h.update(rel.encode() + b"\0")
        h.update(digest(full, {}).encode() if os.path.isfile(full) else b"missing")
    return h.hexdigest()[:16]


# The gate's self-test (scripts/gate-selftest.sh) tests these and nothing else: it runs again when
# one changes, and its green run is reused otherwise, like a lane's crate.
SELFTEST_FILES = ["scripts/gate.sh", "scripts/gate.py", "scripts/gate-selftest.sh",
                  "scripts/gate-supervise.py", "scripts/rustdoc-only.sh"]


def selftest_key(root=ROOT):
    h = hashlib.sha256()
    for rel in SELFTEST_FILES:
        full = os.path.join(root, rel)
        h.update(rel.encode() + b"\0")
        if os.path.isfile(full):
            with open(full, "rb") as f:
                h.update(hashlib.sha256(f.read()).digest())
        else:
            h.update(b"missing")
    # The shells it runs under.
    for tool in (["bash", "--version"], [sys.executable, "--version"]):
        out = subprocess.run(tool, capture_output=True, text=True).stdout.splitlines()
        h.update((out[0] if out else tool[0]).encode() + b"\0")
    return h.hexdigest()[:16]


def cmd_selftest_key(a):
    print(selftest_key(a.root or ROOT))


def cmd_selftest_reuse(a):
    """Exit 0 (and say so) when the self-test was green with this key; 1 when it must run."""
    if CI:
        sys.exit("    gate self-test: runs (CI always runs it)")
    if os.environ.get("STYX_GATE_NO_REUSE"):
        sys.exit("    gate self-test: runs (reuse off: the change measures or selects)")
    path = os.path.join(records_dir(), "selftest.tsv")
    if os.path.exists(path):
        with open(path) as f:
            for line in f:
                c = line.rstrip("\n").split("\t")
                if len(c) >= 2 and c[0] == a.key:
                    print(f"    gate self-test: REUSED: green at {c[1]} with key {a.key} (gate.sh, gate.py, "
                          "gate-selftest.sh, gate-supervise.py, bash and python unchanged)")
                    return
    sys.exit(f"    gate self-test: runs (no green run with key {a.key})")


def cmd_selftest_record(a):
    if CI:
        return
    with open(os.path.join(records_dir(), "selftest.tsv"), "a") as f:
        fcntl.flock(f, fcntl.LOCK_EX)
        f.write(f"{a.key}\t{time.strftime('%Y-%m-%dT%H:%M:%S')}\n")


# --- the workspace ---------------------------------------------------------------

_meta = None
_meta_text = None
# Workspace crates whose build differs between default and all features: a feature of their own or
# an optional dependency (filled by metadata()).
_varying = set()


def metadata():
    """Workspace crates: name -> (directory relative to ROOT, no_std, workspace deps incl. dev,
    workspace deps a library build links: normal and build)."""
    global _meta, _meta_text, _varying
    if _meta is None:
        # gate.py run hands its copy to the steps (STYX_GATE_META): the workspace cannot change
        # during a gate without the record being refused.
        cached = os.environ.get("STYX_GATE_META")
        if cached and os.path.isfile(cached):
            with open(cached) as f:
                _meta_text = f.read()
        else:
            _meta_text = subprocess.run(
                ["cargo", "metadata", "--locked", "--format-version", "1", "--all-features", "--color", "never"],
                cwd=ROOT, check=True, capture_output=True, text=True).stdout
        m = json.loads(_meta_text)
        members = {p["id"]: p for p in m["packages"] if p["id"] in m["workspace_members"]}
        deps, lib_deps = {}, {}
        for node in m["resolve"]["nodes"]:
            if node["id"] in members:
                name = members[node["id"]]["name"]
                ws = [d for d in node["deps"] if d["pkg"] in members]
                deps[name] = {members[d["pkg"]]["name"] for d in ws}
                lib_deps[name] = {members[d["pkg"]]["name"] for d in ws
                                  if any(k["kind"] in (None, "build") for k in d["dep_kinds"])}
        _meta = {}
        _varying = {p["name"] for p in members.values()
                    if any(k != "default" for k in p.get("features") or {})
                    or any(d.get("optional") for d in p.get("dependencies") or [])}
        for p in members.values():
            styx = (p.get("metadata") or {}).get("styx") or {}
            _meta[p["name"]] = (os.path.relpath(os.path.dirname(p["manifest_path"]), ROOT),
                                styx.get("no_std") is True, deps.get(p["name"], set()),
                                lib_deps.get(p["name"], set()))
    return _meta


def closure(name, lib_only=False):
    """The workspace crates NAME's tests build with (lib_only: its library)."""
    meta, seen, todo = metadata(), set(), [name]
    while todo:
        n = todo.pop()
        if n not in seen:
            seen.add(n)
            todo += meta[n][3 if lib_only else 2]
    return seen


def builds_differently_by_feature(name):
    """A crate whose default-features build the change can break while the all-features build
    passes: NAME or a crate built with it (its dependents) reaches, in what it builds with, a crate
    whose code or dependencies change with a feature (a stub under not(feature), an optional
    backend). Returns that crate's name, or None."""
    meta = metadata()
    for user in sorted(meta):
        if user == name or name in closure(user):
            hit = sorted(closure(user) & _varying)
            if hit:
                return hit[0]
    return None


def owner(path):
    best = None
    for name, (d, *_) in metadata().items():
        if path.startswith(d + "/") and (best is None or len(d) > len(metadata()[best][0])):
            best = name
    return best


# --- selection ---------------------------------------------------------------------

def read_map():
    """(pattern, lanes, no_reuse) per line of the map. A line with no lanes only marks no-reuse."""
    rules = []
    with open(os.path.join(ROOT, "scripts", "gate-lanes.map")) as f:
        for line in f:
            line = line.split("#", 1)[0].strip()
            if line:
                pattern, *lanes = line.split()
                no_reuse = "no-reuse" in lanes
                rules.append((pattern, [x for x in lanes if x != "no-reuse"], no_reuse))
    return rules


def no_reuse_files(base):
    """The changed files that measure or select (the map's no-reuse lines): any of them in the
    change turns reuse off, so every crate's tests run."""
    rules = [p for p, _, n in read_map() if n]
    return [f for f in changed_files(base) if any(fnmatch.fnmatchcase(f, p) for p in rules)]


def changed_files(base):
    files = set(git("diff", "--name-only", base, "--").split())
    files |= set(git("ls-files", "--others", "--exclude-standard").split())
    return sorted(files)


def is_text(path, code):
    """Documentation that no code or script reads: its name is in no string literal of code (an
    include_str!("../README.md"), a path a test or script opens). A mention in a comment or a
    docstring is not a read."""
    if not (path.startswith("docs/") or path.endswith(".md") or path.startswith("LICENSE")):
        return False
    read = re.compile(r"[\"'/]" + re.escape(os.path.basename(path)) + r"[\"']")
    for p in code:
        full = os.path.join(ROOT, p)
        if p != path and os.path.isfile(full):
            with open(full, errors="replace") as f:
                if read.search(f.read()):
                    return False
    return True


def code_files():
    """The files whose text decides whether a document is named by code (is_text)."""
    return git("ls-files", "crates", "scripts", ".config", ".cargo", ".github").split()


def code_changes(paths, code=None):
    """The paths a gate tests: all but documentation no code or script reads."""
    code = code_files() if code is None else code
    return sorted(p for p in paths if not is_text(p, code))


def select_default(base):
    """{variant: reasons} the change selects, and the crates it changes."""
    rules = read_map()
    meta = metadata()
    no_std_reach = set()
    for name, (_, no_std, _, _) in meta.items():
        if no_std:
            no_std_reach |= closure(name, lib_only=True)
    code = code_files()
    picks, crates = {}, set()

    def add(lanes, why):
        for v in expand(lanes):
            picks.setdefault(v, set()).add(why)

    for path in changed_files(base):
        crate = owner(path)
        if crate:
            crates.add(crate)
        for pattern, lanes, _ in rules:
            if fnmatch.fnmatchcase(path, pattern) and lanes:
                add(lanes, path)
                break
        else:
            if crate:
                add(["host:all"], path)
                full = os.path.join(ROOT, path)
                if os.path.exists(full) and affected.behind_feature(full, os.path.join(ROOT, meta[crate][0])):
                    add(["host:default"], path + " (not(feature) code)")
                else:
                    varying = builds_differently_by_feature(crate)
                    if varying:
                        add(["host:default"], f"{path} (built with {varying}, which changes with a feature)")
                if crate in no_std_reach:
                    add(["bare"], path + " (no_std reach)")
            elif is_text(path, code):
                add(["host:checks"], path)
            else:
                add(["host:all"], path)
    picks.setdefault("host:checks", set()).add("always")
    return picks, sorted(crates)


# --- history: trailers, catch-ups, blocks ---------------------------------------------

def parse_trailer(msg):
    lanes = {}
    for line in msg.splitlines():
        if line.startswith("Gate-Lanes:"):
            for tok in line.split(":", 1)[1].split():
                k, _, v = tok.partition("=")
                lanes[k] = v
    return lanes


def catchups():
    path = os.path.join(records_dir(), "catchup.tsv")
    out = []
    if os.path.exists(path):
        with open(path) as f:
            for line in f:
                c = line.rstrip("\n").split("\t")
                if len(c) >= 4:
                    out.append(c)  # commit, variant, status, date
    return out


def behind(rev=UPSTREAM):
    """Per variant: (last passing commit or None, commits behind, days since the oldest uncovered)."""
    log = git("log", "--first-parent", "--format=%H%x00%ct%x00%B%x1e", rev)
    commits = []
    for rec in log.split("\x1e"):
        rec = rec.strip("\n")
        if rec:
            h, ct, body = rec.split("\x00", 2)
            commits.append((h, int(ct), parse_trailer(body)))
    caught = {}
    for commit, variant, status, _ in catchups():
        if status in STATUS_OK:
            caught.setdefault(commit, set()).add(variant)
    now = time.time()
    out = {}
    for v in ALL:
        last, n = None, 0
        for h, _, lanes in commits:
            if lanes.get(v, "").split("(")[0] in STATUS_OK or v in caught.get(h, ()):
                last = h
                break
            n += 1
        oldest = commits[n - 1][1] if n else now
        out[v] = (last, n, (now - oldest) / 86400 if n else 0.0)
    return out


def blocks():
    d = os.path.join(records_dir(), "blocks")
    out = {}
    for name in os.listdir(d):
        with open(os.path.join(d, name)) as f:
            out[name.replace("_", ":", 1)] = f.read().strip()
    return out


def clear_block(variant):
    path = os.path.join(records_dir(), "blocks", variant.replace(":", "_", 1))
    if os.path.exists(path):
        os.unlink(path)
        print(f"gate: block on {variant} cleared (passed)")


def set_block(variant, text):
    with open(os.path.join(records_dir(), "blocks", variant.replace(":", "_", 1)), "w") as f:
        f.write(text + "\n")


# --- running -------------------------------------------------------------------------

def worktree_tree():
    """The tree the working tree would commit as (edits and untracked files, .gitignore applied)."""
    with tempfile.TemporaryDirectory() as tmp:
        env = dict(os.environ, GIT_INDEX_FILE=os.path.join(tmp, "index"))
        git("read-tree", "HEAD", env=env)
        git("add", "-A", env=env)
        return git("write-tree", env=env).strip()


def run_gate(selected, tree, narrow=None, doctests=True, no_reuse=()):
    """Run scripts/gate.sh's steps for the selected variants; {variant: status}."""
    results = tempfile.NamedTemporaryFile(prefix="styx-gate-lanes.", delete=False)
    results.close()
    metadata()
    meta_file = results.name + ".meta"
    with open(meta_file, "w") as f:
        f.write(_meta_text)
    env = dict(os.environ, STYX_GATE_SELECTED=" ".join(selected), STYX_GATE_RESULTS=results.name,
               STYX_GATE_TREE=tree, STYX_GATE_META=meta_file)
    if not CI:
        env["STYX_GATE_RECORDS"] = records_dir()
    if narrow is not None:
        env["STYX_GATE_NARROW"] = " ".join(narrow)
    if not doctests:
        env["STYX_CHECK_DOCTESTS"] = "0"
    if no_reuse:
        env["STYX_GATE_NO_REUSE"] = " ".join(no_reuse)
    sys.stdout.flush()
    if os.environ.get("STYX_GATE_DRY"):
        sys.exit("gate: STYX_GATE_DRY is scripts/gate-selftest.sh's: a dry gate runs no step and records nothing")
    rc = subprocess.run([os.path.join(ROOT, "scripts", "gate.sh")], cwd=ROOT, env=env).returncode
    status = lane_status(results.name, selected, rc)
    os.unlink(results.name)
    os.unlink(meta_file)
    return status, rc


def lane_status(results_path, selected, rc):
    """{variant: status} from gate.sh's lane marks and its exit status. A lane passed when it
    marked itself passed (after its last step); a red gate whose lanes all say passed fails them all."""
    started, passed, counted, notes = set(), set(), set(), {}
    with open(results_path) as f:
        for line in f:
            v, _, what = line.strip().partition(" ")
            if what.startswith("reuse "):
                print(f"gate: {v} {what}")
                counted.add(v)
            elif what.startswith("note "):
                # A step that had nothing to run: <variant>/<step>=not-run(<reason>) on the gate line.
                _, step, state = what.split(" ", 2)
                notes[f"{v}/{step}"] = state
            else:
                (passed if what == "passed" else started).add(v)
    for v in selected:
        if v not in counted and v in started | passed:
            print(f"gate: {v} reuse: none (no keyed runs: its steps always run)")
    status = {}
    for v in selected:
        if v in passed:
            status[v] = "passed"
        elif v in started or rc == 0:
            status[v] = "failed"
        else:
            status[v] = "not-run(gate_stopped)"
    if rc != 0 and all(s == "passed" for s in status.values()):
        # A step outside every lane (the lock check, the seed, the supervisor) failed.
        for v in selected:
            status[v] = "failed"
    for key, state in notes.items():
        if key.split("/")[0] in selected:
            status[key] = state
    return status


def cmd_lane_status(a):
    """The Gate-Lanes trailer that a gate.sh run's lane marks and exit status give (gate-selftest.sh)."""
    print(trailer_of(lane_status(a.results, a.variants, a.rc)))


def trailer_of(status):
    """The variants' statuses, then each step that had nothing to run (variant/step=not-run(why))."""
    steps = sorted(k for k in status if "/" in k)
    return "Gate-Lanes: " + " ".join([f"{v}={status.get(v, 'not-run')}" for v in ALL]
                                     + [f"{k}={status[k]}" for k in steps])


def write_record(tree, base, trailer, green):
    head = git("rev-parse", "HEAD").strip()
    with open(os.path.join(records_dir(), "gates.tsv"), "a") as f:
        fcntl.flock(f, fcntl.LOCK_EX)
        f.write("\t".join([tree, base, head, datetime.datetime.now().isoformat(timespec="seconds"),
                           "1" if green else "0", trailer, scripts_hash()]) + "\n")


def gate_records():
    path = os.path.join(records_dir(), "gates.tsv")
    out = []
    if os.path.exists(path):
        with open(path) as f:
            for line in f:
                c = line.rstrip("\n").split("\t")
                if len(c) == 7:
                    out.append(c)
    return out


def green_record(tree):
    for c in reversed(gate_records()):
        if c[0] == tree and c[4] == "1":
            return c
    return None


def narrowed_crates(base, named):
    """check.sh's narrowing: the named crates, or the changed ones; None when the change reaches
    the whole workspace (a file outside every crate, or a crate's manifest)."""
    if named:
        unknown = sorted(set(named) - set(metadata()))
        if unknown:
            sys.exit(f"gate: no such crate: {' '.join(unknown)}")
        return sorted(named)
    crates = set()
    changed = changed_files(base)
    readable = set(test_readable(tracked_files()))
    for path in changed:
        crate = owner(path)
        if crate is None:
            if path.endswith(".md") or path.startswith("docs/"):
                continue
            if path.startswith("scripts/") and path not in readable:
                continue  # a tool no test reads: it changes no test's result (test_readable)
            return None
        if path == os.path.join(metadata()[crate][0], "Cargo.toml"):
            return None
        crates.add(crate)
    return sorted(crates)


def mark(what):
    """The gate's time outside its steps (diagnostic): seconds since gate.py run started."""
    t0 = os.environ.get("STYX_GATE_T0")
    if t0:
        print(f"gate: at {time.time() - float(t0):.1f} s: {what}", flush=True)


def cmd_run(a):
    os.environ["STYX_GATE_T0"] = repr(time.time())
    if CI:
        selected = list(ALL)
        print("gate: CI runs every lane, reuses nothing and records nothing")
        status, rc = run_gate(selected, "-")
        print(f"GATE {'green' if rc == 0 else 'RED'}: " + ", ".join(f"{v} {status[v]}" for v in ALL))
        return rc
    if UPSTREAM.startswith("origin/"):
        git("fetch", "-q", "origin", check=False)
    mark("fetched")
    base = git("merge-base", UPSTREAM, "HEAD").strip()
    # A gate whose result is already known does not run: nothing it tests changed. Only
    # documentation that no code or script reads (or nothing) differs from the base, so the base's
    # result stands. land.sh pushes such a change with no trailers. --lanes still runs what it
    # names (a run on a changed machine has no known result), and so does --catch-up.
    if not a.catch_up and not a.lanes:
        changed = changed_files(base)
        if not code_changes(changed):
            shown = " ".join(changed[:5]) + (" ..." if len(changed) > 5 else "")
            what = f"documentation only ({shown})" if changed else "nothing"
            print(f"gate: {what} changed since {base[:12]}: nothing a gate tests changed, so no gate"
                  " runs; scripts/land.sh pushes it without trailers")
            return 0
    picks, crates = select_default(base)
    behind_now = behind(UPSTREAM)
    narrow = narrowed_crates(base, a.crates) if a.narrow else None
    if a.narrow and narrow is None:
        print("gate: a change outside the crates or to a crate's manifest: check.sh runs the gate")
    forced = {}
    if narrow is None:
        for v, (last, n, days) in behind_now.items():
            if n > DRIFT_COMMITS or days > DRIFT_DAYS:
                forced[v] = f"drift: {n} commits, {days:.1f} days behind"
        for v, text in blocks().items():
            if v in picks:
                forced[v] = f"blocked: {text}"
    if a.catch_up:
        if git("status", "--porcelain").strip():
            sys.exit("gate: --catch-up needs a clean worktree")
        if git("rev-parse", "HEAD").strip() != git("rev-parse", UPSTREAM).strip():
            sys.exit(f"gate: --catch-up runs on {UPSTREAM}; check it out (detached) first")
        default = {v: {f"behind {n}"} for v, (last, n, days) in behind_now.items() if n > 0}
        forced = {}
    else:
        default = {v: set(w) for v, w in picks.items()}
    for v, why in forced.items():
        default.setdefault(v, set()).add(why)
    dropped = {}
    for d in a.drop:
        lane, _, reason = d.partition(":")
        names = [lane]
        head, _, rest = reason.partition(":")
        if f"{lane}:{head}" in ALL:  # LANE:VARIANT:REASON
            names, reason = [f"{lane}:{head}"], rest
        if not reason:
            sys.exit(f"gate: --drop {d}: give a reason (LANE:REASON)")
        for v in expand(names):
            if v in forced:
                sys.exit(f"gate: --drop {v}: cannot drop ({forced[v]})")
            if v == "host:checks":
                sys.exit("gate: --drop host:checks: it always runs")
            if v in default:
                dropped[v] = reason
    added = expand(a.lanes.replace(",", " ").split()) if a.lanes else []
    selected = ordered([v for v in default if v not in dropped] + added)
    if narrow == []:
        print("gate: no crate changed: the checks only")
        selected = ["host:checks"]
    print("gate: base", base[:12])
    for v in ALL:
        if v in selected:
            print(f"  {v:18} run      {reasons(default.get(v, {'--lanes'}))}")
        elif v in dropped:
            print(f"  {v:18} not run  (dropped: {dropped[v]})")
        else:
            print(f"  {v:18} not run  (not selected; {behind_now[v][1]} commits behind)")
    if narrow:
        print(f"  tests narrowed to the crates that reach {' '.join(narrow)}: NOT THE GATE")
    no_reuse = no_reuse_files(base)
    if no_reuse:
        print(f"  reuse off: the change measures or selects ({' '.join(no_reuse[:5])}"
              f"{' ...' if len(no_reuse) > 5 else ''})")
    if a.dry_run:
        return 0
    doctests = True
    if narrow is not None:
        doctests = any(affected.holds_doctest(os.path.join(ROOT, p)) for p in changed_files(base))
    tree = worktree_tree()
    mark("lanes chosen, tree hashed")
    status, rc = run_gate(selected, tree, narrow, doctests, no_reuse)
    mark("gate.sh ended")
    for v, reason in dropped.items():
        status[v] = "not-run(dropped:" + re.sub(r"[^A-Za-z0-9_.,/-]+", "_", reason) + ")"
    for v in ALL:
        status.setdefault(v, "not-run(behind)")
    green = rc == 0 and all(status[v] in STATUS_OK for v in selected)
    partial = len(selected) < len(ALL)
    summary = ", ".join([f"{v} {status[v]}" for v in ALL]
                        + [f"{k} {status[k]}" for k in sorted(k for k in status if "/" in k)])
    if narrow is not None:
        print(f"CHECK {'green' if green else 'RED'} (not the gate): {summary}")
        return 0 if green else 1
    if worktree_tree() != tree:
        print(f"GATE {'green' if green else 'RED'}: {summary}")
        print("gate: the files changed while the gate ran: nothing recorded, run it again")
        return 1
    if a.catch_up:
        head = git("rev-parse", "HEAD").strip()
        with open(os.path.join(records_dir(), "catchup.tsv"), "a") as f:
            for v in selected:
                f.write(f"{head}\t{v}\t{status[v]}\t{datetime.datetime.now().isoformat(timespec='seconds')}\n")
        for v in selected:
            if status[v] in STATUS_OK:
                clear_block(v)
            else:
                bisect(v, behind_now[v][0], head)
    else:
        for v in selected:
            if status[v] in STATUS_OK:
                clear_block(v)
    trailer = trailer_of(status)
    write_record(tree, base, trailer, green)
    mark("recorded")
    print(f"GATE {'green' if green else 'RED'}{' (partial)' if partial and green else ''}: {summary}")
    if green and not a.catch_up:
        print("Commit with these trailers (scripts/gate.sh --trailer prints them again):")
        print(trailer)
        print(f"Gate-Tree: {tree}")
    print_behind(UPSTREAM)
    return 0 if green else 1


def reasons(why):
    """The forced reasons first, then the files (the first three, and how many more)."""
    first = sorted(w for w in why if w.startswith(("drift", "blocked", "always", "behind", "--lanes")))
    files = sorted(set(why) - set(first))
    text = first + files[:3] + ([f"{len(files) - 3} more files"] if len(files) > 3 else [])
    return "; ".join(text)


def bisect(variant, good, bad):
    """The first first-parent commit in (good, bad] where VARIANT fails; the variant is blocked."""
    if not good:
        set_block(variant, f"red at {bad[:12]} (no earlier pass to bisect from)")
        return
    revs = git("rev-list", "--first-parent", "--reverse", f"{good}..{bad}").split()
    lo, hi = 0, len(revs) - 1  # revs[hi] is red
    start = git("rev-parse", "HEAD").strip()
    try:
        while lo < hi:
            mid = (lo + hi) // 2
            git("checkout", "-q", "--detach", revs[mid])
            st = run_gate([variant], worktree_tree())[0].get(variant)
            print(f"gate: bisect {variant}: {revs[mid][:12]} {st}", flush=True)
            if st in STATUS_OK:
                lo = mid + 1
            else:
                hi = mid
    finally:
        git("checkout", "-q", "--detach", start)
    set_block(variant, f"first red at {revs[hi][:12]} (bisected over {len(revs)} commits)")
    print(f"gate: {variant} blocked: first red at {revs[hi][:12]}")


def print_behind(rev):
    b, bl = behind(rev), blocks()
    print(f"lanes behind on {rev}:")
    for v in ALL:
        last, n, days = b[v]
        state = "current" if n == 0 else f"{n} commits, {days:.1f} days behind"
        print(f"  {v:18} {state:32} last pass {last[:12] if last else 'none'}"
              f"{'  BLOCKED ' + bl[v] if v in bl else ''}")


def green_record_same_code(tree, newest=50):
    """The newest green record whose tree differs from TREE in documentation only (is_text): the
    code that gate tested is this tree's code, so its result stands (no ceremony run)."""
    code = None
    for c in [c for c in reversed(gate_records()) if c[4] == "1"][:newest]:
        r = subprocess.run(["git", "-C", ROOT, "diff", "--name-only", c[0], tree],
                           capture_output=True, text=True)
        if r.returncode:
            continue  # the recorded tree object is gone
        differs = r.stdout.split()
        code = code_files() if code is None else code
        if not code_changes(differs, code):
            return c
    return None


def cmd_trailer(a):
    tree = worktree_tree()
    rec = green_record(tree) or green_record_same_code(tree)
    if not rec:
        sys.exit(f"gate: no green gate record for this tree ({tree}), nor for one with the same code: "
                 "run scripts/gate.sh")
    print(rec[5])
    # The tested tree: verify-push checks the pushed tree differs from it in documentation only.
    print(f"Gate-Tree: {rec[0]}")


def has_trailers(msg):
    lines = msg.splitlines()
    return any(x.startswith("Gate-Lanes:") for x in lines) and any(x.startswith("Gate-Tree:") for x in lines)


def cmd_verify_push(a):
    head = git("rev-parse", a.rev).strip()
    code = code_files()
    # Only code is compared: documentation that no code or script reads never costs a gate, and
    # never makes a tested tree differ from the pushed one.
    if not code_changes(git("diff", "--name-only", a.onto, head).split(), code):
        print(f"gate: documentation only since {a.onto}: no gate needed")
        return
    # The newest commit since the base with trailers carries the gate; commits after it may change
    # documentation only (checked below as a difference from the tested tree).
    carrier = None
    for rev in git("rev-list", "--first-parent", f"{a.onto}..{head}").split():
        if has_trailers(git("log", "-1", "--format=%B", rev)):
            carrier = rev
            break
    if carrier is None:
        sys.exit("REFUSED: no commit since the base has Gate-Lanes/Gate-Tree trailers, and the change is "
                 "not documentation only (scripts/gate.sh --trailer)")
    msg = git("log", "-1", "--format=%B", carrier)
    lanes_line = [line for line in msg.splitlines() if line.startswith("Gate-Lanes:")]
    tree_line = [line for line in msg.splitlines() if line.startswith("Gate-Tree:")]
    tested = tree_line[-1].split(":", 1)[1].strip()
    rec = green_record(tested)
    if not rec:
        sys.exit(f"REFUSED: no green gate record for tree {tested}")
    if rec[5] != lanes_line[-1]:
        sys.exit(f"REFUSED: the Gate-Lanes trailer differs from the record of tree {tested}:\n"
                 f"  commit: {lanes_line[-1]}\n  record: {rec[5]}")
    tree = git("rev-parse", f"{head}^{{tree}}").strip()
    if tree != tested:
        base = rec[1]
        moved = set(git("diff", "--name-only", base, a.onto).split())
        mine = set(git("diff", "--name-only", base, tested).split())
        differs = set(git("diff", "--name-only", tested, tree).split())
        both = code_changes(moved & mine, code)
        if both:
            sys.exit(f"REFUSED: {a.onto} changed files the change also changes "
                     f"({' '.join(both[:5])}): run the gate again")
        untested = code_changes(differs - moved, code)
        if untested:
            sys.exit(f"REFUSED: the pushed tree differs from the tested one in files {a.onto} did not move "
                     f"({' '.join(untested[:5])}): run the gate again")
        print(f"gate: tested tree {tested[:12]}, pushed with {len(differs)} files changed since "
              f"(moved by {a.onto}, or documentation)")
    bl = blocks()
    for v in bl:
        if f"{v}=passed" not in rec[5] and f"{v}=reused" not in rec[5]:
            if v in select_default(rec[1])[0]:
                sys.exit(f"REFUSED: {v} is blocked ({bl[v]}) and this change selects it without passing it")
    print("gate: record ok:", lanes_line[-1])


def cmd_behind(a):
    print_behind(a.rev)


def cmd_release_check(a):
    commit = git("rev-parse", a.commit).strip()
    lanes = parse_trailer(git("log", "-1", "--format=%B", commit))
    caught = {v for c, v, s, _ in catchups() if c == commit and s in STATUS_OK}
    missing = [v for v in ALL if lanes.get(v, "").split("(")[0] not in STATUS_OK and v not in caught]
    if missing:
        print(f"release-check: REFUSED for {commit[:12]}: not passed at this commit: {' '.join(missing)}")
        print("  (run `scripts/gate.sh --lanes all` on exactly this commit first)")
        return 1
    print(f"release-check: every lane passed at {commit[:12]}")
    return 0


# --- reuse: content keys of host:all's per-crate test runs ---------------------------------

def tracked_files():
    out = git("ls-files", "-z", "--cached", "--others", "--exclude-standard")
    return sorted(p for p in out.split("\0") if p and os.path.isfile(os.path.join(ROOT, p)))


def hiding_allowed():
    """Scripts are hidden only in a gate worktree of the area (scripts/area.sh: <area>/wt/NAME),
    never in a checkout someone works in, and never as root (root reads a file with no
    permissions). Elsewhere every script is an input, as before the guard."""
    area = os.path.realpath(os.environ.get("STYX_AREA") or "/mnt/nvme-wd/styx")
    return (os.path.realpath(ROOT).startswith(os.path.join(area, "wt") + os.sep)
            and os.geteuid() != 0 and not CI)


def test_readable(files):
    """The files a test can read: all but the scripts that are not inputs. A script is an input
    when a crate's source names it (or a directory of its path) in a string literal, when it is
    one of the gate's own files (GATE_FILES), or when an input program (.sh, .py) names it: a
    script reached through another one is an input too. The rest cannot be read while the gate builds and tests
    (scripts_hidden): a test that reaches one through a path built at run time (a join, a concat!,
    a constant of another crate) fails with a permission error instead of passing on a key that
    left the script out. So a measuring tool edited in scripts/ reruns nothing, and a stale green
    cannot come of it."""
    named = set()
    lit = re.compile(rb'"([^"\n]{1,200})"')
    for rel in files:
        if rel.startswith("crates/") and rel.endswith(".rs"):
            with open(os.path.join(ROOT, rel), "rb") as f:
                for m in lit.finditer(f.read()):
                    for part in m.group(1).decode(errors="replace").split("/"):
                        named.add(part)
    scripts = [rel for rel in files if rel.startswith("scripts/")]
    if not hiding_allowed():
        return list(files)
    inputs = {rel for rel in scripts
              if rel in GATE_FILES or any(part in named for part in rel.split("/")[1:])}
    grew = True
    while grew:
        grew = False
        text = b""
        for rel in sorted(inputs):
            if rel.endswith((".sh", ".py")):  # programs; the lane map names every script
                with open(os.path.join(ROOT, rel), "rb") as f:
                    text += f.read()
        for rel in scripts:
            if rel not in inputs and os.path.basename(rel).encode() in text:
                inputs.add(rel)
                grew = True
    return [rel for rel in files if not rel.startswith("scripts/") or rel in inputs]


def not_inputs():
    files = tracked_files()
    keep = set(test_readable(files))
    return [rel for rel in files if rel.startswith("scripts/") and rel not in keep]


def cmd_scripts_hidden(a):
    """--on: the scripts that are no test's input lose every permission while the gate builds and
    tests; --off: every script gets its committed mode back (also after a gate that was killed)."""
    if a.on:
        if not hiding_allowed():
            print("    every script is a test input here (not a gate worktree of the area): none hidden")
            return
        for rel in not_inputs():
            os.chmod(os.path.join(ROOT, rel), 0)
            print(f"    not readable while the gate runs: {rel}")
    else:
        modes = {}
        for line in git("ls-files", "-s", "--", "scripts").splitlines():
            mode, _, _, rel = line.split(None, 3)
            modes[rel] = 0o755 if mode == "100755" else 0o644
        for rel in git("ls-files", "--others", "--exclude-standard", "--", "scripts").split():
            modes[rel] = 0o755  # not committed yet: a script, so runnable
        for rel, want in modes.items():
            full = os.path.join(ROOT, rel)
            if os.path.isfile(full) and os.stat(full).st_mode & 0o777 == 0:
                os.chmod(full, want)
        # The exec bit is tracked: after the restore no script's mode differs from the index.
        changed = script_mode_drift(ROOT)
        if changed:
            sys.exit("gate: scripts' modes differ from the index after the restore:\n  "
                     + "\n  ".join(changed))


def script_mode_drift(root):
    """Each tracked script whose exec bit differs from its mode in the index, as `git diff` names a
    mode change: read with a stat against `git ls-files -s`, which writes nothing. `git diff` would
    refresh the index (the hide and restore moved the scripts' ctime) and write all of it back, at
    every gate's end, to wherever the worktree's git directory is (2026-11-27: 97 KB, on another
    disk). A script deleted from the worktree is not a mode change, for either; one added to or
    removed from the index during the gate is checked against the index as it is now."""
    out = []
    for line in git("ls-files", "-s", "--", "scripts", cwd=root).splitlines():
        mode, _, _, rel = line.split(None, 3)
        if mode not in ("100644", "100755"):
            continue
        try:
            st = os.lstat(os.path.join(root, rel))
        except FileNotFoundError:
            continue
        if not stat.S_ISREG(st.st_mode):
            continue
        have = "100755" if st.st_mode & stat.S_IXUSR else "100644"
        if have != mode:
            out.append(f"mode change {mode} => {have} {rel}")
    return out


def cmd_script_mode_drift(a):
    changed = script_mode_drift(os.path.realpath(a.root))
    print("\n".join(changed))
    sys.exit(1 if changed else 0)


def shared_libs(binary, cache):
    """The shared libraries a binary loads, by path and file identity (a package update moves them)."""
    if binary in cache:
        return cache[binary]
    r = subprocess.run(["ldd", binary], capture_output=True, text=True)
    libs = []
    for line in r.stdout.splitlines():
        m = re.search(r"=>\s*(/\S+)", line) or re.match(r"\s*(/\S+)", line)
        if m and os.path.exists(m.group(1)):
            st = os.stat(m.group(1))
            libs.append(f"{os.path.realpath(m.group(1))}:{st.st_size}:{st.st_mtime_ns}:{st.st_ino}")
    cache[binary] = sorted(libs)
    return cache[binary]


def tool_versions(pool):
    cmds = (["rustc", "-vV"], ["cargo", "-V"], ["cargo", "nextest", "--version"], ["uname", "-r"],
            ["cc", "--version"], ["pipewire", "--version"])
    runs = [pool.submit(subprocess.run, cmd, cwd=ROOT, capture_output=True, text=True) for cmd in cmds]
    out = []
    for r in runs:
        try:
            out.append(r.result().stdout.strip())
        except OSError:
            out.append("absent")
    return out


def machine_facts(cache):
    """What a test with an outside need (a device, a daemon, a GPU) meets on this machine, beyond
    the tools: the software Vulkan driver the gate points every GPU test at (its manifest and the
    library it names, which is loaded by name, not linked), the vivid test driver's build, the
    dma-heaps, and whether the PipeWire daemon's socket is there. Any change reruns every crate."""
    facts = []
    for var in ("VK_DRIVER_FILES", "VK_ICD_FILENAMES"):
        for icd in filter(None, os.environ.get(var, "").split(":")):
            facts.append(f"{var}={icd}:{digest(icd, cache) if os.path.isfile(icd) else 'absent'}")
            try:
                with open(icd) as f:
                    lib = json.load(f)["ICD"]["library_path"]
                facts.append(f"icd-lib={lib}:{digest(lib, cache) if os.path.isfile(lib) else 'absent'}")
            except (OSError, ValueError, KeyError):
                facts.append("icd-lib=unreadable")
    try:
        with open("/sys/module/vivid/srcversion") as f:
            facts.append("vivid=" + f.read().strip())
    except OSError:
        facts.append("vivid=absent")
    try:
        facts.append("dma-heaps=" + ",".join(sorted(os.listdir("/dev/dma_heap"))))
    except OSError:
        facts.append("dma-heaps=absent")
    runtime = os.environ.get("XDG_RUNTIME_DIR", "")
    facts.append("pipewire-socket=" + str(bool(runtime) and os.path.exists(os.path.join(runtime, "pipewire-0"))))
    return facts


def load_digests():
    """Binary digests from earlier gates, by (path, size, mtime, inode): a test binary is hashed
    again only when cargo wrote it again."""
    try:
        with open(os.path.join(records_dir(), "digests.json")) as f:
            return {tuple(k): v for k, v in json.load(f)}
    except (OSError, ValueError):
        return {}


def save_digests(cache, paths):
    keep = [[list(k), v] for k, v in cache.items() if k[0] in paths]
    path = os.path.join(records_dir(), "digests.json")
    with open(path + ".tmp", "w") as f:
        json.dump(keep, f)
    os.replace(path + ".tmp", path)


def key_of(crate, binaries, files, cache, ldd_cache, common):
    """A crate's test run key, or (None, why)."""
    meta = metadata()
    d = meta[crate][0]
    reach = closure(crate)
    other_rust = {meta[n][0] for n in meta if n != crate}
    h = hashlib.sha256()
    h.update(b"styx-gate-key-v2\0" + common)
    for b in sorted(binaries):
        h.update(b"binary\0" + os.path.basename(b).encode() + b"\0" + binary_digest(b, cache).encode())
        for lib in shared_libs(b, ldd_cache):
            h.update(b"lib\0" + lib.encode() + b"\0")
    names = set()
    for rel in files:
        mine = rel.startswith(d + "/")
        in_crates = rel.startswith("crates/")
        if not mine and (rel.endswith((".md",)) or rel.startswith("docs/") or rel.startswith(".github/")):
            continue
        if not mine and in_crates and (rel.endswith(".rs") or rel.endswith("/Cargo.toml")) \
                and any(rel.startswith(o + "/") for o in other_rust):
            # Another crate's Rust code: what a test runs of it is in the binary's bytes.
            dep = owner(rel)
            if dep in reach and rel.endswith(".rs"):
                with open(os.path.join(ROOT, rel), "rb") as f:
                    names.update(m.group(1).decode() for m in ENV_READ.finditer(f.read()))
            continue
        full = os.path.join(ROOT, rel)
        h.update(b"file\0" + rel.encode() + b"\0" + digest(full, cache).encode())
        if mine and rel.endswith(".rs"):
            with open(full, "rb") as f:
                text = f.read()
            names.update(m.group(1).decode() for m in ENV_READ.finditer(text))
    read = sorted(n for n in names if n.startswith(GATE_ENV_PREFIXES))
    if read:
        sys.exit(f"gate: {crate}'s sources read the gate's own variable {' '.join(read)}: a test may not")
    names.update(k for k in os.environ if k.startswith(ENV_PREFIXES))
    for name in sorted(n for n in names if not gate_var(n)):
        value = os.environ.get(name)
        if value and not name.startswith(("CARGO", "RUST")) and name not in DIRS_KEYED_BY_VALUE \
                and os.path.exists(value):
            if not os.path.isfile(value):
                return None, f"{name} names a directory ({value}), whose content is not keyed"
            h.update(b"envfile\0" + digest(value, cache).encode())
        h.update(b"env\0" + name.encode() + (b"\0unset" if value is None else b"=" + value.encode()) + b"\0")
    return h.hexdigest(), None


def key_records():
    path = os.path.join(records_dir(), "keys.tsv")
    out = {}
    if os.path.exists(path):
        with open(path) as f:
            for line in f:
                c = line.rstrip("\n").split("\t")
                if len(c) == 6:
                    out[c[0]] = c  # key, variant, crate, date, tree, commit
    return out


def doctest_targets():
    """Workspace crates with a library target that rustdoc tests (cargo metadata's doctest flag):
    crate -> that target's name (what rustdoc gets as --crate-name)."""
    metadata()
    m = json.loads(_meta_text)
    return {p["name"]: t["name"] for p in m["packages"] if p["id"] in m["workspace_members"]
            for t in p["targets"] if t.get("doctest")}


def doctest_crates():
    return sorted(doctest_targets())


def doctests_needed(reused, narrow=None):
    """The crates whose doctests run: each with a doctest target whose key moved or that builds
    with (closure: transitively, dev-dependencies included) a workspace crate whose key moved. A
    crate never keyed (no nextest test) is never reused, so it counts as moved."""
    moved = set(metadata()) - set(reused)
    return [c for c in doctest_crates() if closure(c) & moved
            and (narrow is None or closure(c) & narrow)]


def cmd_reuse_plan(a):
    """From a nextest list of the lane's build: which crates run and which reuse a green run. Writes
    the plan (crate -> key) to --plan and prints the filterset of the crates to run ("" = none).
    With --doc FILE, also writes the crates whose doctests must run, one per line: the doctests
    follow the same keys. A crate whose key matched a green run reuses its doctest verdict with the
    rest of its tests (the record is written only after both the nextest run and the doctests are
    green); every other crate with a doctest target needs them: a moved key, no key, or no nextest
    test to key. gate.sh runs them over the whole workspace when the list is not empty (a -p
    selection would unify features differently and rebuild dependencies)."""
    with open(a.list) as f:
        suites = json.load(f)["rust-suites"]
    per_crate = {}
    for s in suites.values():
        # A binary whose tests are all #[ignore] runs nothing (nextest would call it "no tests").
        if any(not t["ignored"] for t in s["testcases"].values()):
            per_crate.setdefault(s["package-name"], []).append(s["binary-path"])
    narrow = set(os.environ["STYX_GATE_NARROW"].split()) if "STYX_GATE_NARROW" in os.environ else None
    if narrow is not None:
        reach = {c for c in per_crate if closure(c) & narrow}
    else:
        reach = set(per_crate)
    meta = metadata()
    files = test_readable(tracked_files())
    cache = {} if CI else load_digests()
    ldd_cache = {}
    binaries = sorted({b for c in reach for b in per_crate[c]})
    with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
        versions = tool_versions(pool)
        # Hashing and ldd per binary, side by side (both wait on the disk or a child process).
        list(pool.map(lambda b: (binary_digest(b, cache), shared_libs(b, ldd_cache)), binaries))
    if not CI:
        save_digests(cache, set(binaries))
    common = "\0".join([a.variant, ROOT, scripts_hash(), *versions, *machine_facts(cache)]).encode()
    # A change to what measures or selects (STYX_GATE_NO_REUSE, from the map's no-reuse lines)
    # reuses nothing: every crate runs, and its new key is recorded after a green run.
    off = os.environ.get("STYX_GATE_NO_REUSE", "").split()
    records = {} if CI or off else key_records()
    plan, run, reused, outside, unkeyed = {}, [], [], [], []

    def say(text):
        print(text, file=sys.stderr)
    if off and not CI:
        say(f"    reuse off: the change measures or selects ({' '.join(off[:5])}{' ...' if len(off) > 5 else ''})")
    for crate in sorted(per_crate):
        if crate not in reach:
            continue
        d = meta[crate][0]
        need = None
        for rel in files:
            if rel.startswith(d + "/") and rel.endswith(".rs"):
                with open(os.path.join(ROOT, rel), errors="replace") as f:
                    if OUTSIDE_NEED.search(f.read()):
                        need = rel
                        break
        if CI:
            run.append(crate)
            continue
        if need:
            outside.append(crate)
        key, why = key_of(crate, per_crate[crate], files, cache, ldd_cache, common)
        if key is None:
            say(f"    {crate:22} runs: NO KEY: {why}")
            run.append(crate)
            unkeyed.append(crate)
            continue
        plan[crate] = key
        rec = records.get(key)
        if rec:
            say(f"    {crate:22} REUSED: green at {rec[5][:12] if rec[5] != '-' else 'tree ' + rec[4][:12]}"
                  f" ({rec[3]})")
            reused.append(crate)
        else:
            say(f"    {crate:22} runs: new key{f' (an outside need: {need})' if need else ''}")
            run.append(crate)
    with open(a.plan, "w") as f:
        json.dump({c: k for c, k in plan.items() if c in run}, f)
    counts = (f"{len(run) + len(reused)} crates: {len(reused)} reused, {len(run)} run "
              f"({len(unkeyed)} unkeyed, {len(run) - len(unkeyed)} new key; "
              f"{len(outside)} with an outside need)")
    say(f"    {a.variant} reuse: {counts}")
    results = os.environ.get("STYX_GATE_RESULTS")
    if results:
        with open(results, "a") as f:
            f.write(f"{a.variant} reuse {counts}\n")
    if a.doc:
        # A dependent's doctest can exercise the changed crate (doctests_needed). The list is from
        # the change, never a fixed list of crates that have doctests: a crate that gains its
        # first doctest runs it.
        targets = doctest_targets()
        docs = doctests_needed(reused, narrow)
        with open(a.doc, "w") as f:
            f.write("".join(c + "\n" for c in docs))
        # The same crates by the name rustdoc is given (--crate-name: the library target's name),
        # for scripts/rustdoc-only.sh.
        with open(a.doc + ".libs", "w") as f:
            f.write("".join(targets[c] + "\n" for c in docs))
        say(f"    {a.variant} doctests: {len(docs)} of {len(targets)} crates need them (a moved key,"
            f" or a dependent of one){': none, every verdict reused' if not docs else ''}")
    print(" | ".join(f"package(={c})" for c in run))


def cmd_alloc_selftest(a):
    """The alloc key's mutants, on a copy of ELF (default: this Python's own executable, an ELF on
    every Linux host and runner), through objcopy: a non-alloc section added keeps the key, the same
    section made alloc moves it, a .rodata byte changed moves it, and a truncated or malformed file
    keys its whole bytes (it never matches the original's alloc key). Exit 1 on any miss."""
    src = a.elf or os.path.realpath(sys.executable)
    fails = []
    with tempfile.TemporaryDirectory() as tmp:
        def path(n):
            return os.path.join(tmp, n)
        with open(src, "rb") as f:
            base = f.read()
        with open(path("base"), "wb") as f:
            f.write(base)
        with open(path("payload"), "wb") as f:
            f.write(b"styx gate alloc self-test\n" * 8)

        def key(n):
            return binary_digest(path(n), {})

        def objcopy(out, *args):
            subprocess.run(["objcopy", *args, path("base"), path(out)], check=True, capture_output=True)
        k0 = key("base")

        def check(what, ok):
            print(f"    alloc key: {what}: {'ok' if ok else 'MISSED'}")
            if not ok:
                fails.append(what)
        check("the original is keyed by its loaded sections", k0.startswith("alloc:"))
        objcopy("note", "--add-section", f".styx-probe={path('payload')}")
        check("a non-alloc section added keeps the key", key("note") == k0)
        objcopy("alloc", "--add-section", f".styx-probe={path('payload')}",
                "--set-section-flags", ".styx-probe=alloc,contents,load,readonly")
        check("the same section set alloc moves the key", key("alloc") != k0)
        shoff, = struct.unpack_from("<Q", base, 0x28)
        shnum, shstrndx = struct.unpack_from("<HH", base, 0x3C)
        secs = [struct.unpack_from("<IIQQQQ", base, shoff + i * 64) for i in range(shnum)]
        stroff = secs[shstrndx][4]
        rodata = [sec for sec in secs if base[stroff + sec[0]:base.index(b"\0", stroff + sec[0])] == b".rodata"]
        if not rodata or rodata[0][5] == 0:
            check("the copy has a .rodata to change", False)
        else:
            off = rodata[0][4] + rodata[0][5] // 2
            changed = bytearray(base)
            changed[off] ^= 0xFF
            with open(path("rodata"), "wb") as f:
                f.write(changed)
            check("a .rodata byte changed moves the key", key("rodata") != k0)
        with open(path("short"), "wb") as f:
            f.write(base[:len(base) // 2])
        k = key("short")
        check("a truncated file keys its whole bytes", k.startswith("file:") and k != k0)
        bad = bytearray(base)
        struct.pack_into("<Q", bad, 0x28, len(base) * 4)
        with open(path("bad"), "wb") as f:
            f.write(bad)
        k = key("bad")
        check("a malformed section table keys the whole bytes", k.startswith("file:") and k != k0)
        # The headers intact, one alloc section's size past the end of the file.
        if rodata:
            i = secs.index(rodata[0])
            past = bytearray(base)
            struct.pack_into("<Q", past, shoff + i * 64 + 0x20, len(base) * 2)
            with open(path("past"), "wb") as f:
                f.write(past)
            k = key("past")
            check("a section past the end keys the whole bytes", k.startswith("file:") and k != k0)
        # The section names cut inside an alloc section's name (a read that raises).
        first = next(sec for sec in secs if sec[2] & SHF_ALLOC)
        cut = bytearray(base)
        struct.pack_into("<Q", cut, shoff + shstrndx * 64 + 0x20, first[0] + 1)
        with open(path("cut"), "wb") as f:
            f.write(cut)
        k = key("cut")
        check("unreadable section names key the whole bytes", k.startswith("file:") and k != k0)
    if fails:
        sys.exit(f"gate: the alloc key self-test missed: {'; '.join(fails)}")


def cmd_reuse_record(a):
    """After a green run: record the keys of the crates that ran."""
    if CI:
        return
    with open(a.plan) as f:
        plan = json.load(f)
    tree = os.environ.get("STYX_GATE_TREE") or worktree_tree()
    head = git("rev-parse", "HEAD").strip()
    commit = head if git("rev-parse", "HEAD^{tree}").strip() == tree else "-"
    now = time.strftime("%Y-%m-%dT%H:%M:%S")
    with open(os.path.join(records_dir(), "keys.tsv"), "a") as f:
        fcntl.flock(f, fcntl.LOCK_EX)
        for crate, key in sorted(plan.items()):
            f.write("\t".join([key, a.variant, crate, now, tree, commit]) + "\n")


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)
    r = sub.add_parser("run")
    r.add_argument("--lanes", default="")
    r.add_argument("--drop", action="append", default=[])
    r.add_argument("--dry-run", action="store_true")
    r.add_argument("--catch-up", action="store_true")
    r.add_argument("--narrow", action="store_true", help="scripts/check.sh: tests of the crates the change reaches")
    r.add_argument("crates", nargs="*", help="with --narrow: these crates instead of the changed ones")
    sub.add_parser("trailer")
    v = sub.add_parser("verify-push")
    v.add_argument("rev", nargs="?", default="HEAD")
    v.add_argument("--onto", default=UPSTREAM)
    b = sub.add_parser("behind")
    b.add_argument("rev", nargs="?", default=UPSTREAM)
    rc = sub.add_parser("release-check")
    rc.add_argument("commit")
    sub.add_parser("alloc-selftest").add_argument("--elf")
    rp = sub.add_parser("reuse-plan")
    rp.add_argument("--list", required=True)
    rp.add_argument("--plan", required=True)
    rp.add_argument("--variant", default="host:all")
    rp.add_argument("--doc")
    sh = sub.add_parser("scripts-hidden")
    sh.add_argument("--on", action="store_true")
    sub.add_parser("script-mode-drift").add_argument("--root", default=ROOT)
    sk = sub.add_parser("selftest-key")
    sk.add_argument("--root")
    sr = sub.add_parser("selftest-reuse")
    sr.add_argument("key")
    sc = sub.add_parser("selftest-record")
    sc.add_argument("key")
    ls = sub.add_parser("lane-status")
    ls.add_argument("--results", required=True)
    ls.add_argument("--rc", type=int, required=True)
    ls.add_argument("variants", nargs="+")
    rr = sub.add_parser("reuse-record")
    rr.add_argument("--plan", required=True)
    rr.add_argument("--variant", default="host:all")
    a = p.parse_args()
    if a.cmd == "run" and a.crates and not a.narrow:
        sys.exit("gate: crate names go with --narrow (scripts/check.sh)")
    fn = {"selftest-key": cmd_selftest_key, "selftest-reuse": cmd_selftest_reuse,
          "selftest-record": cmd_selftest_record, "lane-status": cmd_lane_status, "scripts-hidden": cmd_scripts_hidden, "script-mode-drift": cmd_script_mode_drift, "run": cmd_run, "trailer": cmd_trailer, "verify-push": cmd_verify_push, "behind": cmd_behind,
          "release-check": cmd_release_check, "reuse-plan": cmd_reuse_plan, "alloc-selftest": cmd_alloc_selftest,
          "reuse-record": cmd_reuse_record}[a.cmd]
    sys.exit(fn(a) or 0)


if __name__ == "__main__":
    main()
