#!/usr/bin/env bash
# The inner loop: the gate cut down to what a change can reach. A worker runs this before pushing;
# the landing gate (scripts/gate.sh) is the proof.
#
#   scripts/check.sh                 the crates whose files differ from origin/next
#                                    (STYX_CHECK_BASE names another base)
#   scripts/check.sh styx-frame ...  the named crates' tests (every other pass as in the gate)
#
# It runs scripts/gate.sh with whole passes left out, never part of one: every command it runs is
# the gate's, on the gate's build (no -p or --exclude, which change how features unify), so a red
# here is red in the gate. What it leaves out (scripts/affected.py --plan):
#   - the tests of crates the change cannot reach: nextest runs rdeps(<crate>) of the changed ones;
#   - one of the two feature passes (clippy and tests): the default one, or the all-features one
#     when a changed file is behind a feature;
#   - the doctests, when no changed file holds one;
#   - the no_std checks of crates that did not change.
# Each one left out is printed at the end; the gate runs it. Green here is not the gate. A change
# outside the crates or to a crate's manifest runs the gate itself.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

crates=()
pass=all
doctests=1
no_std=()
if [ $# -eq 0 ]; then
  while read -r kind value; do
    case "$kind" in
      --workspace)
        echo "a change outside the crates or to a crate's manifest: running the gate"
        exec scripts/gate.sh
        ;;
      crate) crates+=("$value") ;;
      pass) pass=$value ;;
      doctests) doctests=$value ;;
      no_std) no_std+=("$value") ;;
    esac
  done < <(python3 scripts/affected.py --plan "${STYX_CHECK_BASE:-origin/next}")
else
  # Named crates: only their tests are narrowed; every other pass runs as in the gate.
  crates=("$@")
  pass=""
fi
if [ ${#crates[@]} -eq 0 ]; then
  echo "no crate changed"
  exit 0
fi
filter=""
for c in "${crates[@]}"; do
  filter+="${filter:+ | }rdeps(=$c)"
done
if [ -z "$pass" ]; then
  STYX_TEST_FILTER="$filter" exec scripts/gate.sh
fi
STYX_TEST_FILTER="$filter" STYX_CHECK_PASS="$pass" STYX_CHECK_DOCTESTS="$doctests" \
  STYX_CHECK_NO_STD="${no_std[*]}" exec scripts/gate.sh
