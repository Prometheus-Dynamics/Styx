# The landing gate

`scripts/gate.sh` runs the checks a change needs before it lands, and records what ran.
`scripts/gate.py` holds the rules; this page is the short form. The shape follows Eidos's
LANE-SELECT.

## Lanes

| lane:variant | what runs |
|---|---|
| `host:checks` | format, no product names, git dependencies in the workspace only, one simulated camera, test files in binaries, shared state, one shm source |
| `host:all` | clippy and tests with all features (nextest, then the doctests), the test-group filter check |
| `host:default` | clippy with default features; the tests of the crates whose tests can differ under default features (build, list, the default-only tests) |
| `bare:thumbv7em`, `bare:riscv32imac` | the no_std check of every no_std crate for that target |
| `bare:riscv32imc` | the same on a target with no compare-and-swap: a crate with a `critical-section` feature is checked with it; then styx-frame's tests on the host on its portable-atomic path (`--cfg styx_portable_atomic`) |

A lane name alone means all its variants; `all` means every variant. When `host:default` and
`host:all` both run, the default-only list check runs too. vivid, PipeWire and lavapipe cases are
in `host:all` and `host:default`.

`host:default passed` covers clippy over every target with default features, and building and
listing the tests of the crates whose tests can differ under default features: a `cfg` or
`cfg_attr` naming `not(...)` with a feature in it, a build script that reads `CARGO_FEATURE_*`, or a
crate `.config/default-only.txt` names (`scripts/test-layout.py default-only-crates`, each built as
itself with `-p`). Every other crate's tests are the all-features pass's, and the clippy step has
type-checked them under default features. It runs only the tests listed in
`.config/default-only.txt`. That list is empty today, so the gate line records
`host:default/default-only-tests=not-run(list_empty)` beside it.

Off CI (no `GITHUB_ACTIONS`), `host:all`'s clippy runs beside its tests in `<target>/clippy`,
started as a reflink copy of the target (`STYX_GATE_CLIPPY_BESIDE=0` puts it back in front). In
one target the two serialize on cargo's build lock; side by side, the doc-line core shape's median
wall went 99.8 s to 83.7 s over 4 alternating pairs (load 42-116, nice 10), at about 30 more
CPU-s and 1.5 GB of disk for the second target. A clippy failure still fails `host:all`, named,
after the tests are awaited. CI keeps one target (it is the independent clean check).

The test-group filter check (`scripts/test-layout.py filters`) lists the tests step's binaries
through nextest's reuse-build inputs (`reuse-inputs`: binaries-metadata from the step's own list,
the workspace metadata, and the tree and each binary's size, mtime and inode), so it starts no
cargo and waits on no cargo lock. Inputs made for another tree, or whose binaries changed since,
are refused loudly; with no inputs it builds and lists with cargo as before. The tests step's
nextest run uses the same inputs (`reuse-checked`, the same refusal), so the run starts no cargo
either: no resolve on the path, and no wait on the package-cache lock beside the doctests' cargo.

Lanes that share no build run side by side, and each one's output is printed when the host lanes
end:
- host:checks reads the tree and builds nothing; it runs beside the builds.
- host:default builds in `<target>/default` with half of the build jobs left after the bare lanes,
  beside host:all in `<target>` with the other half. One after the other, the gate used 2.3 of
  the cores it could get.
- The bare lanes build in `<target>/bare` with two of the build jobs.

styx-capi's `c_proof` links the C static library, built with the package's default features in
`<target>/debug/c-proof`. The gate builds it beside the all-features test binaries, so the test
finds it current. One gate runs at a
time (`STYX_GATE_SLOTS`, default 1): two side by side each took twice as long. The gate runs at nice `STYX_NICE` (default 10), the one level every dep's heavy runs use: it raises
its own level to it, and says so when started above it. Between systemd scopes and services the
share is set by CPUWeight, not nice: a heavy run stays in a unit at the default weight (100), and no
Styx script sets CPUWeight, CPUQuota or IOWeight (checked 2026-11-27).

## The gate's own CARGO_HOME

