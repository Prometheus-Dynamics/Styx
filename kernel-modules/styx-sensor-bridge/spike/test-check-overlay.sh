#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0
#
# Tests for check-overlay.sh and the runtime overlay, on the host.
#
#   test-check-overlay.sh [LIVE]   LIVE: optional copy of a device's /proc/device-tree (or .dtb)
#
# Builds a small base tree shaped like a HeliOS CM5 booted with the ov9782
# overlay on cam0 (same labels and paths), then checks that the runtime overlay
# passes on it, that the base alone fails, and that broken variants (receiver
# not redirected, bridge pointing back, sensor left enabled, link frequency
# mismatch) fail.

set -uo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
check="$here/check-overlay.sh"
src="$here/../dts/styx-sensor-bridge-cm5-runtime-overlay.dts"
workspace="$(cd "$here/../../../.." && pwd)"
host_bin="$workspace/HeliOS-architecture-overhaul/gaia/build/helios-full-cm5/image/buildroot-output/host/bin"
if [[ -z "${DTC:-}" ]]; then
    if [[ -x "$host_bin/dtc" ]]; then DTC="$host_bin/dtc"; else DTC=dtc; fi
fi
command -v "$DTC" >/dev/null || { echo "SKIP: no dtc"; exit 0; }

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
failures=0
expect() { # name want_rc cmd...
    local name="$1" want="$2"
    shift 2
    "$@" >"$tmp/out" 2>&1
    local rc=$?
    if [[ "$rc" == "$want" ]]; then
        echo "ok   $name"
    else
        echo "FAIL $name (exit $rc, want $want)"
        sed 's/^/     /' "$tmp/out"
        failures=$((failures + 1))
    fi
}

cat >"$tmp/base.dts" <<'EOF'
/dts-v1/;
/ {
	#address-cells = <2>;
	#size-cells = <1>;
	cam0_clk: cam0_clk { compatible = "fixed-clock"; #clock-cells = <0>; clock-frequency = <24000000>; };
	cam0_reg: cam0_reg { compatible = "regulator-fixed"; };
	cam_dummy_reg: cam_dummy_reg { compatible = "regulator-fixed"; };
	axi {
		#address-cells = <1>;
		#size-cells = <0>;
		i2c_csi_dsi0: i2c@88000 {
			reg = <0x88000>;
			#address-cells = <1>;
			#size-cells = <0>;
			status = "okay";
			ov9782@60 {
				compatible = "ovti,ov9782";
				reg = <0x60>;
				status = "okay";
				clocks = <&cam0_clk>;
				avdd-supply = <&cam0_reg>;
				dovdd-supply = <&cam_dummy_reg>;
				dvdd-supply = <&cam_dummy_reg>;
				port {
					cam_ep: endpoint {
						remote-endpoint = <&csi_ep>;
						clock-lanes = <0>;
						data-lanes = <1 2>;
						link-frequencies = /bits/ 64 <400000000>;
					};
				};
			};
		};
		csi0: csi@110000 {
			reg = <0x110000>;
			status = "okay";
			port {
				csi_ep: endpoint {
					phandle = <0x10a>;
					remote-endpoint = <&cam_ep>;
					data-lanes = <1 2>;
				};
			};
		};
	};
};
EOF

"$DTC" -@ -q -I dts -O dtb -o "$tmp/base.dtb" "$tmp/base.dts" || { echo "FAIL base tree"; exit 1; }
"$DTC" -@ -q -I dts -O dtb -o "$tmp/overlay.dtbo" "$src" || { echo "FAIL overlay compile"; exit 1; }

expect "runtime overlay passes on a HeliOS-shaped tree" 0 "$check" "$tmp/base.dtb" "$tmp/overlay.dtbo"
expect "base tree alone fails" 1 "$check" --merged "$tmp/base.dtb"

grep -v 'remote-endpoint = <&bridge_ep>;' "$src" >"$tmp/noreceiver.dts"
"$DTC" -@ -q -I dts -O dtb -o "$tmp/noreceiver.dtbo" "$tmp/noreceiver.dts"
expect "fails when the receiver still points at the sensor" 1 "$check" "$tmp/base.dtb" "$tmp/noreceiver.dtbo"

sed 's|/\* No remote-endpoint: see above. \*/|remote-endpoint = <0x10a>;|' "$src" >"$tmp/backref.dts"
"$DTC" -@ -q -I dts -O dtb -o "$tmp/backref.dtbo" "$tmp/backref.dts"
expect "fails when the bridge endpoint points back" 1 "$check" "$tmp/base.dtb" "$tmp/backref.dtbo"

sed 's/status = "disabled";/status = "okay";/' "$src" >"$tmp/enabled.dts"
"$DTC" -@ -q -I dts -O dtb -o "$tmp/enabled.dtbo" "$tmp/enabled.dts"
expect "fails when the sensor client stays enabled" 1 "$check" "$tmp/base.dtb" "$tmp/enabled.dtbo"

sed 's|link-frequencies = /bits/ 64 <400000000>;|link-frequencies = /bits/ 64 <200000000>;|' "$src" \
    >"$tmp/freq.dts"
"$DTC" -@ -q -I dts -O dtb -o "$tmp/freq.dtbo" "$tmp/freq.dts"
expect "fails on a link frequency mismatch" 1 "$check" "$tmp/base.dtb" "$tmp/freq.dtbo"

if [[ -n "${1:-}" ]]; then
    expect "runtime overlay passes on $1" 0 "$check" "$1" "$tmp/overlay.dtbo"
fi

if ((failures)); then
    echo "$failures test(s) failed"
    exit 1
fi
echo "all tests passed"
