#!/usr/bin/env python3
"""The workspace crates owning the files that differ from a revision (default: origin/next),
untracked files included.

Usage: affected.py [<rev>]          one package name per line
       affected.py --plan [<rev>]   what scripts/check.sh runs (below)

A change outside every crate (the workspace manifest, the lock file, .cargo/, .config/, scripts/,
the toolchain), or to a crate's own manifest (its features and dependencies), prints "--workspace":
only the whole gate covers it. Markdown files and docs/ outside the crates change no build and are
left out.

--plan prints, one per line:
  crate <name>     a changed crate (scripts/check.sh tests it and its dependents)
  pass default     the one feature pass to run: "all" when a changed file is behind a feature
  pass all         (a cfg naming a feature in the file, or in the attributes of a `mod` that
                   declares it, or of one declaring that, up to the crate root), else "default"
  doctests 1       a changed file holds a doctest (a code fence in a doc comment, or a Markdown
                   file in a crate, which a crate may include as its documentation)
  no_std <dir>     a changed crate whose manifest says no_std = true (its directory name)
or "--workspace" alone.
"""
import json
import os
import re
import subprocess
import sys

FEATURE = re.compile(r"\bcfg(_attr)?\s*\([^\n]*\bfeature\s*=")
FENCE = re.compile(r"^\s*(///|//!).*```")
ATTR = re.compile(r"^\s*#\[")
PATH_ATTR = re.compile(r'#\[path\s*=\s*"([^"]+)"\]')


def read(path):
    try:
        with open(path, encoding="utf-8", errors="replace") as f:
            return f.read()
    except OSError:
        return ""


def rust_files(crate_dir):
    for top, dirs, files in os.walk(crate_dir):
        dirs[:] = [d for d in dirs if d != "target"]
        for name in files:
            if name.endswith(".rs"):
                yield os.path.join(top, name)


def declarations(path, crate_dir):
    """(declaring file, attribute lines) for each `mod` item that may declare the file at path."""
    stem = os.path.splitext(os.path.basename(path))[0]
    if stem == "mod":
        stem = os.path.basename(os.path.dirname(path))
    decl = re.compile(r"^\s*(pub(\([^)]*\))?\s+)?mod\s+" + re.escape(stem) + r"\s*;")
    found = []
    for other in rust_files(crate_dir):
        if other == path:
            continue
        lines = read(other).split("\n")
        for i, line in enumerate(lines):
            attrs = []
            j = i - 1
            while j >= 0 and ATTR.match(lines[j]):
                attrs.append(lines[j])
                j -= 1
            named = decl.match(line)
            pathed = False
            for a in attrs:
                m = PATH_ATTR.search(a)
                if m and re.match(r"^\s*(pub(\([^)]*\))?\s+)?mod\s+\w+\s*;", line):
                    target = os.path.normpath(os.path.join(os.path.dirname(other), m.group(1)))
                    pathed = target == os.path.normpath(path)
                    named = named and not m  # a #[path] mod names its own file
            if named or pathed:
                found.append((other, attrs))
    return found


def declared_behind_feature(path, crate_dir, seen):
    """Whether a `mod` declaring the file, or one declaring that, up to the root, names a feature."""
    if path in seen:
        return False
    seen.add(path)
    for other, attrs in declarations(path, crate_dir):
        if any(FEATURE.search(a) for a in attrs) or declared_behind_feature(other, crate_dir, seen):
            return True
    return False


def behind_feature(path, crate_dir):
    """Whether a feature decides if the file at path is compiled, or what it compiles."""
    if not path.endswith(".rs"):
        return False
    return bool(FEATURE.search(read(path))) or declared_behind_feature(path, crate_dir, set())


def holds_doctest(path):
    if path.endswith(".md"):
        return "```" in read(path)
    if path.endswith(".rs"):
        return any(FENCE.match(line) for line in read(path).split("\n"))
    return False


def main():
    args = sys.argv[1:]
    plan = "--plan" in args
    args = [a for a in args if a != "--plan"]
    rev = args[0] if args else "origin/next"
    meta = json.loads(subprocess.run(
        ["cargo", "metadata", "--locked", "--format-version", "1", "--no-deps"],
        check=True, capture_output=True, text=True,
    ).stdout)
    root = meta["workspace_root"]
    members = [p for p in meta["packages"] if p["id"] in meta["workspace_members"]]
    dirs = {p["name"]: os.path.dirname(p["manifest_path"]) for p in members}
    no_std = {
        p["name"] for p in members
        if ((p.get("metadata") or {}).get("styx") or {}).get("no_std") is True
    }

    def git(*args):
        return subprocess.run(
            ["git", *args], check=True, capture_output=True, text=True, cwd=root,
        ).stdout.split("\n")

    files = git("diff", "--name-only", rev, "--") + git("ls-files", "--others", "--exclude-standard")
    owners = set()
    feature = doctests = False
    for f in filter(None, files):
        path = os.path.join(root, f)
        owner = max(
            (n for n, d in dirs.items() if path.startswith(d + os.sep)),
            key=lambda n: len(dirs[n]), default=None,
        )
        if owner is None:
            if f.endswith(".md") or f.startswith("docs/"):
                continue
            print("--workspace")
            return
        if path == os.path.join(dirs[owner], "Cargo.toml"):
            print("--workspace")
            return
        owners.add(owner)
        # A deleted file is gone: what declared it changed too, and is judged on its own.
        if os.path.exists(path):
            feature = feature or behind_feature(path, dirs[owner])
            doctests = doctests or holds_doctest(path)
    if not plan:
        for n in sorted(owners):
            print(n)
        return
    for n in sorted(owners):
        print("crate", n)
    if owners:
        print("pass", "all" if feature else "default")
        print("doctests", 1 if doctests else 0)
    for n in sorted(owners & no_std):
        print("no_std", os.path.basename(dirs[n]))


if __name__ == "__main__":
    main()
