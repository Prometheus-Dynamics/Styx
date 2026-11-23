# The landing gate

`scripts/gate.sh` runs the checks a change needs before it lands, and records what ran.
`scripts/gate.py` holds the rules; this page is the short form. The shape follows Eidos's
LANE-SELECT.

## Lanes

| lane:variant | what runs |
|---|---|
| `host:checks` | format, no product names, git dependencies in the workspace only, one simulated camera, test files in binaries, shared state, one shm source |
| `host:all` | clippy and tests with all features (nextest, then the doctests), the test-group filter check |
| `host:default` | clippy and tests with default features (the build, and the default-only tests) |
| `bare:thumbv7em`, `bare:riscv32imac` | the no_std check of every no_std crate for that target |
| `bare:riscv32imc` | the same on a target with no compare-and-swap: a crate with a `critical-section` feature is checked with it; then styx-frame's tests on the host on its portable-atomic path (`--cfg styx_portable_atomic`) |

A lane name alone means all its variants; `all` means every variant. When `host:default` and
`host:all` both run, the default-only list check runs too. vivid, PipeWire and lavapipe cases are
in `host:all` and `host:default`.

`host:default passed` covers clippy and building every default-features test binary. It runs only
the tests listed in `.config/default-only.txt`. That list is empty today, so the gate line records
`host:default/default-only-tests=not-run(list_empty)` beside it.

## Nothing to run is not passed

A step meant to run something that matches nothing fails: a test-group filter term that names no
test, a `.config/default-only.txt` line that names no default-features test, a nextest run whose
expression matches no test (`--no-tests=fail`), a bare lane that finds no no_std crate. A step that
was selected and had nothing to run by design is recorded on the gate line as
`<variant>/<step>=not-run(<reason>)`, never as passed: `host:default/default-only-tests` when the
list is empty, `host:all/tests` when every crate reused a green run.

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
- Documentation that no code or script names selects `host:checks`; any other file `host:all`.
- `host:checks` always runs.
- A variant more than 10 commits or 3 days behind on `origin/next`, and a blocked variant the
  change selects, are always included and cannot be dropped.

`--lanes LIST` adds lanes; `--drop LANE:REASON` (or `LANE:VARIANT:REASON`) drops a default lane
and records it as not run with the reason; `--dry-run` prints the selection.

## Reuse

In `host:all` each crate's tests are one run with a content key over what its result depends on:
its test binaries' bytes, the shared libraries they load, the files a test can read (the
repository's files other than other crates' Rust sources and manifests and documentation), the
environment its sources and its workspace dependencies' sources read (a variable naming a file is
keyed by the file's content; one naming a directory gives no key), the tools, the gate's own
scripts and the worktree's path. A crate whose key equals a green run's is printed `REUSED` and not
run. An edited crate always reruns; its dependents reuse only when their binaries come out
byte-identical.

A crate whose sources declare an outside need (`styx_frame::not_run` or `dma_case`: a device, a
daemon, a GPU) always runs, and so does a crate that gets no key; each is printed with the reason.
Keys are recorded only after a green run, and match only within one worktree path.

A change to what measures or selects reuses nothing: the map's `no-reuse` lines (the gate's
scripts, the lane map, the reuse key code, `.config/` with the filter lists, `data/`, the
simulated camera in `crates/camera/sim`, and the case declarations in `styx_frame::not_run`).
The gate prints `reuse off` with the files, and every crate's tests run. The gate's own files are
also in every key, so a key made under other gate rules never matches. The doctests,
clippy, the default-features pass and the no_std checks always run (cargo's own freshness makes
the unchanged ones cheap).

A target directory belongs to one worktree: the gate records the worktree that first built in it
(`<target>/.styx-worktree`) and refuses to run in it from any other. In a shared target, cargo
takes a workspace crate another worktree built from its newer files as current, and this tree
would be tested with that one's code. The record is never part of a seed.

## Records

Records live in `STYX_GATE_RECORDS` (default `~/.local/state/styx-gate`). A green run prints the
trailers to commit with (`scripts/gate.sh --trailer` prints them again):

```
Gate-Lanes: host:checks=passed host:all=passed host:default=not-run(behind) ...
Gate-Tree: <tree id>
```

`scripts/land.sh` pushes to `next`: it refuses a tip without the trailers, with trailers that
differ from the green record of their tree, or with a tree that differs from the tested one other
than by files `origin/next` moved and the change did not touch. It pushes only a fast-forward. A
gate whose files changed while it ran records nothing.

## Behind, catch-up, release

`scripts/lanes-behind.sh [REV]` prints, per variant, the last commit it passed at.
`scripts/gate.sh --catch-up` (on a clean detached checkout of `origin/next`, when nothing lands)
runs the behind variants there; a red one is bisected over its behind range and blocked until a
gate passes it.

`scripts/release-check.sh COMMIT` refuses unless every variant passed at exactly that commit. It is
part of the user's tagging step and never runs a gate.

## The inner loop

`scripts/check.sh [CRATE...]` is `scripts/gate.sh --narrow`: the lanes the change selects, with the
tests narrowed to the crates that reach the changed (or named) ones and the doctests left out when
no changed file holds one. Lanes behind on `origin/next` are not added. It records no gate, only
the keys of crates whose runs were green, which a later gate can reuse. Green there is not the gate.

Continuous integration runs every lane, reuses nothing and records nothing.
