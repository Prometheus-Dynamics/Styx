#!/usr/bin/env bash
# Cross-builds Styx for 64-bit Arm Linux devices (Raspberry Pi CM5; docs/development.md
# "Building for the device"). Two kinds of build:
#
#   glibc  aarch64-unknown-linux-gnu against a Buildroot output's toolchain and sysroot (only
#          read): what links libcamera or other C libraries of the image. Give the Buildroot
#          output directory (the one holding host/ and staging/) with --sysroot or
#          STYX_AARCH64_BUILDROOT.
#   musl   aarch64-unknown-linux-musl, linked statically by rust-lld: no toolchain or sysroot,
#          runs on any aarch64 Linux (and under qemu-aarch64), for what needs no C library
#          (the native stack, V4L2, the camera service client, most tests).
#
#   scripts/cross-aarch64.sh [--sysroot DIR | --musl] [-p PKG]... [--features F]
#                            [--bins a,b] [--examples a,b] [--profile P] [--out DIR] [--tests]
#                            [-- cargo args]
#
#   -p PKG        package to build (repeatable; default styx-record-cli)
#   --features F  cargo features (e.g. styx-record-cli/libcamera)
#   --bins a,b    only these binaries (default: every binary of the packages)
#   --examples    these examples of the packages
#   --profile P   cargo profile: `device` (default; release code, built in a fraction of the
#                 time, for trying things on the device) or `release` (what ships)
#   --out DIR     copy the binaries there, stripped, with a SHA256SUMS file
#   --tests       build the packages' test binaries instead (dev profile unless --profile) as a
#                 bundle for scripts/device-tests.sh, in target/device-tests/<build name>/; the
#                 sources are mirrored to STYX_DEVTEST_SRC (default /tmp/styx-devtest-src) and
#                 built from there, so the paths tests read their fixtures from
#                 (env!("CARGO_MANIFEST_DIR")) exist on the device after the run script copies
#                 the same mirror to the same place
#
# Each sysroot gets its own target directory (target/aarch64-<name>-<hash of its path>,
# target/aarch64-musl; likewise under CARGO_BUILD_BUILD_DIR): the C parts (bindgen's libcamera
# bindings, cc builds) depend on the sysroot, which cargo does not track. CARGO_TARGET_DIR moves
# the target/ they are made in.
set -euo pipefail

root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

buildroot="${STYX_AARCH64_BUILDROOT:-}"
musl=0
tests=0
profile=""
features=""
out=""
declare -a packages=() selectors=() extra=()
while (($#)); do
    case "$1" in
    --sysroot) buildroot="$2"; shift ;;
    --musl) musl=1 ;;
    -p | --package) packages+=("$2"); shift ;;
    --features) features="$2"; shift ;;
    --bins) IFS=, read -r -a list <<<"$2"; for b in "${list[@]}"; do selectors+=(--bin "$b"); done; shift ;;
    --examples) IFS=, read -r -a list <<<"$2"; for b in "${list[@]}"; do selectors+=(--example "$b"); done; shift ;;
    --profile) profile="$2"; shift ;;
    --out) out="$2"; shift ;;
    --tests) tests=1 ;;
    --) shift; extra=("$@"); break ;;
    -h | --help) sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $1 (--help)" >&2; exit 2 ;;
    esac
    shift
