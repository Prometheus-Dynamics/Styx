#!/bin/sh
# Per gate lane and variant: the last commit of REV's first-parent history it passed at, and how
# far REV is past it (scripts/gate.py).
#   scripts/lanes-behind.sh [REV]   # default origin/next
cd "$(dirname "$0")/.." || exit 2
exec python3 scripts/gate.py behind "$@"
