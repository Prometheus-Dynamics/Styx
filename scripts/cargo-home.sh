#!/usr/bin/env bash
# The gate's own CARGO_HOME: every committed lock file's crates, fetched before any lane, then used
# offline. The gate never downloads and never waits on another program's lock on ~/.cargo's package
# cache (a wait the gate's bounds cannot tell from a hang). ~/.cargo is never read or written here.
#
#   scripts/cargo-home.sh fetch    fill <area>/cache/cargo-home from every committed Cargo.lock
#                                  (cargo fetch --locked; online only when something is missing),
#                                  write its fetch.log, and bring every lane's home up to it
#   scripts/cargo-home.sh home L   the CARGO_HOME of lane L (it must exist: fetch makes it)
#
# fetch.log records each lock file's sha256, the sources (the registry, each git source at its
# commit), the number of registry crate files, a sha256 over their sums and the git checkouts.
#
# One home per lane. Cargo takes its package-cache lock for writing while it resolves, even offline:
# ten `cargo metadata --locked` side by side in one home printed 29 "Blocking waiting for file lock on
# package cache" lines (2026-11-27). The lanes' homes are copies of the fetched one (reflinks where
# the disk has them) at fixed paths, <area>/cache/cargo-home.lanes/<lane>, brought up to each new
# fetch in place: a dependency's fingerprint holds its source path, which is under CARGO_HOME, so a
# new path rebuilt every dependency (host:all's test build: 1,174 CPU-s, 2026-11-27). A home goes with
# a target directory: main is host:all's (its tests, doctests and the C proof's target, which the
# c_proof test builds again with the environment it runs in), clippy, default and bare are the beside
# lanes' own, and checks builds nothing.
#
# Each home's config.toml sets cargo offline and turns off its automatic cleaning of the package
# cache. The area is $STYX_AREA (default /mnt/nvme-wd/styx; scripts/area.sh). Not in CI: CI keeps the
# runner's CARGO_HOME, which rust-cache restores and saves.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
area=${STYX_AREA:-/mnt/nvme-wd/styx}
base=${STYX_CARGO_HOME_DIR:-$area/cache/cargo-home}
lanes=$base.lanes
LANES="main clippy default bare checks"

die() { echo "cargo-home: $*" >&2; exit 1; }
config() {
  printf '%s\n' "# Written by scripts/cargo-home.sh: the gate's own CARGO_HOME." "[net]" "offline = true" \
    "[cache]" 'auto-clean-frequency = "never"'
}

case ${1:-} in
  fetch)
    [ -z "${GITHUB_ACTIONS:-}" ] || die "not in CI: CI keeps the runner's CARGO_HOME"
    mkdir -p "$base" "$lanes"
    exec 9> "$base/.styx-fetch.lock"
    flock 9
    config > "$base/config.toml"
    locks=$(git ls-files '*Cargo.lock')
    [ -n "$locks" ] || die "no committed Cargo.lock"
    sums=""
    for lock in $locks; do sums+="$lock $(sha256sum "$lock" | cut -d' ' -f1)"$'\n'; done
    # The lock files are what decides the fetch: when they are the ones the last fetch recorded, and
    # the crates it counted are all there, nothing is fetched or summed again.
    if [ -f "$base/fetch.log" ] && [ "$(sed -n 's/^lock: //p' "$base/fetch.log")" = "${sums%$'\n'}" ] \
      && [ "$(find "$base/registry/cache" -name '*.crate' 2> /dev/null | wc -l)" \
        = "$(sed -n 's/^crates: \([0-9]*\) .*/\1/p' "$base/fetch.log")" ]; then
      echo "cargo-home: the lock files are the last fetch's: nothing fetched"
    else
      for lock in $locks; do
        manifest=$(dirname "$lock")/Cargo.toml
        # Offline first: online only when something is missing.
        CARGO_HOME=$base cargo fetch --locked --offline --quiet --manifest-path "$manifest" 2> /dev/null ||
          CARGO_HOME=$base cargo fetch --locked --config net.offline=false --manifest-path "$manifest" ||
          die "cargo fetch failed for $lock"
      done
      # shellcheck disable=SC2086 # the lock file paths have no spaces (git ls-files above)
      sources=$(sed -n 's/^source = "\([a-z]*+[^?#"]*\)[^#"]*\(#[0-9a-f]*\)\{0,1\}".*/\1\2/p' $locks | sort -u | tr '\n' ' ')
      crates=""
      [ ! -d "$base/registry/cache" ] ||
        crates=$(cd "$base/registry/cache" && find . -name '*.crate' -print0 | LC_ALL=C sort -z | xargs -0r sha256sum)
      {
        printf '%s\n' "${sums%$'\n'}" | sed 's/^/lock: /'
        echo "sources: $sources"
        echo "crates: $(printf '%s\n' "$crates" | grep -c .) registry crate files, sha256 over their sums $(printf '%s\n' "$crates" | sha256sum | cut -c1-64)"
        echo "git checkouts: $(find "$base/git/checkouts" -mindepth 2 -maxdepth 2 -type d 2> /dev/null | wc -l)"
      } > "$base/fetch.log.new"
      mv "$base/fetch.log.new" "$base/fetch.log"
      echo "cargo-home: fetched into $base"
    fi
    key=$(sha256sum < "$base/fetch.log" | cut -c1-16)
    # Every lane's home is brought up to this fetch, in place: newer and missing files are copied
    # (reflinks where the disk has them), nothing a lane extracted is touched.
    for lane in $LANES; do
      h=$lanes/$lane
      [ "$(cat "$h/.styx-fetch-key" 2> /dev/null)" = "$key" ] && continue
      mkdir -p "$h"
      for d in registry git; do
        [ ! -d "$base/$d" ] || cp -a --reflink=auto -u "$base/$d" "$h/" || die "the $lane lane's home could not be brought up to the fetch"
      done
      config > "$h/config.toml"
      echo "$key" > "$h/.styx-fetch-key"
      echo "cargo-home: the $lane lane's home is at fetch $key"
    done
    sed 's/^/cargo-home: /' "$base/fetch.log"
    echo "cargo-home: fetch $key; lane homes in $lanes"
    ;;
  home)
    lane=${2:-}
    case " $LANES " in *" $lane "*) ;; *) die "no lane $lane (lanes: $LANES)" ;; esac
    [ -f "$lanes/$lane/.styx-fetch-key" ] || die "the $lane lane has no home: run scripts/cargo-home.sh fetch"
    echo "$lanes/$lane"
    ;;
  *) die "usage: scripts/cargo-home.sh fetch | home LANE" ;;
esac
