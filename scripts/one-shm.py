#!/usr/bin/env python3
"""Fails when the workspace resolves any `shm-*` crate from more than one version or source.

Two crates that take the shared ring from different revisions get different types for the same
ring, pool and hold table, and cannot hand them to each other.
"""
import json
import subprocess
import sys

meta = json.loads(
    subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--all-features"],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
)
seen = {}
for package in meta["packages"]:
    if package["name"].startswith("shm-"):
        seen.setdefault(package["name"], set()).add((package["version"], package["source"] or "path"))
bad = {name: sorted(where) for name, where in seen.items() if len(where) > 1}
for name, where in sorted(bad.items()):
    print(f"{name} is resolved {len(where)} times:")
    for version, source in where:
        print(f"  {version} from {source}")
if bad:
    sys.exit(1)
print(f"{len(seen)} shm crates, one source each")
