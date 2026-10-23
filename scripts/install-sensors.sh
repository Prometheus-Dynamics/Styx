#!/usr/bin/env bash
# Installs Styx's sensor data into an image's root (docs/simulation.md#packaging):
#
#   scripts/install-sensors.sh DESTDIR [--sim [SPEC]]
#
#   $DESTDIR/usr/share/styx/sensors/<sensor>.toml         each sensor description (one per
#                                                          sensor, as Gaia's hardware check
#                                                          expects)
#   $DESTDIR/usr/share/styx/sensors/<sensor>.kernel.toml  data for sensors a kernel driver owns
#   $DESTDIR/usr/share/styx/tuning/<sensor>.json ...      the tunings (and overlays)
#   $DESTDIR/etc/styx/sim-cameras                         with --sim: the simulated cameras of a
#                                                          sim image (default SPEC: ov9782 on the
#                                                          test chart)
#
# The same data is built into Styx (an image without these files works); installed, it is what
# the image's checks see and what overrides the built-in copies.
set -euo pipefail

usage() { echo "usage: $0 DESTDIR [--sim [SPEC]]" >&2; exit 2; }
[[ $# -ge 1 ]] || usage
dest="$1"; shift
sim=""
if [[ $# -gt 0 ]]; then
    [[ "$1" == --sim ]] || usage
    sim="${2:-ov9782,scene=chart}"
fi

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
sensors="$dest/usr/share/styx/sensors"
tuning="$dest/usr/share/styx/tuning"
install -d -m 0755 "$sensors" "$tuning"
for f in "$root"/crates/sensor/sensors/*.toml; do
    install -m 0644 "$f" "$sensors/$(basename "$f")"
done
for f in "$root"/crates/sensor/sensors/kernel/*.toml; do
    install -m 0644 "$f" "$sensors/$(basename "$f" .toml).kernel.toml"
done
for f in "$root"/crates/pipeline/tuning/*; do
    install -m 0644 "$f" "$tuning/$(basename "$f")"
done
if [[ -n "$sim" ]]; then
    install -d -m 0755 "$dest/etc/styx"
    printf '# Simulated cameras (docs/simulation.md), one spec per line.\n%s\n' "$sim" \
        > "$dest/etc/styx/sim-cameras"
    chmod 0644 "$dest/etc/styx/sim-cameras"
fi
echo "installed: $(find "$sensors" "$tuning" -type f | wc -l) files under $dest/usr/share/styx${sim:+, $dest/etc/styx/sim-cameras}"
