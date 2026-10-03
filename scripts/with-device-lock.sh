#!/bin/sh
# Runs a command on the dev device while holding the shared device lock, then releases it.
#
#   scripts/with-device-lock.sh <owner> <remote shell command>
#   DEVICE=root@helios DMESG_LOG=target/helios-dmesg-x.log WAIT=1 scripts/with-device-lock.sh ...
#
# - The lock is taken atomically (mkdir) with a random token in its owner file; the command runs
#   only if that succeeded.
# - Release removes the lock only if the token is still ours, so it can never delete someone
#   else's lock (a reboot wipes /tmp, and another agent may have taken it since).
# - WAIT=1 retries every 60 s while someone else holds it; otherwise exits 3 when busy.
# - DMESG_LOG streams the device's kernel log to that host file while the lock is held.
# The command's exit code is returned. Restore the device baseline inside the command.

set -u
owner="${1:?owner}"
shift
cmd="${1:?remote command}"
dev="${DEVICE:-root@helios}"
lock=/tmp/styx-device-lock
token="$owner $(date +%s)-$$-$(od -An -N4 -tx4 /dev/urandom | tr -d ' ')"

take() {
    ssh -o BatchMode=yes "$dev" "mkdir $lock 2>/dev/null && printf '%s\n' '$token' > $lock/owner"
}
release() {
    ssh -o BatchMode=yes "$dev" "[ \"\$(cat $lock/owner 2>/dev/null)\" = '$token' ] && rm -rf $lock" ||
        echo "with-device-lock: lock no longer ours (rebooted or taken); left as is" >&2
}

until take; do
    if [ "${WAIT:-0}" != 1 ]; then
        echo "with-device-lock: busy: $(ssh -o BatchMode=yes "$dev" "cat $lock/owner 2>/dev/null")" >&2
        exit 3
    fi
    sleep 60
done

dmesg_pid=""
if [ -n "${DMESG_LOG:-}" ]; then
    ssh -o BatchMode=yes "$dev" dmesg -w >"$DMESG_LOG" 2>&1 &
    dmesg_pid=$!
fi

ssh -o BatchMode=yes "$dev" "$cmd"
rc=$?

[ -n "$dmesg_pid" ] && kill "$dmesg_pid" 2>/dev/null
release
exit "$rc"
