#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0
#
# Build styx_sensor_bridge.ko (and the CM5 overlay) for an exact device kernel.
#
# A module only loads if its vermagic and the CRCs of every symbol it imports
# (CONFIG_MODVERSIONS) match the running kernel, so it must be built against
# the configured and built kernel tree that produced the device image, not
# just the same version. For HeliOS that tree is Buildroot's
# output/build/linux-custom. This script never writes to that tree: `prepare`
# copies the external-module build subset (headers, .config, Module.symvers,
# host scripts) out of it with the kernel's own scripts/package/install-extmod-build.
#
#   ./build.sh prepare   copy the kbuild subset from $KERNEL_TREE into $STYX_KBUILD_ROOT
#   ./build.sh build     build the module and overlay into $STYX_KBUILD_ROOT/out
#   ./build.sh verify    check vermagic (and, with DEVICE=root@host, the device's
#                        vermagic and symbol CRCs, read-only over ssh)
#   ./build.sh all       prepare (if needed) + build + verify
#   ./build.sh clean
#
# Environment:
#   KERNEL_TREE       built kernel tree (default: HeliOS Gaia CM5 Buildroot output next to this repo)
#   BR_HOST           Buildroot host dir with the cross toolchain (default: derived from KERNEL_TREE)
#   CROSS_COMPILE     toolchain prefix (default: $BR_HOST/bin/aarch64-linux-, else aarch64-linux-gnu-)
#   KERNEL_RELEASE    expected release (default: 6.12.47-v8-16k)
#   STYX_KBUILD_ROOT  work dir outside the git tree (default: ../linux-build-styx next to the repo)
#   DEVICE            ssh target for `verify` (optional, e.g. root@helios); read-only commands only

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
workspace="$(cd "$repo/.." && pwd)"

KERNEL_RELEASE="${KERNEL_RELEASE:-6.12.47-v8-16k}"
STYX_KBUILD_ROOT="${STYX_KBUILD_ROOT:-$workspace/linux-build-styx}"
KERNEL_TREE="${KERNEL_TREE:-$workspace/HeliOS-architecture-overhaul/gaia/build/helios-full-cm5/image/buildroot-output/build/linux-custom}"
BR_HOST="${BR_HOST:-$(cd "$KERNEL_TREE/../.." 2>/dev/null && pwd)/host}"
if [[ -z "${CROSS_COMPILE:-}" ]]; then
    if [[ -x "$BR_HOST/bin/aarch64-linux-gcc" ]]; then
        CROSS_COMPILE="$BR_HOST/bin/aarch64-linux-"
    else
        CROSS_COMPILE="aarch64-linux-gnu-"
    fi
fi

kbuild="$STYX_KBUILD_ROOT/kbuild-$KERNEL_RELEASE"
objdir="$STYX_KBUILD_ROOT/obj/styx-sensor-bridge"
outdir="$STYX_KBUILD_ROOT/out/$KERNEL_RELEASE"
module="styx_sensor_bridge"
overlays=(styx-sensor-bridge-cm5 styx-sensor-bridge-cm5-runtime styx-sensor-bridge-cm5-runtime-emb)

log() { printf '[styx-bridge] %s\n' "$*" >&2; }
die() { log "error: $*"; exit 1; }

prepare() {
    [[ -f "$KERNEL_TREE/Module.symvers" && -f "$KERNEL_TREE/.config" ]] ||
        die "KERNEL_TREE=$KERNEL_TREE is not a built kernel tree (no Module.symvers/.config)"
    local release
    release="$(cat "$KERNEL_TREE/include/config/kernel.release")"
    [[ "$release" == "$KERNEL_RELEASE" ]] ||
        die "kernel tree is $release, expected $KERNEL_RELEASE"
    grep -q '^CONFIG_MODVERSIONS=y' "$KERNEL_TREE/.config" || log "note: MODVERSIONS is off in this tree"

    log "copying kbuild subset of $KERNEL_TREE -> $kbuild"
    rm -rf "$kbuild.tmp"
    # install-extmod-build only reads the tree. CC == HOSTCC skips its
    # "rebuild host programs" step: the tree's scripts were built on this host.
    (cd "$KERNEL_TREE" &&
        env srctree=. SRCARCH=arm64 CC=gcc HOSTCC=gcc MAKE=make \
            sh scripts/package/install-extmod-build "$kbuild.tmp")
    cp "$KERNEL_TREE/.config" "$kbuild.tmp/.config"
    {
        echo "kernel_tree=$KERNEL_TREE"
        echo "kernel_release=$release"
        echo "uts_version=$(sed -n 's/.*UTS_VERSION "\(.*\)"/\1/p' "$KERNEL_TREE/include/generated/utsversion.h")"
        echo "module_symvers_sha256=$(sha256sum "$KERNEL_TREE/Module.symvers" | cut -d' ' -f1)"
    } >"$kbuild.tmp/styx-provenance.txt"
    rm -rf "$kbuild"
    mv "$kbuild.tmp" "$kbuild"
    log "prepared $kbuild"
}

