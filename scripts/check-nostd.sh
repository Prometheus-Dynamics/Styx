#!/usr/bin/env bash
# The platform-neutral crates without std (docs/portability.md): each built and linted with
# --no-default-features for a Cortex-M4F/M7, a Cortex-M33, a RISC-V microcontroller,
# WebAssembly and the host (styx-hal also without alloc: it has no allocator dependency at all),
# the no_std smoke crate (examples/nostd-smoke) built for the same targets and its logic run as a
# host test with every dependency built without std; the firmware-like camera
# (examples/nostd-camera: styx-runtime's Camera on a mock platform with the pipeline core's
# software ISP loop, 3A, stills and metrics, raw frames handed out as styx-core FrameLeases
# through a styx-core queue) built for the same targets, run as a host test without std and with
# it, and the two runs' traces compared bit for bit. styx-core's frame path (FrameLease, pools,
# queues, transforms, metrics) is also built with spin locks and with critical-section locks,
# and for two targets without compare-and-swap (Cortex-M0, RISC-V without atomics) with
# `critical-section`. Last, the firmware images of examples/mcu-footprint (docs/mcu.md:
# configurations A-D, the whole stack, for a Cortex-M7/M4F and a Cortex-M0+) are linked and
# linted, their flash checked against examples/mcu-footprint/size-limits.txt
# (scripts/mcu-size.sh --check), and their logic run on the host for the heap high-water marks.
#
#   scripts/check-nostd.sh             the checks
#   scripts/check-nostd.sh --verify    first prove that the lint groups below lint every crate
#                                      with exactly the features it gets on its own (CI)
#
# Few cargo invocations: each costs seconds of start-up on a busy machine, and one invocation
# builds its crates for all targets in parallel. The crates are linted in groups, each group in
# one invocation for every target at once (--target repeated). A group's crates share one
# feature resolution, and features are unified across the targets of one invocation, so a group
# is only valid if no package of any member's tree gets another feature set than the member
# alone gives it on one target; `--verify` checks exactly that with `cargo tree` and names the
# package and member that break it (then split the group). mcu-footprint's images are built one
# target at a time: their dependencies' features differ between the targets.
#
# Needs the targets: rustup target add thumbv7em-none-eabihf thumbv8m.main-none-eabihf
# riscv32imac-unknown-none-elf wasm32-unknown-unknown thumbv6m-none-eabi
# riscv32imc-unknown-none-elf (the script adds them when rustup is there).
set -euo pipefail

root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root_dir"

verify=0
case "${1:-}" in
--verify) verify=1 ;;
"") ;;
*)
    echo "unknown argument: $1 (--verify)" >&2
    exit 2
    ;;
esac

host="$(rustc -vV | sed -n 's/^host: //p')"
targets=(thumbv7em-none-eabihf thumbv8m.main-none-eabihf riscv32imac-unknown-none-elf wasm32-unknown-unknown)
# Targets without compare-and-swap: styx-core only, through critical-section.
nocas_targets=(thumbv6m-none-eabi riscv32imc-unknown-none-elf)

# The lint groups: "crate:features" without default features (the examples have none). Every
# crate:features pair is linted for every target above and the host.
groups=(
    "styx-core-rs: styx-sensor:postcard styx-algo: styx-dng: styx-pisp:"
    "styx-core-rs:neon,x86 styx-sensor: styx-softisp:neon,x86 styx-pipeline:"
    "styx-core-rs:neon,x86,critical-section styx-sensor:postcard,toml,short-history"
    "styx-hal: styx-algo:all-algorithms,toml styx-softisp:neon,x86,fp16,poly-tone"
    "styx-runtime:"
    "styx-nostd-smoke:"
    "styx-nostd-camera:"
)

if command -v rustup >/dev/null 2>&1; then
    installed="$(rustup target list --installed)"
    for target in "${targets[@]}" "${nocas_targets[@]}"; do
        grep -qx "$target" <<<"$installed" || rustup target add "$target"
    done
fi

# cargo arguments for a group: -p per crate, features qualified by crate.
group_args() {
    local entry crate features feature
    local -a args=(--no-default-features) qualified=() list
    for entry in $1; do
        crate="${entry%%:*}"
        features="${entry#*:}"
        args+=(-p "$crate")
        IFS=, read -r -a list <<<"$features"
        for feature in "${list[@]}"; do qualified+=("$crate/$feature"); done
    done
    ((${#qualified[@]})) && args+=(--features "$(IFS=,; echo "${qualified[*]}")")
    printf '%s\n' "${args[@]}"
}

target_args() {
    local t
    for t in "$@"; do printf -- '--target\n%s\n' "$t"; done
}

# Each package of a tree with its features, one per line.
features_of() {
    cargo tree -q -e normal,build --prefix none -f '{p} [{f}]' "$@" |
        sed 's/ (\*)$//; s/ (proc-macro)//' | LC_ALL=C sort -u
}

if ((verify)); then
    echo "==> verifying the lint groups (no package's features change by grouping)"
    failed=0
    all_targets=("${targets[@]}" "$host")
    mapfile -t targs < <(target_args "${all_targets[@]}")
    for group in "${groups[@]}"; do
        mapfile -t gargs < <(group_args "$group")
        merged="$(features_of "${targs[@]}" "${gargs[@]}")"
        for entry in $group; do
            mapfile -t eargs < <(group_args "$entry")
            for target in "${all_targets[@]}"; do
                missing="$(LC_ALL=C comm -23 <(features_of --target "$target" "${eargs[@]}") \
                    <(echo "$merged"))"
                if [[ -n "$missing" ]]; then
                    echo "    $entry for $target gets other features when linted with: $group" >&2
                    sed 's/^/        alone: /' <<<"$missing" >&2
                    failed=1
                fi
            done
        done
    done
    ((failed)) && exit 1
    echo "    ${#groups[@]} groups valid"
fi

mapfile -t targs < <(target_args "${targets[@]}" "$host")
for group in "${groups[@]}"; do
    echo "==> without std for ${targets[*]} and the host: $group"
    mapfile -t gargs < <(group_args "$group")
    cargo clippy -q "${targs[@]}" "${gargs[@]}" -- -D warnings
done

echo "==> styx-core-rs without std or compare-and-swap for ${nocas_targets[*]} (critical-section)"
mapfile -t targs < <(target_args "${nocas_targets[@]}")
cargo clippy -q -p styx-core-rs --no-default-features --features neon,x86,critical-section \
    "${targs[@]}" -- -D warnings

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
