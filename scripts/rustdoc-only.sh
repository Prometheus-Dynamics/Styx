#!/usr/bin/env bash
# rustdoc for the gate's doctests off CI (scripts/gate.sh sets RUSTDOC to this): a `--test` run of
# a crate in STYX_DOC_ONLY (a file, one --crate-name per line: the reuse plan's crates whose key
# moved and their dependents) runs; any other crate's is skipped and named. Everything else goes
# to the real rustdoc (STYX_RUSTDOC) untouched: no list or no list file (no plan), no --crate-name,
# no --test.
set -euo pipefail
real=${STYX_RUSTDOC:-rustdoc}
list=${STYX_DOC_ONLY:-}
name="" test=0 prev=""
for a in "$@"; do
  case $a in
    --crate-name=*) name=${a#--crate-name=} ;;
    --test) test=1 ;;
  esac
  [ "$prev" = --crate-name ] && name=$a
  prev=$a
done
if [ -z "$list" ] || [ ! -f "$list" ] || [ -z "$name" ] || [ "$test" = 0 ] || grep -qxF -- "$name" "$list"; then
  exec "$real" "$@"
fi
echo "doctests skipped: $name (its key and every workspace crate it builds with reused a green run)" >&2
