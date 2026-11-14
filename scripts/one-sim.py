#!/usr/bin/env python3
"""Fails when a file outside `crates/camera/sim` defines a simulated camera driver.

The simulated sensor, receiver and ISP (`impl CameraSensor for`, `impl CameraReceiver for`,
`impl CameraIsp for`) live once, in `styx-sim`. A second copy drifts from the first, so a test
that takes its own copy tests a different simulator.

`ALLOWED` lists the files that still carry a copy and are moved by another PR. Remove an entry
when its copy is gone.
"""
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
HOME = ROOT / "crates" / "camera" / "sim"
PATTERN = re.compile(r"impl\s+(CameraSensor|CameraReceiver|CameraIsp)\s+for\b")
SKIP_DIRS = {"target", ".git"}
ALLOWED = {
    # The simulated PiSP's tests: another PR changes them and moves them into styx-sim.
    "crates/isp/pisp/tests/sim/mod.rs",
}

found = {}
for path in sorted(ROOT.rglob("*.rs")):
    rel = path.relative_to(ROOT)
    if any(part in SKIP_DIRS or part.startswith("target") for part in rel.parts):
        continue
    if HOME in path.parents or path.parent == HOME:
        continue
    text = path.read_text(encoding="utf-8", errors="replace")
    hits = sorted({m.group(1) for m in PATTERN.finditer(text)})
    if hits:
        found[rel.as_posix()] = hits
bad = {name: hits for name, hits in found.items() if name not in ALLOWED}
for name, hits in sorted(bad.items()):
    print(f"{name} defines a simulated camera driver outside styx-sim: {', '.join(hits)}")
if bad:
    print("move the driver into crates/camera/sim (styx-sim) and use it from there")
    sys.exit(1)
stale = sorted(name for name in ALLOWED if name not in found)
for name in stale:
    print(f"{name} is in ALLOWED but no longer defines a driver: remove it from scripts/one-sim.py")
if stale:
    sys.exit(1)
print(f"one simulated camera: drivers only in styx-sim ({len(ALLOWED)} allowed outside, see scripts/one-sim.py)")