build() {
    [[ -f "$kbuild/Module.symvers" ]] || die "run '$0 prepare' first"
    command -v "${CROSS_COMPILE}gcc" >/dev/null || die "no compiler ${CROSS_COMPILE}gcc"
    local want_cc
    want_cc="$(sed -n 's/^CONFIG_CC_VERSION_TEXT="\(.*\)"/\1/p' "$kbuild/.config")"
    if [[ "$("${CROSS_COMPILE}gcc" --version | head -1)" != "$want_cc" ]]; then
        log "warning: compiler differs from the kernel's ($want_cc)"
    fi

    # Build in a copy so the git tree stays free of build products.
    rm -rf "$objdir"
    mkdir -p "$objdir" "$outdir"
    cp "$here/Kbuild" "$here/$module.c" "$here/styx_sensor_bridge.h" "$objdir/"

    log "building $module.ko with ${CROSS_COMPILE}gcc"
    make -C "$kbuild" M="$objdir" ARCH=arm64 CROSS_COMPILE="$CROSS_COMPILE" \
        CONFIG_VIDEO_STYX_SENSOR_BRIDGE=m W=1 modules
    cp "$objdir/$module.ko" "$outdir/"

    local dtc=""
    for c in "$KERNEL_TREE/scripts/dtc/dtc" "$BR_HOST/bin/dtc" "$(command -v dtc || true)"; do
        [[ -n "$c" && -x "$c" ]] && { dtc="$c"; break; }
    done
    if [[ -n "$dtc" ]]; then
        for overlay in "${overlays[@]}"; do
            log "compiling $overlay overlay with $dtc"
            "$dtc" -@ -q -I dts -O dtb -o "$outdir/$overlay.dtbo" "$here/dts/$overlay-overlay.dts"
        done
    else
        log "warning: no dtc found; overlay not compiled"
    fi
    log "outputs in $outdir"
}

verify() {
    local ko="$outdir/$module.ko"
    [[ -f "$ko" ]] || die "no $ko; run '$0 build'"
    local vermagic want
    vermagic="$(modinfo -F vermagic "$ko")"
    want="$KERNEL_RELEASE SMP preempt mod_unload modversions aarch64"
    log "module vermagic: $vermagic"
    if [[ -n "${DEVICE:-}" ]]; then
        # Read-only: modinfo of a module the device already has.
        want="$(ssh -o BatchMode=yes "$DEVICE" 'modinfo -F vermagic ov9282')"
        log "device vermagic: $want"
    fi
    [[ "$(echo "$vermagic" | xargs)" == "$(echo "$want" | xargs)" ]] || die "vermagic mismatch (want '$want')"
    log "vermagic OK"

    if [[ -n "${DEVICE:-}" ]]; then
        # Compare the CRCs our Module.symvers gives with the CRCs recorded in
        # modules on the device (they import the same core symbols).
        local devdir="$STYX_KBUILD_ROOT/device-modules"
        mkdir -p "$devdir"
        local k="/lib/modules/$KERNEL_RELEASE/kernel/drivers/media"
        for m in v4l2-core/videodev v4l2-core/v4l2-async v4l2-core/v4l2-fwnode mc/mc \
            platform/raspberrypi/rp1_cfe/rp1-cfe i2c/ov9282; do
            ssh -o BatchMode=yes "$DEVICE" "cat $k/$m.ko.xz" | xz -dc >"$devdir/$(basename "$m").ko"
        done
        python3 "$here/tools/check_crcs.py" --objcopy "${CROSS_COMPILE}objcopy" \
            --symvers "$kbuild/Module.symvers" --module "$ko" "$devdir"/*.ko
    fi
}

clean() {
    rm -rf "$objdir" "$outdir"
}

cmd="${1:-all}"
case "$cmd" in
prepare) prepare ;;
build) build ;;
verify) verify ;;
all)
    [[ -f "$kbuild/Module.symvers" ]] || prepare
    build
    verify
    ;;
clean) clean ;;
*) die "unknown command $cmd (prepare|build|verify|all|clean)" ;;
esac
