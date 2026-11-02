#!/usr/bin/env bash
# AVX-512 on a host without it: the software ISP's and styx-core's tests run under Intel SDE
# as an Ice Lake server (`-icx`: AVX-512 F, BW, VBMI), so the AVX-512 leaves are dispatched and
# checked against the scalar ones. Correctness only: times under SDE mean nothing.
#
#   SDE=/path/to/sde64 scripts/check-sde.sh [CHIP]     CHIP: an sde64 flag, default -icx
#
# Needs: Intel SDE (Intel's license; not fetched here), e.g. 10.13.1 from
# https://www.intel.com/content/www/us/en/download/684897/intel-software-development-emulator.html
set -euo pipefail
cd "$(dirname "$0")/.."

sde="${SDE:?set SDE to the sde64 binary}"
chip="${1:--icx}"
bins="$(cargo test -p styx-softisp -p styx-core-rs --tests --no-run 2>&1 | grep -o '/[^ )]*/deps/[^ )]*')"
for bin in $bins; do
    echo "==> $(basename "$bin") under SDE $chip"
    (cd crates/softisp && "$sde" "$chip" -- "$bin" --test-threads 16)
done
