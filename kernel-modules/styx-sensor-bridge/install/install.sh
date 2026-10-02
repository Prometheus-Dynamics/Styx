#!/bin/sh
# SPDX-License-Identifier: GPL-2.0
#
# Installs the bridge on a HeliOS CM5 dev box the way an image would boot it: the boot overlay
# in config.txt in place of the HeliOS ov9782 overlay, the module loaded at boot.
#
#   - styx_sensor_bridge.ko -> /lib/modules/<release>/updates/ + depmod (on the persistent root
#     overlay's upper layer; the read-only lower root is not touched). udev loads it at boot by
#     its compatible (styx,sensor-bridge), as Buildroot's install of the module would.
#   - styx-sensor-bridge-cm5.dtbo -> the boot partition's overlays/
#   - camera-mode.sh, the runtime overlays, up.sh/down.sh and styx-bridge.service (disabled) ->
#     /usr/local/lib/styx-bridge, for switching back to libcamera and for runtime tests
#   - camera-mode.sh native (config.txt line swapped, helios-peripherals disabled); add --reboot
#     to reboot into it
#
#   install.sh [--reboot] [root@helios] [kbuild-out-dir]
#
# Run under the device lock. Rerun after rebuilding the module to update it (then reboot, or
# unbind rp1-cfe, rmmod/modprobe, rebind). Back to the image's camera (ov9282, libcamera):
# `sh /usr/local/lib/styx-bridge/camera-mode.sh libcamera` (reboots).

set -eu
here="$(cd "$(dirname "$0")" && pwd)"
reboot=--no-reboot
if [ "${1:-}" = "--reboot" ]; then
    reboot=""
    shift
fi
dev="${1:-root@helios}"
out="${2:-$here/../../../target/kbuild/out/6.12.47-v8-16k}"
dst=/usr/local/lib/styx-bridge

ssh "$dev" "mkdir -p $dst"
for f in "$out/styx_sensor_bridge.ko" "$out/styx-sensor-bridge-cm5.dtbo" \
    "$out/styx-sensor-bridge-cm5-runtime.dtbo" "$out/styx-sensor-bridge-cm5-runtime-emb.dtbo" \
    "$here/camera-mode.sh" "$here/../spike/up.sh" "$here/../spike/down.sh" \
    "$here/../spike/spike-env.sh"; do
    ssh "$dev" "cat > $dst/$(basename "$f")" <"$f"
done
ssh "$dev" "cat > /etc/systemd/system/styx-bridge.service" <"$here/styx-bridge.service"
# shellcheck disable=SC2029 # expanded on the device on purpose
ssh "$dev" "set -e
    r=\$(uname -r)
    mkdir -p /lib/modules/\$r/updates
    cp $dst/styx_sensor_bridge.ko /lib/modules/\$r/updates/
    depmod -a \$r
    test \"\$(modinfo -n styx_sensor_bridge)\" = /lib/modules/\$r/updates/styx_sensor_bridge.ko
    mnt=\$(mktemp -d /tmp/styx-install-boot.XXXXXX)
    mount -o rw /dev/mmcblk0p1 \$mnt
    cp $dst/styx-sensor-bridge-cm5.dtbo \$mnt/overlays/
    sync
    umount \$mnt
    rmdir \$mnt
    systemctl daemon-reload
    sh $dst/camera-mode.sh native $reboot"
