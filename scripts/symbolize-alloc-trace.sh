#!/usr/bin/env bash
# Names the call sites in a hop_breakdown allocation trace (`STYX_ALLOC_TRACE=1`, printed after
# the hop table as `exe+0x...` frames: return addresses relative to the executable's load base).
# Each such frame gets its function and file:line from BINARY, looked up one byte before the
# return address (the call instruction). Other frames (shared objects, `libc.so.6+0x...`) are
# left as they are.
#
#   scripts/symbolize-alloc-trace.sh BINARY LOGFILE
#
# BINARY must be the UNSTRIPPED executable the trace ran: `scripts/cross-aarch64.sh --out DIR`
# copies its binaries stripped, so take the one in the target directory instead, e.g. for the
# musl build: $CARGO_TARGET_DIR/aarch64-musl/aarch64-unknown-linux-musl/release/hop_breakdown
# (the default target/ when CARGO_TARGET_DIR is unset; the profile directory is the --profile
# name, `device` by default). The binary must be the one the device ran: rebuild and copy it
# exactly, then the offsets match. Needs addr2line (binutils) or llvm-addr2line; ADDR2LINE
# picks another.
set -euo pipefail

usage() {
    echo "usage: $0 BINARY LOGFILE" >&2
    exit 2
}
(($# == 2)) || usage
bin="$1"
log="$2"
[[ -f "$bin" ]] || { echo "no such binary: $bin" >&2; exit 2; }
[[ -f "$log" ]] || { echo "no such log: $log" >&2; exit 2; }

tool="${ADDR2LINE:-}"
if [[ -z "$tool" ]]; then
    for t in addr2line llvm-addr2line; do
        if command -v "$t" >/dev/null 2>&1; then tool="$t"; break; fi
    done
fi
[[ -n "$tool" ]] || { echo "no addr2line (binutils) or llvm-addr2line on PATH" >&2; exit 2; }

mapfile -t offsets < <(grep -o 'exe+0x[0-9a-f]\+' "$log" | sed 's/^exe+//' | sort -u)
declare -A where=()
if ((${#offsets[@]})); then
    addrs=()
    for o in "${offsets[@]}"; do
        addrs+=("$(printf '0x%x' $((o - 1)))")
    done
    # -f: the function, -C: demangled; two lines per address: the function, then file:line.
    mapfile -t out < <("$tool" -f -C -e "$bin" "${addrs[@]}")
    named=0
    for i in "${!offsets[@]}"; do
        fn="${out[2 * i]:-??}"
        loc="${out[2 * i + 1]:-??:0}"
        [[ "$fn" != "??" ]] && named=$((named + 1))
        where[${offsets[$i]}]="$fn $loc"
    done
    if ((named == 0)); then
        echo "warning: no address in $bin has a symbol: is it the unstripped build?" >&2
    fi
fi

while IFS= read -r line || [[ -n "$line" ]]; do
    if [[ "$line" =~ ^([[:space:]]*)exe\+(0x[0-9a-f]+)(.*)$ ]]; then
        printf '%sexe+%s  %s%s\n' "${BASH_REMATCH[1]}" "${BASH_REMATCH[2]}" \
            "${where[${BASH_REMATCH[2]}]:-??}" "${BASH_REMATCH[3]}"
    else
        printf '%s\n' "$line"
    fi
done <"$log"
