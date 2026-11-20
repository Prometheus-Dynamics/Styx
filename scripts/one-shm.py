#!/usr/bin/env python3
"""Fails when the workspace's Lemnos crates do not all come from the one pinned Lemnos rev.

Two checks, on the resolved graph (`cargo metadata --locked`):

- One source per `shm-*` crate. Two crates that take the shared ring from different revisions get
  different types for the same ring, pool and hold table, and cannot hand them to each other.
- One Lemnos pin. Every Lemnos dependency in the root `[workspace.dependencies]` is pinned by git
  and `rev` (a full 40-hex rev; no branch, tag or version), all at the same rev, and every resolved
  Lemnos crate (any package from the Lemnos repository, spelled with `.git` or not, and any
  `lemnos-*` or `shm-*` package from elsewhere) is at that rev. A crate pulled in through another
  dependency (Orion's shm, lemnosd's drivers) at another rev fails here, by name.
"""
import json
import re
import subprocess
import sys
import tomllib
from pathlib import Path

LEMNOS_URL = re.compile(r"^(git\+)?https://github\.com/Prometheus-Dynamics/Lemnos(\.git)?/?([?#]|$)")
REV = re.compile(r"^[0-9a-f]{40}$")

root = Path(
    subprocess.run(
        ["cargo", "locate-project", "--workspace", "--message-format", "plain"],
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()
).parent
meta = json.loads(
    subprocess.run(
        ["cargo", "metadata", "--locked", "--format-version", "1", "--all-features"],
        check=True,
        capture_output=True,
        text=True,
        cwd=root,
    ).stdout
)
failed = False

seen = {}
for package in meta["packages"]:
    if package["name"].startswith("shm-"):
        seen.setdefault(package["name"], set()).add((package["version"], package["source"] or "path"))
bad = {name: sorted(where) for name, where in seen.items() if len(where) > 1}
for name, where in sorted(bad.items()):
    print(f"{name} is resolved {len(where)} times:")
    for version, source in where:
        print(f"  {version} from {source}")
failed |= bool(bad)

deps = tomllib.loads((root / "Cargo.toml").read_text())["workspace"]["dependencies"]
pins = {}
for name, spec in sorted(deps.items()):
    if not isinstance(spec, dict) or not LEMNOS_URL.match(spec.get("git", "")):
        continue
    extra = sorted(k for k in ("branch", "tag", "version") if k in spec)
    rev = spec.get("rev", "")
    if extra or not REV.match(rev):
        print(f"{name}: a Lemnos dependency is pinned by git and a full rev only, found rev={rev!r} {extra}")
        failed = True
    pins.setdefault(rev, []).append(name)
if len(pins) > 1:
    print("the Lemnos dependencies name more than one rev:")
    for rev, names in sorted(pins.items()):
        print(f"  {rev}: {', '.join(names)}")
    failed = True
if not pins:
    print("no Lemnos dependency in [workspace.dependencies]: cannot tell the pin")
    sys.exit(1)
pin = next(iter(pins))

lemnos = 0
strays = []
for package in meta["packages"]:
    source = package["source"] or "path"
    if LEMNOS_URL.match(source):
        lemnos += 1
        if source.rsplit("#", 1)[-1] != pin:
            strays.append(f"{package['name']} {package['version']} from {source}")
    elif package["name"].startswith(("lemnos-", "shm-")) or package["name"] == "lemnosd":
        lemnos += 1
        strays.append(f"{package['name']} {package['version']} from {source}")
for stray in sorted(strays):
    print(f"not at the Lemnos pin {pin}: {stray}")
failed |= bool(strays)

if failed:
    sys.exit(1)
print(f"{len(seen)} shm crates, one source each; {lemnos} Lemnos crates, all at {pin}")
