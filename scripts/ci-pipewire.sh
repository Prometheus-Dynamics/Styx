#!/usr/bin/env bash
# Starts a PipeWire daemon and its session manager for this job, so that the proofs with a real
# PipeWire consumer run in CI instead of printing SKIPPED. A hosted runner has no user session:
# the daemon gets a runtime directory and a session bus of its own, and both are handed to the
# later steps through GITHUB_ENV. Ends non-zero, saying why, when the daemon does not come up.
set -euo pipefail

runtime="${RUNNER_TEMP:-/tmp}/styx-xdg-runtime"
mkdir -p "$runtime"
chmod 700 "$runtime"
export XDG_RUNTIME_DIR="$runtime"

bus="unix:path=$runtime/bus"
dbus-daemon --session --address="$bus" --fork --nopidfile
export DBUS_SESSION_BUS_ADDRESS="$bus"

nohup pipewire > "$runtime/pipewire.log" 2>&1 &
for _ in $(seq 100); do
    [ -S "$runtime/pipewire-0" ] && break
    sleep 0.1
done
if [ ! -S "$runtime/pipewire-0" ]; then
    echo "the PipeWire daemon did not open its socket:" >&2
    cat "$runtime/pipewire.log" >&2
    exit 1
fi
nohup wireplumber > "$runtime/wireplumber.log" 2>&1 &

# The session manager is up when it has made its own client on the daemon.
for _ in $(seq 100); do
    pw-cli ls Client 2>/dev/null | grep -qi wireplumber && break
    sleep 0.1
done
if ! pw-cli ls Client 2>/dev/null | grep -qi wireplumber; then
    echo "the session manager did not connect to the daemon:" >&2
    cat "$runtime/wireplumber.log" >&2
    exit 1
fi

echo "PipeWire $(pipewire --version | tail -n 1) with a session manager, runtime $runtime"
if [ -n "${GITHUB_ENV:-}" ]; then
    {
        echo "XDG_RUNTIME_DIR=$runtime"
        echo "DBUS_SESSION_BUS_ADDRESS=$bus"
    } >> "$GITHUB_ENV"
fi
