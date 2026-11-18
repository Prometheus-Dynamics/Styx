#!/usr/bin/env python3
"""The workspace crates a change reaches: the named crates, or the crates owning the changed files,
and every workspace crate that depends on them (dev-dependencies too, so their tests are in).

Usage: affected.py [crate ...]          names, e.g. styx-frame
       affected.py --changed [<rev>]    the crates whose files differ from <rev> (default: origin/next)
       affected.py --libs crate ...     of the named crates, the ones with a library (doctests)
Prints one package name per line. A change outside every crate (the workspace manifest, the lock
file, .cargo/, scripts/, the toolchain) prints "--workspace": only the whole gate covers it.
"""
import json
import os
import subprocess
import sys


def metadata():
    out = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--no-deps"],
        check=True, capture_output=True, text=True,
    ).stdout
    return json.loads(out)


def main():
    meta = metadata()
    root = meta["workspace_root"]
    members = {p["name"]: p for p in meta["packages"] if p["id"] in meta["workspace_members"]}
    dirs = {name: os.path.dirname(p["manifest_path"]) for name, p in members.items()}

    args = sys.argv[1:]
    if args and args[0] == "--libs":
        for name in args[1:]:
            if any("lib" in t["kind"] or "proc-macro" in t["kind"] for t in members[name]["targets"]):
                print(name)
        return
    if args and args[0] == "--changed":
        rev = args[1] if len(args) > 1 else "origin/next"
        files = subprocess.run(
            ["git", "diff", "--name-only", rev, "--"], check=True, capture_output=True, text=True,
            cwd=root,
        ).stdout.split()
        files += subprocess.run(
            ["git", "ls-files", "--others", "--exclude-standard"], check=True,
            capture_output=True, text=True, cwd=root,
        ).stdout.split()
        start = set()
        for f in files:
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
            start.add(owner)
    else:
        start = set(args)
        unknown = start - members.keys()
        if unknown:
            sys.exit(f"not workspace crates: {', '.join(sorted(unknown))}")

    # Reverse edges over every kind of dependency between workspace crates.
    users = {name: set() for name in members}
    for name, p in members.items():
        for dep in p["dependencies"]:
            if dep["name"] in members and dep.get("path"):
                users[dep["name"]].add(name)
    seen, todo = set(), list(start)
    while todo:
        n = todo.pop()
        if n in seen:
            continue
        seen.add(n)
        todo.extend(users[n] - seen)
    for n in sorted(seen):
        print(n)


if __name__ == "__main__":
    main()
