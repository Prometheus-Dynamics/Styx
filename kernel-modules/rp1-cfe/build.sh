#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0
#
# Builds Styx's rp1-cfe receiver driver (downstream) (rp1-cfe-downstream.ko, module rp1_cfe_downstream)
# against the kernel build tree of the
# image it is for: the image's own Buildroot linux-custom (its .config, Module.symvers and
# toolchain), never a separately configured tree. A different config can keep vermagic and
# still change struct module (tracing on or off), which the kernel then refuses to load.
#
#   ./build.sh build    rp1-cfe-downstream.ko into $OUT (default <scratch>/styx-rp1-cfe/<release>)
#   ./build.sh verify   vermagic, and with DEVICE=root@host the .gnu.linkonce.this_module
#                       size against the device's own rp1-cfe-downstream.ko (read-only over ssh)
#   ./build.sh diff     the Styx patch against the Raze image's driver (git history has the
#                       stock import and the Raze patches as their own commits)
#   ./build.sh all      build + verify
#
# KERNEL_TREE  required: the device image's Buildroot linux-custom (a built tree);
#              BR_HOST its host dir (default: KERNEL_TREE/../../host).
# OUT          output directory (outside the repository: build output never goes to its disk).

set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
KERNEL_TREE="${KERNEL_TREE:?set KERNEL_TREE to the built linux-custom tree of the device image}"
BR_HOST="${BR_HOST:-$KERNEL_TREE/../../host}"
CROSS_COMPILE="${CROSS_COMPILE:-$BR_HOST/bin/aarch64-linux-}"
release="$(cat "$KERNEL_TREE/include/config/kernel.release" 2>/dev/null || true)"
OUT="${OUT:-${TMPDIR:-/tmp}/styx-rp1-cfe/$release}"

log() { printf '[rp1-cfe] %s\n' "$*" >&2; }
die() { log "error: $*"; exit 1; }

build() {
    [[ -n "$release" && -f "$KERNEL_TREE/Module.symvers" ]] ||
        die "$KERNEL_TREE is not a built kernel tree (no kernel.release / Module.symvers)"
    command -v "${CROSS_COMPILE}gcc" >/dev/null || die "no compiler ${CROSS_COMPILE}gcc"
    local obj="$OUT/obj"
    rm -rf "$obj"
    mkdir -p "$obj"
    cp "$here"/Kbuild "$here"/*.c "$here"/*.h "$obj/"
    log "building rp1-cfe-downstream.ko for $release"
    make -C "$KERNEL_TREE" M="$obj" ARCH=arm64 CROSS_COMPILE="$CROSS_COMPILE" W=1 modules
    cp "$obj/rp1-cfe-downstream.ko" "$OUT/"
    log "output: $OUT/rp1-cfe-downstream.ko"
}

this_module_size() {
    "${CROSS_COMPILE}objdump" -h "$1" | awk '/gnu.linkonce.this_module/{print $3}'
}

verify() {
    local ko="$OUT/rp1-cfe-downstream.ko"
    [[ -f "$ko" ]] || die "no $ko; run '$0 build'"
    [[ "$(modinfo -F name "$ko")" == rp1_cfe_downstream ]] || die "module name is not rp1_cfe_downstream"
    log "vermagic: $(modinfo -F vermagic "$ko")"
    if [[ -n "${DEVICE:-}" ]]; then
        local dev_ko="$OUT/device-rp1-cfe-downstream.ko"
        ssh -o BatchMode=yes "$DEVICE" "cat /lib/modules/\$(uname -r)/kernel/drivers/media/platform/raspberrypi/rp1_cfe/rp1-cfe-downstream.ko*" \
            >"$dev_ko.raw"
        if xz -t "$dev_ko.raw" 2>/dev/null; then xz -dc "$dev_ko.raw" >"$dev_ko"; else mv "$dev_ko.raw" "$dev_ko"; fi
        local want got
        want="$(ssh -o BatchMode=yes "$DEVICE" 'modinfo -F vermagic rp1_cfe_downstream')"
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
    base="$(git -C "$here" log -1 --format=%h --grep="import the downstream rp1_cfe driver" -- .)"
    git -C "$here" diff "$base" -- '*.c' '*.h' ;;
all) build; verify ;;
*) die "unknown command $1 (build|verify|diff|all)" ;;
esac