done
((${#packages[@]})) || packages=(styx-record-cli)
if ((tests)); then profile="${profile:-dev}"; else profile="${profile:-device}"; fi

target_root="${CARGO_TARGET_DIR:-$root_dir/target}"
host="$(rustc -vV | sed -n 's/^host: //p')"
llvm_bin="$(rustc --print sysroot)/lib/rustlib/$host/bin"

if ((musl)); then
    triple=aarch64-unknown-linux-musl
    name=musl
    # Static, self-contained: rustup's musl target brings the C runtime and libc.a.
    export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld
    # C code without libc headers (criterion's alloca, a test dependency): the host's clang,
    # freestanding. C code that needs a libc needs the glibc build.
    if command -v clang >/dev/null 2>&1; then
        export CC_aarch64_unknown_linux_musl=clang
        export CFLAGS_aarch64_unknown_linux_musl="--target=aarch64-linux-musl -ffreestanding -nostdlibinc"
        export AR_aarch64_unknown_linux_musl="$llvm_bin/llvm-ar"
    fi
    strip_tool="$llvm_bin/llvm-strip"
    [[ -x "$strip_tool" ]] || strip_tool=""
else
    triple=aarch64-unknown-linux-gnu
    if [[ -z "$buildroot" ]]; then
        echo "cross-aarch64.sh: give the Buildroot output with --sysroot DIR (or" \
            "STYX_AARCH64_BUILDROOT), or build with --musl" >&2
        exit 2
    fi
    buildroot="$(cd "$buildroot" && pwd)"
    hostbin="$buildroot/host/bin"
    cc="$hostbin/aarch64-linux-gcc"
    cxx="$hostbin/aarch64-linux-g++"
    [[ -x "$cc" ]] || { echo "cross-aarch64.sh: no $cc (not a Buildroot output?)" >&2; exit 2; }
    # `staging` is Buildroot's link to the toolchain sysroot.
    if [[ -e "$buildroot/staging" ]]; then
        sysroot="$(readlink -f "$buildroot/staging")"
    else
        sysroot="$("$cc" -print-sysroot)"
    fi
    name="$(basename "$buildroot")-$(printf '%s' "$sysroot" | sha256sum | cut -c1-8)"
    export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER="$cc"
    export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C link-arg=--sysroot=$sysroot"
    export CC_aarch64_unknown_linux_gnu="$cc" CXX_aarch64_unknown_linux_gnu="$cxx"
    export AR_aarch64_unknown_linux_gnu="$hostbin/aarch64-linux-gcc-ar"
    export CFLAGS_aarch64_unknown_linux_gnu="--sysroot=$sysroot"
    export CXXFLAGS_aarch64_unknown_linux_gnu="--sysroot=$sysroot"
    export PKG_CONFIG_ALLOW_CROSS=1 PKG_CONFIG_SYSROOT_DIR="$sysroot"
    export PKG_CONFIG_LIBDIR="$sysroot/usr/lib/pkgconfig:$sysroot/usr/share/pkgconfig"
    unset PKG_CONFIG_PATH
    # bindgen (libcamera-sys) runs the host's clang over the image's C++ headers: the cross
    # compiler's own include search path (libstdc++ and GCC's headers) and the sysroot.
    isystem=""
    while read -r dir; do
        isystem+=" -isystem $dir"
    done < <("$cxx" -xc++ -E -v /dev/null 2>&1 |
        sed -n '/#include <...> search starts here:/,/End of search list./{/^ /p}' |
        grep -v -e "^ $sysroot" -e include-fixed | sed 's/^ //')
    export BINDGEN_EXTRA_CLANG_ARGS_aarch64_unknown_linux_gnu="--target=aarch64-linux-gnu --sysroot=$sysroot$isystem"
    strip_tool="$hostbin/aarch64-linux-strip"
fi
# --target-dir rather than CARGO_TARGET_DIR: sccache counts CARGO_* variables in its key, so the
# variable would keep it from sharing dependencies between these builds.
target_dir="$target_root/aarch64-$name"
# The same split for cargo's intermediate files when they live elsewhere (scripts/dev-env.sh).
[[ -n "${CARGO_BUILD_BUILD_DIR:-}" ]] && export CARGO_BUILD_BUILD_DIR="$CARGO_BUILD_BUILD_DIR/aarch64-$name"

if command -v rustup >/dev/null 2>&1; then
    rustup target list --installed | grep -qx "$triple" || rustup target add "$triple"
fi

declare -a pkg_args=()
for p in "${packages[@]}"; do pkg_args+=(-p "$p"); done

if ((!tests)); then
    cd "$root_dir"
    mapfile -t bins < <(cargo build --target-dir "$target_dir" --profile "$profile" \
        --target "$triple" "${pkg_args[@]}" \
        ${features:+--features "$features"} "${selectors[@]}" "${extra[@]}" \
        --message-format=json-render-diagnostics |
        jq -r 'select(.reason == "compiler-artifact" and .executable != null) | .executable')
    printf 'built: %s\n' "${bins[@]}"
    if [[ -n "$out" ]]; then
        mkdir -p "$out"
        names=()
        for bin in "${bins[@]}"; do
            if [[ -n "$strip_tool" ]]; then
                "$strip_tool" -o "$out/$(basename "$bin")" "$bin"
            else
                cp "$bin" "$out/"
            fi
            names+=("$(basename "$bin")")
        done
        (cd "$out" && sha256sum "${names[@]}" | tee SHA256SUMS)
    fi
    exit 0
fi

# --tests: build from a mirror of the working tree at the path the device will have it at.
src="${STYX_DEVTEST_SRC:-/tmp/styx-devtest-src}"
[[ "$src" == /tmp/* ]] || { echo "STYX_DEVTEST_SRC must be under /tmp (it is copied to the same path on the device)" >&2; exit 2; }
mkdir -p "$src"
files="$(mktemp)"
trap 'rm -f "$files"' EXIT
(cd "$root_dir" && git ls-files -z --cached --others --exclude-standard) >"$files"
rsync -a --ignore-missing-args --from0 --files-from="$files" "$root_dir/" "$src/"
cp -f "$root_dir/Cargo.lock" "$src/"
# Drop what is no longer in the working tree (cargo would still find a stale tests/*.rs).
(cd "$src" && find . -path ./target -prune -o -type f -print0 | sed -z 's#^\./##') |
    LC_ALL=C sort -z | LC_ALL=C comm -z -23 - <({
        cat "$files"
        printf 'Cargo.lock\0'
    } | LC_ALL=C sort -z) | (cd "$src" && xargs -0 -r rm -f)
bundle="$target_root/device-tests/$name"
rm -rf "$bundle"
mkdir -p "$bundle/bin"
cd "$src"
# One line per test binary: its package directory (relative to the sources) and its name.
cargo test --no-run --target-dir "$target_dir" --profile "$profile" --target "$triple" \
    "${pkg_args[@]}" ${features:+--features "$features"} "${extra[@]}" \
    --message-format=json-render-diagnostics |
    jq -r --arg src "$src/" 'select(.reason == "compiler-artifact" and .executable != null
        and .profile.test) | [(.manifest_path | ltrimstr($src) | rtrimstr("Cargo.toml")
        | rtrimstr("/")), .executable] | @tsv' |
    while IFS=$'\t' read -r pkg_dir exe; do
        cp "$exe" "$bundle/bin/"
        printf '%s\t%s\n' "${pkg_dir:-.}" "$(basename "$exe")"
    done >"$bundle/tests.tsv"
printf '%s\n' "$src" >"$bundle/src-path"
printf '%s\n' "$triple" >"$bundle/triple"
echo "$(wc -l <"$bundle/tests.tsv") test binaries in $bundle (sources mirrored at $src)"
echo "run them: scripts/device-tests.sh $bundle   (or --qemu for a musl bundle)"
