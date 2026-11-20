#!/bin/sh
# Refuses (non-zero, naming them) unless every gate lane and variant passed at exactly COMMIT.
# Part of the user's tagging step; it never runs a gate.
#   scripts/release-check.sh COMMIT
cd "$(dirname "$0")/.." || exit 2
exec python3 scripts/gate.py release-check "$@"
