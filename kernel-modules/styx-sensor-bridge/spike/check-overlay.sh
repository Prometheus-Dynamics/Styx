#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0
#
# Offline check of the runtime bridge overlay (host only, changes nothing).
#
#   check-overlay.sh BASE DTBO     apply DTBO to BASE with fdtoverlay and check the result
#   check-overlay.sh --merged DTB  check an already merged tree
#
# BASE is a .dtb or a copy of a device's /proc/device-tree directory, e.g.
#   ssh root@helios 'cd /proc/device-tree && tar cf - .' | tar xf - -C live-fs
#
# Checks: the bridge node exists and is enabled; the csi0 receiver endpoint
# points at the bridge endpoint (and not back, see the overlay); the ov9782 I2C
# client is disabled; the bridge carries the sensor node's clock, supplies,
# lanes and link frequencies; styx,i2c-bus is the sensor's adapter; both ends
# agree on the clock mode. Exits 1 on any failure.
#
# Tools: DTC, FDTOVERLAY, FDTGET (default: the HeliOS Buildroot host tools
# next to this repository, else PATH).

set -uo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
workspace="$(cd "$here/../../../.." && pwd)"
host_bin="$workspace/HeliOS-architecture-overhaul/gaia/build/helios-full-cm5/image/buildroot-output/host/bin"
tool() {
    if [[ -x "$host_bin/$1" ]]; then echo "$host_bin/$1"; else echo "$1"; fi
}
DTC="${DTC:-$(tool dtc)}"
FDTOVERLAY="${FDTOVERLAY:-$(tool fdtoverlay)}"
FDTGET="${FDTGET:-$(tool fdtget)}"

BRIDGE="${BRIDGE_PATH:-/styx-sensor-bridge-cam0}"
SENSOR_I2C_LABEL="${SENSOR_I2C_LABEL:-i2c_csi_dsi0}"
SENSOR_NODE_NAME="${SENSOR_NODE_NAME:-ov9782@60}"
CSI_LABEL="${CSI_LABEL:-csi0}"

usage() { sed -n '4,8p' "$0" >&2; exit 2; }

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

to_dtb() {
    if [[ -d "$1" ]]; then
        "$DTC" -q -I fs -O dtb -o "$2" "$1"
    else
        cp "$1" "$2"
    fi
}

case "${1:-}" in
--merged)
    [[ $# -eq 2 ]] || usage
    to_dtb "$2" "$tmp/merged.dtb" || { echo "cannot read $2" >&2; exit 2; }
    ;;
"" | -h | --help) usage ;;
*)
    [[ $# -eq 2 ]] || usage
    to_dtb "$1" "$tmp/base.dtb" || { echo "cannot read $1" >&2; exit 2; }
    if ! "$FDTOVERLAY" -i "$tmp/base.dtb" -o "$tmp/merged.dtb" "$2"; then
        echo "FAIL fdtoverlay could not apply $2" >&2
        exit 1
    fi
    ;;
esac
dtb="$tmp/merged.dtb"

failures=0
pass() { echo "PASS $*"; }
fail() { echo "FAIL $*"; failures=$((failures + 1)); }

# get TYPE PATH PROP -> value (empty when missing)
get() { "$FDTGET" -t "$1" "$dtb" "$2" "$3" 2>/dev/null; }
phandle() { get x "$1" phandle; }
same() { # name expected actual
    if [[ -n "$2" && "$2" == "$3" ]]; then pass "$1 ($3)"; else fail "$1: want '$2', got '$3'"; fi
}

i2c_path="$(get s /__symbols__ "$SENSOR_I2C_LABEL")"
csi_path="$(get s /__symbols__ "$CSI_LABEL")"
[[ -n "$i2c_path" ]] || fail "no __symbols__/$SENSOR_I2C_LABEL"
[[ -n "$csi_path" ]] || fail "no __symbols__/$CSI_LABEL"
sensor="$i2c_path/$SENSOR_NODE_NAME"
csi_ep="$csi_path/port/endpoint"
bridge_ep="$BRIDGE/port/endpoint"

same "bridge compatible" "styx,sensor-bridge" "$(get s "$BRIDGE" compatible)"
same "bridge status" "okay" "$(get s "$BRIDGE" status)"
same "sensor I2C client status" "disabled" "$(get s "$sensor" status)"
same "receiver status" "okay" "$(get s "$csi_path" status)"
same "receiver endpoint -> bridge endpoint" "$(phandle "$bridge_ep")" "$(get x "$csi_ep" remote-endpoint)"
# No back-reference: fw_devlink would defer the bridge's probe until rp1-cfe binds.
if "$FDTGET" "$dtb" "$bridge_ep" remote-endpoint >/dev/null 2>&1; then
    fail "bridge endpoint has a remote-endpoint (fw_devlink would tie its probe to rp1-cfe)"
else
    pass "bridge endpoint has no remote-endpoint"
fi
same "bridge styx,i2c-bus = sensor adapter" "$(phandle "$i2c_path")" "$(get x "$BRIDGE" styx,i2c-bus)"
same "bridge I2C address = sensor reg" "$(get x "$sensor" reg)" "$(get x "$BRIDGE" styx,i2c-address)"
for prop in clocks avdd-supply dovdd-supply dvdd-supply; do
    same "bridge $prop = sensor $prop" "$(get x "$sensor" "$prop")" "$(get x "$BRIDGE" "$prop")"
done
for prop in data-lanes clock-lanes link-frequencies; do
    same "bridge endpoint $prop = sensor endpoint $prop" \
        "$(get x "$sensor/port/endpoint" "$prop")" "$(get x "$bridge_ep" "$prop")"
done
same "receiver data-lanes = bridge data-lanes" \
    "$(get x "$bridge_ep" data-lanes)" "$(get x "$csi_ep" data-lanes)"
# Clock mode must agree on both ends (the sensor is set up for a continuous clock).
bridge_nc="$(get x "$bridge_ep" clock-noncontinuous >/dev/null && echo yes || echo no)"
csi_nc="$(get x "$csi_ep" clock-noncontinuous >/dev/null && echo yes || echo no)"
same "clock-noncontinuous on both ends" "$csi_nc" "$bridge_nc"

if ((failures)); then
    echo "overlay check: $failures failure(s)"
    exit 1
fi
echo "overlay check: all passed"
