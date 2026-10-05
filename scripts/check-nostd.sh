#!/usr/bin/env bash
# The platform-neutral crates without std (docs/portability.md): each built and linted with
# --no-default-features for a Cortex-M4F/M7, a Cortex-M33, a RISC-V microcontroller and
# WebAssembly (styx-hal also without alloc: it has no allocator dependency at all), the no_std
# smoke crate (examples/nostd-smoke) built for the same targets and its logic run as a host
# test with every dependency built without std; the firmware-like camera (examples/nostd-camera:
# styx-runtime's Camera on a mock platform with the pipeline core's software ISP loop, 3A,
# stills and metrics, raw frames handed out as styx-core FrameLeases through a styx-core queue)
# built for the same targets, run as a host test without std and with it, and the two runs'
# traces compared bit for bit. styx-core's frame path (FrameLease, pools, queues, transforms,
# metrics) is also built with spin locks and with critical-section locks, and for two targets
# without compare-and-swap (Cortex-M0, RISC-V without atomics) with `critical-section`. Last,
# the firmware images of examples/mcu-footprint (docs/mcu.md: configurations A-D, the whole
# stack, for a Cortex-M7/M4F and a Cortex-M0+) are linked and linted, their flash checked
# against examples/mcu-footprint/size-limits.txt (scripts/mcu-size.sh --check), and their
# logic run on the host for the heap high-water marks.
#
# Needs the targets: rustup target add thumbv7em-none-eabihf thumbv8m.main-none-eabihf
# riscv32imac-unknown-none-elf wasm32-unknown-unknown thumbv6m-none-eabi
# riscv32imc-unknown-none-elf (the script adds them when rustup is there).
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
    "styx-algo:all-algorithms,toml"
    "styx-sensor:"
    "styx-sensor:postcard"
    "styx-sensor:postcard,toml,short-history"
    "styx-core-rs:"
    "styx-core-rs:neon,x86"
    "styx-core-rs:neon,x86,critical-section"
    "styx-softisp:neon,x86"
    "styx-softisp:neon,x86,fp16,poly-tone"
)

# Targets without compare-and-swap: styx-core only, through critical-section.
nocas_targets=(thumbv6m-none-eabi riscv32imc-unknown-none-elf)

if command -v rustup >/dev/null 2>&1; then
    installed="$(rustup target list --installed)"
    for target in "${targets[@]}" "${nocas_targets[@]}"; do
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

for target in "${nocas_targets[@]}"; do
    echo "==> styx-core-rs without std or compare-and-swap for $target (critical-section)"
    cargo clippy -q -p styx-core-rs --no-default-features --features neon,x86,critical-section \
        --target "$target" -- -D warnings
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

echo "==> styx-core-rs tests without std (the frame path on spin locks, then critical sections)"
cargo test -q -p styx-core-rs --no-default-features --features neon,x86 --lib
cargo test -q -p styx-core-rs --no-default-features --features critical-section --lib

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

echo "==> mcu-footprint firmware images (thumbv7em, thumbv6m): lint, link, size limits"
for target in thumbv7em-none-eabihf thumbv6m-none-eabi; do
    cargo clippy -q -p styx-mcu-footprint --profile mcu --target "$target" --lib --bins \
        -- -D warnings
done
./scripts/mcu-size.sh --check

echo "==> mcu-footprint heap high-water marks on the host"
cargo test -q -p styx-mcu-footprint --release --test heap
