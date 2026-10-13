#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0
#
# Builds Styx's ov9282 driver (ov9282.ko, module ov9282) against the kernel build tree of the
# image it is for: the image's own Buildroot linux-custom (its .config, Module.symvers and
# toolchain), never a separately configured tree. A different config can keep vermagic and
# still change struct module (tracing on or off), which the kernel then refuses to load.
#
#   ./build.sh build    ov9282.ko into $OUT (default <scratch>/styx-ov9282/<release>)
#   ./build.sh verify   vermagic, and with DEVICE=root@host the .gnu.linkonce.this_module
#                       size against the device's own ov9282.ko (read-only over ssh)
#   ./build.sh diff     the Styx patch against the Raze image's driver (git history has the
#                       stock import and the Raze patches as their own commits)
#   ./build.sh all      build + verify
#
# KERNEL_TREE  the image's Buildroot linux-custom (default: the PhotonVision Raze image next to
#              the repository); BR_HOST its host dir (default: KERNEL_TREE/../../host).
# OUT          output directory (outside the repository: build output never goes to its disk).

set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
workspace="$(cd "$here/../../.." && pwd)"
KERNEL_TREE="${KERNEL_TREE:-$workspace/photon-image-modifier/build/gaia/photonvision-full-raze/image/buildroot-output/build/linux-custom}"
BR_HOST="${BR_HOST:-$KERNEL_TREE/../../host}"
CROSS_COMPILE="${CROSS_COMPILE:-$BR_HOST/bin/aarch64-linux-}"
release="$(cat "$KERNEL_TREE/include/config/kernel.release" 2>/dev/null || true)"
OUT="${OUT:-${TMPDIR:-/tmp}/styx-ov9282/$release}"

log() { printf '[ov9282] %s\n' "$*" >&2; }
die() { log "error: $*"; exit 1; }

build() {
    [[ -n "$release" && -f "$KERNEL_TREE/Module.symvers" ]] ||
        die "$KERNEL_TREE is not a built kernel tree (no kernel.release / Module.symvers)"
    command -v "${CROSS_COMPILE}gcc" >/dev/null || die "no compiler ${CROSS_COMPILE}gcc"
    local obj="$OUT/obj"
    rm -rf "$obj"
    mkdir -p "$obj"
    cp "$here/Kbuild" "$here/ov9282.c" "$obj/"
    log "building ov9282.ko for $release"
    make -C "$KERNEL_TREE" M="$obj" ARCH=arm64 CROSS_COMPILE="$CROSS_COMPILE" W=1 modules
    cp "$obj/ov9282.ko" "$OUT/"
    log "output: $OUT/ov9282.ko"
}

this_module_size() {
    "${CROSS_COMPILE}objdump" -h "$1" | awk '/gnu.linkonce.this_module/{print $3}'
}

verify() {
    local ko="$OUT/ov9282.ko"
    [[ -f "$ko" ]] || die "no $ko; run '$0 build'"
    [[ "$(modinfo -F name "$ko")" == ov9282 ]] || die "module name is not ov9282"
    log "vermagic: $(modinfo -F vermagic "$ko")"
    if [[ -n "${DEVICE:-}" ]]; then
        local dev_ko="$OUT/device-ov9282.ko"
        ssh -o BatchMode=yes "$DEVICE" "cat /lib/modules/\$(uname -r)/kernel/drivers/media/i2c/ov9282.ko*" \
            >"$dev_ko.raw"
        if xz -t "$dev_ko.raw" 2>/dev/null; then xz -dc "$dev_ko.raw" >"$dev_ko"; else mv "$dev_ko.raw" "$dev_ko"; fi
        local want got
        want="$(ssh -o BatchMode=yes "$DEVICE" 'modinfo -F vermagic ov9282')"
        [[ "$(modinfo -F vermagic "$ko" | xargs)" == "$(echo "$want" | xargs)" ]] ||
            die "vermagic mismatch (device: $want)"
        want="$(this_module_size "$dev_ko")"
        got="$(this_module_size "$ko")"
        [[ "$want" == "$got" ]] || die "struct module size $got, the device kernel's $want: wrong kernel tree"
        log "vermagic and struct module size ($got) match the device"
    fi
}

case "${1:-all}" in
build) build ;;
verify) verify ;;
diff)
    raze="$(git -C "$here" log -1 --format=%h --grep="Raze image's OV9782 patches" -- ov9282.c)"
    git -C "$here" diff "$raze" -- ov9282.c ;;
all) build; verify ;;
*) die "unknown command $1 (build|verify|diff|all)" ;;
esac
