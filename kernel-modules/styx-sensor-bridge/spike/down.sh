#!/bin/sh
# SPDX-License-Identifier: GPL-2.0
#
# Undoes up.sh, in reverse order:
#
#   1. stop a running native-spike (SIGINT, so it powers the sensor down)
#   2. unbind rp1-cfe (it holds the bridge subdev)
#   3. rmmod styx_sensor_bridge (switches the bridge's supplies and clock off)
#   4. remove the runtime overlay (ov9782@60 comes back as I2C client 10-0060)
#   5. bind ov9282 to 10-0060 (if the i2c core did not already)
#   6. bind rp1-cfe (it finds the ov9782 again)
#   7. start helios-peripherals
#
# Idempotent and best effort: every step runs even if an earlier one failed.
# If anything fails, a reboot restores everything (nothing here persists).

set -u
. "$(dirname "$0")/spike-env.sh"

failures=0
bad() {
    printf '[spike]   FAILED: %s\n' "$*"
    failures=$((failures + 1))
}

say "step 1 (stop native-spike)"
pids="$(pidof native-spike 2>/dev/null || true)"
if [ -n "$pids" ]; then
    kill -INT $pids
    no_spike() { [ -z "$(pidof native-spike 2>/dev/null)" ]; }
    wait_for 100 no_spike && ok "native-spike exited" || bad "native-spike ($pids) still running"
else
    ok "not running"
fi

say "step 2 (unbind $CFE_DRIVER)"
if bound platform "$CFE_DRIVER" "$CFE_DEV" && overlay_applied; then
    echo "$CFE_DEV" >"/sys/bus/platform/drivers/$CFE_DRIVER/unbind" || bad "unbind $CFE_DRIVER"
fi
if overlay_applied && bound platform "$CFE_DRIVER" "$CFE_DEV"; then
    bad "$CFE_DRIVER still bound"
else
    ok "$CFE_DRIVER not bound to the bridge"
fi

say "step 3 (rmmod $MODULE)"
if module_loaded; then
    rmmod "$MODULE" || bad "rmmod $MODULE (see dmesg)"
fi
module_loaded && bad "$MODULE still loaded" || ok "$MODULE not loaded"

say "step 4 (remove the runtime overlay)"
if [ -d "$OVERLAY_DIR" ]; then
    rmdir "$OVERLAY_DIR" || bad "rmdir $OVERLAY_DIR (see dmesg)"
fi
if [ -d "$OVERLAY_DIR" ] || [ -d "$BRIDGE_NODE" ]; then
    bad "overlay still present"
else
    ok "overlay removed"
fi
[ "$(dt_str "$SENSOR_NODE/status")" = "okay" ] && ok "ov9782 node enabled" || bad "ov9782 node not enabled"
[ "$(dt_hex "$CSI_EP/remote-endpoint")" = "$(dt_hex "$SENSOR_NODE/port/endpoint/phandle")" ] &&
    ok "csi0 endpoint -> ov9782 endpoint" || bad "csi0 endpoint does not point at the ov9782"

say "step 5 (bind $SENSOR_DRIVER)"
client_present() { [ -e "/sys/bus/i2c/devices/$SENSOR_CLIENT" ]; }
wait_for 20 client_present || bad "I2C client $SENSOR_CLIENT did not come back"
if client_present && ! bound i2c "$SENSOR_DRIVER" "$SENSOR_CLIENT"; then
    echo "$SENSOR_CLIENT" >"/sys/bus/i2c/drivers/$SENSOR_DRIVER/bind" || bad "bind $SENSOR_DRIVER"
fi
wait_for 20 bound i2c "$SENSOR_DRIVER" "$SENSOR_CLIENT" && ok "$SENSOR_DRIVER bound to $SENSOR_CLIENT" ||
    bad "$SENSOR_DRIVER not bound"

say "step 6 (bind $CFE_DRIVER)"
if ! bound platform "$CFE_DRIVER" "$CFE_DEV"; then
    echo "$CFE_DEV" >"/sys/bus/platform/drivers/$CFE_DRIVER/bind" || bad "bind $CFE_DRIVER"
fi
wait_for 50 bound platform "$CFE_DRIVER" "$CFE_DEV" && ok "$CFE_DRIVER bound" || bad "$CFE_DRIVER not bound"

say "step 7 (start $SERVICE)"
service_active || systemctl start "$SERVICE" || bad "systemctl start $SERVICE"
wait_for 50 service_active && ok "$SERVICE active" || bad "$SERVICE not active"

if [ "$failures" -gt 0 ]; then
    say "down: $failures step(s) failed. A reboot restores everything (nothing here persists)."
    exit 1
fi
say "down: done, the camera is back with ov9282 and $SERVICE"
