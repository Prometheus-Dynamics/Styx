#!/bin/sh
# Runs a command on the dev device while holding the shared device lock, then releases it.
#
#   scripts/with-device-lock.sh <owner> <remote shell command>
#   DEVICE=root@<device> DMESG_LOG=target/dmesg-x.log WAIT=1 scripts/with-device-lock.sh ...
#
# - The lock is flock(1) on /run/lock/raze-gpu.lock on the device (LOCK names another), the one
#   every project's board runs share. The kernel frees it when the holder dies, so it cannot go
#   stale. The owner's name is in the file while it is held.
# - The command runs without the lock's descriptor: a daemon it starts does not keep the lock.
# - WAIT=1 blocks until the lock is free; otherwise exits 3 when busy.
# - DMESG_LOG streams the device's kernel log to that host file during the run.
# The command's exit code is returned. Restore the device baseline inside the command.

set -u
owner="${1:?owner}"
shift
cmd="${1:?remote command}"
dev="${DEVICE:?set DEVICE to the device to lock, e.g. DEVICE=root@<device>}"
lock="${LOCK:-/run/lock/raze-gpu.lock}"
nonblock=-n
[ "${WAIT:-0}" = 1 ] && nonblock=

quoted="'$(printf %s "$cmd" | sed "s/'/'\\\\''/g")'"
remote="exec 9>>$lock
if ! flock $nonblock 9; then
    echo \"with-device-lock: busy: \$(cat $lock 2>/dev/null)\" >&2
    exit 3
fi
echo '$owner' \"\$(date)\" >$lock
sh -c $quoted 9>&-
rc=\$?
: >$lock
exit \$rc"

dmesg_pid=""
if [ -n "${DMESG_LOG:-}" ]; then
    ssh -o BatchMode=yes "$dev" dmesg -w >"$DMESG_LOG" 2>&1 &
    dmesg_pid=$!
fi

ssh -o BatchMode=yes "$dev" "$remote"
rc=$?

[ -n "$dmesg_pid" ] && kill "$dmesg_pid" 2>/dev/null
exit "$rc"