Off CI, `gate.py run` fills the gate's own CARGO_HOME before any cargo runs
(scripts/cargo-home.sh fetch): `cargo fetch --locked` from every committed lock file into
`<area>/cache/cargo-home` (online only when something is missing), with its `fetch.log`: each lock
file's sha256, the sources, the number of registry crate files, a sha256 over their sums and the git
checkouts. The same lock files again fetch nothing. Each lane then runs offline in a home of its own,
`<area>/cache/cargo-home.lanes/<lane>` (main, clippy, default, bare, checks): copies of the fetched
home, reflinks where the disk has them, brought up to each new fetch in place. Cargo takes its
package-cache lock for writing while it resolves, even offline, so lanes sharing one home waited on
each other there; and a dependency's fingerprint holds its source path, under CARGO_HOME, so a home
stays at one path and goes with one target directory (a new path rebuilt every dependency: 1,174
CPU-s for host:all's test build). host:all keeps `main`, which gate.py sets; gate.sh's `lane_home`
sets the others and fails a lane, by name, whose home is missing. The gate never reads or writes
~/.cargo. CARGO_HOME is keyed like any CARGO_ variable (one path, one key); the seeds' key holds it
too. CI keeps the runner's CARGO_HOME (rust-cache's), and STYX_OWN_CARGO_HOME=0 keeps the caller's.

## A failing step fails its lane

Every lane's status comes from its own pass mark, written after its last step, and from the gate's
exit status (scripts/gate.py lane_status). host:checks runs scripts/gate-selftest.sh: a dry gate
(STYX_GATE_DRY) runs each step as `true` except one planted to fail (a failing command, then a
passing one), through the same lanes, subshells and waits, at each lane's first and last step; the
planted lane must come out failed and the gate red, and an unplanted dry gate green in every lane.
`gate.py run` refuses a dry gate, so one never records anything. The self-test is keyed on the files
it tests (gate.py SELFTEST_FILES, with the bash and python versions): after a green run with the
same key it is reused (`host:checks/gate-selftest=reused(<key>)` on the gate line), and it always
runs in CI and when reuse is off. It checks its own key first: a changed gate.sh must run it again.

## Nothing to run is not passed

A step meant to run something that matches nothing fails: a test-group filter term that names no
test, a `.config/default-only.txt` line that names no default-features test, a nextest run whose
expression matches no test (`--no-tests=fail`), a bare lane that finds no no_std crate. A step that
was selected and had nothing to run by design is recorded on the gate line as
`<variant>/<step>=not-run(<reason>)`, never as passed: `host:default/default-only-tests` when the
list is empty, `host:all/tests` and `host:all/doctests` when every crate reused a green run.

## What runs by default

The files changed since `git merge-base origin/next HEAD` (committed or not, untracked included)
pick the lanes:

- `scripts/gate-lanes.map` first: build files, the toolchain, `.cargo/`, `.config/` and the gate's
  scripts select every lane; `data/` selects `host:all`.
- A file of a crate selects `host:all`. It also selects `host:default` when it holds code compiled
  only without a feature (`cfg(not(feature = ..))` in it or on a `mod` that declares it), or when
  the crate, or a crate built with it, builds with a crate whose code or dependencies change with a
  feature (a feature of its own or an optional dependency): its default build can break while the
  all-features build passes. It selects `bare` when the crate is no_std or a no_std crate's library
  depends on it.
- Documentation that no code or script reads selects `host:checks`; any other file `host:all`.
- `host:checks` always runs.
- A variant more than 10 commits or 3 days behind on `origin/next`, and a blocked variant the
  change selects, are always included and cannot be dropped.

`--lanes LIST` adds lanes; `--drop LANE:REASON` (or `LANE:VARIANT:REASON`) drops a default lane
and records it as not run with the reason; `--dry-run` prints the selection.

## Reuse

In `host:all` each crate's tests are one run with a content key over what its result depends on:
its test binaries' bytes, the shared libraries they load, the files a test can read (the
repository's files other than other crates' Rust sources and manifests, documentation, and the
scripts that are no test's input: a measuring tool edited in `scripts/` reruns nothing, and does
not widen `scripts/check.sh` to every crate), the
environment its sources and its workspace dependencies' sources read (a variable naming a file is
keyed by the file's content; one naming a directory gives no key, except `XDG_RUNTIME_DIR`, keyed
by its value: the tests keep lock files there, and the PipeWire socket is a machine fact), the tools, the gate's own
scripts and the worktree's path. A script is a test's input when a crate's source names it (or a directory of its path) in a
string literal, when it is one of the gate's own files, or when an input program names it. Every
other script has no permissions while the lanes build and test (`gate.py scripts-hidden`; the gate
gives them back when it ends, and at its start after a gate stopped by force). A test that reaches
one by a path built at run time fails with a permission error, so a script left out of the key
cannot give a stale green: `scripts/key-guard-proof.sh` plants such a test and checks the run is
red on it. Scripts are hidden only in a gate worktree of the area (`<area>/wt/NAME`), never in a
checkout someone works in, never as root (root reads a file with no permissions) and never in CI;
there every script is an input. After the restore the gate fails if any script's mode differs
from the index (the exec bit is tracked), read with a stat against `git ls-files -s`: `git diff`
would refresh the index after the hide and restore and write all of it back at every gate's end.

