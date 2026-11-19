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
       test-layout.py default-only-filter   the filterset that runs .config/default-only.txt
                                            (empty output: none)

default-only: tests run in the all-features pass only. A test that exists only under default
features (cfg(not(feature = ...))) would then never run unless the default pass runs it, so the
two nextest lists (`--message-format json`) are compared and the difference must be the list.

filters: a group's filter, or one `|` term of it, that matches nothing (a renamed module or
binary) would silently stop putting its tests in the group. Each term is listed with nextest; none
may come back empty.
"""
import json
import os
import re
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
    bad = 0
    for profile, expr in terms:
        out = subprocess.run(
            ["cargo", "nextest", "list", "--locked", "--workspace", *cargo_args,
             "-E", expr, "--message-format", "json"],
            cwd=ROOT, capture_output=True, text=True,
        )
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
    elif sys.argv[1:2] == ["default-only-filter"]:
        default_only_filter()
    else:
        sys.exit(__doc__)
