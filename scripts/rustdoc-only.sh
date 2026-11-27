#!/usr/bin/env bash
# rustdoc for the gate's doctests off CI (scripts/gate.sh sets RUSTDOC to this): a `--test` run of
# a crate in STYX_DOC_ONLY (a file, one --crate-name per line: the reuse plan's crates whose key
# moved and their dependents) runs; any other crate's is skipped and named. Everything else goes
# to the real rustdoc (STYX_RUSTDOC) untouched: no list or no list file (no plan), no --crate-name,
# no --test.
# With STYX_DOC_COLLECT (a directory), a listed crate's `--test` run is not run here: its call
# (the real rustdoc and its arguments, the environment, the directory) is written to
# <dir>/<crate>/{argv,env,cwd} and the call succeeds; `gate.py doctests-run` runs every collected
# call side by side once cargo ends, and its verdict is the step's.
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
if [ -z "$list" ] || [ ! -f "$list" ] || [ -z "$name" ] || [ "$test" = 0 ]; then
  exec "$real" "$@"
fi
if grep -qxF -- "$name" "$list"; then
  collect=${STYX_DOC_COLLECT:-}
  [ -n "$collect" ] || exec "$real" "$@"
  rec="$collect/$name"
  mkdir -- "$rec" || { echo "rustdoc-only: $name: its doctests' call was collected twice" >&2; exit 1; }
  printf '%s\0' "$real" "$@" > "$rec/argv"
  env -0 > "$rec/env"
  printf '%s' "$PWD" > "$rec/cwd"
  echo "doctests collected: $name (they run beside the other crates' when cargo ends)"
  exit 0
fi
echo "doctests skipped: $name (its key and every workspace crate it builds with reused a green run)" >&2
