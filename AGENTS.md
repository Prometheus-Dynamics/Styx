# Working on the Styx `next` branch

`next` is a clean-slate rebuild of Styx as a native-first media system, with libcamera, V4L2 and
FFmpeg kept for coverage. It started empty on purpose. `dev` is the frozen reference, and nothing
here changes it. Read [`docs/design.md`](docs/design.md) first.

The work is a pre-planned list of PRs into `next`.

## If you are picking this up

1. Read [`docs/design.md`](docs/design.md).
2. Run `python3 scripts/pr-status.py` to see what's merged, open, ready and blocked.
3. Take a PR marked ready and read its brief at `docs/prs/<id>.md`. The brief is the whole
   assignment: what it needs, what it creates, which files on `dev` to read, what to build and what
   proves it.

[`TODO.md`](TODO.md) is the full list.

## Working a PR

- Base branch `next`, working branch `pr/<id>`, one PR into `next`.
- Read `dev` as a reference: `git fetch origin dev`, then `git show origin/dev:<path>`. Bring in only
  what the brief lists, and rework it to fit. Never check out another agent's checkout.
- Read Lemnos, Daedalus and Orion `next` the same way, at the commits locked in the root
  `Cargo.toml`. Lemnos `docs/design.md` (Cameras, The frame stream) and its `contracts/camera-*`
  set the native path's order and types. Daedalus `docs/foreign-values.md` says how Styx's frame
  becomes a graph value. Orion `docs/design.md` (topics, buffers) says how a ring becomes a topic.
- Stay inside the paths your PR creates. Don't edit the root `Cargo.toml`, `TODO.md`, or a changelog.
  `Cargo.lock` is committed and the gate builds with `--locked`: a PR that changes a manifest
  commits the lock change `cargo update --workspace` makes.
- Add a crate by creating its directory under `crates/<folder>/`. Name it `styx-<directory>`.
- A crate with two or more integration-test files builds them as one binary: `tests/all.rs` has a
  module per file (`#[path = "x.rs"] mod x;`). A new `tests/x.rs` goes into `tests/all.rs`, or the
  gate fails it. A file that needs its process to itself stays its own `[[test]]`, with the reason
  beside it in `Cargo.toml`: its own `#[global_allocator]`, counts of the process (`/proc/self`,
  a shared device's counters), or a process-wide record or switch of a sim that other tests' sims
  read or write. nextest gives each test a process, but `cargo test` runs a binary's tests as
  threads of one.
- If your PR needs a shared interface changed (`crates/core/*`), stop and say so in the PR
  description. Core interfaces change only through A-series PRs.
- A need named `X<n>` is an outside block (see `docs/design.md`). Don't work around it and don't
  open PRs in other repositories for it.
- A PR merges when the lander's `scripts/gate.sh` passes. It runs the lanes the change selects
  (`docs/gate.md`), and the landed commit carries its `Gate-Lanes` and `Gate-Tree` trailers
  (`scripts/gate.sh --trailer`); `scripts/land.sh` pushes only a tip whose trailers match a green
  record. Paste the proof named in the brief into the PR description.

## Worker flow

- The landing gate is the proof. A worker does not run the full gate before pushing. It runs
  `scripts/check.sh`, the brief's proof that the change's test fails before it and passes after,
  and nothing more. Nothing lands on a red gate. `check.sh` is the gate with the lanes the change
  selects, its tests narrowed to the crates the change reaches and the doctests left out when no
  changed file holds one. It records no gate. Green there is not the gate.
- Repeat runs are set by risk, and the count is stated in the PR: many runs only for a change to
  timing or concurrency; three for a join of branches that each passed, and for an ordinary change.
- A red landing gate is read and given a named cause. Never rerun it in hope.
- Heavy jobs stay within the cores they are given: `taskset -c 6-31`, `-j 13`, and the gate's two
  slots. Don't start a second build beside your own.
- Start warm. Cargo keys every workspace crate's build on the worktree's absolute path, so a new
  worktree path compiles the whole workspace. Keep worktrees at fixed paths and reuse one, with its
  `CARGO_TARGET_DIR`, for the next item once the previous one has landed. A new worktree with its
  `CARGO_TARGET_DIR` outside it starts from the seed by default: the dependencies of the last green
  gate (`scripts/cargo-seed.sh`).
- A fail-before proof that edits a file and restores it leaves a new file time, and cargo then
  rebuilds everything downstream of that file. Don't put the old time back (`touch -r`): cargo
  judges a build current by file times, and would keep the build of the edited file. Run the
  "before" side in a second kept worktree at the base commit instead, or take the rebuild.
- A brief names its exact base commit and the files in scope, and fits in one context. Longer work
  hands off through a written state file next to the brief.
- Board runs go one runner per board, side by side with software work.

## Rules

- Native first, coverage second. A camera with a native chain runs native; otherwise libcamera;
  otherwise V4L2. One backend owns a camera at a time, and the choice is logged with its reason.
- Every backend gives the same `FrameLease`, controls and metadata.
- Movers don't know what they move. Lemnos, Orion and Daedalus carry opaque bytes with an id, a
  time and a memory location. Never ask them for a frame type, a frame route or a frame special case.
- Hardware facts are Lemnos's: V4L2 bindings, ISP layouts and tiling, hotplug, the shared ring.
  Use its library crates; don't write a second copy in Styx.
- Frame times are `CLOCK_MONOTONIC` nanoseconds.
- No library needs another to work. Orion, Daedalus, libcamera and FFmpeg are optional features.
  Default features build and run without any of them.
- Steady-state paths allocate nothing and copy nothing they don't have to. Count copies where they
  happen.
- No product, board or camera module names in code. Tuning and chip facts are data. Tuning files
  ship with Styx's install; a board build may add one.
- 3A, tuning and ISP job contents are Styx's. Talking to hardware is Lemnos's on the native path.
  Don't put register access, I2C or ioctls for native hardware in Styx.
- No servers in the library: preview, metrics and recording are consumers of frames.
- Failure rule: never give up, backoff capped at 5 to 10 s, no default auto-off, an easy manual off,
  auto-off only as a configured policy, backoff state visible. A camera that drops comes back.
- `unsafe` only in SIMD kernels, buffer mapping, the libcamera and FFmpeg bindings, with a `SAFETY`
  comment on each block.
- Boards you can't reach are marked `untested on board`. Don't hold work back for one.
- Timings count only when measured on the board named in the brief.
- Never claim something works without having run it. Report failures with their output.
- Never push tags. Never force-push `next` or `dev`. Don't change `dev`.
- Don't estimate how long anything will take. Describe size by scope and risk.
- Builds and scratch go on NVMe or SSD, never the Github HDD. Each worktree has its own
  `CARGO_TARGET_DIR`. Builds use `taskset -c 6-31` and `-j 13`.

## Board runs

Briefs marked `Runs: board` need real hardware and run in a local session on the user's machine
after the PR merges. On the Raze, take `/run/lock/raze-gpu.lock` and stop `helios-engine` during
the run, then start it again.
