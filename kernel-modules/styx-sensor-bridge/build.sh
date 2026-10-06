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
# Kernel: STYX_KERNEL=6.12 (default, HeliOS / Raze 1.0.x, 6.12.47-v8-16k) or 7.2 (Raze 1.1.0,
# raspberrypi/linux rpi-7.2.y). See ../kernel-env.sh for the defaults each selects.
#
# Environment:
#   STYX_KERNEL       6.12 or 7.2 (selects the defaults below)
#   KERNEL_TREE       built kernel tree, or the O= object tree of one (default: per STYX_KERNEL)
#   KERNEL_SRC        its source tree (default: $KERNEL_TREE/source for an O= tree)
#   BR_HOST           Buildroot host dir with the cross toolchain (default: derived from KERNEL_TREE,
#                     else the HeliOS Gaia toolchain)
#   CROSS_COMPILE     toolchain prefix (default: $BR_HOST/bin/aarch64-linux-, else aarch64-linux-gnu-)
#   KERNEL_RELEASE    expected release (default: 6.12.47-v8-16k; for 7.2 the tree's)
#   STYX_KBUILD_ROOT  work dir outside the git tree (default: ../linux-build-styx next to the repo)
#   DEVICE            ssh target for `verify` (optional, e.g. root@helios); read-only commands only

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
workspace="$(cd "$repo/.." && pwd)"

# shellcheck source=../kernel-env.sh
source "$here/../kernel-env.sh"

kbuild="$STYX_KBUILD_ROOT/kbuild-$KERNEL_RELEASE"
objdir="$STYX_KBUILD_ROOT/obj/styx-sensor-bridge"
outdir="$STYX_KBUILD_ROOT/out/$KERNEL_RELEASE"
module="styx_sensor_bridge"
overlays=(styx-sensor-bridge-cm5 styx-sensor-bridge-cm5-runtime styx-sensor-bridge-cm5-runtime-emb
    styx-cam0-i2c-fast)
# Base device trees the overlays are test-applied to (fdtoverlay) when the tree has them.
base_dtbs=(bcm2712-rpi-cm5-cm5io bcm2712-rpi-5-b)

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
        env srctree="$KERNEL_SRC" SRCARCH=arm64 CC=gcc HOSTCC=gcc MAKE=make \
            sh "$KERNEL_SRC/scripts/package/install-extmod-build" "$kbuild.tmp")
    cp "$KERNEL_TREE/.config" "$kbuild.tmp/.config"
    {
        echo "kernel_tree=$KERNEL_TREE"
        echo "kernel_src=$KERNEL_SRC"
        # Only when the source tree is itself a git checkout (not inside another repository).
        if [[ "$(git -C "$KERNEL_SRC" rev-parse --show-toplevel 2>/dev/null)" == "$(cd "$KERNEL_SRC" && pwd -P)" ]]; then
            echo "kernel_commit=$(git -C "$KERNEL_SRC" rev-parse HEAD)"
        fi
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
        check_overlays
    else
        log "warning: no dtc found; overlay not compiled"
    fi
    log "outputs in $outdir"
}

# Applies each compiled overlay to the kernel tree's CM5/Pi 5 device trees with fdtoverlay: every
# label an overlay targets or references must exist there (a renamed node fails here, not at
# boot). The boot overlay is applied with its default (cam1) targets; the firmware's
# __overrides__ (cam0) are not evaluated by fdtoverlay.
check_overlays() {
    local fdtoverlay="" base dtb overlay
    for c in "$KERNEL_TREE/scripts/dtc/fdtoverlay" "$BR_HOST/bin/fdtoverlay" "$(command -v fdtoverlay || true)"; do
        [[ -n "$c" && -x "$c" ]] && { fdtoverlay="$c"; break; }
    done
    [[ -n "$fdtoverlay" ]] || { log "note: no fdtoverlay; overlays not test-applied"; return 0; }
    for base in "${base_dtbs[@]}"; do
        dtb="$KERNEL_TREE/arch/arm64/boot/dts/broadcom/$base.dtb"
        [[ -f "$dtb" ]] || { log "note: no $base.dtb in the kernel tree (make dtbs); not test-applied"; continue; }
        for overlay in "${overlays[@]}"; do
            "$fdtoverlay" -i "$dtb" -o "$objdir/$base+$overlay.dtb" "$outdir/$overlay.dtbo" ||
                die "$overlay does not apply to $base.dtb"
        done
        log "overlays apply to $base.dtb"
    done
}

verify() {
    local ko="$outdir/$module.ko"
    [[ -f "$ko" ]] || die "no $ko; run '$0 build'"
    local vermagic want
    vermagic="$(modinfo -F vermagic "$ko")"
    want="$(kernel_vermagic)"
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
            "$CFE_MODULE_PATH" i2c/ov9282; do
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
