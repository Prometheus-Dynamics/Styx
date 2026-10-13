#!/bin/sh
# SPDX-License-Identifier: GPL-2.0
#
# Swaps the ov9282 driver on a running dev box (the image's root is read-only erofs, so
# /lib/modules cannot change; an image ships the Styx module from its build instead, see
# ../buildroot). Run on the device as root, under the device lock, with the camera's users
# stopped (systemctl stop photonvision):
#
#   swap.sh styx ./ov9282.ko    the Styx module
#   swap.sh stock               the image's own module back
#
# The receiver goes first: unbinding rp1-cfe tears the whole camera media graph down, and
# binding it again builds it anew around whichever ov9282 is loaded. Re-binding the sensor
# alone into a live graph does not work with rp1-cfe-downstream (media_device_register_entity
# warnings, no sensor subdev until a reboot).

set -eu
cfe=/sys/bus/platform/drivers/rp1-cfe
dev=$(ls "$cfe" | grep -m1 '\.csi$') || { echo "no rp1-cfe device bound" >&2; exit 1; }
case "${1:-}" in
styx) ko="${2:?the Styx ov9282.ko}" ;;
stock) ko="" ;;
*) echo "usage: $0 styx ./ov9282.ko | stock" >&2; exit 2 ;;
esac
echo "$dev" >"$cfe/unbind"
sleep 1
rmmod ov9282
if [ -n "$ko" ]; then insmod "$ko"; else modprobe ov9282; fi
sleep 1
echo "$dev" >"$cfe/bind"
sleep 3
sensor=""
for d in /sys/class/video4linux/v4l-subdev*; do
    grep -q "ov9782\|ov9282" "$d/name" 2>/dev/null && sensor="/dev/$(basename "$d")"
done
echo "ov9282 srcversion $(cat /sys/module/ov9282/srcversion), sensor subdev ${sensor:-none}"
[ -n "$sensor" ] || { echo "no sensor subdev: reboot to recover" >&2; exit 1; }
