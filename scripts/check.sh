#!/usr/bin/env bash
# The inner loop: the gate with the lanes the change selects and its tests narrowed to what the
# change can reach. A worker runs this before pushing; the landing gate (scripts/gate.sh) is the
# proof.
#
#   scripts/check.sh                 the crates whose files differ from origin/next
#   scripts/check.sh styx-frame ...  the named crates instead
#
# One code path with the gate: scripts/gate.py picks the lanes from the diff as the gate does, and
# runs the gate's own steps on the gate's build (no -p or --exclude, which change how features
# unify), so a red here is red in the gate. What it leaves out: the tests of crates the change
# cannot reach (nextest runs rdeps(<crate>) of the changed ones), the doctests when no changed file
# holds one, and lanes behind on origin/next that the change does not select. A change outside the
# crates or to a crate's manifest narrows nothing. Green here is not the gate, and records nothing.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
exec scripts/gate.sh --narrow "$@"
