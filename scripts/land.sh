#!/bin/sh
# The lander's push: HEAD goes to origin/next only when its Gate-Lanes and Gate-Tree trailers match
# a green gate record of its tree (scripts/gate.py verify-push), and only as a fast-forward.
#   scripts/land.sh
set -eu
cd "$(dirname "$0")/.."
git fetch -q origin
python3 scripts/gate.py verify-push HEAD --onto origin/next
git merge-base --is-ancestor origin/next HEAD || { echo "land: HEAD is not on origin/next: rebase, then run the gate" >&2; exit 1; }
git push origin HEAD:next
# A toolchain no seed has yet: its seed is baked now, in the background (scripts/seed-bake.sh).
scripts/seed-bake.sh --if-due --background || echo "land: the seed bake did not start (the landing stands)" >&2
