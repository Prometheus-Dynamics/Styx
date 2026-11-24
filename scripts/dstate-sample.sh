#!/usr/bin/env bash
# What a process tree waits on: every INTERVAL seconds (default 0.2) until PID ends, the state
# and kernel wait channel of each thread of PID and of its descendants. At the end it prints one
# line per (state, wait channel) with its count of samples, most first; threads asleep in a futex
# or a pipe (waiting for each other, not for the kernel) are counted apart on one line.
#
#   scripts/dstate-sample.sh PID [INTERVAL] > counts
#
# D is an uninterruptible wait (a lock or I/O in the kernel), R is running or runnable. Reads
# /proc only; signals nothing.
set -euo pipefail
pid=${1:?pid}; interval=${2:-0.2}
samples=$(mktemp)
trap 'rm -f "$samples"' EXIT
tree() { # PID and its descendants
  local p=$1 c
  echo "$p"
  for c in $(cat /proc/"$p"/task/*/children 2> /dev/null); do tree "$c"; done
}
while kill -0 "$pid" 2> /dev/null; do
  for p in $(tree "$pid"); do
    for t in /proc/"$p"/task/*; do
      st=$(cut -d' ' -f3 "$t/stat" 2> /dev/null) || continue
      [ -n "$st" ] || continue
      echo "$st $(cat "$t/wchan" 2> /dev/null || echo ?)"
    done
  done >> "$samples"
  sleep "$interval"
done
total=$(wc -l < "$samples")
idle=$(grep -cE '^S (futex|__futex|anon_pipe|pipe_read|do_wait|do_epoll|ep_poll|poll_schedule)' "$samples" || true)
echo "samples $total (threads x ticks); asleep on each other (futex, pipe, wait, poll): $idle"
grep -vE '^S (futex|__futex|anon_pipe|pipe_read|do_wait|do_epoll|ep_poll|poll_schedule)' "$samples" |
  sed 's/ 0$/ -/' | sort | uniq -c | sort -rn | head -12
