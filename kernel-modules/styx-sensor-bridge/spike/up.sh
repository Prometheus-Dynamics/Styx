#!/bin/sh
# SPDX-License-Identifier: GPL-2.0
#
# Puts the OV9782 on cam0 behind the Styx sensor bridge, at runtime only:
#
#   0. preflight checks (read-only)
#   1. stop helios-peripherals (it owns the cameras)
#   2. unbind ov9282 from 10-0060 and rp1-cfe from 1f00110000.csi
#   3. apply the runtime overlay through configfs
#   4. insmod styx_sensor_bridge.ko
#   5. rebind rp1-cfe (it finds the bridge as its sensor)
#
# Run on the device from the directory holding this script, spike-env.sh,
# styx_sensor_bridge.ko and styx-sensor-bridge-cm5-runtime.dtbo (e.g. /tmp/styx-spike).
# Idempotent: steps already done are skipped. Stops at the first failure and
# says how to undo. Never touches /boot, the A/B images or the updater.
# Undo with down.sh; a reboot restores everything.
#
#   up.sh --check   preflight only (read-only), then print the current state

set -u
. "$(dirname "$0")/spike-env.sh"

step=""
fail() {
    printf '[spike] FAILED at %s: %s\n' "$step" "$*"
    printf '[spike] To undo: sh %s/down.sh   (a reboot also restores everything)\n' "$SPIKE_DIR"
    exit 1
}

# 0 --------------------------------------------------------------------------
step="step 0 (preflight)"
say "$step"
[ "$(id -u)" = 0 ] || fail "must run as root"
[ "$(uname -r)" = "$KERNEL_RELEASE" ] || fail "kernel is $(uname -r), module built for $KERNEL_RELEASE"
[ -f "$MODULE_KO" ] || fail "missing $MODULE_KO"
[ -f "$OVERLAY_DTBO" ] || fail "missing $OVERLAY_DTBO"
[ -d "$OVERLAYS" ] || fail "no configfs overlay support at $OVERLAYS"
want="$(modinfo -F vermagic "$SENSOR_DRIVER" 2>/dev/null)"
have="$(modinfo -F vermagic "$MODULE_KO" 2>/dev/null)"
[ -n "$have" ] && [ "$(echo $have)" = "$(echo $want)" ] ||
    fail "vermagic '$have' does not match the running kernel's modules ('$want')"
ok "kernel $KERNEL_RELEASE, vermagic matches"
[ -d "$SENSOR_NODE" ] || fail "no sensor node $SENSOR_NODE (is ov9782-overlay on cam0 applied?)"
[ -d "$CSI_EP" ] || fail "no csi0 endpoint $CSI_EP"
if overlay_applied; then
    ok "overlay already applied"
elif [ -d "$OVERLAY_DIR" ]; then
    fail "$OVERLAY_DIR exists but is not applied (status: $(cat "$OVERLAY_DIR/status" 2>/dev/null)); run down.sh"
else
    [ "$(dt_hex "$CSI_EP/remote-endpoint")" = "$(dt_hex "$SENSOR_NODE/port/endpoint/phandle")" ] ||
        fail "csi0 endpoint does not point at the ov9782 endpoint; unexpected device tree"
    ok "csi0 endpoint -> ov9782 endpoint"
    [ -z "$(ls "$OVERLAYS")" ] || warn "other runtime overlays are applied: $(ls "$OVERLAYS")"
fi

if [ "${1:-}" = "--check" ]; then
    say "state: $SERVICE $(systemctl is-active "$SERVICE")," \
        "$SENSOR_DRIVER $(bound i2c "$SENSOR_DRIVER" "$SENSOR_CLIENT" && echo bound || echo unbound)," \
        "$CFE_DRIVER $(bound platform "$CFE_DRIVER" "$CFE_DEV" && echo bound || echo unbound)," \
        "overlay $(overlay_applied && echo applied || echo absent)," \
        "$MODULE $(module_loaded && echo loaded || echo absent)," \
        "camera users: $(camera_users)"
    say "preflight passed (nothing changed)"
    exit 0
fi

# 1 --------------------------------------------------------------------------
step="step 1 (stop $SERVICE)"
say "$step"
if service_active; then
    systemctl stop "$SERVICE" || fail "systemctl stop $SERVICE"
fi
service_active && fail "$SERVICE is still active"
ok "$SERVICE stopped"
wait_for 50 no_camera_users ||
    fail "camera nodes still open by PID(s): $(camera_users)"
ok "no process holds the camera nodes"

# 2 --------------------------------------------------------------------------
step="step 2 (unbind $SENSOR_DRIVER and $CFE_DRIVER)"
say "$step"
if bound i2c "$SENSOR_DRIVER" "$SENSOR_CLIENT"; then
    echo "$SENSOR_CLIENT" >"/sys/bus/i2c/drivers/$SENSOR_DRIVER/unbind" || fail "unbind $SENSOR_DRIVER"
