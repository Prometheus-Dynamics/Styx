#!/bin/sh
# SPDX-License-Identifier: GPL-2.0
#
# Installs the bridge on a HeliOS CM5 so it comes up at every boot, with no /boot or image
# changes: files go to /usr/local/lib/styx-bridge on the persistent root overlay, and
# styx-bridge.service runs spike/up.sh at boot. helios-peripherals is disabled at boot (it is
# started again by `systemctl stop styx-bridge`, which runs down.sh).
#
#   install.sh [root@helios] [kbuild-out-dir]
#
# Run under the device lock. Rerun after rebuilding the module to update it.
# Uninstall: systemctl disable --now styx-bridge && systemctl enable --now helios-peripherals

set -eu
here="$(cd "$(dirname "$0")" && pwd)"
dev="${1:-root@helios}"
out="${2:-$here/../../../target/kbuild/out/6.12.47-v8-16k}"
dst=/usr/local/lib/styx-bridge

ssh "$dev" "mkdir -p $dst"
for f in "$out/styx_sensor_bridge.ko" "$out/styx-sensor-bridge-cm5-runtime.dtbo" \
    "$out/styx-sensor-bridge-cm5-runtime-emb.dtbo" "$here/../spike/up.sh" \
    "$here/../spike/down.sh" "$here/../spike/spike-env.sh"; do
    ssh "$dev" "cat > $dst/$(basename "$f")" <"$f"
done
ssh "$dev" "cat > /etc/systemd/system/styx-bridge.service" <"$here/styx-bridge.service"
# down.sh then up.sh, so a module that is already loaded is replaced by the installed one.
ssh "$dev" "systemctl daemon-reload && systemctl disable helios-peripherals &&
    systemctl enable styx-bridge && { systemctl stop styx-bridge; sh $dst/down.sh >/dev/null; } ;
    systemctl start styx-bridge && systemctl --no-pager status styx-bridge | head -3"
