#!/usr/bin/env bash
# Styx's footprint on microcontrollers (docs/mcu.md): builds the firmware images of
# examples/mcu-footprint (configurations A-D) for Cortex-M7/M4F and Cortex-M0+ with the `mcu`
# profile and prints flash (.vector_table + .text + .rodata + .data) and static RAM (.data +
# .bss; the heap arena, in .uninit, is left out) per configuration.
#
#   scripts/mcu-size.sh                 the table
#   scripts/mcu-size.sh --check         also fail if a configuration grew past its limit in
#                                       examples/mcu-footprint/size-limits.txt
#   MCU_PROFILE=mcu-speed scripts/mcu-size.sh    opt-level "s" instead of "z"
#   MCU_TARGETS="thumbv7em-none-eabihf" ...      other targets
#   MCU_FEATURES="..." ...                       features of styx-mcu-footprint
#
# Needs the targets and llvm-tools (added when rustup is there).
set -euo pipefail

root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root_dir"

check=0
[[ "${1:-}" == "--check" ]] && check=1
profile="${MCU_PROFILE:-mcu}"
read -r -a targets <<<"${MCU_TARGETS:-thumbv7em-none-eabihf thumbv6m-none-eabi}"
features="${MCU_FEATURES:-}"
limits="examples/mcu-footprint/size-limits.txt"

host="$(rustc -vV | sed -n 's/^host: //p')"
size_tool="$(rustc --print sysroot)/lib/rustlib/$host/bin/llvm-size"
if command -v rustup >/dev/null 2>&1; then
    [[ -x "$size_tool" ]] || rustup component add llvm-tools
    installed="$(rustup target list --installed)"
    for target in "${targets[@]}"; do
        grep -qx "$target" <<<"$installed" || rustup target add "$target"
    done
fi

failed=0
printf '%-24s %-6s %10s %10s\n' target config flash static-ram
for target in "${targets[@]}"; do
    cargo build -q -p styx-mcu-footprint --profile "$profile" --target "$target" \
        ${features:+--features "$features"} --bins
    for config in a b c d; do
        elf="target/$target/$profile/footprint-$config"
        read -r flash ram < <("$size_tool" -A "$elf" | awk '
            $1 == ".vector_table" || $1 == ".text" || $1 == ".rodata" { flash += $2 }
            $1 == ".data" { flash += $2; ram += $2 }
            $1 == ".bss" { ram += $2 }
            END { print flash, ram }')
        name="$(tr '[:lower:]' '[:upper:]' <<<"$config")"
        printf '%-24s %-6s %10d %10d\n' "$target" "$name" "$flash" "$ram"
        if ((check)) && [[ "$profile" == "mcu" && -z "$features" ]]; then
            limit="$(awk -v t="$target" -v c="$name" '$1 == t && $2 == c { print $3 }' "$limits")"
            if [[ -n "$limit" ]] && ((flash > limit)); then
                echo "    $target $name: $flash bytes of flash, over its limit of $limit ($limits)" >&2
                failed=1
            fi
        fi
    done
done
exit "$failed"
