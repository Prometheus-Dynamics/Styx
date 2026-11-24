#!/usr/bin/env bash
# A fresh worktree's first build starts from a seed: the dependencies' build products of an earlier
# green gate, so only the workspace's own crates compile.
#
#   scripts/cargo-seed.sh restore   an absent or empty target directory starts from the seed for this
#                                   key (or the newest seed: cargo rebuilds whatever differs)
#   scripts/cargo-seed.sh save      after a green gate: keep this target's dependencies as the seed
#                                   for its key, unless one exists
#
# On by default for a target directory kept outside its worktree (CARGO_TARGET_DIR, one per worktree,
# on the targets' filesystem): the seeds are kept beside it, in <its parent>/seeds, so a copy is a
# reflink. STYX_CARGO_SEED_DIR names another place, and set empty turns seeding off. A target inside
# the worktree, and CI (GITHUB_ACTIONS), never seed.
#
# Dependencies only. A workspace crate's build products carry its checkout's absolute paths
# (env!("CARGO_MANIFEST_DIR"), file!(), include_str!), and in another worktree cargo can find them
# current, so a test would read the other checkout's files. The seed drops every build product of a
# workspace package (its .fingerprint, build and deps entries, by package and target name), every
# final artifact, and the C proof's target; then a seed holding any byte of its worktree's path is
# refused. A dependency's fingerprint holds the compiler, the profile, its features and its source
# path (the same registry and git checkouts for every worktree): whatever does not match is rebuilt.
#
# The key: Cargo.lock, the [profile.*] sections of Cargo.toml, .cargo/config.toml,
# rust-toolchain.toml and `rustc -vV`. The two newest seeds are kept. Every write is under a lock,
# and a seed is renamed into place only when complete.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

die() { echo "cargo-seed: $*" >&2; exit 1; }
[ -z "${GITHUB_ACTIONS:-}" ] || exit 0
target=${CARGO_TARGET_DIR:-$(pwd)/target}
if [ -n "${STYX_CARGO_SEED_DIR+set}" ]; then
  seeds=$STYX_CARGO_SEED_DIR
elif [ -n "${CARGO_TARGET_DIR:-}" ] && [ "${CARGO_TARGET_DIR#/}" != "$CARGO_TARGET_DIR" ] \
  && [ "${CARGO_TARGET_DIR#"$(pwd -P)"/}" = "$CARGO_TARGET_DIR" ]; then
  seeds=$(dirname "$CARGO_TARGET_DIR")/seeds
else
  seeds=""
fi
[ -n "$seeds" ] || exit 0
mkdir -p "$seeds"

# The key reads the committed Cargo.lock (the gate has checked it against the manifests).
key=$( { cat Cargo.lock
         awk '/^\[/{p = ($0 ~ /^\[profile/)} p' Cargo.toml
         cat .cargo/config.toml rust-toolchain.toml 2>/dev/null || true
         rustc -vV; } | sha256sum | cut -c1-16)

# Copies a target directory's dependency build products into a new directory: every file but the
# workspace's. A workspace package's units go by its .fingerprint/<package>-<hash> directory: that
# directory, its build/<package>-* scripts and every deps/ file carrying the hash (a dependency may
# share a target's name, not its hash). A library with a C crate type has no hash in its file names:
# those go by target name. Final artifacts (the files directly in a profile directory), examples,
# incremental state, test scratch and the C proof's own target are all the workspace's. One pass
# over the target names the files to keep; only those are copied (by reflink where it can).
copy_dependencies() {
  local from="$1" to="$2"
  cargo metadata --locked --format-version 1 --no-deps | python3 scripts/seed-list.py "$from" > "$to.list"
  # A list with no file is a fault of the listing, never a target with nothing to keep.
  grep -qz '^F' "$to.list" || die "not saved: the listing of $from named no file"
  mkdir -p "$to"
  (
    cd "$from"
    grep -z '^F' "$to.list" | cut -z -c2- | xargs -0 -r cp -a --reflink=auto --parents -t "$to"
    grep -z '^D' "$to.list" | cut -z -c2- | (cd "$to" && xargs -0 -r mkdir -p)
  )
  rm -f "$to.list"
}

case ${1:-} in
  restore)
    if [ -d "$target" ] && [ -n "$(ls -A "$target" 2>/dev/null)" ]; then exit 0; fi
    (
      flock -s -w 600 9 || die "the seed lock was not free in 600 s"
      src=$seeds/$key
      if [ ! -f "$src/.seed-complete" ]; then
        src=""
        for d in $(ls -1dt "$seeds"/*/ 2>/dev/null); do
          if [ -f "$d/.seed-complete" ]; then src=${d%/}; break; fi
        done
      fi
      if [ -z "$src" ]; then echo "cargo-seed: no seed yet: a cold build" >&2; exit 0; fi
      mkdir -p "$(dirname "$target")"
      rm -rf "$target"
      cp -a --reflink=auto "$src" "$target"
      rm -f "$target/.seed-complete"
      other=""
      [ "$(basename "$src")" = "$key" ] || other=" (another key: cargo rebuilds what differs)"
      echo "cargo-seed: $target starts from the seed $(basename "$src")$other" >&2
    ) 9> "$seeds/.lock"
    ;;
  save)
    [ -d "$target" ] || exit 0
    (
      flock -w 600 9 || die "the seed lock was not free in 600 s"
      [ -f "$seeds/$key/.seed-complete" ] && exit 0
      tmp=$seeds/.new-$key.$$
      rm -rf "$tmp"
      copy_dependencies "$target" "$tmp"
      worktree=$(pwd -P)
      left=$(grep -rlF "$worktree/" "$tmp" || true)
      if [ -n "$left" ]; then
        rm -rf "$tmp"
        die "not saved: $(echo "$left" | wc -l) files still name $worktree, e.g. $(echo "$left" | head -3 | tr '\n' ' ')"
      fi
      touch "$tmp/.seed-complete"
      rm -rf "${seeds:?}/$key"
      mv "$tmp" "$seeds/$key"
      # The two newest complete seeds stay; older ones go.
      ls -1dt "$seeds"/*/ | tail -n +3 | while read -r d; do rm -rf "${d%/}"; done
      echo "cargo-seed: kept the dependencies of $target as the seed $key ($(du -sh "$seeds/$key" | cut -f1))" >&2
    ) 9> "$seeds/.lock"
    ;;
  *) die "usage: cargo-seed.sh restore|save" ;;
esac
