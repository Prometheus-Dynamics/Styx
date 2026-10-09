#!/usr/bin/env bash
# Runs a bundle of aarch64 test binaries made by `scripts/cross-aarch64.sh --tests` on the dev
# device, or under qemu-aarch64 on this machine (docs/development.md "Tests on the device").
#
#   scripts/device-tests.sh BUNDLE [--qemu] [--only NAME] [-- test args]
#
#   BUNDLE       target/device-tests/<build name> (printed by cross-aarch64.sh --tests)
#   --qemu       run here under qemu-aarch64 (a musl bundle: static binaries; a glibc bundle
#                needs QEMU_LD_PREFIX set to its sysroot) instead of on the device
#   --only NAME  only the test binaries whose name starts with NAME (repeatable)
#   -- args      passed to every test binary (a filter, --ignored, --test-threads 1, ...)
#   DEVICE       the device, required (e.g. root@<host>); WAIT=1 to wait for the device lock
#
# On the device everything goes under /tmp: the bundle's sources at the same path they were
# built from (STYX_DEVTEST_SRC, /tmp/styx-devtest-src by default; the tests find their fixtures
# there), the binaries in /tmp/styx-devtest-bin. The run holds the device lock
# (scripts/with-device-lock.sh); PhotonVision, when it runs, is stopped first and started again
# by a trap on any exit. Each binary runs from its package's directory, as cargo test does. The
# exit code is 0 when every binary passed.
set -euo pipefail

root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

bundle=""
qemu=0
declare -a only=() args=()
while (($#)); do
    case "$1" in
    --qemu) qemu=1 ;;
    --only) only+=("$2"); shift ;;
    --) shift; args=("$@"); break ;;
    -h | --help) sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'; exit 0 ;;
    -*) echo "unknown argument: $1 (--help)" >&2; exit 2 ;;
    *) bundle="$1" ;;
    esac
    shift
done
[[ -n "$bundle" && -f "$bundle/tests.tsv" ]] || {
    echo "device-tests.sh: give a bundle made by scripts/cross-aarch64.sh --tests" >&2
    exit 2
}
bundle="$(cd "$bundle" && pwd)"
src="$(cat "$bundle/src-path")"

# The selected binaries: "package dir<TAB>binary".
selected="$(awk -F'\t' -v only="${only[*]:-}" '
    BEGIN { n = split(only, o, " ") }
    { keep = n == 0; for (i = 1; i <= n; i++) if (index($2, o[i]) == 1) keep = 1 }
    keep' "$bundle/tests.tsv")"
[[ -n "$selected" ]] || { echo "device-tests.sh: no test binary matches" >&2; exit 2; }

quoted_args=""
for a in "${args[@]}"; do quoted_args+=" $(printf '%q' "$a")"; done

if ((qemu)); then
    qemu_bin="$(command -v qemu-aarch64 || command -v qemu-aarch64-static)" || {
        echo "device-tests.sh: no qemu-aarch64" >&2
        exit 2
    }
    failed=0
    while IFS=$'\t' read -r dir exe; do
        echo "==> $exe ($dir)"
        (cd "$src/$dir" && CARGO_MANIFEST_DIR="$src/$dir" "$qemu_bin" "$bundle/bin/$exe" "${args[@]}") ||
            failed=1
    done <<<"$selected"
    exit "$failed"
fi

dev="${DEVICE:?set DEVICE to the device to run on, e.g. DEVICE=root@<host>}"
bin_dir=/tmp/styx-devtest-bin
# The run, as one remote shell script: PhotonVision stopped if it runs and started again on
# any exit, every binary from its package directory, the exit code 1 if any failed.
remote="set -u
pv=\$(systemctl is-active photonvision 2>/dev/null || true)
if [ \"\$pv\" = active ]; then
    trap 'systemctl start photonvision' EXIT
    trap 'exit 130' INT TERM HUP
    systemctl stop photonvision
    sleep 2
fi
failed=0
"
while IFS=$'\t' read -r dir exe; do
    remote+="echo '==> $exe ($dir)'
(cd '$src/$dir' && CARGO_MANIFEST_DIR='$src/$dir' $bin_dir/$exe$quoted_args) || failed=1
"
done <<<"$selected"
remote+='exit $failed'

# Copying is plain file transfer into /tmp (no camera, no PhotonVision), between locked steps.
echo "==> copying the bundle and its sources to $dev:/tmp"
ssh -o BatchMode=yes "$dev" "mkdir -p '$src' $bin_dir"
(cd "$src" && tar -cf - --exclude=./target .) | ssh -o BatchMode=yes "$dev" "tar -xf - -C '$src'"
tar -cf - -C "$bundle/bin" . | ssh -o BatchMode=yes "$dev" "tar -xf - -C $bin_dir"
DEVICE="$dev" "$root_dir/scripts/with-device-lock.sh" device-tests "$remote"
