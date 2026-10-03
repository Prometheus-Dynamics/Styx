#!/bin/sh
# SPDX-License-Identifier: GPL-2.0
#
# Switches the HeliOS CM5 dev box between the two cam0 configurations, by the overlay line in
# config.txt (boot partition /dev/mmcblk0p1), then reboots:
#
#   native     dtoverlay=styx-sensor-bridge-cm5,cam0,clk-continuous: the OV9782 behind the Styx
#              sensor bridge (bound at boot, styx_sensor_bridge.ko from
#              /lib/modules/$(uname -r)/updates), helios-peripherals disabled. The shipping
#              configuration and the dev-box baseline.
#   libcamera  dtoverlay=ov9782-overlay,cam0,clk-continuous: the image's own configuration
#              (ov9282 driver, libcamera), helios-peripherals enabled.
#   status     prints the configured and the running mode (nothing changed)
#
#   camera-mode.sh native|libcamera|status [--no-reboot]
#
# Only that one line of config.txt is changed; the first switch keeps the original as
# config.txt.pre-styx-bridge-boot. Run under the device lock.

set -eu

NATIVE_LINE="dtoverlay=styx-sensor-bridge-cm5,cam0,clk-continuous"
LIBCAMERA_LINE="dtoverlay=ov9782-overlay,cam0,clk-continuous"
BOOT_DEV="${BOOT_DEV:-/dev/mmcblk0p1}"
SERVICE="helios-peripherals"

say() { printf '[camera-mode] %s\n' "$*"; }
die() {
    say "error: $*"
    exit 1
}

mode="${1:-status}"
reboot=1
[ "${2:-}" = "--no-reboot" ] && reboot=0

running_mode() {
    if [ -d /sys/bus/platform/drivers/styx-sensor-bridge/styx-sensor-bridge-cam ]; then
        echo "native (bridge bound at boot)"
    elif [ -d /sys/bus/platform/drivers/styx-sensor-bridge/styx-sensor-bridge-cam0 ]; then
        echo "native (runtime overlay, styx-bridge.service)"
    elif [ -e /sys/bus/i2c/drivers/ov9282/10-0060 ]; then
        echo "libcamera (ov9282 bound)"
    else
        echo "unknown (neither the bridge nor ov9282 is bound)"
    fi
}

mnt="$(mktemp -d /tmp/camera-mode-boot.XXXXXX)"
cleanup() {
    mountpoint -q "$mnt" && umount "$mnt"
    rmdir "$mnt"
}
trap cleanup EXIT

case "$mode" in
status)
    mount -o ro "$BOOT_DEV" "$mnt"
    if grep -qx "$NATIVE_LINE" "$mnt/config.txt"; then
        say "configured: native"
    elif grep -qx "$LIBCAMERA_LINE" "$mnt/config.txt"; then
        say "configured: libcamera"
    else
        say "configured: unknown (no cam0 line of either kind in config.txt)"
    fi
    say "running: $(running_mode); $SERVICE $(systemctl is-enabled "$SERVICE" 2>/dev/null || true)"
    exit 0
    ;;
native)
    from="$LIBCAMERA_LINE"
    to="$NATIVE_LINE"
    ;;
libcamera)
    from="$NATIVE_LINE"
    to="$LIBCAMERA_LINE"
    ;;
*) die "usage: $0 native|libcamera|status [--no-reboot]" ;;
esac

if [ "$mode" = native ]; then
    [ -f "/lib/modules/$(uname -r)/updates/styx_sensor_bridge.ko" ] ||
        die "styx_sensor_bridge.ko is not installed in /lib/modules/$(uname -r)/updates (run install.sh)"
fi

mount -o rw "$BOOT_DEV" "$mnt"
cfg="$mnt/config.txt"
if [ "$mode" = native ]; then
    [ -f "$mnt/overlays/styx-sensor-bridge-cm5.dtbo" ] ||
        die "no overlays/styx-sensor-bridge-cm5.dtbo on the boot partition (run install.sh)"
fi
if grep -qx "$to" "$cfg"; then
    say "config.txt already selects $mode"
else
    grep -qx "$from" "$cfg" || die "config.txt has no '$from' line to replace; not changed"
    [ -f "$mnt/config.txt.pre-styx-bridge-boot" ] || cp "$cfg" "$mnt/config.txt.pre-styx-bridge-boot"
    sed "s/^$from\$/$to/" "$cfg" >"$cfg.new"
    grep -qx "$to" "$cfg.new" || die "rewrite failed; config.txt not changed"
    mv "$cfg.new" "$cfg"
    say "config.txt: '$from' -> '$to'"
fi
sync

# The runtime bridge (styx-bridge.service) needs the ov9782 boot overlay; it is never enabled
# at boot here.
systemctl disable styx-bridge.service >/dev/null 2>&1 || true
if [ "$mode" = native ]; then
    systemctl disable "$SERVICE" >/dev/null 2>&1 || true
else
    systemctl enable "$SERVICE" >/dev/null 2>&1 || true
fi
say "$SERVICE: $(systemctl is-enabled "$SERVICE" 2>/dev/null || true) at boot"

cleanup
trap - EXIT
if [ "$reboot" = 1 ]; then
    say "rebooting into $mode"
    systemctl reboot
else
    say "takes effect at the next boot"
fi
