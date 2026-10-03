#!/bin/sh
# SPDX-License-Identifier: GPL-2.0
#
# Removes the override installed by install.sh: the image's own pisp_be is used again.
#
#   uninstall.sh [--reload|--reboot] [root@helios]
#
# Run under the device lock (scripts/with-device-lock.sh).

set -eu
action=""
case "${1:-}" in
--reload | --reboot)
    action="$1"
    shift
    ;;
esac
dev="${1:-root@helios}"
# shellcheck disable=SC2029 # expanded on the device on purpose
ssh "$dev" "set -e
    r=\$(uname -r)
    rm -f /lib/modules/\$r/updates/pisp-be.ko
    depmod -a \$r
    echo \"pisp_be is now \$(modinfo -n pisp_be)\""
case "$action" in
--reload)
    ssh "$dev" "set -e; rmmod pisp_be; modprobe pisp_be; echo reloaded"
    ;;
--reboot)
    ssh "$dev" "reboot" || true
    ;;
esac
