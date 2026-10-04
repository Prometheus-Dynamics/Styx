#!/usr/bin/env bash
# The platform-neutral crates without std (docs/portability.md): each built and linted with
# --no-default-features for a Cortex-M33, a RISC-V microcontroller and WebAssembly, the no_std
# smoke crate (examples/nostd-smoke) built for the same targets and its logic run as a host
# test with every dependency built without std.
#
# Needs the targets: rustup target add thumbv8m.main-none-eabihf riscv32imac-unknown-none-elf
# wasm32-unknown-unknown (the script adds them when rustup is there).
set -euo pipefail

root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root_dir"

targets=(thumbv8m.main-none-eabihf riscv32imac-unknown-none-elf wasm32-unknown-unknown)
# crate: features besides no default ones
crates=(
    "styx-dng:"
    "styx-pisp:"
    "styx-algo:"
    "styx-sensor:"
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
