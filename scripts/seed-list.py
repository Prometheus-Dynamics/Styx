#!/usr/bin/env python3
"""The files of a target directory that a seed keeps (scripts/cargo-seed.sh): every file but the
workspace's, as NUL-separated F<path> (a file) and D<path> (an empty directory) lines relative to
the target. Reads `cargo metadata --no-deps` on stdin; the target is the one argument.

A file of its own, not a `python3 -c '...'` in the shell script: an apostrophe in a comment there
ended the shell quote, the program ran cut short and listed nothing, and no seed was kept."""
import fnmatch, json, os, sys
target = sys.argv[1]
meta = json.load(sys.stdin)
pkgs, names = [], []
for p in meta["packages"]:
    if p["id"] in meta["workspace_members"]:
        pkgs.append(p["name"])
        names += [t["name"].replace("-", "_") for t in p["targets"]]
# The gate's record of the target's worktree (scripts/gate.sh) holds its path: never in a seed.
trees = {os.path.join(target, "tmp"), os.path.join(target, "nextest"),
         os.path.join(target, ".styx-worktree")}
files = set()
def listdir(d):
    try:
        return os.listdir(d)
    except OSError:
        return []
# A profile directory holds deps/ and .fingerprint/: <profile>, <triple>/<profile>, or either one
# inside a target of its own made in this one (the gate's default/ and bare/), at any depth.
profiles = []
for top, dirs, _ in os.walk(target):
    if "deps" in dirs and ".fingerprint" in dirs:
        profiles.append(top)
        dirs[:] = []
    else:
        dirs[:] = [d for d in dirs if not os.path.islink(os.path.join(top, d))]
for profile in profiles:
    for e in listdir(profile):
        f = os.path.join(profile, e)
        if os.path.isfile(f) and not os.path.islink(f):
            files.add(f)
    trees.update(os.path.join(profile, x) for x in ("examples", "incremental", "c-proof"))
    deps = listdir(os.path.join(profile, "deps"))
    fps = listdir(os.path.join(profile, ".fingerprint"))
    builds = listdir(os.path.join(profile, "build"))
    tags = set()
    for name in pkgs:
        for fp in fps:
            if fp.startswith(name + "-") and os.path.isdir(os.path.join(profile, ".fingerprint", fp)):
                tags.add(fp.rsplit("-", 1)[1])
                trees.add(os.path.join(profile, ".fingerprint", fp))
        trees.update(os.path.join(profile, "build", e) for e in builds if e.startswith(name + "-"))
    # A deps/ file of a unit carries "-<hash>" in its name (the glob *-<hash>*): each "-" in a name
    # is looked up once, for each length a hash has, rather than every name against every hash.
    sizes = {len(tag) for tag in tags}
    for e in deps:
        at = e.find("-")
        while at >= 0:
            if any(e[at + 1:at + 1 + n] in tags for n in sizes):
                files.add(os.path.join(profile, "deps", e))
                break
            at = e.find("-", at + 1)
    for name in names:
        files.update(os.path.join(profile, "deps", e) for e in deps
                     if fnmatch.fnmatchcase(e, f"{name}.*") or fnmatch.fnmatchcase(e, f"lib{name}.*"))
out = sys.stdout.buffer
for top, dirs, entries in os.walk(target):
    dirs[:] = [d for d in dirs if os.path.join(top, d) not in trees]
    kept = 0
    for e in entries + [d for d in dirs if os.path.islink(os.path.join(top, d))]:
        f = os.path.join(top, e)
        if f not in files and f not in trees:
            out.write(b"F" + os.path.relpath(f, target).encode() + b"\0")
            kept += 1
    if not kept and not dirs and top != target:
        out.write(b"D" + os.path.relpath(top, target).encode() + b"\0")
