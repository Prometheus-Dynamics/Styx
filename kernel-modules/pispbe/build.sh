#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0
#
# Build the patched PiSP back end driver (pisp-be.ko, module pisp_be) for an exact
# device kernel, the way ../styx-sensor-bridge/build.sh builds the bridge: against the
# external-module subset of the configured and built kernel tree that produced the
# image (vermagic and every imported symbol CRC must match). The kernel tree is only
# read; the kbuild subset is the bridge's (`../styx-sensor-bridge/build.sh prepare`).
#
#   ./build.sh build     build into <repo>/target/kernel-modules/<release>/pisp-be.ko
#   ./build.sh verify    vermagic; with DEVICE=root@host also the imported symbol CRCs
#                        against modules on the device (read-only over ssh)
#   ./build.sh diff      the patch against the stock source in $KERNEL_TREE
#   ./build.sh all       build + verify
#   ./build.sh clean
#
# Environment: STYX_KERNEL (6.12 or 7.2), KERNEL_TREE, KERNEL_SRC, BR_HOST, CROSS_COMPILE,
# KERNEL_RELEASE, STYX_KBUILD_ROOT, DEVICE as in ../styx-sensor-bridge/build.sh (defaults in
# ../kernel-env.sh). The source here is rpi-7.2.y's driver with the Styx changes; it builds
# against both kernels (README).

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
workspace="$(cd "$repo/.." && pwd)"

# shellcheck source=../kernel-env.sh
source "$here/../kernel-env.sh"

kbuild="$STYX_KBUILD_ROOT/kbuild-$KERNEL_RELEASE"
objdir="$repo/target/kernel-modules/obj/pispbe"
outdir="$repo/target/kernel-modules/$KERNEL_RELEASE"
stock="drivers/media/platform/raspberrypi/pisp_be"

log() { printf '[pispbe] %s\n' "$*" >&2; }
die() { log "error: $*"; exit 1; }

build() {
    [[ -f "$kbuild/Module.symvers" ]] ||
        die "no kbuild subset in $kbuild; run ../styx-sensor-bridge/build.sh prepare"
    [[ -f "$kbuild/include/uapi/linux/media/raspberrypi/pisp_be_config.h" ]] ||
        die "$kbuild has no PiSP uAPI headers"
    command -v "${CROSS_COMPILE}gcc" >/dev/null || die "no compiler ${CROSS_COMPILE}gcc"
    rm -rf "$objdir"
    mkdir -p "$objdir" "$outdir"
    cp "$here/Kbuild" "$here/pisp_be.c" "$here/pisp_be_formats.h" "$objdir/"
    log "building pisp-be.ko with ${CROSS_COMPILE}gcc"
    make -C "$kbuild" M="$objdir" ARCH=arm64 CROSS_COMPILE="$CROSS_COMPILE" W=1 modules
    cp "$objdir/pisp-be.ko" "$outdir/"
    log "output: $outdir/pisp-be.ko"
}

verify() {
    local ko="$outdir/pisp-be.ko"
    [[ -f "$ko" ]] || die "no $ko; run '$0 build'"
    [[ "$(modinfo -F name "$ko")" == pisp_be ]] || die "module name is not pisp_be"
    local vermagic want
    vermagic="$(modinfo -F vermagic "$ko")"
    want="$(kernel_vermagic)"
    if [[ -n "${DEVICE:-}" ]]; then
        want="$(ssh -o BatchMode=yes "$DEVICE" 'modinfo -F vermagic videodev')"
    fi
    log "module vermagic: $vermagic"
    [[ "$(echo "$vermagic" | xargs)" == "$(echo "$want" | xargs)" ]] || die "vermagic mismatch (want '$want')"
    log "vermagic OK"
    if [[ -n "${DEVICE:-}" ]]; then
        local devdir="$repo/target/kernel-modules/device-modules"
        mkdir -p "$devdir"
        local k="/lib/modules/$KERNEL_RELEASE/kernel/drivers/media"
        for m in v4l2-core/videodev mc/mc common/videobuf2/videobuf2-common \
            common/videobuf2/videobuf2-v4l2 common/videobuf2/videobuf2-dma-contig \
            platform/raspberrypi/pisp_be/pisp-be; do
            ssh -o BatchMode=yes "$DEVICE" "cat $k/$m.ko.xz" | xz -dc >"$devdir/$(basename "$m").ko"
        done
        python3 "$here/../styx-sensor-bridge/tools/check_crcs.py" --objcopy "${CROSS_COMPILE}objcopy" \
            --symvers "$kbuild/Module.symvers" --module "$ko" "$devdir"/*.ko
    fi
}

diff_stock() {
    # The patch against the kernel tree's stock source (for STYX_KERNEL=7.2 the Styx changes
    # alone; against 6.12 it also shows the upstream changes between the two kernels).
    for f in pisp_be.c pisp_be_formats.h; do
        diff -u --label "a/$stock/$f" --label "b/$stock/$f" "$KERNEL_SRC/$stock/$f" "$here/$f" || true
    done
}

case "${1:-all}" in
build) build ;;
verify) verify ;;
diff) diff_stock ;;
all) build; verify ;;
clean) rm -rf "$objdir" "$outdir/pisp-be.ko" ;;
*) die "unknown command $1 (build|verify|diff|all|clean)" ;;
esac
