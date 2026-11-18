#!/usr/bin/env python3
"""Lists, and with --delete removes, Cargo target directories whose worktree is gone or merged.

Usage: prune-targets.py <targets-root> [--delete] [--idle-minutes N]

Each directory under <targets-root> is a target directory when it holds a deps/ directory
(<profile>/deps, or <triple>/<profile>/deps). Its
worktree is read from the dependency files Cargo wrote next to its final artifacts
(<profile>/*.d name the sources they were built from, by absolute path). It is prunable when:
  - its worktree no longer exists, or the worktree's branch is merged into origin/next
    (every commit of it is in origin/next) and the worktree has no uncommitted change; and
  - nothing in it was written for --idle-minutes (default 60): a build in progress is left alone.
A directory whose worktree cannot be told, or that belongs to another repository, is kept and
listed as such. Nothing is deleted without --delete.
"""
import os
import re
import shutil
import subprocess
import sys
import time

MARK = "/crates/"


def deps_dirs(target):
    """The deps/ directories of a target directory: <profile>/deps, or <triple>/<profile>/deps."""
    found = []
    for first in sorted(os.listdir(target)):
        a = os.path.join(target, first)
        if not os.path.isdir(a) or os.path.islink(a):
            continue
        if os.path.isdir(os.path.join(a, "deps")):
            found.append(os.path.join(a, "deps"))
        for second in sorted(os.listdir(a)):
            b = os.path.join(a, second, "deps")
            if os.path.isdir(b):
                found.append(b)
    return found


def worktree_of(target):
    """The source root the target directory was built from, from its dependency files."""
    # The dependency files next to the final artifacts (<profile>/*.d) name sources by absolute
    # path; those in deps/ name a workspace's own sources relative to it, so only theirs of other
    # checkouts show.
    places = [os.path.dirname(d) for d in deps_dirs(target)] + deps_dirs(target)
    for place in places:
        for name in os.listdir(place):
            if not name.endswith(".d"):
                continue
            try:
                with open(os.path.join(place, name), encoding="utf-8", errors="replace") as f:
                    text = f.read(65536)
            except OSError:
                continue
            for path in re.findall(r"(/\S+?)" + re.escape(MARK), text):
                if not path.startswith(os.path.expanduser("~/.cargo")):
                    return path
    return None


def newest_write(path, stop_after):
    """The newest modification time under path, stopping early once one is newer than stop_after."""
    newest = 0.0
    for root, dirs, files in os.walk(path):
        for name in files + dirs:
            try:
                m = os.lstat(os.path.join(root, name)).st_mtime
            except OSError:
                continue
            if m > newest:
                newest = m
                if newest > stop_after:
                    return newest
    return newest


def git(worktree, *args):
    return subprocess.run(["git", "-C", worktree, *args], capture_output=True, text=True)


def verdict(worktree):
    """Why the worktree's target directory may go, or None to keep it, and a note."""
    if not os.path.isdir(worktree):
        return "worktree gone", ""
    if git(worktree, "rev-parse", "--git-dir").returncode != 0:
        return None, "not a git worktree"
    if not os.path.isfile(os.path.join(worktree, "crates/core/frame/Cargo.toml")):
        return None, "another repository"
    if git(worktree, "status", "--porcelain").stdout.strip():
        return None, "uncommitted changes"
    head = git(worktree, "rev-parse", "HEAD").stdout.strip()
    merged = git(worktree, "merge-base", "--is-ancestor", head, "origin/next").returncode == 0
    if merged:
        return "branch merged into origin/next", ""
    return None, "branch not merged"


def main():
    args = sys.argv[1:]
    if not args or args[0].startswith("-"):
        sys.exit(__doc__)
    root = args[0]
    delete = "--delete" in args
    idle = 60
    if "--idle-minutes" in args:
        idle = int(args[args.index("--idle-minutes") + 1])
    cutoff = time.time() - idle * 60
    for name in sorted(os.listdir(root)):
        target = os.path.join(root, name)
        if os.path.islink(target) or not os.path.isdir(target):
            continue
        if not deps_dirs(target):
            continue
        worktree = worktree_of(target)
        if worktree is None:
            print(f"keep   {target}: its worktree cannot be told")
            continue
        why, note = verdict(worktree)
        if why is None:
            print(f"keep   {target}: {worktree} ({note})")
            continue
        if newest_write(target, cutoff) > cutoff:
            print(f"keep   {target}: {worktree} ({why}, but written in the last {idle} min)")
            continue
        if delete:
            shutil.rmtree(target)
            print(f"pruned {target}: {worktree} ({why})")
        else:
            print(f"prune  {target}: {worktree} ({why}); --delete to remove")


if __name__ == "__main__":
    main()
