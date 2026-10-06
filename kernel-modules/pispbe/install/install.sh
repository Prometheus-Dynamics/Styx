#!/bin/sh
# SPDX-License-Identifier: GPL-2.0
#
# Installs the patched PiSP back end driver on a HeliOS CM5 dev box as an override of the
# image's module: pisp-be.ko -> /lib/modules/<release>/updates/ + depmod (the persistent root
# overlay's upper layer; the read-only image is not touched). modprobe and udev then load it
# instead of kernel/drivers/media/platform/raspberrypi/pisp_be/pisp-be.ko.xz (kmod searches
# updates/ first).
#
#   install.sh [--reload|--reboot] [root@helios] [module]
#
#   KERNEL_RELEASE (default 6.12.47-v8-16k) picks the default module, e.g. 7.2.9-v8-16k.
#
#   --reload  swap the running module now (refused while the back end is in use: stop
#             libcamera users first, e.g. helios-peripherals in libcamera mode)
#   --reboot  reboot into it
#
# Run under the device lock (scripts/with-device-lock.sh). Undo: uninstall.sh.

set -eu
here="$(cd "$(dirname "$0")" && pwd)"
action=""
case "${1:-}" in
--reload | --reboot)
    action="$1"
    shift
    ;;
esac
dev="${1:-root@helios}"
ko="${2:-$here/../../../target/kernel-modules/${KERNEL_RELEASE:-6.12.47-v8-16k}/pisp-be.ko}"
[ -f "$ko" ] || { echo "no $ko; run ../build.sh first" >&2; exit 1; }

ssh "$dev" "cat > /tmp/pisp-be-new.ko" <"$ko"
# shellcheck disable=SC2029 # expanded on the device on purpose
ssh "$dev" "set -e
    r=\$(uname -r)
    test \"\$(modinfo -F vermagic /tmp/pisp-be-new.ko | xargs)\" = \"\$(modinfo -F vermagic videodev | xargs)\"
    mkdir -p /lib/modules/\$r/updates
    mv /tmp/pisp-be-new.ko /lib/modules/\$r/updates/pisp-be.ko
    depmod -a \$r
    test \"\$(modinfo -n pisp_be)\" = /lib/modules/\$r/updates/pisp-be.ko
    echo \"installed: \$(modinfo -n pisp_be)\""
case "$action" in
--reload)
    ssh "$dev" "set -e; rmmod pisp_be; modprobe pisp_be; test -e /sys/module/pisp_be/parameters/skip_unchanged_config; echo reloaded"
    ;;
--reboot)
    ssh "$dev" "reboot" || true
    ;;
esac
