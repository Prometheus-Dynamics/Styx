#!/usr/bin/env bash
# The platform-neutral crates without std (docs/portability.md): each built and linted with
# --no-default-features for a Cortex-M4F/M7, a Cortex-M33, a RISC-V microcontroller and
# WebAssembly (styx-hal also without alloc: it has no allocator dependency at all), the no_std
# smoke crate (examples/nostd-smoke) built for the same targets and its logic run as a host
# test with every dependency built without std; the firmware-like camera (examples/nostd-camera:
# styx-runtime's Camera on a mock platform with the pipeline core's software ISP loop, 3A,
# stills and metrics) built for the same targets, run as a host test without std and with it,
# and the two runs' traces compared bit for bit.
#
# Needs the targets: rustup target add thumbv7em-none-eabihf thumbv8m.main-none-eabihf
# riscv32imac-unknown-none-elf wasm32-unknown-unknown (the script adds them when rustup is
# there).
set -euo pipefail

root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root_dir"

targets=(thumbv7em-none-eabihf thumbv8m.main-none-eabihf riscv32imac-unknown-none-elf wasm32-unknown-unknown)
# crate: features besides no default ones
crates=(
    "styx-hal:"
    "styx-runtime:"
    "styx-pipeline:"
    "styx-dng:"
    "styx-pisp:"
    "styx-algo:"
    "styx-sensor:"
    "styx-sensor:postcard"
    "styx-core-rs:neon,x86"
    "styx-softisp:neon,x86"
)

if command -v rustup >/dev/null 2>&1; then
    installed="$(rustup target list --installed)"
    for target in "${targets[@]}"; do
        grep -qx "$target" <<<"$installed" || rustup target add "$target"
    done
fi

for target in "${targets[@]}"; do
    for entry in "${crates[@]}"; do
        crate="${entry%%:*}"
        features="${entry#*:}"
        echo "==> $crate without std for $target"
        cargo clippy -q -p "$crate" --no-default-features ${features:+--features "$features"} \
            --target "$target" -- -D warnings
    done
    echo "==> styx-nostd-smoke for $target"
    cargo clippy -q -p styx-nostd-smoke --target "$target" -- -D warnings
    echo "==> styx-nostd-camera for $target"
    cargo clippy -q -p styx-nostd-camera --target "$target" -- -D warnings
done

# The same crates without std on the host: the no_std code paths with SIMD (x86 / NEON leaves
# chosen from the compile-time target features).
for entry in "${crates[@]}"; do
    crate="${entry%%:*}"
    features="${entry#*:}"
    echo "==> $crate without std for the host"
    cargo clippy -q -p "$crate" --no-default-features ${features:+--features "$features"} \
        -- -D warnings
done

echo "==> styx-nostd-smoke host test (dependencies without std)"
cargo test -q -p styx-nostd-smoke

echo "==> styx-nostd-camera host run without std, then with std: traces bit for bit"
trace_dir="$(mktemp -d)"
trap 'rm -rf "$trace_dir"' EXIT
STYX_NOSTD_TRACE="$trace_dir/nostd.txt" cargo test -q -p styx-nostd-camera
STYX_NOSTD_TRACE="$trace_dir/std.txt" cargo test -q -p styx-nostd-camera --features std
if ! cmp -s "$trace_dir/nostd.txt" "$trace_dir/std.txt"; then
    diff "$trace_dir/nostd.txt" "$trace_dir/std.txt" | head -20
    echo "the no_std and std runs differ" >&2
    exit 1
fi
echo "    $(wc -l <"$trace_dir/std.txt") frames and shots identical"