fi
bound i2c "$SENSOR_DRIVER" "$SENSOR_CLIENT" && fail "$SENSOR_DRIVER still bound to $SENSOR_CLIENT"
ok "$SENSOR_DRIVER unbound from $SENSOR_CLIENT"
if overlay_applied; then
    # rp1-cfe can only have bound after the overlay (step 5), i.e. to the bridge.
    ok "overlay already applied: leaving $CFE_DRIVER as it is"
else
    if bound platform "$CFE_DRIVER" "$CFE_DEV"; then
        echo "$CFE_DEV" >"/sys/bus/platform/drivers/$CFE_DRIVER/unbind" || fail "unbind $CFE_DRIVER"
    fi
    bound platform "$CFE_DRIVER" "$CFE_DEV" && fail "$CFE_DRIVER still bound to $CFE_DEV"
    ok "$CFE_DRIVER unbound from $CFE_DEV"
fi

# 3 --------------------------------------------------------------------------
step="step 3 (apply runtime overlay)"
say "$step"
if ! overlay_applied; then
    mkdir "$OVERLAY_DIR" || fail "mkdir $OVERLAY_DIR"
    cat "$OVERLAY_DTBO" >"$OVERLAY_DIR/dtbo" || fail "writing the overlay (see dmesg)"
fi
overlay_applied || fail "overlay status '$(cat "$OVERLAY_DIR/status" 2>/dev/null)' (see dmesg)"
ok "overlay applied"
[ -d "$BRIDGE_NODE" ] || fail "no $BRIDGE_NODE in the live tree"
[ "$(dt_str "$SENSOR_NODE/status")" = "disabled" ] || fail "ov9782 node not disabled"
[ "$(dt_hex "$CSI_EP/remote-endpoint")" = "$(dt_hex "$BRIDGE_NODE/port/endpoint/phandle")" ] ||
    fail "csi0 endpoint does not point at the bridge"
ok "bridge node present, ov9782 disabled, csi0 -> bridge"
wait_for 20 sensor_client_gone ||
    fail "I2C client $SENSOR_CLIENT still exists: address 0x60 stays busy for i2c-dev"
ok "I2C client $SENSOR_CLIENT removed (0x60 free for i2c-dev)"

# 4 --------------------------------------------------------------------------
step="step 4 (insmod $MODULE)"
say "$step"
if ! module_loaded; then
    insmod "$MODULE_KO" || fail "insmod (see dmesg)"
fi
if wait_for 30 bound platform "$BRIDGE_DRIVER" "$BRIDGE_DEV"; then
    ok "bridge bound"
else
    # A deferred probe (a supplier not ready) is retried when rp1-cfe binds in step 5.
    warn "$BRIDGE_DRIVER has not bound $BRIDGE_DEV yet: $(dmesg | grep -i styx | tail -2)"
fi

# 5 --------------------------------------------------------------------------
step="step 5 (bind $CFE_DRIVER)"
say "$step"
if ! bound platform "$CFE_DRIVER" "$CFE_DEV"; then
    echo "$CFE_DEV" >"/sys/bus/platform/drivers/$CFE_DRIVER/bind" || fail "bind $CFE_DRIVER (see dmesg)"
fi
wait_for 50 bound platform "$CFE_DRIVER" "$CFE_DEV" || fail "$CFE_DRIVER not bound"
# The async notifier completes when the bridge subdev is found; the video nodes appear then.
found=""
n=50
while [ $n -gt 0 ] && [ -z "$found" ]; do
    for v in /sys/class/video4linux/video*; do
        [ "$(cat "$v/name" 2>/dev/null)" = "rp1-cfe-csi2_ch0" ] && found="/dev/$(basename "$v")"
    done
    [ -n "$found" ] || sleep 0.1
    n=$((n - 1))
done
[ -n "$found" ] || fail "rp1-cfe did not register its video nodes: $(dmesg | grep -i -E 'rp1-cfe|cfe' | tail -3)"
ok "$CFE_DRIVER bound, rp1-cfe-csi2_ch0 is $found"
wait_for 30 bound platform "$BRIDGE_DRIVER" "$BRIDGE_DEV" ||
    fail "$BRIDGE_DRIVER did not bind $BRIDGE_DEV: $(dmesg | grep -i styx | tail -3)"
subdev="$(bridge_subdev)" || fail "no bridge subdev node"
ok "bridge subdev $subdev ($(cat "/sys/class/video4linux/$(basename "$subdev")/name"))"

say "up: done. Next: ./native-spike --dry-run, then ./native-spike --description ov9782.toml"
if [ -x "$SPIKE_DIR/native-spike" ] && [ -f "$SPIKE_DIR/ov9782.toml" ]; then
    say "running the read-only dry run"
    "$SPIKE_DIR/native-spike" --dry-run --description "$SPIKE_DIR/ov9782.toml" ||
        warn "dry run reported problems (the stack is up; undo with down.sh)"
fi
