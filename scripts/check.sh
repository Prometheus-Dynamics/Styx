#!/usr/bin/env bash
# The inner loop: the gate with its tests narrowed to the crates a change reaches.
#
#   scripts/check.sh                 the crates whose files differ from origin/next, and their users
#   scripts/check.sh styx-frame ...  the named crates and their users
#
# This is scripts/gate.sh with STYX_TEST_FILTER set to rdeps(<crate>) for each crate (nextest's
# filterset: the crate's tests and those of every workspace crate depending on it). Every build,
# format, lint, doctest, no_std and source check runs as in the gate, on the same build, so a red
# here is red in the gate and a gate after it rebuilds nothing. Only tests of crates the change
# cannot reach are left out, so green here is not the gate: run the gate before landing. A change
# outside every crate (manifests, lock file, config, scripts) runs the gate itself.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

if [ $# -eq 0 ]; then
  mapfile -t crates < <(python3 scripts/affected.py)
else
  crates=("$@")
fi
if [ "${crates[0]:-}" = "--workspace" ]; then
  echo "a change outside the crates: running the gate"
  exec scripts/gate.sh
fi
if [ ${#crates[@]} -eq 0 ]; then
  echo "no crate changed"
  exit 0
fi
filter=""
for c in "${crates[@]}"; do
  filter+="${filter:+ | }rdeps(=$c)"
done
STYX_TEST_FILTER="$filter" exec scripts/gate.sh
