#!/bin/sh
# Runs the comparison set on the HeliOS CM5. Run as root from a /tmp directory holding:
#   styx-compare (built with --features libcamera,native), ov9782.toml,
#   up.sh down.sh spike-env.sh styx_sensor_bridge.ko styx-sensor-bridge-cm5-runtime.dtbo
# while holding the device lock (docs/native-stack/README.md), with the kernel log streamed
# to the host.
#
#   1. stop helios-peripherals (it owns the camera)
#   2. libcamera: processed NV12, then raw BYR2
#   3. up.sh (sensor bridge), native raw pBAA, down.sh (restores the device)
#   4. start helios-peripherals, merge the results into compare.json and compare.md
#
# Settings: FPS (30), FRAMES (300), REPEAT (3), NATIVE (1; 0 skips the bridge), OUT
# (./results).

set -u
D="$(cd "$(dirname "$0")" && pwd)"
cd "$D" || exit 1
FPS="${FPS:-30}"
FRAMES="${FRAMES:-300}"
REPEAT="${REPEAT:-3}"
OUT="${OUT:-$D/results}"
mkdir -p "$OUT"
results=""

run() {
    label="$1"
    shift
    ./styx-compare run --label "$label" --fps "$FPS" --frames "$FRAMES" --repeat "$REPEAT" \
        --out "$OUT/$label.json" --save "$OUT/$label.frame" "$@" 2>"$OUT/$label.log"
    status=$?
    echo "[compare] $label: exit $status"
    if [ "$status" = 0 ]; then
        results="$results $OUT/$label.json"
    else
        tail -5 "$OUT/$label.log"
    fi
}

systemctl stop helios-peripherals
run libcamera-nv12 --backend libcamera --format NV12
run libcamera-raw --backend libcamera --format BYR2
if [ "${NATIVE:-1}" = 1 ]; then
    if sh up.sh >"$OUT/up.log" 2>&1; then
        STYX_SENSOR_PATH="$D/ov9782.toml" run native-raw --backend native --format pBAA
    else
        echo "[compare] up.sh failed:"
        tail -5 "$OUT/up.log"
    fi
    sh down.sh >"$OUT/down.log" 2>&1 || {
        echo "[compare] down.sh failed:"
        tail -5 "$OUT/down.log"
    }
fi
systemctl start helios-peripherals
echo "[compare] helios-peripherals $(systemctl is-active helios-peripherals)"
# shellcheck disable=SC2086 # one path per word
[ -n "$results" ] && ./styx-compare report --json "$OUT/compare.json" --md "$OUT/compare.md" $results
[ -f "$OUT/compare.md" ] && cat "$OUT/compare.md"
