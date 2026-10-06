# SPDX-License-Identifier: GPL-2.0
#
# Kernel selection shared by styx-sensor-bridge/build.sh and pispbe/build.sh (sourced, bash).
#
# Two Raspberry Pi kernels are in the field:
#
#   STYX_KERNEL=6.12  (default) the HeliOS / Raze 1.0.x image kernel: Raspberry Pi tag
#                     stable_20250916, release 6.12.47-v8-16k, built by the HeliOS Gaia
#                     Buildroot (helios-full-cm5) next to this repository.
#   STYX_KERNEL=7.2   Raze 1.1.0: raspberrypi/linux rpi-7.2.y. The default tree is a
#                     configured and built O= tree at ../linux-rpi-7.2/build next to the
#                     repository (bcm2712_defconfig with Buildroot's 4K pages and VIDEO_OV9282=m,
#                     see the README); point KERNEL_TREE at the Buildroot linux-custom of the
#                     image instead to match a device. The Raze 1.1.0 kernel (commit, hash,
#                     ov9782 patches) is defined by the Raze device package in
#                     Atlas-Hardware-Manager (dev), $RAZE_PKG.
#
# Every variable can still be set directly:
#   KERNEL_TREE       built kernel tree (in-tree build, or the O= object tree of an out-of-tree one)
#   KERNEL_SRC        its source tree (default: $KERNEL_TREE/source for an O= tree, else $KERNEL_TREE)
#   BR_HOST           Buildroot host dir with the cross toolchain
#   CROSS_COMPILE     toolchain prefix (default: $BR_HOST/bin/aarch64-linux-, else aarch64-linux-gnu-)
#   KERNEL_RELEASE    expected release (6.12: 6.12.47-v8-16k; 7.2: read from $KERNEL_TREE)
#   STYX_KBUILD_ROOT  work dir outside the git tree (default: ../linux-build-styx next to the repo)
#   RAZE_PKG          the Raze device package (default: ../Atlas-Hardware-Manager/devices/raze/gaia;
#                     not the old Atlas-raze checkout)

# shellcheck shell=bash
# Callers set $workspace (the directory containing the repository).

STYX_KERNEL="${STYX_KERNEL:-6.12}"
helios_br="$workspace/HeliOS-architecture-overhaul/gaia/build/helios-full-cm5/image/buildroot-output"
RAZE_PKG="${RAZE_PKG:-$workspace/Atlas-Hardware-Manager/devices/raze/gaia}"

case "$STYX_KERNEL" in
6.12)
    KERNEL_TREE="${KERNEL_TREE:-$helios_br/build/linux-custom}"
    KERNEL_RELEASE="${KERNEL_RELEASE:-6.12.47-v8-16k}"
    ;;
7.2)
    KERNEL_TREE="${KERNEL_TREE:-$workspace/linux-rpi-7.2/build}"
    if [[ -z "${KERNEL_RELEASE:-}" ]]; then
        KERNEL_RELEASE="$(cat "$KERNEL_TREE/include/config/kernel.release" 2>/dev/null || true)"
        [[ -n "$KERNEL_RELEASE" ]] ||
            { echo "error: KERNEL_TREE=$KERNEL_TREE has no kernel.release; set KERNEL_RELEASE" >&2; exit 1; }
    fi
    ;;
*)
    echo "error: STYX_KERNEL=$STYX_KERNEL (6.12 or 7.2)" >&2
    exit 1
    ;;
esac

if [[ -z "${KERNEL_SRC:-}" ]]; then
    if [[ -d "$KERNEL_TREE/source" ]]; then
        KERNEL_SRC="$(cd "$KERNEL_TREE/source" && pwd -P)"
    else
        KERNEL_SRC="$KERNEL_TREE"
    fi
fi

if [[ -z "${BR_HOST:-}" ]]; then
    # A Buildroot linux-custom sits in <output>/build/; otherwise use the HeliOS toolchain
    # (the compiler the Raze images are built with).
    BR_HOST="$(cd "$KERNEL_TREE/../.." 2>/dev/null && pwd)/host"
    [[ -x "$BR_HOST/bin/aarch64-linux-gcc" ]] || BR_HOST="$helios_br/host"
fi
if [[ -z "${CROSS_COMPILE:-}" ]]; then
    if [[ -x "$BR_HOST/bin/aarch64-linux-gcc" ]]; then
        CROSS_COMPILE="$BR_HOST/bin/aarch64-linux-"
    else
        CROSS_COMPILE="aarch64-linux-gnu-"
    fi
fi
STYX_KBUILD_ROOT="${STYX_KBUILD_ROOT:-$workspace/linux-build-styx}"

# The device's camera receiver module: rp1-cfe (6.12) became rp1-cfe-downstream in rpi-7.2.y
# (same driver and compatible; the mainline driver is rp1-cfe, compatible
# raspberrypi,rp1-cfe-upstream).
case "$KERNEL_RELEASE" in
6.*) CFE_MODULE_PATH="platform/raspberrypi/rp1_cfe/rp1-cfe" ;;
*) CFE_MODULE_PATH="platform/raspberrypi/rp1_cfe/rp1-cfe-downstream" ;;
esac

# The vermagic a module built for this kernel carries.
kernel_vermagic() {
    echo "$KERNEL_RELEASE SMP preempt mod_unload modversions aarch64"
}

export STYX_KERNEL RAZE_PKG KERNEL_TREE KERNEL_SRC KERNEL_RELEASE BR_HOST CROSS_COMPILE STYX_KBUILD_ROOT
