#!/usr/bin/env python3
"""Checks that every test is built and run, and every test group still names its tests.

Usage: test-layout.py files                 a crate that lists its test binaries itself
                                            (autotests = false) leaves no tests/*.rs file out
       test-layout.py filters [cargo args]  every filter in .config/nextest.toml matches a test

files: a crate's integration tests are one binary, tests/all.rs, with one module per file. Cargo
then builds only the binaries the manifest lists, so a new tests/x.rs that no binary includes
would never build or run. Each top-level tests/*.rs file must be a [[test]] path of the manifest
or a module of one (a `#[path = "x.rs"]` or `mod x;` in a test file).

       test-layout.py default-only <default list> <all-features list>
                                            the tests only the default build has are exactly the
                                            ones in .config/default-only.txt
       test-layout.py shared-state          no file of a merged test binary shares process state
                                            unless .config/shared-state.txt says why it is safe
       test-layout.py default-only-filter   the filterset that runs .config/default-only.txt
                                            (empty output: none)
       test-layout.py default-only-present <default list>
                                            every line of .config/default-only.txt names a test
                                            the default build has

default-only: tests run in the all-features pass only. A test that exists only under default
features (cfg(not(feature = ...))) would then never run unless the default pass runs it, so the
two nextest lists (`--message-format json`) are compared and the difference must be the list.

shared-state: cargo test runs a merged binary's tests as threads of one process (nextest gives
each test its own). A file of tests/all.rs that installs a global allocator, reads /proc/self, or
names a `pub` item of a sim or test-support module (tests/ helpers, or a crates/ path with a sim,
support, testing, mock or fake part) that reaches a process-wide static (atomic, Mutex, RwLock,
OnceLock, OnceCell, LazyLock) must be its own [[test]], or be listed with its reason. A counter
only ever advanced with fetch_add is a source of unique names and is not counted.

filters: a group's filter, or one `|` term of it, that matches nothing (a renamed module or
binary) would silently stop putting its tests in the group. Each term is listed with nextest; none
may come back empty.
"""
import json
import os
import re
import concurrent.futures
import subprocess
import sys
import tomllib

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def files():
    bad = []
    for top, _, names in os.walk(os.path.join(ROOT, "crates")):
        if "Cargo.toml" not in names or "/target" in top:
            continue
        with open(os.path.join(top, "Cargo.toml"), "rb") as f:
            manifest = tomllib.load(f)
        if manifest.get("package", {}).get("autotests", True):
            continue
        tests = os.path.join(top, "tests")
        if not os.path.isdir(tests):
            continue
        listed = {os.path.normpath(os.path.join(top, t["path"])) for t in manifest.get("test", [])}
        included = set()
        for path in listed:
            text = open(path, encoding="utf-8").read()
            for rel in re.findall(r'#\[path\s*=\s*"([^"]+)"\]', text):
                included.add(os.path.normpath(os.path.join(os.path.dirname(path), rel)))
            for stem in re.findall(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*;", text, re.M):
                included.add(os.path.normpath(os.path.join(tests, stem + ".rs")))
        for name in sorted(os.listdir(tests)):
            path = os.path.normpath(os.path.join(tests, name))
            if name.endswith(".rs") and path not in listed and path not in included:
                bad.append(os.path.relpath(path, ROOT))
    for path in bad:
        print(f"{path}: in no test binary (add it to the crate's tests/all.rs)")
    if bad:
        sys.exit(1)
    print("every test file is in a test binary")


def alternatives(expr):
    """The top-level `|` terms of a filterset (outside parentheses and /regex/ delimiters)."""
    terms, depth, regex, start = [], 0, False, 0
    for i, c in enumerate(expr):
        if c == "/":
            regex = not regex
        elif regex:
            continue
        elif c == "(":
            depth += 1
        elif c == ")":
            depth -= 1
        elif c == "|" and depth == 0:
            terms.append(expr[start:i].strip())
            start = i + 1
    terms.append(expr[start:].strip())
    return terms


def filters(cargo_args):
    with open(os.path.join(ROOT, ".config", "nextest.toml"), "rb") as f:
        config = tomllib.load(f)
    found = []
    for name, profile in config.get("profile", {}).items():
        for override in profile.get("overrides", []):
            if "filter" in override:
                found.append((name, override["filter"]))
    # Each alternative of a filter (its top-level `|` terms) must match on its own: a renamed
    # module in one term would hide behind the others' matches.
    terms = [(profile, term) for profile, expr in found for term in alternatives(expr)]
    # The terms are listed side by side: each list runs every test binary's --list, and one at a
    # time they were most of this check's time. Results are printed in the terms' order.
    def listed(term):
        return subprocess.run(
            ["cargo", "nextest", "list", "--locked", "--workspace", *cargo_args,
             "-E", term[1], "--message-format", "json"],
            cwd=ROOT, capture_output=True, text=True,
        )

    with concurrent.futures.ThreadPoolExecutor(max_workers=len(terms) or 1) as pool:
        outs = list(pool.map(listed, terms))
    bad = 0
    for (profile, expr), out in zip(terms, outs):
        if out.returncode != 0:
            print(f"profile {profile}: {expr}\n{out.stderr}")
            bad += 1
            continue
        suites = json.loads(out.stdout)["rust-suites"].values()
        count = sum(
            1 for s in suites for t in s["testcases"].values() if t["filter-match"]["status"] == "matches"
        )
        print(f"profile {profile}: {count} tests match {expr}")
        if count == 0:
            bad += 1
    if bad:
        print(f"{bad} filter(s) in .config/nextest.toml match no test")
        sys.exit(1)


SUPPORT_DIR = re.compile(r"(^|/)(sim|support|testing|test_support|mock|fake)s?(/|\.rs$)")
STATEFUL_STATIC = re.compile(
    r"\bstatic\s+(?:mut\s+)?([A-Z][A-Z0-9_]*)\s*:\s*[^=;]*\b(Atomic\w*|Mutex|RwLock|OnceLock|OnceCell|LazyLock|LazyCell)\b"
)
# Only a plain `pub` item can be named from a test; `pub(crate)` and `pub(super)` cannot.
FN = re.compile(r"^\s*(?:(pub)\s+|pub\([^)]*\)\s+)?(?:const\s+|async\s+|unsafe\s+)*fn\s+(\w+)", re.M)


def braced(text, at):
    """The text from `at` to the end of the first `{...}` block that starts after it."""
    start = text.find("{", at)
    if start < 0:
        return ""
    depth = 0
    for i in range(start, len(text)):
        if text[i] == "{":
            depth += 1
        elif text[i] == "}":
            depth -= 1
            if depth == 0:
                return text[at:i + 1]
    return text[at:]


def stateful_items(path):
    """The public items of a support source file that reach one of its process-wide statics: a
    `pub static` of that kind, or a fn that names one, declares one, or calls a fn of this file that
    does (followed to a fixed point)."""
    text = open(path, encoding="utf-8").read()
    statics = {name for name, _ in STATEFUL_STATIC.findall(text)}
    # A counter only ever advanced with fetch_add hands each caller a number no other caller gets:
    # a source of unique names, not state one test can see of another's. Only that: an atomic that
    # is also loaded, stored or swapped anywhere is state another test can set or read, and counts.
    statics = {s for s in statics
               if len(re.findall(rf"\b{s}\b", text)) - 1 != len(re.findall(rf"\b{s}\s*\.\s*fetch_add\s*\(", text))}
    if not statics:
        return set()
    fns = {m.group(2): (bool(m.group(1)), braced(text, m.end())) for m in FN.finditer(text)}
    touching = {name for name, (_, body) in fns.items()
                if any(re.search(rf"\b{s}\b", body) for s in statics)}
    while True:
        more = {name for name, (_, body) in fns.items() if name not in touching
                and any(re.search(rf"\b{t}\s*\(", body) for t in touching)}
        if not more:
            break
        touching |= more
    public = {name for name in touching if fns[name][0]}
    public |= {n for n in statics if re.search(rf"\bpub\s+static\s+(?:mut\s+)?{n}\b", text)}
    return public


def merged_files():
    """{test file: its crate} for every file that is a module of a crate's tests/all.rs."""
    out = {}
    for top, _, names in os.walk(os.path.join(ROOT, "crates")):
        if "/target" in top or os.path.basename(top) != "tests" or "all.rs" not in names:
            continue
        text = open(os.path.join(top, "all.rs"), encoding="utf-8").read()
        for rel in re.findall(r'#\[path\s*=\s*"([^"]+)"\]', text):
            out[os.path.normpath(os.path.join(top, rel))] = os.path.dirname(top)
    return out


def shared_state_entries():
    entries = {}
    with open(os.path.join(ROOT, ".config", "shared-state.txt"), encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if line and not line.startswith("#"):
                path, _, reason = line.partition(" ")
                entries[path] = reason.strip()
    return entries


def shared_state():
    stateful = {}
    for top, _, names in os.walk(os.path.join(ROOT, "crates")):
        if "/target" in top:
            continue
        for name in names:
            path = os.path.join(top, name)
            rel = os.path.relpath(path, ROOT)
            in_tests = "/tests/" in rel
            if name.endswith(".rs") and (SUPPORT_DIR.search(rel) or in_tests):
                for item in stateful_items(path):
                    stateful.setdefault(item, set()).add(rel)
    found = {}
    for path in merged_files():
        rel = os.path.relpath(path, ROOT)
        text = open(path, encoding="utf-8").read()
        why = []
        if "#[global_allocator]" in text:
            why.append("installs a global allocator")
        if "/proc/self" in text:
            why.append("reads /proc/self")
        for item, homes in sorted(stateful.items()):
            if os.path.relpath(path, ROOT) in homes:
                continue
            used = re.search(rf"::\s*{item}\b", text) or re.search(
                rf"^\s*use\s[^;]*\{{[^}}]*\b{item}\b[^;]*;", text, re.M)
            if used:
                why.append(f"uses {item} ({', '.join(sorted(homes))}), which reaches a process-wide static")
        if why:
            found[rel] = why
    entries = shared_state_entries()
    bad = 0
    for rel, why in sorted(found.items()):
        if rel not in entries:
            print(f"{rel}: in a merged test binary, and it {'; '.join(why)}. Give it its own [[test]] "
                  "with the reason in Cargo.toml, or list it in .config/shared-state.txt with why its "
                  "state is its own test's")
            bad += 1
    for rel in sorted(set(entries) - set(found)):
        print(f"{rel}: in .config/shared-state.txt but not a merged file that shares process state; "
              "remove the line")
        bad += 1
    if bad:
        sys.exit(1)
    print(f"merged test files sharing process state: {len(found)}, each with its reason")


def default_only_entries():
    entries = set()
    with open(os.path.join(ROOT, ".config", "default-only.txt"), encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if line and not line.startswith("#"):
                binary, _, test = line.partition(" ")
                entries.add((binary, test.strip()))
    return entries


def listed(path):
    with open(path, encoding="utf-8") as f:
        suites = json.load(f)["rust-suites"]
    return {(binary, test) for binary, suite in suites.items() for test in suite["testcases"]}


def default_only(default_list, all_list):
    only = listed(default_list) - listed(all_list)
    entries = default_only_entries()
    bad = 0
    for binary, test in sorted(only - entries):
        print(f"{binary} {test}: exists only under default features; add the line to .config/default-only.txt")
        bad += 1
    for binary, test in sorted(entries - only):
        print(f"{binary} {test}: in .config/default-only.txt but not a default-only test; remove the line")
        bad += 1
    if bad:
        sys.exit(1)
    print(f"default-only tests: {len(only)}, all in .config/default-only.txt")


def default_only_present(default_list):
    missing = sorted(default_only_entries() - listed(default_list))
    for binary, test in missing:
        print(f"{binary} {test}: in .config/default-only.txt but no default-features test has that name")
    if missing:
        sys.exit(1)


def default_only_filter():
    terms = [f"(binary_id(={b}) & test(={t}))" for b, t in sorted(default_only_entries())]
    print(" | ".join(terms))


if __name__ == "__main__":
    if sys.argv[1:2] == ["files"]:
        files()
    elif sys.argv[1:2] == ["filters"]:
        filters(sys.argv[2:])
    elif sys.argv[1:2] == ["default-only"] and len(sys.argv) == 4:
        default_only(sys.argv[2], sys.argv[3])
    elif sys.argv[1:2] == ["shared-state"]:
        shared_state()
    elif sys.argv[1:2] == ["default-only-present"] and len(sys.argv) == 3:
        default_only_present(sys.argv[2])
    elif sys.argv[1:2] == ["default-only-filter"]:
        default_only_filter()
    else:
        sys.exit(__doc__)
