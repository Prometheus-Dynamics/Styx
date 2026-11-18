#!/usr/bin/env python3
"""The workspace crates owning the files that differ from a revision (default: origin/next),
untracked files included.

Usage: affected.py [<rev>]
Prints one package name per line. A change outside every crate (the workspace manifest, the lock
file, .cargo/, .config/, scripts/, the toolchain) prints "--workspace": only the whole gate covers
it. Markdown files and docs/ outside the crates change no build and are left out.
"""
import json
import os
import subprocess
import sys


def main():
    rev = sys.argv[1] if len(sys.argv) > 1 else "origin/next"
    meta = json.loads(subprocess.run(
        ["cargo", "metadata", "--locked", "--format-version", "1", "--no-deps"],
        check=True, capture_output=True, text=True,
    ).stdout)
    root = meta["workspace_root"]
    dirs = {
        p["name"]: os.path.dirname(p["manifest_path"])
        for p in meta["packages"] if p["id"] in meta["workspace_members"]
    }

    def git(*args):
        return subprocess.run(
            ["git", *args], check=True, capture_output=True, text=True, cwd=root,
        ).stdout.split("\n")

    files = git("diff", "--name-only", rev, "--") + git("ls-files", "--others", "--exclude-standard")
    owners = set()
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
        owners.add(owner)
    for n in sorted(owners):
        print(n)


if __name__ == "__main__":
    main()
