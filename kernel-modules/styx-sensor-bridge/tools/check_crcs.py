#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0
"""Check that a Module.symvers matches a running kernel.

Every module built with CONFIG_MODVERSIONS records, in its __versions section,
the CRC of each symbol it imports. Modules taken from the device therefore
tell us the device kernel's CRCs. If they all equal the CRCs in the
Module.symvers we build against, and our module's imports resolve in it, the
module will pass the kernel's version checks.
"""

import argparse
import os
import struct
import subprocess
import sys
import tempfile


def read_symvers(path):
    crcs = {}
    with open(path) as f:
        for line in f:
            parts = line.rstrip("\n").split("\t")
            if len(parts) >= 2:
                crcs[parts[1]] = int(parts[0], 16)
    return crcs


def read_versions(objcopy, ko):
    """(symbol, crc) pairs from a module's __versions section (64-bit layout)."""
    with tempfile.NamedTemporaryFile(dir=os.path.dirname(os.path.abspath(ko))) as tmp:
        subprocess.run(
            [objcopy, "-O", "binary", "--only-section=__versions", ko, tmp.name],
            check=True,
        )
        data = open(tmp.name, "rb").read()
    out = []
    for off in range(0, len(data), 64):
        crc = struct.unpack_from("<Q", data, off)[0]
        name = data[off + 8 : off + 64].split(b"\0")[0].decode()
        out.append((name, crc))
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--objcopy", default="aarch64-linux-gnu-objcopy")
    ap.add_argument("--symvers", required=True)
    ap.add_argument("--module", required=True, help="our freshly built module")
    ap.add_argument("device_modules", nargs="+", help="modules copied from the device")
    args = ap.parse_args()

    symvers = read_symvers(args.symvers)
    checked = bad = 0
    device_crcs = {}
    for ko in args.device_modules:
        for name, crc in read_versions(args.objcopy, ko):
            checked += 1
            device_crcs[name] = crc
            if symvers.get(name) != crc:
                bad += 1
                print(f"MISMATCH {os.path.basename(ko)}: {name} device={crc:#x} "
                      f"symvers={symvers.get(name, 0):#x}")
    print(f"device CRCs checked against Module.symvers: {checked}, mismatches: {bad}")

    ours = read_versions(args.objcopy, args.module)
    overlap = [(n, c) for n, c in ours if n in device_crcs]
    ours_bad = [n for n, c in overlap if device_crcs[n] != c]
    missing = [n for n, _ in ours if n not in symvers]
    print(f"our imports: {len(ours)}, also imported by device modules: {len(overlap)}, "
          f"CRC differences: {len(ours_bad)}, unresolved: {len(missing)}")
    for n in ours_bad + missing:
        print(f"  problem: {n}")
    return 1 if bad or ours_bad or missing else 0


if __name__ == "__main__":
    sys.exit(main())