A crate whose key equals a green run's is printed `REUSED` and not
run. An edited crate always reruns; its dependents reuse only when their test binaries load the
same bytes: a binary is keyed by its allocated (SHF_ALLOC) sections, each by name, address, flags
and bytes (a NOBITS section by its size), without the build-id, so debug info and symbol tables
do not move a key. Keyed on the whole file, 68 of 131 binaries rebuilt from an identical source
after other edits differed (incremental codegen), and 39 crates would have rerun for nothing. A
file that is not a readable 64-bit ELF is keyed by all its bytes, which never match an ELF's key.
The gate self-test checks this on a real ELF and on four broken versions of the digest.

A crate whose sources declare an outside need (`styx_frame::not_run` or `dma_case`: a device, a
daemon, a GPU) is keyed like any other, with what it meets on this machine in the key as well: the
kernel, the C compiler, the PipeWire version and whether its socket is there, the software Vulkan
driver's manifest and library, the vivid driver's build and the dma-heaps. A change to any of them
reruns every crate. A crate that gets no key always runs, printed with the reason. Keys are
recorded only after a green run, and match only within one worktree path. A reused crate's
not-run cases are those of its green run.

A change to what measures or selects reuses nothing: the map's `no-reuse` lines (the gate's
scripts, the lane map, the reuse key code, `.config/` with the filter lists, `data/`, the
simulated camera in `crates/camera/sim`, and the case declarations in `styx_frame::not_run`).
The gate prints `reuse off` with the files, and every crate's tests run. The gate's own files are
also in every key, so a key made under other gate rules never matches. The doctests follow the
same keys: when every crate is reused by key they reuse its doctest verdict too; when any crate
with a doctest target has a moved key, no key or no nextest test to key, cargo runs the doctests over
the whole workspace (`-p`, or `--exclude` for the rest, unifies features over the selected crates
only, and rebuilt dependencies for 71 s; cargo's workspace feature unification is unstable). Off
CI, `scripts/rustdoc-only.sh` stands in for rustdoc: it runs the doctests of each crate whose key
moved and of every workspace crate that builds with one (transitively, dev-dependencies
included), and names each crate it skips. The list comes from the change, not from which crates
have doctests today. With no list, or a call that is not a crate's `--test` run, it is plain
rustdoc; CI runs every crate's doctests. The listed crates' calls are not run one after the other
as cargo reaches them: the wrapper writes each call down (`STYX_DOC_COLLECT`), and once cargo ends
`gate.py doctests-run` runs them side by side, the lane's build jobs at a time, without cargo's
jobserver (gone by then), and prints each crate's output whole, in crate order. A call that fails,
a listed crate with no call, and a call that cannot be replayed (its record, directory or rustdoc
gone) each fail the step. A key is
recorded only after both its nextest run and the doctests are green. Clippy, the default-features pass and the no_std
checks always run (cargo's own freshness makes the unchanged ones cheap).

A target directory belongs to one worktree: the gate records the worktree that first built in it
(`<target>/.styx-worktree`) and refuses to run in it from any other. In a shared target, cargo
takes a workspace crate another worktree built from its newer files as current, and this tree
would be tested with that one's code. The record is never part of a seed.

A seed (scripts/cargo-seed.sh) is saved only from a target built from empty. A new toolchain
(rust-toolchain.toml, `rustc -vV`) leaves no seed any build can use, so its seed is baked outside
the gate: scripts/land.sh starts scripts/seed-bake.sh in the background when no seed has the
toolchain, and it builds clippy and the test binaries (all features and the defaults) at the landed
commit in a worktree and an empty target of its own, at nice 10, then saves the seed. The gate
never starts it (its supervisor stops whatever a step leaves running). Proven 2026-11-27: 636 s,
2196 + 243 CPU-s at load 96-121, a seed of 2048 deps/ entries.

## Records

Records live in `STYX_GATE_RECORDS` (default `~/.local/state/styx-gate`). A green run prints the
trailers to commit with (`scripts/gate.sh --trailer` prints them again; when the tree has no green
record, it prints those of the newest green tree with the same code, so a documentation edit after
the gate needs no new run):

```
Gate-Lanes: host:checks=passed host:all=passed host:default=not-run(behind) ...
Gate-Tree: <tree id>
```

The tree id is the tree the working tree would commit as (edits and untracked files, `.gitignore`
applied). Outside CI the gate keeps its own index for it, one per checkout, in
`<area>/cache/<checkout>-<hash>/gate-index` (`STYX_GATE_INDEX`): `read-tree --reset HEAD` keeps the
stat data of every unchanged file, so `add -A` hashes only what changed, and git's racy-entry check
reads a file changed within the index's own timestamp. An index git refuses is made again from
HEAD. `scripts/gate.py worktree-tree [--index FILE]` prints the tree (a fresh index without one).
The commit a record names (HEAD, when its tree is the gate's) is decided from HEAD and its tree as
they were hashed (`STYX_GATE_HEAD`): at the end of a gate only the ref is read, and a HEAD that
moved since reads its tree again.

`scripts/land.sh` pushes to `next`. It compares code only: documentation that no code or script
names is left out of every comparison, so a push that changes only such documentation needs no
gate and no trailers. Otherwise the newest commit since `origin/next` with the trailers carries the
gate, and it refuses when there is none, when the trailers differ from the green record of their
tree, or when the pushed tree differs from the tested one in code other than files `origin/next`
moved and the change did not touch. It pushes only a fast-forward. A gate whose files changed
while it ran records nothing.

No gate runs when its result is known: with only such documentation changed since the base, or
nothing, `scripts/gate.sh` and `scripts/check.sh` say so and exit 0 with no run and no record.
`--lanes` and `--catch-up` still run.

## Behind, catch-up, release

`scripts/lanes-behind.sh [REV]` prints, per variant, the last commit it passed at.
`scripts/gate.sh --catch-up` (on a clean detached checkout of `origin/next`, when nothing lands)
runs the behind variants there; a red one is bisected over its behind range and blocked until a
gate passes it.

The gate's fetch of `origin` starts no background `git maintenance run --auto` (`-c maintenance.auto=false`):
it read the disk beside the gate's own git calls. The repository's other git commands still run it.

The trailers come from the first-parent log, which is git's longest read in a gate when the repository
is on a slow disk. Outside CI the parsed log is kept in `<area>/cache/gate-first-parent.json`
(`STYX_GATE_FIRST_PARENT_CACHE`), keyed by its tip commit: a gate reads only `<tip>..origin/next`,
and the whole log again when the tip is not on that first-parent line (a rewind, a side line, a
commit git no longer has) or the file cannot be read. `scripts/gate.py first-parent-log` shows a read.

`scripts/release-check.sh COMMIT` refuses unless every variant passed at exactly that commit. It is
part of the user's tagging step and never runs a gate.

## The inner loop

`scripts/check.sh [CRATE...]` is `scripts/gate.sh --narrow`: the lanes the change selects, with the
tests narrowed to the crates that reach the changed (or named) ones and the doctests left out when
no changed file holds one. Lanes behind on `origin/next` are not added. It records no gate, only
the keys of crates whose runs were green, which a later gate can reuse. Green there is not the gate.

Continuous integration runs every lane, reuses nothing and records nothing.

CI compiles the workspace crates through sccache on the Actions cache (`.github/workflows/next.yml`:
the action pinned by commit, the binary's sha256 checked before use, `sccache --show-stats` at the
end of every run). sccache keys on content, so a cached compile can never stand in for a changed
source. A cache of the whole target directory was measured beside it and declined (2026-11-27,
branch ci-exp/cache): it is fast when it restores (gate 63-65 s), but cargo trusts mtimes, so it
needs a guard (every file at one old time, the files changed since the cached commit at now), and
what build scripts read outside git (pkg-config, system headers) must key the cache. Keyed on the
runner image and the installed packages, it missed 4 of 9 runs: hosted runners under one image
label (ubuntu26 20261116.202.1) came with three different package sets, and each miss was a full
cold build (705-1010 s), at 1.1 GB per save against the repository's 10 GB.
