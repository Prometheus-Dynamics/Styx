# SPDX-License-Identifier: GPL-2.0
# Shared settings and helpers for up.sh and down.sh (sourced; POSIX sh, BusyBox).
#
# Everything here is specific to the HeliOS CM5 image with the OV9782 on cam0
# (kernel 6.12.47-v8-16k). Nothing touches /boot, the A/B images or the updater.

SPIKE_DIR="${SPIKE_DIR:-$(cd "$(dirname "$0")" && pwd)}"

# The kernel the module was built for (KERNEL_RELEASE=7.2.9-v8-16k for an rpi-7.2.y image).
KERNEL_RELEASE="${KERNEL_RELEASE:-6.12.47-v8-16k}"
SERVICE="helios-peripherals"

MODULE="styx_sensor_bridge"
MODULE_KO="$SPIKE_DIR/styx_sensor_bridge.ko"
BRIDGE_DRIVER="styx-sensor-bridge"
BRIDGE_DEV="styx-sensor-bridge-cam0"

# OVERLAY_DTBO=.../styx-sensor-bridge-cm5-runtime-emb.dtbo selects the variant with an
# embedded data pad.
OVERLAY_DTBO="${OVERLAY_DTBO:-$SPIKE_DIR/styx-sensor-bridge-cm5-runtime.dtbo}"
OVERLAYS="/sys/kernel/config/device-tree/overlays"
OVERLAY_DIR="$OVERLAYS/styx-sensor-bridge"

SENSOR_DRIVER="ov9282"
SENSOR_CLIENT="10-0060"
CFE_DRIVER="rp1-cfe"
CFE_DEV="1f00110000.csi"

DT="/proc/device-tree"
SENSOR_NODE="$DT/axi/pcie@1000120000/rp1/i2c@88000/ov9782@60"
CSI_EP="$DT/axi/pcie@1000120000/rp1/csi@110000/port/endpoint"
BRIDGE_NODE="$DT/$BRIDGE_DEV"

say() { printf '[spike] %s\n' "$*"; }
ok() { printf '[spike]   ok: %s\n' "$*"; }
warn() { printf '[spike]   warning: %s\n' "$*"; }

# Property value as hex bytes ("00 00 01 0a"), empty if missing.
dt_hex() { [ -f "$1" ] && od -An -tx1 "$1" | tr -s ' \n' ' ' | sed 's/^ //; s/ $//'; }
# String property without the trailing NUL.
dt_str() { [ -f "$1" ] && tr -d '\000' <"$1"; }

service_active() { systemctl is-active --quiet "$SERVICE"; }
module_loaded() { grep -q "^$MODULE " /proc/modules; }
bound() { [ -e "/sys/bus/$1/drivers/$2/$3" ]; }
overlay_applied() { [ -d "$OVERLAY_DIR" ] && [ "$(cat "$OVERLAY_DIR/status" 2>/dev/null)" = "applied" ]; }

# Waits up to $1 tenths of a second for the command in the remaining arguments to succeed.
wait_for() {
    n="$1"
    shift
    while [ "$n" -gt 0 ]; do
        "$@" && return 0
        sleep 0.1
        n=$((n - 1))
    done
    "$@"
}

# The v4l-subdev node the bridge registered, if any.
bridge_subdev() {
    for d in /sys/class/video4linux/v4l-subdev*; do
        [ -e "$d/device/driver" ] || continue
        if [ "$(basename "$(readlink "$d/device/driver")")" = "$BRIDGE_DRIVER" ]; then
            echo "/dev/$(basename "$d")"
            return 0
        fi
    done
    return 1
}

# The cam0 nodes: every video/subdev node of rp1-cfe or a sensor on it, wherever numbered.
camera_nodes() {
    echo /dev/media0
    for d in /sys/class/video4linux/*; do
        [ -e "$d/device/driver" ] || continue
        case "$(basename "$(readlink "$d/device/driver")")" in
        "$CFE_DRIVER" | "$BRIDGE_DRIVER" | "$SENSOR_DRIVER") echo "/dev/$(basename "$d")" ;;
        esac
    done
}

# Processes holding camera device nodes open (BusyBox fuser prints PIDs).
camera_users() {
    for n in $(camera_nodes); do
        [ -e "$n" ] && fuser "$n" 2>/dev/null
    done | tr -s ' \n' '\n\n' | grep . | sort -u | tr '\n' ' ' | sed 's/ $//'
}
no_camera_users() { [ -z "$(camera_users)" ]; }
sensor_client_gone() { [ ! -e "/sys/bus/i2c/devices/$SENSOR_CLIENT" ]; }
