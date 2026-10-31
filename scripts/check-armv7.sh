#!/usr/bin/env bash
# 32-bit Arm with NEON (Flame's RV1106G2, Cortex-A7): the frame path, the software ISP and the
# 3A algorithms built for armv7 musl hard-float with NEON and their tests run under QEMU, and
# a camera application's feature sets linted for the target with their C dependencies. Static
# binaries linked by rust-lld; `zig cc` builds the C files (libjpeg-turbo, lz4, aws-lc).
#
#   scripts/check-armv7.sh
#
# Needs: rustup target armv7-unknown-linux-musleabihf, qemu-arm-static, zig.
set -euo pipefail
cd "$(dirname "$0")/.."

triple=armv7-unknown-linux-musleabihf
var=ARMV7_UNKNOWN_LINUX_MUSLEABIHF
tools="$(mktemp -d)"
trap 'rm -rf "$tools"' EXIT
# cc-rs passes --target / -march / -mfpu / -mfloat-abi for clang or gcc; zig takes its own.
cat >"$tools/cc" <<'SH'
#!/bin/sh
for a in "$@"; do
    shift
    case "$a" in --target=* | -march=* | -mfpu=* | -mfloat-abi=* | -mcpu=*) ;; *) set -- "$@" "$a" ;; esac
done
exec zig cc -target arm-linux-musleabihf -mcpu=cortex_a7 "$@"
SH
sed 's/zig cc/zig c++/' "$tools/cc" >"$tools/c++"
printf '#!/bin/sh\nexec zig ar "$@"\n' >"$tools/ar"
chmod +x "$tools/cc" "$tools/c++" "$tools/ar"

export "CARGO_TARGET_${var}_LINKER=rust-lld"
export "CARGO_TARGET_${var}_RUSTFLAGS=-C target-feature=+neon,+crt-static -C link-self-contained=yes"
export "CARGO_TARGET_${var}_RUNNER=qemu-arm-static -cpu cortex-a7"
export "CC_${triple//-/_}=$tools/cc" "CXX_${triple//-/_}=$tools/c++" "AR_${triple//-/_}=$tools/ar"

cargo test -q --release --target "$triple" -p styx-core-rs -p styx-softisp -p styx-algo --lib --tests
cargo clippy -q --target "$triple" -p styx --features native -- -D warnings
cargo clippy -q --target "$triple" -p styx --features \
    facade,metrics-serde,preview,codec-turbojpeg,replay-mcap,replay-mcap-lz4,replay-softisp,replay-png \
    -- -D warnings
cargo clippy -q --target "$triple" -p styx --features \
    native,v4l2,raw-decoders,serde,hotplug,async,codec-turbojpeg -- -D warnings
