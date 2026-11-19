#!/usr/bin/env python3
"""Runs the gate so that nothing it starts outlives it or holds its output.

    gate-supervise.py <gate script> [args]   (scripts/gate.sh runs itself under this)

A process that a step starts and then leaves behind, an orphan, keeps a copy of the gate's
stdout and stderr. When the gate's output is a pipe (CI, `| tee`, `$(...)`), the reader of that
pipe sees no end of it while the orphan lives, and waits for ever after the gate has finished.

This process becomes the child subreaper of everything under it (prctl PR_SET_CHILD_SUBREAPER):
an orphan of any depth, one that started a session of its own included, becomes its child rather
than init's. The gate writes into pipes of this process, which copies them to its own stdout and
stderr. When the gate exits, every process left under this one is named, sent SIGTERM, then
SIGKILL, and the gate fails. The pipes are then read for a short while only: nothing left
running can hold them open past that.
"""
import ctypes
import os
import selectors
import signal
import subprocess
import sys
import time

PR_SET_CHILD_SUBREAPER = 36
# How long the output is read after the gate and its leftovers are gone.
DRAIN = 2.0
# How long a leftover has between SIGTERM and SIGKILL.
GRACE = 3.0


def descendants(root):
    """Every live process under `root`, by its parent links in /proc."""
    children = {}
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        try:
            with open(f"/proc/{entry}/stat", "rb") as f:
                stat = f.read()
        except OSError:
            continue
        # The command name is in parentheses and may hold spaces: fields follow the last ')'.
        fields = stat[stat.rfind(b")") + 2:].split()
        state, ppid = fields[0], int(fields[1])
        if state != b"Z":
            children.setdefault(ppid, []).append(int(entry))
    out, todo = [], [root]
    while todo:
        for pid in children.get(todo.pop(), []):
            out.append(pid)
            todo.append(pid)
    return out


def command(pid):
    try:
        with open(f"/proc/{pid}/cmdline", "rb") as f:
            return f.read().replace(b"\0", b" ").decode(errors="replace").strip() or "?"
    except OSError:
        return "?"


def reap():
    """Collects every exited child (the gate, and orphans handed to this process)."""
    done = {}
    while True:
        try:
            pid, status = os.waitpid(-1, os.WNOHANG)
        except ChildProcessError:
            return done
        if pid == 0:
            return done
        done[pid] = status


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    libc = ctypes.CDLL(None, use_errno=True)
    if libc.prctl(PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) != 0:
        sys.exit(f"gate-supervise: prctl(PR_SET_CHILD_SUBREAPER): {os.strerror(ctypes.get_errno())}")
    env = dict(os.environ, STYX_GATE_SUPERVISOR=str(os.getpid()))
    gate = subprocess.Popen(sys.argv[1:], stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, env=env, start_new_session=True)
    # Stopping this process stops the gate and everything under it, through the same cleanup.
    stopped = []
    for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(sig, lambda s, _f: stopped.append(s))

    sel = selectors.DefaultSelector()
    outs = {gate.stdout.fileno(): sys.stdout.buffer, gate.stderr.fileno(): sys.stderr.buffer}
    for fd in outs:
        os.set_blocking(fd, False)
        sel.register(fd, selectors.EVENT_READ)

    def pump(timeout):
        for key, _ in sel.select(timeout):
            try:
                data = os.read(key.fd, 65536)
            except BlockingIOError:
                continue
            if data:
                outs[key.fd].write(data)
                outs[key.fd].flush()
            else:
                sel.unregister(key.fd)

    status = None
    while status is None:
        if stopped:
            os.killpg(gate.pid, signal.SIGTERM)
            stopped.clear()
        if sel.get_map():
            pump(0.2)
        else:
            time.sleep(0.2)
        status = reap().get(gate.pid)
    gate.returncode = os.waitstatus_to_exitcode(status)
    code = gate.returncode if gate.returncode >= 0 else 128 - gate.returncode

    left = descendants(os.getpid())
    if left:
        print(f"gate: {len(left)} process(es) left running after the gate; stopping them:",
              file=sys.stderr)
        for pid in left:
            print(f"    {pid} {command(pid)}", file=sys.stderr)
        for pid in left:
            try:
                os.kill(pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
        deadline = time.monotonic() + GRACE
        while time.monotonic() < deadline and descendants(os.getpid()):
            reap()
            time.sleep(0.1)
        for pid in descendants(os.getpid()):
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        time.sleep(0.1)
        reap()
        if code == 0:
            code = 1
    deadline = time.monotonic() + DRAIN
    while sel.get_map() and time.monotonic() < deadline:
        pump(max(0.0, deadline - time.monotonic()))
    sys.stdout.flush()
    sys.stderr.flush()
    os._exit(code)


if __name__ == "__main__":
    main()
