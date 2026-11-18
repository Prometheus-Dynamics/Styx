#!/usr/bin/env bash
# A fresh worktree's first build starts from a seed: the dependencies' build products of an earlier
# green gate, so only the workspace's own crates compile.
#
#   scripts/cargo-seed.sh restore   an absent or empty target directory starts from the seed for this
#                                   key (or the newest seed: cargo rebuilds whatever differs)
#   scripts/cargo-seed.sh save      after a green gate: keep this target's dependencies as the seed
#                                   for its key, unless one exists
#
# Opt-in: nothing happens unless STYX_CARGO_SEED_DIR is set (CI never sets it). Put it on the targets'
# filesystem, so a copy is a reflink.
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
seeds=${STYX_CARGO_SEED_DIR:-}
[ -n "$seeds" ] || exit 0
target=${CARGO_TARGET_DIR:-$(pwd)/target}
mkdir -p "$seeds"

# The key reads the committed Cargo.lock (the gate has checked it against the manifests).
key=$( { cat Cargo.lock
         awk '/^\[/{p = ($0 ~ /^\[profile/)} p' Cargo.toml
         cat .cargo/config.toml rust-toolchain.toml 2>/dev/null || true
         rustc -vV; } | sha256sum | cut -c1-16)

# Removes the workspace's build products from a copy of a target directory.
strip_workspace() {
  local dir="$1" names profile
  names=$(cargo metadata --locked --format-version 1 --no-deps | python3 -c '
import json, sys
meta = json.load(sys.stdin)
for p in meta["packages"]:
    if p["id"] in meta["workspace_members"]:
        print("pkg", p["name"])
        for t in p["targets"]:
            print("target", t["name"].replace("-", "_"))
')
  # Each profile directory: <profile>/deps, or <triple>/<profile>/deps.
  while read -r profile; do
    profile=$(dirname "$profile")
    # Final artifacts (uplifted binaries and libraries, their .d files), examples, incremental
    # state, test scratch and the C proof's own target are all the workspace's.
    find "$profile" -maxdepth 1 -type f -delete
    rm -rf "$profile/examples" "$profile/incremental" "$profile/c-proof"
    # A unit's deps/ files carry the hash of its .fingerprint/<package>-<hash> directory: those of
    # a workspace package go by that hash (a dependency may share a target's name, not its hash).
    # A library with a C crate type has no hash in its file names: those go by target name.
    while read -r kind name; do
      if [ "$kind" = pkg ]; then
        for fp in "$profile/.fingerprint/$name"-*; do
          [ -d "$fp" ] || continue
          hash=${fp##*-}
          find "$profile/deps" -maxdepth 1 -name "*-$hash*" -delete
          rm -rf "$fp"
        done
        rm -rf "$profile/build/$name"-*
      else
        find "$profile/deps" -maxdepth 1 \( -name "$name.*" -o -name "lib$name.*" \) -delete
      fi
    done <<< "$names"
  done < <(find "$dir" -mindepth 2 -maxdepth 3 -type d -name deps)
  rm -rf "$dir/tmp" "$dir/nextest"
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
      cp -a --reflink=auto "$target" "$tmp"
      strip_workspace "$tmp"
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
