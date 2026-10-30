#!/usr/bin/env bash
# RISC-V with vectors (the Kendryte K230D's C908 cores, RVV 1.0): the software ISP and the 3A
# algorithms built for riscv64 musl with RVV (+v, +zvfh) and their tests run under QEMU
# (rv64, VLEN 128), and the native camera stack linted for the target. Static binaries linked
# by rust-lld; `zig cc` builds the few C files of test dependencies (criterion's alloca).
#
#   scripts/check-riscv.sh
#
# Needs: rustup target riscv64gc-unknown-linux-musl, qemu-riscv64-static, zig.
set -euo pipefail
cd "$(dirname "$0")/.."

triple=riscv64gc-unknown-linux-musl
var=RISCV64GC_UNKNOWN_LINUX_MUSL
tools="$(mktemp -d)"
trap 'rm -rf "$tools"' EXIT
# cc-rs passes --target / -march / -mabi for clang or gcc; zig takes its own -target.
cat >"$tools/cc" <<'SH'
#!/bin/sh
for a in "$@"; do
    shift
    case "$a" in --target=* | -march=* | -mabi=*) ;; *) set -- "$@" "$a" ;; esac
done
exec zig cc -target riscv64-linux-musl "$@"
SH
printf '#!/bin/sh\nexec zig ar "$@"\n' >"$tools/ar"
chmod +x "$tools/cc" "$tools/ar"

export "CARGO_TARGET_${var}_LINKER=rust-lld"
export "CARGO_TARGET_${var}_RUSTFLAGS=-C target-feature=+v,+zvfh,+crt-static -C link-self-contained=yes"
export "CARGO_TARGET_${var}_RUNNER=qemu-riscv64-static -cpu rv64,v=true,vlen=128,zvfh=true"
export "CC_${triple//-/_}=$tools/cc" "AR_${triple//-/_}=$tools/ar"

cargo test -q --release --target "$triple" -p styx-softisp -p styx-algo --lib --tests
cargo clippy -q --target "$triple" -p styx --features native -- -D warnings
