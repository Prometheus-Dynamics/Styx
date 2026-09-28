#!/bin/sh
# Regenerates the layout assertions of crates/pisp/src/uapi/layout.rs from a Linux source tree
# with the Raspberry Pi PiSP drivers (the tree the device runs), and diffs them.
#
#   ./run.sh <linux-source-tree> [out-dir]
#
# The checks are compiled for the host; the structures have fixed-width fields only, so the
# layout is the same on aarch64 (the kernel's own static_asserts check the key sizes too).
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
linux="$1"
out="${2:-$here/../../../target/pisp-layout-check}"
mkdir -p "$out"
cc -DFE -I"$linux/drivers/media/platform/raspberrypi/rp1_cfe" -o "$out/fe" "$here/check.c"
cc -DBE -I"$linux/include/uapi/linux/media/raspberrypi" -o "$out/be" "$here/check.c"
{ "$out/fe"; "$out/be"; } >"$out/layout.rs.inc"
sed -n '/^\/\/ BEGIN GENERATED/,/^\/\/ END GENERATED/p' "$here/../src/uapi/layout.rs" |
    sed '1d;$d' >"$out/current.inc"
if diff -u "$out/current.inc" "$out/layout.rs.inc"; then
    echo "layout.rs matches the headers in $linux"
else
    echo "layout.rs differs from the headers in $linux (generated: $out/layout.rs.inc)"
    exit 1
fi
